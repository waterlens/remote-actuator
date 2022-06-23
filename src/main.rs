use std::{env, fs, io, path::PathBuf, str::FromStr, sync::Arc};

use axum::{
    body::{Body, StreamBody},
    extract::{Extension, Path},
    http::{header, Request, Response, StatusCode},
    middleware::{self, Next},
    response::{AppendHeaders, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use colored::Colorize;
use duckscript::{runner, types::runtime::Context};
use either::Either::{self, Left, Right};
use env_logger::Env;
use log::{error, info, warn};
use once_cell::sync::OnceCell;
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    ConnectOptions, Connection, SqlitePool,
};
use strum_macros::AsRefStr;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::io::ReaderStream;
use toml::value::Datetime;
use uuid::Uuid;
use walkdir::WalkDir;

#[derive(Debug, Deserialize)]
struct LocalConfigWrapper {
    config: LocalConfig,
}

#[derive(Debug, Deserialize)]
struct LocalConfig {
    listen: String,
    database: PathBuf,
    workdir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct BundleConfig {
    task: Task,
}

#[derive(Debug, Deserialize)]
struct Task {
    time: Datetime,
    nonce: String,
    run: Run,
    commit: Option<String>,
    content: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Run {
    before: Option<String>,
    script: String,
    after: Option<String>,
}

#[derive(Debug, Serialize)]
struct ExecResult {
    code: i32,
    stdout: Option<String>,
    stderr: Option<String>,
    extra: Option<String>,
}

#[derive(Debug, Serialize)]
struct TaskResult {
    before: Option<ExecResult>,
    after: Option<ExecResult>,
    script: ExecResult,
}

#[derive(Debug, Serialize)]
struct ExecutionRecord {
    time: String,
    uuid: Uuid,
    commit: Option<String>,
    result: Vec<(PathBuf, Either<TaskResult, ScriptExecutionError>)>,
}

#[derive(Debug, Serialize, AsRefStr)]
enum BundleExecutionError {
    BundleDecodingFailed,
    BundleDirCleanFailed,
    BundleDirCreationFailed,
    UnpackingFailed,
    BundleConfigNotFound,
    BundleConfigNotAFile,
    ReadConfigFailed,
    ConfigWrongFormat,
    NoContentDir,
    UnableToChangeCurrentDir,
    DatabaseQueryFailed,
    NonceExisted,
}

#[derive(Debug, Serialize)]
enum ScriptExecutionError {
    DuckScriptInitializationFailed,
    BeforeScriptFailed,
    ScriptFailed,
    AfterScriptFailed,
    NoOutputExitCode,
    ExitCodeIsNotNumber,
}

static CONFIG: OnceCell<LocalConfig> = OnceCell::new();
static DATABASE: OnceCell<SqlitePool> = OnceCell::new();

impl LocalConfigWrapper {
    fn read_config() -> io::Result<Self> {
        let content = fs::read_to_string("./config.toml")?;
        Ok(toml::from_str(&content)?)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TaskDescription {
    task_bundle: String,
    task_signature: String,
}

#[tokio::main]
async fn main() {
    let env = Env::default().default_filter_or("info,sqlx::query=warn");
    env_logger::init_from_env(env);
    info!("Remote actuator is starting up");

    CONFIG
        .set(
            LocalConfigWrapper::read_config()
                .expect("expect a right config.toml file")
                .config,
        )
        .unwrap();

    info!("Reading configure file done");

    let db = &CONFIG.get().unwrap().database;
    let db_uri = "sqlite:".to_string() + db.to_str().expect("unable to make database str");
    let db = SqliteConnectOptions::from_str(db_uri.as_str())
        .expect("unable to create connection options")
        .create_if_missing(true)
        .connect()
        .await
        .expect("unable to connect to sqlite database");

    db.close();
    let db = SqlitePoolOptions::new()
        .connect(db_uri.as_str())
        .await
        .expect("unable to connect to sqlite database");

    sqlx::query(
        "\
CREATE TABLE IF NOT EXISTS history ( 
    uuid      TEXT PRIMARY KEY,
    ct        TEXT,
    time      TEXT,
    result    TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS nonce (
    nonce     TEXT PRIMARY KEY
);
CREATE INDEX IF NOT EXISTS commit_index ON history (
    ct
);
",
    )
    .execute(&db)
    .await
    .expect("create table failed");

    DATABASE.set(db).unwrap();

    info!("Connected to the database");

    let semaphore = Arc::new(Semaphore::new(1));
    let app = Router::new()
        .route("/tasks", post(post_task))
        .route("/tasks/uuid/:uuid", get(get_task_by_uuid))
        .route("/tasks/commit/:commit", get(get_task_by_commit))
        .route("/database", get(get_database))
        .layer(Extension(semaphore))
        .layer(middleware::from_fn(print_request_response));

    let addr_port = &CONFIG
        .get()
        .unwrap()
        .listen
        .parse()
        .expect("expect a valid listening address");

    let server = axum::Server::bind(addr_port).serve(app.into_make_service());
    info!("Listening on {}", addr_port.to_string().green());

    server.await.unwrap();
}

async fn print_request_response(
    req: Request<Body>,
    next: Next<Body>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let (parts, body) = req.into_parts();
    info!("{} {}", parts.method.to_string().blue(), parts.uri);
    let req = Request::from_parts(parts, body);
    let res = next.run(req).await;
    let (parts, body) = res.into_parts();
    info!("{}", parts.status);
    let res = Response::from_parts(parts, body);
    Ok(res)
}

async fn get_database() -> impl IntoResponse {
    let db = &CONFIG.get().unwrap().database;
    let file = match tokio::fs::File::open(db).await {
        Ok(file) => file,
        Err(_) => return Err(StatusCode::NOT_FOUND),
    };

    let stream = ReaderStream::new(file);
    let body = StreamBody::new(stream);

    let headers = AppendHeaders([
        (header::CONTENT_TYPE, "application/octet-stream"),
        (
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"data.db\"",
        ),
    ]);

    Ok((headers, body))
}

async fn get_task_by_uuid(Path(uuid): Path<Uuid>) -> Result<String, StatusCode> {
    let uuid_s = uuid.as_simple().to_string();
    let tasks: Vec<_> = sqlx::query!("SELECT * FROM history WHERE history.uuid = ?;", uuid_s)
        .fetch_all(DATABASE.get().unwrap())
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?
        .into_iter()
        .map(|record| record.result)
        .collect();

    Ok(format!("[{}]", tasks.join(",")))
}

async fn get_task_by_commit(Path(commit): Path<String>) -> Result<String, StatusCode> {
    let commit_s = commit.as_str();
    let tasks: Vec<_> = sqlx::query!("SELECT * FROM history WHERE history.ct = ?;", commit_s)
        .fetch_all(DATABASE.get().unwrap())
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?
        .into_iter()
        .map(|record| record.result)
        .collect();

    Ok(format!("[{}]", tasks.join(",")))
}

async fn post_task(
    Json(payload): Json<TaskDescription>,
    Extension(semaphore): Extension<Arc<Semaphore>>,
) -> Result<String, StatusCode> {
    if semaphore.available_permits() == 0 {
        warn!("Service is unavailable");
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    let permission = semaphore
        .acquire_owned()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let b = &payload.task_bundle;
    let s = &payload.task_signature;

    let s = base64::decode(s.as_bytes()).map_err(|_| StatusCode::BAD_REQUEST)?;
    let h = Sha512::digest(b.as_bytes());
    if h.as_slice() != s.as_slice() {
        warn!("Invalid digest");
        return Err(StatusCode::FORBIDDEN);
    }

    let uuid = Uuid::new_v4();
    info!("New task with uuid {}", uuid.as_simple().to_string());
    std::thread::spawn(move || run_bundle(uuid, payload.task_bundle, permission));

    Ok(uuid.as_simple().to_string())
}

fn run_bundle(uuid: Uuid, bundle: String, permission: OwnedSemaphorePermit) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    match run_bundle_impl(uuid, bundle, permission) {
        Ok(result) => {
            let json = serde_json::to_string(&result).expect("unable convert to json string");
            let uuid = uuid.as_simple().to_string();
            let time = result.time.to_string();

            info!("Run bundle {} succeeded", uuid.as_str());

            rt.block_on(
                sqlx::query!(
                    "INSERT INTO history (uuid, ct, time, result) VALUES (?, ?, ?, ?);",
                    uuid,
                    result.commit,
                    time,
                    json,
                )
                .execute(DATABASE.get().unwrap()),
            )
            .expect("insert result failed");
        }
        Err(err) => {
            let json = serde_json::to_string(&err).expect("unable convert to json string");
            let uuid = uuid.as_simple().to_string();

            error!(
                "Run bundle {} failed due to {}",
                uuid.as_str(),
                err.as_ref()
            );

            rt.block_on(
                sqlx::query!(
                    "INSERT INTO history (uuid, result) VALUES (?, ?);",
                    uuid,
                    json
                )
                .execute(DATABASE.get().unwrap()),
            )
            .expect("insert error failed");
        }
    }
}

fn validate_bundle_configure(cfg: &BundleConfig) -> Result<(), BundleExecutionError> {
    use BundleExecutionError::*;
    let task = &cfg.task;

    let nonce = task.nonce.as_str();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let nonce_exists: i32 = rt
        .block_on(
            sqlx::query_scalar!(
                "SELECT EXISTS (SELECT 1 FROM nonce WHERE nonce.nonce = ?)",
                nonce
            )
            .fetch_one(DATABASE.get().unwrap()),
        )
        .map_err(|_| DatabaseQueryFailed)?;

    if nonce_exists != 0 {
        return Err(NonceExisted);
    }

    rt.block_on(
        sqlx::query!("INSERT INTO nonce VALUES (?)", nonce).execute(DATABASE.get().unwrap()),
    )
    .map_err(|_| DatabaseQueryFailed)?;

    Ok(())
}

fn run_bundle_impl(
    uuid: Uuid,
    bundle: String,
    _permission: OwnedSemaphorePermit,
) -> Result<ExecutionRecord, BundleExecutionError> {
    use BundleExecutionError::*;

    let bundle_dir_path = &CONFIG.get().unwrap().workdir;
    if bundle_dir_path.exists() {
        info!("Remove previous bundle directory");
        fs::remove_dir_all(bundle_dir_path.as_path()).map_err(|_| BundleDirCleanFailed)?;
    }
    fs::create_dir_all(bundle_dir_path.as_path()).map_err(|_| BundleDirCreationFailed)?;

    info!("Start to decode bundle content");
    let bundle = base64::decode(bundle.as_str()).map_err(|_| BundleDecodingFailed)?;
    let lz4_decoder = lz4_flex::frame::FrameDecoder::new(bundle.as_slice());
    let mut ar = tar::Archive::new(lz4_decoder);
    ar.set_preserve_permissions(true);
    ar.set_overwrite(true);
    ar.set_ignore_zeros(true);
    ar.set_preserve_mtime(true);
    ar.unpack(bundle_dir_path.as_path()).map_err(|err| {
        dbg!(err);
        UnpackingFailed
    })?;

    let bundle_config_path = bundle_dir_path.join("./bundle.toml");
    let meta = fs::metadata(bundle_config_path.as_path()).map_err(|_| BundleConfigNotFound)?;
    if !meta.is_file() {
        return Err(BundleConfigNotAFile);
    }

    info!(
        "Read bundle configure at {}",
        bundle_config_path
            .canonicalize()
            .unwrap_or_default()
            .as_os_str()
            .to_str()
            .unwrap_or_default()
            .blue()
    );

    let config: BundleConfig = toml::from_str(
        fs::read_to_string(bundle_config_path.as_path())
            .map_err(|_| ReadConfigFailed)?
            .as_str(),
    )
    .map_err(|_| ConfigWrongFormat)?;

    validate_bundle_configure(&config)?;

    let task = &config.task;

    let bundle_content_path = bundle_dir_path.join(&task.content);
    let meta = fs::metadata(bundle_content_path.as_path()).map_err(|_| NoContentDir)?;
    if !meta.is_dir() {
        return Err(NoContentDir);
    }

    let current_dir = env::current_dir().map_err(|_| UnableToChangeCurrentDir)?;
    env::set_current_dir(bundle_content_path.as_path()).map_err(|_| UnableToChangeCurrentDir)?;
    info!(
        "Change to working directory {}",
        env::current_dir()
            .map_err(|_| UnableToChangeCurrentDir)?
            .as_os_str()
            .to_str()
            .unwrap_or_default()
            .blue()
    );

    let entries: Vec<_> = WalkDir::new(".")
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .collect();

    info!("The bundle has {} entries", entries.len());

    let final_result: Vec<(_, _)> = entries
        .iter()
        .zip(
            entries
                .iter()
                .map(|entry| -> Result<TaskResult, ScriptExecutionError> {
                    use ScriptExecutionError::*;
                    let path = entry.path();
                    info!(
                        "{}",
                        "------------------------------------------------------------".yellow()
                    );
                    info!(
                        ":: {}",
                        path.as_os_str().to_str().unwrap_or_default().blue()
                    );

                    let mut ctx = Context::new();
                    duckscriptsdk::load(&mut ctx.commands)
                        .map_err(|_| DuckScriptInitializationFailed)?;

                    let s = "ra.content.path";
                    let path = path.as_os_str().to_string_lossy().to_string();
                    ctx.variables.insert(s.to_string(), path);

                    let mk_exec_result_from_ctx =
                        |ctx: &Context| -> Result<_, ScriptExecutionError> {
                            let extra = ctx.variables.get("ra.result.extra").cloned();
                            let stderr = ctx.variables.get("ra.result.stderr").cloned();
                            let stdout = ctx.variables.get("ra.result.stdout").cloned();

                            let s_code = "ra.result.code";
                            let code = ctx
                                .variables
                                .get("ra.result.code")
                                .ok_or(NoOutputExitCode)?
                                .parse::<i32>()
                                .map_err(|_| ExitCodeIsNotNumber)?;
                            info!(":: {} = {}", s_code.green(), code);

                            Ok(ExecResult {
                                extra,
                                code,
                                stdout,
                                stderr,
                            })
                        };

                    let before_script_out = if let Some(before) = &task.run.before {
                        ctx = runner::run_script(before.as_str(), ctx)
                            .map_err(|_| BeforeScriptFailed)?;
                        let r = mk_exec_result_from_ctx(&ctx)?;
                        info!(":: Running before script was done");
                        Some(r)
                    } else {
                        None
                    };

                    ctx = runner::run_script(task.run.script.as_str(), ctx)
                        .map_err(|_| ScriptFailed)?;
                    let script_out = mk_exec_result_from_ctx(&ctx)?;
                    info!(":: Running script was done");

                    let after_script_out = if let Some(after) = &task.run.after {
                        ctx = runner::run_script(after.as_str(), ctx)
                            .map_err(|_| AfterScriptFailed)?;
                        let r = mk_exec_result_from_ctx(&ctx)?;
                        info!(":: Running after script was done");
                        Some(r)
                    } else {
                        None
                    };

                    Ok(TaskResult {
                        before: before_script_out,
                        after: after_script_out,
                        script: script_out,
                    })
                })
                .map(|res| res.map_or_else(Right, Left)),
        )
        .map(|(entry, result)| (entry.path().to_path_buf(), result))
        .collect();

    let record = ExecutionRecord {
        time: task.time.to_string(),
        commit: task.commit.clone(),
        result: final_result,
        uuid,
    };

    info!(
        "Recover to normal directory {}",
        current_dir.as_os_str().to_str().unwrap_or_default().blue()
    );
    env::set_current_dir(current_dir.as_path()).map_err(|_| UnableToChangeCurrentDir)?;

    Ok(record)
}

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::Extension;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use base64::decode;
use ed25519_dalek::PublicKey;
use ed25519_dalek::Signature;
use ed25519_dalek::Verifier;
use lz4_flex::decompress;
use once_cell::sync::OnceCell;
use serde_derive::Deserialize;
use serde_derive::Serialize;
use sha2::Digest;
use sha2::Sha512;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

#[derive(Debug, Serialize, Deserialize)]
struct LocalConfig {
    listen: String,
    key: String,
    database: PathBuf,
    workdir: PathBuf,
}

static CONFIG: OnceCell<LocalConfig> = OnceCell::new();
static PKEY: OnceCell<PublicKey> = OnceCell::new();

impl LocalConfig {
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
    CONFIG
        .set(LocalConfig::read_config().expect("expect a config.toml file"))
        .unwrap();

    let pub_key = &CONFIG.get().unwrap().key;
    let pub_key = decode(pub_key).expect("expect a right ed25519 public key");
    PKEY.set(PublicKey::from_bytes(&pub_key).expect("expect a right ed25519 public key"))
        .unwrap();

    let semaphore = Arc::new(Semaphore::new(1));
    let app = Router::new()
        .route("/tasks", post(post_task))
        .layer(Extension(semaphore));

    axum::Server::bind(
        &CONFIG
            .get()
            .unwrap()
            .listen
            .parse()
            .expect("expect a valid listening address"),
    )
    .serve(app.into_make_service())
    .await
    .unwrap();
}

async fn post_task(
    Json(payload): Json<TaskDescription>,
    Extension(semaphore): Extension<Arc<Semaphore>>,
) -> Result<(), StatusCode> {
    if semaphore.available_permits() == 0 {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    let permission = semaphore
        .acquire_owned()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let b = &payload.task_bundle;
    let s = &payload.task_signature;

    let pk = PKEY.get().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let s = decode(s.as_bytes()).map_err(|_| StatusCode::BAD_REQUEST)?;
    let s = Signature::from_bytes(s.as_slice()).map_err(|_| StatusCode::BAD_REQUEST)?;
    let h = Sha512::digest(b.as_bytes());
    pk.verify(h.as_slice(), &s)
        .map_err(|_| StatusCode::FORBIDDEN)?;

    std::thread::spawn(move || {
        run_bundle(payload.task_bundle, permission);
    });

    Ok(())
}

fn run_bundle(bundle: String, permission: OwnedSemaphorePermit) {
    run_bundle_impl(bundle, permission).expect_err("run bundle failed");
}

fn run_bundle_impl(bundle: String, permission: OwnedSemaphorePermit) -> Result<(), ()>{
    todo!()
}
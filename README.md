# Remote Actuator

This actuator is used to control a remote device to execute some tasks.

## API

- `POST /tasks`

  ```json
  {
      "task_bundle": "...",
      "task_signature": "..."
  }
  ```

- `GET /tasks/uuid/{uuid}`

- `GET /tasks/commit/{commit-hash}`

- `GET /database/{key}`


## Task Bundle Configure Example

```toml
[task]
uuid = "00000000-0000-0000-0000-000000000000"
commit = "df6cc0ec563fd50e6e9fd6f1d6b9d1a315fc5402"
time = 1979-05-27T07:32:00Z
repeat = 3

[task.run.before]
script = """
"""

[task.run]
script = """
"""

[task.run.after]
script = """
"""
```

## Configure Example

```toml
[config]
listen = "[::]:8668"
key = "AAAAC3NzaC1lZDI1NTE5AAAAIFT96jZ2GsimHfkhIjUA0FejiqlRfHIcOlgctZpSOfsG"
database = "/run/remote-actuator/history.sqlite"
workdir = "/run/remote-actuator/runs/"
```

# Configuration Reference

This document provides a comprehensive reference for all configuration options supported by `dvb`.

`dvb` uses [TOML](https://toml.io) configuration. By default, `dvb` looks for its configuration at `/etc/dvb/config.toml`. You can specify a different path using the `--config <PATH>` CLI option or the `DVB_CONFIG` environment variable.

You can generate a full, commented reference configuration file by running:
```sh
dvb init -o dvb.toml
```

---

## Table of Contents

- [Global Settings](#global-settings)
- [Docker Configuration](#docker-configuration)
- [Job Settings (`[[job]]`)](#job-settings-job)
  - [Schedule (`cron` vs `crontext`)](#schedule-cron-vs-crontext)
  - [Timezone & Startup](#timezone--startup)
  - [File Archiving & Retention](#file-archiving--retention)
  - [Container Stopping](#container-stopping)
  - [Storage Backends (`[job.storage]`)](#storage-backends-jobstorage)
    - [Filesystem (`type = "fs"`)](#filesystem-type--fs)
    - [S3 Compatible (`type = "s3"`)](#s3-compatible-type--s3)
    - [SFTP (`type = "sftp"`)](#sftp-type--sftp)
    - [Dropbox (`type = "dropbox"`)](#dropbox-type--dropbox)
  - [Restore Defaults (`[job.restore]`)](#restore-defaults-jobrestore)
  - [Hooks (`[[job.pre]]`, `[[job.post]]`)](#hooks-jobpre-jobpost)
- [Environment Overrides & Secrets](#environment-overrides--secrets)

---

## Global Settings

| Option | Type | Default | Description |
|---|---|---|---|
| `shutdown_grace_secs` | integer | `60` | Maximum time in seconds the scheduler daemon (`dvb run`) waits on `SIGTERM` for active backup jobs to finish before cancelling them. |

Example:
```toml
shutdown_grace_secs = 60
```

---

## Docker Configuration

Container management and container hook execution require access to the Docker daemon.

```toml
[docker]
socket = "/var/run/docker.sock"
```

| Option | Type | Default | Description |
|---|---|---|---|
| `socket` | string | *None* | Path to the Docker unix socket. If omitted, container control (stopping containers, container hooks) is disabled. |

---

## Job Settings (`[[job]]`)

Backup configurations contain one or more `[[job]]` array tables.

### Schedule (`cron` vs `crontext`)

Each job must define **exactly one** of `cron` or `crontext`. Specifying both or neither is an error.

| Option | Type | Description |
|---|---|---|
| `cron` | string | Standard 5-field cron expression (minute, hour, day-of-month, month, day-of-week). E.g. `"0 3 * * *"`. |
| `crontext` | string | Human-friendly schedule expression. E.g. `"every day at 03:00"` or `"every friday at 18:00"`. See [`docs/crontext.md`](crontext.md) for details. |

### Timezone & Startup

| Option | Type | Default | Description |
|---|---|---|---|
| `timezone` | string | System `TZ` or `UTC` | Timezone in IANA format (e.g. `"Europe/Berlin"`, `"America/New_York"`) used to evaluate schedules and compute next fire times. |
| `run_on_start` | boolean | `false` | When `true`, executes this job immediately when the daemon (`dvb run`) starts, prior to waiting for the first scheduled cron run. |

### File Archiving & Retention

| Option | Type | Default | Description |
|---|---|---|---|
| `name` | string | *Required* | Unique name for the job. Must match `^[a-z0-9][a-z0-9_-]{0,62}$` (alphanumeric, underscores, hyphens; max 63 characters). |
| `source` | array of strings | *Required* | List of paths to include in the backup archive. Non-existent paths trigger a validation error during runtime operations. |
| `filename` | string | *Required* | Archive filename pattern containing `strftime` timestamp specifiers. Must include year (`%Y`) and seconds (`%S`), e.g. `"backup-%Y%m%dT%H%M%SZ.tar.zst"`. |
| `compression` | string | `"zstd"` | Archive compression format. Supported values: `"zstd"`, `"gzip"`, `"none"`. |
| `retention_days` | integer | `14` | Backups older than this number of days are candidates for pruning. |
| `min_keep` | integer | `3` | Minimum number of newest backups to keep regardless of age. The newest backup is never deleted. |
| `follow_symlinks` | boolean | `false` | When `true`, follows symlinks and stores target file contents. When `false`, stores symlinks as symbolic links. |

### Container Stopping

Ensure database and filesystem consistency by pausing target containers while archiving.

| Option | Type | Default | Description |
|---|---|---|---|
| `stop_containers` | array of strings | `[]` | List of container names or ID prefixes to stop before archiving and restart afterward. Requires `[docker].socket`. |
| `stop_label` | string | *None* | Label selector matching containers to stop and restart, formatted as `key` or `key=value`. Requires `[docker].socket`. |
| `stop_timeout_secs` | integer | `30` | Seconds to wait for containers to stop gracefully on `SIGTERM` before sending `SIGKILL`. |

---

## Storage Backends (`[job.storage]`)

Each job must define one storage table `[job.storage]` specifying `type`.

### Filesystem (`type = "fs"`)

Stores backups in a local filesystem or mounted network share.

```toml
[job.storage]
type = "fs"
root = "/backups"
prefix = "db"
```

| Option | Type | Default | Description |
|---|---|---|---|
| `type` | string | `"fs"` | Backend identifier. |
| `root` | string | *Required* | Base directory path where backups will be stored. |
| `prefix` | string | `""` | Subdirectory prefix inside `root`. |

### S3 Compatible (`type = "s3"`)

Uploads backups to AWS S3, Cloudflare R2, MinIO, Wasabi, SeaweedFS, or other S3-compatible object stores.

```toml
[job.storage]
type = "s3"
bucket = "my-backup-bucket"
region = "us-east-1"
prefix = "backups"
endpoint = "http://minio:9000"
access_key_id = "..."
secret_access_key = "..."
force_path_style = true
```

| Option | Type | Default | Description |
|---|---|---|---|
| `type` | string | `"s3"` | Backend identifier. |
| `bucket` | string | *Required* | S3 bucket name. |
| `region` | string | *Required* | S3 region (e.g. `"us-east-1"`, `"auto"`). |
| `prefix` | string | `""` | Key prefix/folder inside the bucket. |
| `endpoint` | string | *None* | Custom endpoint URL for non-AWS S3 providers. |
| `access_key_id` | string | *Ambient* | Access key ID. If omitted, uses AWS ambient credentials. |
| `secret_access_key` | string | *Ambient* | Secret access key. |
| `force_path_style` | boolean | `true` | When `true`, addresses the bucket as `endpoint/bucket` rather than `bucket.endpoint`. |

### SFTP (`type = "sftp"`)

Uploads backups to a remote server using SFTP over SSH.

```toml
[job.storage]
type = "sftp"
endpoint = "sftp.example.com:22"
user = "backupuser"
root = "/remote/backups"
key_path = "/run/secrets/id_ed25519"
known_hosts_strategy = "strict"
```

| Option | Type | Default | Description |
|---|---|---|---|
| `type` | string | `"sftp"` | Backend identifier. |
| `endpoint` | string | *Required* | SFTP server address as `host:port`. |
| `user` | string | *Required* | SSH username. |
| `root` | string | *Required* | Remote base directory. |
| `key_path` | string | *Required* | Path to the private SSH key file. |
| `known_hosts_strategy` | string | `"strict"` | Host key verification mode: `"strict"` (fails on unknown host) or `"accept_new"` (records host key on first connect). |

### Dropbox (`type = "dropbox"`)

Uploads backups to Dropbox using OAuth 2.0 refresh tokens.

```toml
[job.storage]
type = "dropbox"
root = "/backups"
client_id = "/run/secrets/dropbox_client_id"
client_secret = "/run/secrets/dropbox_client_secret"
refresh_token = "/run/secrets/dropbox_refresh_token"
```

| Option | Type | Default | Description |
|---|---|---|---|
| `type` | string | `"dropbox"` | Backend identifier. |
| `root` | string | *Required* | Destination directory in Dropbox. |
| `client_id` | string | *Required* | Dropbox OAuth app client ID. |
| `client_secret` | string | *Required* | Dropbox OAuth app client secret. |
| `refresh_token` | string | *Required* | OAuth refresh token. |

---

## Restore Defaults (`[job.restore]`)

Configures default values for the `dvb restore <job>` command.

```toml
[job.restore]
dir = "/restore"
script = "/scripts/pg_restore.sh"
script_timeout_secs = 3600
cleanup = true
```

| Option | Type | Default | Description |
|---|---|---|---|
| `dir` | string | *None* | Default directory to extract restored files into. Can be overridden with `--to <dir>`. |
| `script` | string | *None* | Path to post-extraction hook script. Can be overridden with `--script <path>`. |
| `script_timeout_secs` | integer | `300` | Maximum execution time in seconds for the restore script before SIGKILL. |
| `cleanup` | boolean | `false` | If `true`, removes the extracted staging directory after the restore script finishes successfully. |

See [`docs/restore.md`](restore.md) for full details on the restore flow and environment variables.

---

## Hooks (`[[job.pre]]`, `[[job.post]]`)

Hooks run commands before and after archive creation.

```toml
[[job.pre]]
cmd = ["pg_dump", "-U", "postgres", "-f", "/backup/pgdata/dump.sql"]
container = "postgres"
timeout_secs = 300

[[job.post]]
cmd = ["/bin/notify.sh", "Database backup complete"]
run_on = "always"
timeout_secs = 60
```

| Option | Type | Default | Description |
|---|---|---|---|
| `cmd` | array of strings | *Required* | Command and arguments vector executed directly (without a shell). |
| `container` | string | *None* | Container name or ID to execute the command inside via Docker exec. If omitted, runs in the local `dvb` process/container. Requires `[docker].socket`. |
| `timeout_secs` | integer | `300` | Process execution timeout in seconds before termination. |
| `run_on` | string | `"success"` | (*Post-hooks only*) When to run the hook: `"success"` (default), `"failure"`, or `"always"`. |

Hooks receive execution context via environment variables:
- `DVB_JOB`: Job name
- `DVB_STATUS`: Status (`pending`, `success`, `failure`)
- `DVB_ARCHIVE`: Destination object name
- `DVB_ERROR`: Error description if failed

---

## Environment Overrides & Secrets

Settings can be overridden via environment variables using double underscores (`__`) as table separators:

| Environment Variable | Equivalent TOML Setting |
|---|---|
| `DVB_CONFIG` | Config path (default `/etc/dvb/config.toml`) |
| `DVB_LOG` | Tracing filter directive (e.g. `dvb=debug`) |
| `DVB_LOG_FORMAT` | Log format: `text` or `json` |
| `DVB_LOCK_DIR` | Lock directory (default `/run/dvb`) |
| `DVB__SHUTDOWN_GRACE_SECS` | `shutdown_grace_secs` |
| `DVB__JOB__0__STORAGE__SECRET_ACCESS_KEY` | `[[job]][0].storage.secret_access_key` |

All sensitive keys (passwords, secret access keys, tokens) are wrapped in redacting structures in `dvb`, preventing credentials from leaking in logs or CLI outputs.

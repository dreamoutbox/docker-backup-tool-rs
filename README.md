# dvb (Docker Volume Backup)

[![CI](https://github.com/dreamoutbox/docker-backup-tool-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/dreamoutbox/docker-backup-tool-rs/actions/workflows/ci.yml)

`dvb` is a Docker volume backup utility written in Rust. It tars and compresses mounted directory paths, streams archives to remote storage backends (S3, SFTP, Dropbox, or local filesystem), enforces retention policies, and coordinates container lifecycles and hook scripts.

---

## Quick Start (Docker Compose)

Run `dvb` as a background daemon alongside your application containers:

```yaml
services:
  # Application database
  db:
    image: postgres:18-alpine
    container_name: db
    environment:
      POSTGRES_USER: postgres
      POSTGRES_PASSWORD: secretpassword
      POSTGRES_DB: app
    volumes:
      - pgdata:/var/lib/postgresql

  # dvb backup daemon
  backup:
    image: dreamoutbox/dvb:latest
    container_name: backup
    restart: unless-stopped
    volumes:
      # Data volumes to back up (mount :ro for safety)
      - pgdata:/backup/pgdata:ro
      # dvb configuration file
      - ./dvb.toml:/etc/dvb/config.toml:ro
      # Docker socket for container stop/start/exec controls
      - /var/run/docker.sock:/var/run/docker.sock:ro
      # Optional: hook scripts directory
      - ./scripts:/scripts:ro

volumes:
  pgdata:
```

Start the daemon:

```sh
docker compose up -d
```

View daemon logs and scheduled run times:

```sh
docker compose logs -f backup
```

---

## Initial Configuration

Generate a reference configuration file (`dvb.toml`) with default permissions (`0600`):

```sh
# Option 1: Pipe stdout
docker run --rm dreamoutbox/dvb init -o - > dvb.toml

# Option 2: Direct write to current directory
docker run --rm -v "$PWD":/work -w /work dreamoutbox/dvb init -o dvb.toml
```

By default, `dvb` reads `/etc/dvb/config.toml`. Mount your generated `dvb.toml` to that path inside the container.

---

## Configuration Reference

Edit `dvb.toml`. Each backup job is defined under `[job.<name>]`.

```toml
# =================================================
# Global Settings
# =================================================

# Grace period (seconds) to wait for running jobs on SIGTERM before cancel
shutdown_grace_secs = 60

[docker]
# Docker socket path; required for stop_containers, stop_label, or container hooks
socket = "/var/run/docker.sock"

# =================================================
# Job Definition: [job.<name>]
# =================================================

[job.db]
# Schedule: standard 5-field cron (UTC by default) or crontext natural expression
cron = "0 3 * * *"
# crontext = "every day at 03:00"

# Optional timezone for schedule evaluation (e.g. "America/New_York", "UTC")
timezone = "UTC"

# Run this job immediately when the daemon starts
run_on_start = false

# List of container paths to archive
source = ["/backup/pgdata"]

# Archive filename format; must include second-level timestamp (%Y and %S) for retention parsing
filename = "db-%Y%m%dT%H%M%SZ.tar.zst"

# Compression format: "zstd" (default), "gzip", or "none"
compression = "zstd"

# Retention policy: retain backups for N days, keeping at least M newest backups
retention_days = 14
min_keep = 3

# Follow symlinks instead of archiving them as symlinks
follow_symlinks = false

# Consistent backups: stop containers before archive creation, restart immediately after
stop_containers = ["db"]
# stop_label = "dvb.backup=true"
stop_timeout_secs = 30

# -----------------------------------------------
# Storage Backend: 
# S3 (AWS, MinIO, Cloudflare R2, Wasabi, SeaweedFS)
# -----------------------------------------------
[job.db.storage]
type = "s3"
bucket = "my-backup-bucket"
region = "us-east-1"
prefix = "backups/db"
endpoint = "http://minio:9000"         # Omit for AWS S3
access_key_id = "minioadmin"          # Or set via environment variables / secrets
secret_access_key = "minioadmin"
force_path_style = true               # Required for MinIO and path-style providers

# -----------------------------------------------
# Storage Backend: SFTP (alternative)
# -----------------------------------------------
# [job.db.storage]
# type = "sftp"
# endpoint = "sftp.example.com:22"
# user = "backupuser"
# root = "/remote/backups"
# key_path = "/run/secrets/id_ed25519"
# known_hosts_strategy = "strict"      # "strict" (default) or "accept_new"

# -----------------------------------------------
# Storage Backend: Dropbox (alternative)
# -----------------------------------------------
# [job.db.storage]
# type = "dropbox"
# root = "/backups"
# prefix = "db"
# token = "sl.u.long-lived-token"      # Direct developer access token, OR:
# client_id = "dropbox_app_key"        # OAuth app credentials for automatic token refresh
# client_secret = "dropbox_app_secret"
# refresh_token = "dropbox_refresh_token"

# -----------------------------------------------
# Storage Backend: Local Filesystem (alternative)
# -----------------------------------------------
# [job.db.storage]
# type = "fs"
# root = "/mnt/nas/backups"
# prefix = "db"

# -----------------------------------------------
# Hooks: Pre & Post Backup
# -----------------------------------------------

# Runs before archiving. If this fails, backup aborts and stopped containers restart.
[job.db.pre_backup]
cmd = ["pg_dump", "-U", "postgres", "-f", "/backup/pgdata/dump.sql"]
container = "db"                      # Run inside container via docker exec (omit to run locally in dvb)
timeout_secs = 300

# Runs after archiving and after stopped containers restart.
[job.db.post_backup]
script = "/scripts/notify.sh"         # Path to executable script (mutually exclusive with cmd)
run_on = "always"                     # "success" (default), "failure", or "always"
timeout_secs = 60

# -----------------------------------------------
# Hooks: Pre & Post Restore
# -----------------------------------------------

# Runs before archive is downloaded or unpacked.
[job.db.pre_restore]
cmd = ["/bin/check-storage.sh"]
timeout_secs = 60

# Runs after archive is extracted to staging directory.
[job.db.post_restore]
dir = "/restore"                      # Default target directory
script = "/scripts/pg_restore.sh"     # Or: cmd = ["pg_restore", "-U", "postgres", "-d", "app"]
container = "db"                      # Omit to run inside dvb container
timeout_secs = 3600
```

### Environment Variable Overrides

Configuration keys can be overridden via environment variables using double underscores (`__`):
- `DVB__JOB__DB__STORAGE__SECRET_ACCESS_KEY="secret"`
- `DVB__JOB__DB__CRON="0 4 * * *"`

---

## Manually Trigger Backup

Trigger an immediate backup for a specific job without waiting for its scheduled cron run:

```sh
docker exec backup dvb backup db
```

---

## List Backups

List all stored backup archives and timestamps for a job:

```sh
docker exec backup dvb list db
```

Inspect configured jobs, schedules, and remote storage stats:

```sh
# Local overview
docker exec backup dvb jobs

# Query remote backends for backup counts and total size
docker exec backup dvb jobs --remote
```

---

## Perform Restore

Restore the latest backup or a specific archive:

```sh
# Restore newest backup using default settings from config
docker exec backup dvb restore db

# Dry-run: view restore plan without downloading or extracting
docker exec backup dvb restore db --dry-run

# Restore a specific archive object
docker exec backup dvb restore db --name db-20261014T030000Z.tar.zst

# Restore the newest backup taken at or before a given timestamp
docker exec backup dvb restore db --at 2026-10-14T00:00:00Z

# Extract to a custom directory and run a specific restore script
docker exec backup dvb restore db --to /restore --script /scripts/pg_restore.sh

# Stop target containers during restore
docker exec backup dvb restore db --stop-containers
```

### Restore Lifecycle and Integrity

1. **Integrity Check:** `dvb` verifies the archive against its remote `.sha256` sidecar during download. If verification fails, the process aborts immediately and no files are extracted.
2. **Atomic Extraction:** Archives extract into a temporary staging directory (`.dvb-staging-*`). Files are promoted only after successful extraction.
3. **Hook Execution:** Post-restore hooks or scripts receive the following environment variables:
   - `DVB_RESTORE_DIR`: Absolute path to extracted files.
   - `DVB_TARGET_DIR`: Configured or requested target directory.
   - `DVB_JOB`: Name of the job.
   - `DVB_ARCHIVE`: Name of the restored archive object.

---

## Development Getting Started

### Prerequisites

- Rust 2024 edition (Rust 1.88+)
- `cargo-nextest` (`cargo install cargo-nextest --locked`)
- Docker (for integration tests)

### Build and Test

```sh
# Build dvb binary
make build
# or: cargo build --release --locked --bin dvb

# Run test suite serially with nextest (default -j 1 to avoid port/resource contention)
make test
# or: ./scripts/test.sh

# Run all tests including container-backed S3 and SFTP integration tests
make test-all
# or: ./scripts/test.sh --all

# Linting and formatting
make check
# or: cargo fmt --check && cargo clippy --all-targets -- -D warnings

# Verify rustls TLS stack (ensures no OpenSSL/native-tls dependencies)
make openssl-check
```

### Build Docker Image

```sh
# Build container image with local build script
make docker-build

# Or build multi-arch image with buildx
docker buildx build --platform linux/amd64,linux/arm64 -t dreamoutbox/dvb:latest .
```

---

## License

MIT - see [LICENSE](LICENSE).

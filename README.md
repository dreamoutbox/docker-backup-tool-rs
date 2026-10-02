# docker-backup-tool-rs

`dvb` — a Docker volume backup tool written in Rust.

It tars and compresses mounted paths, streams the archive to a storage backend
(S3, SFTP, Dropbox, local filesystem) and rotates old backups. It runs either as
a scheduler daemon (`dvb run`) or as a one-shot CLI invoked through
`docker exec <container> dvb backup <job>`.

Implementation is in progress; `plan.md` holds the phased plan and the status of
each phase.

## Status

| Phase | Scope | State |
|---|---|---|
| 0 | Project scaffold, CLI skeleton, Docker build | done |
| 1 | Config, archiving, `fs` storage, job locks | done |
| 2 | S3/SFTP backends, retention, `list`/`prune`/`check` | done |
| 3 | Docker stop/start, pre/post hooks | done |
| 4 | Scheduler daemon, graceful shutdown | done |
| 5 | Dropbox backend, `stage = "local"` | not started |

## Commands

| Command | Effect |
|---|---|
| `dvb run` | Run the scheduler daemon and execute jobs on their cron schedules |
| `dvb backup <job>` | Archive the job's sources to storage, right now |
| `dvb list <job>` | List stored backups with their size and parsed timestamp |
| `dvb prune <job> [--dry-run]` | Apply the retention policy without a new backup |
| `dvb check` | Validate the config and round-trip each backend |

Exit codes: `0` success, `1` failure, `2` partial — the archive reached storage
but a post hook or the retention pass failed. A failed run never leaves a
partial object behind, so a truncated archive cannot be mistaken for a good
backup.

## Quick Start

Run the example environment with Docker Compose:

```sh
# Start the backup daemon, database and SeaweedFS S3 gateway
docker compose -f examples/docker-compose.example.yml up -d

# Inspect scheduled jobs and next fire times
docker logs backup

# Trigger an immediate one-shot backup while the daemon is running
docker exec backup dvb backup db

# List stored backups
docker exec backup dvb list db

# Test retention policy without deleting files
docker exec backup dvb prune db --dry-run
```

## Configuration

TOML, at `/etc/dvb/config.toml` by default.

```toml
# Top-level settings
shutdown_grace_secs = 60                       # wait up to 60s for running jobs on SIGTERM

[docker]
socket = "/var/run/docker.sock"                # optional; enables container stop/start/exec

[[job]]
name = "db"
cron = "0 3 * * *"                             # 5-field cron, evaluated in TZ (default UTC)
run_on_start = true                            # take an initial backup when daemon starts
source = ["/backup/pgdata"]                    # paths inside this container
filename = "pgdata-%Y%m%dT%H%M%SZ.tar.zst"     # must contain %Y and %S
compression = "zstd"                           # zstd | gzip | none
retention_days = 14
min_keep = 3
stop_containers = ["postgres"]                 # stop while archiving for consistency
stop_timeout_secs = 30

  [job.storage]
  type = "fs"
  root = "/backups"
  prefix = "db"
```

Tables nested under a `[[job]]` must be written `[job.storage]`, not
`[storage]`: a bare `[storage]` header after `[[job]]` starts a *new top-level*
table and the job silently loses its backend.

`filename` must contain a second-resolution timestamp (`%Y` and `%S`) because
retention parses the timestamp back out of the object name.

Jobs are scheduled according to the `TZ` environment variable (default `UTC`), e.g.
`TZ=America/New_York` or `TZ=Europe/Berlin`.

### Storage backends

```toml
# Local filesystem
  [job.storage]
  type = "fs"
  root = "/backups"
  prefix = "db"

# S3 and S3-compatible (AWS, R2, Wasabi, SeaweedFS, ...)
  [job.storage]
  type = "s3"
  bucket = "my-bucket"
  region = "eu-central-1"
  endpoint = "http://minio:9000"     # omit for AWS
  prefix = "db"
  access_key_id = "..."              # omit both to use the ambient
  secret_access_key = "..."          # AWS credential chain
  force_path_style = true            # the default

# SFTP. Shells out to the `ssh` binary, hence openssh-client in the image.
  [job.storage]
  type = "sftp"
  endpoint = "backup.example.com:2222"
  user = "backup"
  root = "/srv/backups"
  key_path = "/run/secrets/id_ed25519"
  known_hosts_strategy = "strict"    # or "accept_new"
```

SFTP host key verification is always on. `strict` (the default) refuses unknown
hosts; `accept_new` records them on first use. There is no option to disable
verification.

### Environment

| Variable | Purpose |
|---|---|
| `DVB_CONFIG` | Config file path |
| `DVB_LOG` | `EnvFilter` directives, e.g. `dvb=debug,s3=trace` |
| `DVB_LOG_FORMAT` | `text` or `json` |
| `DVB_LOCK_DIR` | Lock file directory (default `/run/dvb`) |
| `DVB__JOB__0__NAME` | Config override, `__` separates levels |

Secrets in the config are wrapped in a redacting type: `Debug` and `Display`
print `***redacted***`, so they cannot leak into logs or `dvb check` output.

## Docker control and hooks

Container control is optional, and a job that stops containers or runs a hook
inside one needs the daemon's socket:

```toml
[docker]
socket = "/var/run/docker.sock"
```

Validation rejects a job that needs the daemon without it, so a missing socket
surfaces at load time rather than half way through a backup.

### Stopping containers

```toml
[[job]]
stop_containers = ["postgres", "api"]  # by name or id prefix
stop_label = "dvb.stop"                # or by label: `key`, or `key=value`
stop_timeout_secs = 30                 # SIGTERM grace period before SIGKILL
```

Both selectors may be given. Names match exactly: Docker's own name filter is a
substring match, which would let a job asking for `db` silently pick up
`db-archive`. A selector that matches nothing is an error, since it means the
backup was taken without stopping anything.

Only containers that were **running** when the run started are stopped, and only
those are started again. Restart happens before the post hooks and before
pruning, on every path out of a run: success, failure, abort and
SIGINT/SIGTERM. A restart that fails is a hard failure (exit `1`), because
containers left stopped are worse than a missed rotation.

### Hooks

```toml
  [[job.pre]]
  cmd = ["pg_dump", "-f", "/backup/pgdata/dump.sql"]
  container = "postgres"      # omit to run on this machine
  timeout_secs = 300          # SIGKILL once this elapses

  [[job.post]]
  cmd = ["/bin/notify.sh"]
  run_on = "always"           # success (default) | failure | always
```

`cmd` is an argument vector and never a shell string: it runs the program
directly. Anything that needs a shell says so itself with
`cmd = ["/bin/sh", "-c", "..."]`. Naming a `container` without `[docker].socket`
is a validation error.

Pre hooks run first and stop at the first failure: no backup is taken, and the
post hooks still run with `DVB_STATUS=failure`. Post hooks all get their turn,
because they are cleaning up, and the first failure is reported.

Every hook is handed:

| Variable | Value |
|---|---|
| `DVB_JOB` | job name |
| `DVB_STATUS` | `pending`, `success` or `failure` |
| `DVB_ARCHIVE` | the object name the archive is going to |
| `DVB_ERROR` | why the run failed, empty when it did not |

Local hooks are killed when their timeout elapses and their output is forwarded
to the log line by line. Container hooks run through `docker exec` and are
judged by their exit code, which the API reports rather than raises.

### Signals

`SIGINT` and `SIGTERM` do not kill the process mid-upload. They cancel the run,
which drops the archiver, removes the partial object, restarts whatever was
stopped, runs the post hooks, and only then exits non-zero. A second signal
cannot interrupt that cleanup.

In daemon mode (`dvb run`), `SIGTERM` stops scheduling new runs and waits up to
`shutdown_grace_secs` (default 60s) for active jobs to finish before cancelling them.

### Database consistency

Hot-copying raw database files (PostgreSQL `base/`, MySQL `ibdata1`, SQLite `.db`) from
live containers is unsafe: files can be modified in mid-read or in-memory caches may
not be flushed to disk, producing corrupt archives that fail on restore.

Ensure consistency by either:
1. Stopping the container during backup with `stop_containers = ["db"]` or
   `stop_label = "dvb.stop"`, so the database engine shuts down cleanly before
   files are archived; OR
2. Taking a logical dump via a container pre-hook:
   ```toml
   [[job.pre]]
   cmd = ["pg_dump", "-U", "postgres", "-f", "/backup/pgdata/dump.sql"]
   container = "db"
   ```

### Security notes

The Docker socket `/var/run/docker.sock` provides root-equivalent host access.
Mounting it `:ro` does NOT restrict API actions: any process that can write to the socket
stream can issue container exec/stop/start calls.

For production setups with strict isolation, consider using `docker-socket-proxy`
with only the necessary API endpoints enabled:
- `CONTAINERS=1`
- `POST=1`

## Retention

A backup is deleted when it is both older than `retention_days` **and** outside
the newest `min_keep`. The newest backup is never deleted, so a misconfigured
retention window cannot wipe everything. Every candidate deletion is logged.

Timestamps come from the object name, never from mtime: SFTP and Dropbox do not
report reliable mtimes, and a copied backup would otherwise look new. Objects
that do not match the job's filename pattern are never deleted.

Pruning runs only after a successful upload, and a delete that fails is
reported rather than skipped.

## Development

```sh
make check          # cargo fmt --check + clippy -D warnings + tests
make openssl-check  # fails if openssl/native-tls sneak into the tree
make docker-build   # cargo-chef multi-stage image
```

Multi-arch builds with Docker Buildx:

```sh
docker buildx build --platform linux/amd64,linux/arm64 -t dreamoutbox/dvb:latest .
```

Two scripts sit behind those targets, so the Makefile and a manual run cannot
drift apart:

```sh
scripts/build-image.sh [-t TAG] [--no-cache]   # build the container image
scripts/run-tests.sh [-j N] [--all]            # run the suite under cargo-nextest
```

`run-tests.sh` defaults to `-j 1`, one test at a time, because the
container-backed tests each start a real server and running several at once is
the easy way to OOM a small machine. Raise it when the machine can take it:

```sh
scripts/run-tests.sh -j 4        # four tests at a time
scripts/run-tests.sh --all -j 2  # plus the containers, two at a time
make test JOBS=4 ALL=1           # the same through make
```

TLS is rustls throughout. `openssl-probe` shows up in `cargo tree` as a
Windows-only helper of `rustls-native-certs` and links nothing on Linux, so
`make openssl-check` looks for `openssl-sys` and `native-tls` instead.

### Integration tests

The S3, SFTP and Docker tests need Docker and are `#[ignore]`d; `--all` (or
`make test-all`) runs them:

```sh
scripts/run-tests.sh --all                      # everything, serially
cargo test --test s3_test -- --ignored         # just the SeaweedFS gateway
cargo test --test sftp -- --ignored             # just atmoz/sftp
cargo test --test docker_hooks -- --ignored     # stop/start, hooks, SIGTERM
```

`docker_hooks` proves the parts a unit test cannot: that `State.StartedAt`
changes (so a container really was stopped and restarted, not merely left
running), that a failed upload and a SIGTERM both restore it, and that a partial
object is removed. Its test container traps SIGTERM, because as PID 1 a process
ignores default signal dispositions and `docker stop` would otherwise wait out
its whole timeout.

They cap each container at 0.5 CPU and 512 MiB so a run cannot starve the
machine. Override when a test is genuinely too slow under the cap:

```sh
DVB_TEST_CPU=2 DVB_TEST_MEM_MB=2048 cargo test --test sftp -- --ignored
```

`tests/fixtures/sftp_test_key` is a committed, test-only SSH key. It is
worthless outside an ephemeral container; see `tests/fixtures/README.md`.

### Memory

The archive is compressed on the blocking pool and piped through a 512 KiB
duplex, so neither the compressor nor the storage client buffers the whole
archive. `tests/memory.rs` checks both halves of that claim: the largest chunk a
sink ever sees is bounded by the pipe, and peak RSS while uploading a 256 MiB
archive stays under 256 MiB (measured via `/proc/self/statm`).

## License

MIT — see [LICENSE](LICENSE).
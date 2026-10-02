# dvb: Docker volume backup CLI in Rust, phased implementation plan

## Agent instructions (read first)

- Implement **one phase at a time**. Do not start the next phase until the current phase's acceptance criteria pass.
- After each phase run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`. All must pass.
- Pin dependency versions. **Verify every OpenDAL API against the pinned version's docs** (the `Writer`/`Operator` API changed between 0.4x and 0.5x). Do not guess signatures.
- Prefer rustls everywhere. After adding deps, `cargo tree | grep -i openssl` must be empty (SFTP shells out to the `ssh` binary; that is expected).
- No `unwrap()`/`expect()` outside tests. Library-ish modules use `thiserror`, `main.rs` uses `anyhow`.
- Never log secrets. Config types holding secrets must have a redacting `Debug`.
- Commit per phase with a message `phase N: <summary>`.

## Product summary

Single static-ish binary `dvb`, shipped as a Docker image. Runs as a daemon (`dvb run`) that schedules multiple cron jobs, and as a one-shot CLI (`docker exec <ctr> dvb backup <job>`). Each job tars + compresses one or more mounted paths, streams the archive to a storage backend via OpenDAL, optionally stops containers through the Docker socket for consistency, runs pre/post commands, and prunes backups older than N days.

## Final CLI

```
dvb run [--config PATH]                  # daemon, schedules all jobs
dvb backup <job> [--config PATH]         # one-shot
dvb prune <job> [--dry-run]              # retention only
dvb list <job>                           # list remote backups
dvb check                                # validate config + test storage/docker connectivity
```

Global flags: `--config` (default `/etc/dvb/config.toml`, env `DVB_CONFIG`), `--log-format text|json`, `-v`.

Exit codes: `0` ok, `1` failure, `2` partial (backup uploaded, but prune or post hook failed).

## Config format (TOML, env override with `DVB__` prefix, `*_FILE` suffix for secrets)

```toml
[docker]
socket = "/var/run/docker.sock"     # optional; absent => stop/exec features disabled

[[job]]
name = "db"
cron = "0 3 * * *"                  # standard 5-field, evaluated in TZ env (default UTC)
source = ["/backup/pgdata"]         # paths inside the backup container
filename = "pgdata-%Y%m%dT%H%M%SZ.tar.zst"   # chrono strftime; must contain a timestamp
compression = "zstd"                # zstd | gzip | none
retention_days = 14
min_keep = 3
stop_containers = ["postgres"]      # names, or labels via stop_label = "dvb.stop-during-backup=db"
stop_timeout_secs = 30
follow_symlinks = false

  [job.storage]
  type = "s3"                       # s3 | sftp | dropbox | fs
  # backend-specific keys below

  [[job.pre]]
  cmd = ["pg_dump", "-f", "/backup/pgdata/dump.sql"]
  container = "postgres"            # optional; present => docker exec, absent => local
  timeout_secs = 300

  [[job.post]]
  cmd = ["/bin/notify.sh"]
  run_on = "always"                 # success | failure | always
  timeout_secs = 60
```

Backend keys:

- `s3`: `bucket, region, endpoint?, prefix, access_key_id?, secret_access_key?, force_path_style?` (allow `*_FILE` for secrets; fall back to default credential chain)
- `sftp`: `endpoint (host:port), user, root, key_path?, known_hosts_strategy (strict|accept_new), password?`
- `dropbox`: `root, client_id, client_secret, refresh_token`
- `fs`: `root` (used for tests and local targets)

Env passed to hook commands: `DVB_JOB`, `DVB_STATUS` (`success|failure`), `DVB_ARCHIVE` (object name), `DVB_ERROR` (empty on success).

## Crates

| Concern | Crate |
|---|---|
| CLI | `clap` (derive, env feature) |
| Runtime | `tokio` (full) |
| Storage | `opendal` (`services-s3`, `services-sftp`, `services-dropbox`, `services-fs`, `services-memory`) |
| Docker | `bollard` |
| Cron | `croner`, `chrono`, `chrono-tz` |
| Archive | `tar`, `zstd`, `flate2`, `tokio-util` (`io` for `SyncIoBridge`, `CancellationToken`) |
| Config | `figment` (toml + env), `serde` |
| Errors / logs | `thiserror`, `anyhow`, `tracing`, `tracing-subscriber` (env-filter, json) |
| Locking | `fd-lock` |
| Tests | `testcontainers`, `tempfile`, `assert_cmd` |

## Module layout

```
src/
  main.rs        # clap entry, signal wiring, exit codes
  cli.rs
  config.rs
  scheduler.rs
  job.rs         # orchestration
  archive.rs
  storage.rs     # Operator factory per backend, layers
  retention.rs
  docker.rs      # bollard wrapper: list/stop/start/exec
  hooks.rs
  lock.rs
  error.rs
tests/
  e2e_fs.rs
  s3_minio.rs
  sftp.rs
Dockerfile
.dockerignore
docker-compose.example.yml
```

---

## Phase 0: Project scaffold and Docker build (cargo-chef) (DONE)

Goal: compiling skeleton plus a fast, cached, multi-stage image build from day one.

Tasks:

1. `cargo new dvb`, edition 2021 or newer, `rust-version` set, `[profile.release]` with `lto = "thin"`, `codegen-units = 1`, `strip = true`, `panic = "abort"` is **not** allowed (we rely on unwinding for cleanup guards; keep default).
2. Add `clap` skeleton with all subcommands stubbed (`todo!()` is not allowed; return a `NotImplemented` error).
3. `tracing` init (text or json via flag).
4. Add `.dockerignore`: `target/`, `.git/`, `*.md` except README, `docker-compose*.yml`.
5. Write the Dockerfile below.
6. CI-friendly `Makefile` or `justfile` with `fmt`, `lint`, `test`, `docker-build`.

Dockerfile (multi-stage, cargo-chef):

```dockerfile
# syntax=docker/dockerfile:1.7

# ---- Stage 1: chef base (toolchain + cargo-chef preinstalled) ----
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef
WORKDIR /app

# ---- Stage 2: planner (dependency recipe; changes only when Cargo.toml/lock change) ----
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- Stage 3: builder ----
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Cached layer: compiles dependencies only
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo chef cook --release --recipe-path recipe.json
# Only this layer rebuilds on source changes
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked --bin dvb

# ---- Stage 4: runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates openssh-client tzdata \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /run/dvb /etc/dvb
COPY --from=builder /app/target/release/dvb /usr/local/bin/dvb
ENV TZ=UTC
ENTRYPOINT ["dvb"]
CMD ["run", "--config", "/etc/dvb/config.toml"]
```

Notes for the agent:

- `openssh-client` is required because OpenDAL's SFTP service shells out to `ssh`. This rules out a `scratch`/musl-static final image.
- The image runs as root by default so it can read arbitrary mounted volumes and access the Docker socket. Document a `user:` override for read-only volume setups.
- Do not use `--mount=type=cache` for `target/`; chef already caches compiled deps in layers.
- Add a HEALTHCHECK only if a `dvb healthcheck` subcommand is added later (not in scope).

Acceptance criteria:

- `docker build .` succeeds. Touching only `src/main.rs` and rebuilding does **not** recompile dependencies (verify in build output).
- `docker run --rm <img> --help` lists all subcommands.
- Final image contains `dvb`, `ssh`, `ca-certificates`.

---

## Phase 1: Config, archive, local storage, lock (DONE)

Goal: `dvb backup <job>` works end to end against the `fs` backend.

Tasks:

1. `config.rs`: serde structs per the format above, `figment` loading (file, then `DVB__` env), `*_FILE` secret resolution, validation (unique job names, valid cron, filename template contains `%Y` and `%S`-level timestamp, `retention_days >= 1`, `min_keep >= 1`, sources exist). Redacting `Debug` for secrets.
2. `archive.rs`: streaming tar + (zstd | gzip | none) into an `AsyncWrite` through `tokio::io::duplex` and `spawn_blocking` with `SyncIoBridge`. Preserve permissions/ownership/mtime, symlinks stored as symlinks unless `follow_symlinks`. Entry names relative to the source basename. Surface IO errors (do not silently skip unreadable files; fail the job).
3. `storage.rs`: `fn operator(&StorageConfig) -> Result<Operator>`. Phase 1 implements `fs` only. Wrap with `RetryLayer`, `TimeoutLayer`, `LoggingLayer` (verify layer names for the pinned version).
4. `lock.rs`: per-job file lock at `/run/dvb/<job>.lock` using `fd-lock` (path overridable via `DVB_LOCK_DIR` for tests). Required because `docker exec dvb backup` is a separate process from the daemon. If held: exit with a clear error (CLI) or skip with a warning (scheduler).
5. `job.rs`: minimal pipeline: lock, archive to storage, release. On failure after a partial write, delete the partial object.
6. Filename templating via `chrono` in UTC.

Acceptance criteria:

- Unit tests: config validation (good and bad cases), filename templating, lock contention (second acquire fails).
- Integration test (`tests/e2e_fs.rs`): create a temp dir with nested files, symlink, and varied permissions; run `dvb backup`; extract the produced archive and compare the tree.
- A failing source (unreadable file) leaves **no** partial object in storage.

---

## Phase 2: S3 and SFTP backends, retention, list/prune (DONE)

Goal: real remote storage and rotation.

Tasks:

1. `storage.rs`: add `s3` (multipart via writer chunk size, e.g. 8 MiB; support custom `endpoint` and path-style for MinIO/R2) and `sftp` (strict host key checking by default; `accept_new` opt-in; never default to disabled checking).
2. Streaming upload: writer chunking so memory stays bounded regardless of archive size. On error, abort/clean up the partial multipart upload or object.
3. `retention.rs`:
   - List under the job `prefix`, **parse timestamps from filenames** using the job's filename template (not mtime; SFTP/Dropbox mtimes are unreliable).
   - Only consider objects matching the job's filename pattern.
   - Delete when `age > retention_days` AND remaining count >= `min_keep`. Never delete the newest backup.
   - `--dry-run` support; log every deletion.
   - Prune runs only after a successful upload.
4. Implement `dvb list <job>` (name, size, parsed timestamp) and `dvb prune <job> [--dry-run]`.
5. `dvb check` (partial): validate config, then perform a storage round trip (write a small probe object, read it, delete it).

Acceptance criteria:

- Retention unit tests with an injected clock using the OpenDAL `memory` backend: boundary ages, `min_keep` protection, unrelated files untouched, unparsable filenames ignored.
- Integration tests with `testcontainers`: MinIO (S3 upload of a >50 MiB archive, multipart path exercised) and `atmoz/sftp` (upload, list, prune).
- Memory usage during a 1 GiB upload stays roughly constant (document how measured; no full-archive buffering).

---

## Phase 3: Docker integration and hooks (DONE)

Goal: consistent backups through container stop/start and pre/post commands.

Tasks:

1. `docker.rs` (bollard): connect to the configured socket; `list_by_name_or_label`, `stop(timeout)`, `start`, `inspect` state, `exec(container, cmd, env, timeout) -> exit_code + output`.
2. Stop logic: resolve targets by name and/or label, record which were **running**, stop only those, and restart only those. If no socket is configured and `stop_containers` is set, fail validation with a clear error.
3. `StopGuard`: holds the list of containers stopped by this run. Provide `async fn restore(self)` that attempts to start **every** container and collects errors (no short-circuit). Do not rely on `Drop`. Call `restore().await` on every code path.
4. Signal safety: `tokio::signal` for SIGINT/SIGTERM triggers a `CancellationToken`; the job aborts the upload, cleans the partial object, runs `restore()`, then exits non-zero.
5. `hooks.rs`:
   - Local: `tokio::process::Command` with an args array, `kill_on_drop(true)`, timeout, stdout/stderr streamed to `tracing`. No implicit `sh -c`.
   - Container: bollard `create_exec` + `start_exec`, then **inspect the exec** for the exit code (a non-zero exit is not an API error).
   - Env vars `DVB_JOB`, `DVB_STATUS`, `DVB_ARCHIVE`, `DVB_ERROR`.
6. Final pipeline in `job.rs`:

```
acquire lock
 -> pre hooks            (failure => abort; run post hooks with status=failure)
 -> stop containers      (record running ones)
 -> archive + upload     (streamed)
 -> restore containers   (ALWAYS, before post hooks and prune)
 -> post hooks           (respect run_on)
 -> prune                (only if upload succeeded)
release lock
```

7. Exit code `2` when the upload succeeded but prune or a post hook failed.
8. Extend `dvb check`: ping the docker socket, resolve each configured container, and dry-run `exec` with `true` where hooks target containers.

Acceptance criteria:

- Integration test with a real container (alpine `sleep`): running container is stopped during backup and running again afterwards; an already-stopped container stays stopped.
- Test: forced failure mid-upload still restarts containers.
- Test: SIGTERM mid-backup restarts containers, removes the partial object, exits non-zero.
- Hook tests: timeout kills the process, non-zero exit in container exec is detected, `run_on` matrix (success/failure/always) behaves as documented.

---

## Phase 4: Scheduler, daemon, image polish

Goal: `dvb run` is production-usable.

Tasks:

1. `scheduler.rs`: for each job compute the next fire time with `croner` in the `TZ` timezone; `tokio::time::sleep_until`; spawn `run_job` per fire. If the lock is held, skip and log a warning (no queueing). Recompute next fire after each run (handle DST and clock jumps; never fire twice for the same slot).
2. Graceful shutdown: on SIGTERM stop scheduling, wait for running jobs up to `shutdown_grace_secs` (default 60), then cancel via the token (which triggers cleanup from Phase 3).
3. Startup log lists all jobs with their next run time. Optional `run_on_start = true` per job.
4. `docker-compose.example.yml`: backup service with `volumes` mounted `:ro`, config mount, docker socket (with a comment recommending `docker-socket-proxy` limited to `containers` list/stop/start/exec), plus an example `docker exec backup dvb backup db`.
5. Image polish: OCI labels, verify `docker build` cache behavior again with the cargo-chef stages, document multi-arch build (`docker buildx build --platform linux/amd64,linux/arm64`).
6. README: quick start, config reference, exit codes, security notes, consistency notes (hot-copying DB files is unsafe: use a pre hook dump or stop the container).

Acceptance criteria:

- Test with a fake clock or short cron (`* * * * *`): two jobs fire independently; a long-running job causes the next fire to be skipped with a warning.
- SIGTERM during an idle daemon exits 0 quickly; during a running job follows the grace period.
- `docker compose up` with the example config produces a backup in MinIO and the `docker exec` command works while the daemon runs (lock prevents overlap).

---

## Phase 5: Dropbox backend

Goal: Dropbox storage via OpenDAL.

Tasks:

1. `storage.rs`: `dropbox` with `client_id`, `client_secret`, `refresh_token` (OpenDAL refreshes access tokens). Support `*_FILE` for all three.
2. **Before coding the upload path**, check whether the pinned OpenDAL version supports large uploads (>150 MB) for Dropbox. If it does not, implement `stage = "local"` for that backend: write the archive to a temp file, restart containers, then upload using whatever chunked mechanism is supported. If still unsupported, document the size limit and fail early with a clear error when the archive exceeds it.
3. Add generic `stage = "stream" | "local"` job option (default `stream`). `local` writes a temp file under `DVB_TMP_DIR`, restores containers immediately after archiving, then uploads, which reduces container downtime for slow remotes. Delete the temp file in all paths.
4. `dvb auth dropbox` helper: guides the user through the OAuth code flow once and prints a refresh token. (Optional; docs-only if time is short.)
5. Retention and `list` must work on Dropbox with filename-based timestamps.

Acceptance criteria:

- Manual test script `docs/dropbox-manual-test.md` (no CI; requires real credentials).
- `stage = "local"` unit/integration test on `fs` and MinIO: containers are running again **before** the upload finishes (assert ordering via logs/events).
- Documented limits and setup steps in the README.

---

## Phase 6 (later, out of current scope)

- `dvb restore <job> [--at TIMESTAMP]`
- age/GPG encryption before upload
- Webhook/ntfy/Slack notifications
- Prometheus metrics endpoint
- GitHub Actions: lint/test, buildx multi-arch release with cargo-chef layer cache (`cache-from/to type=gha`)

---

## Cross-cutting rules and edge cases (apply in every phase)

- **Docker socket is root-equivalent.** Document it; recommend `docker-socket-proxy`. Mounting the socket `:ro` does not restrict the API.
- Stopping an already-stopped container is a no-op; only restart what this run stopped.
- Containers with a restart policy may restart themselves after a stop. Verify state after stop and warn; do not mutate restart policies automatically.
- Never delete files that do not match the job's filename pattern.
- Retry transient storage errors (OpenDAL `RetryLayer`); do not retry non-idempotent operations blindly.
- All external calls have timeouts.
- Redact secrets in logs and in `dvb check` output.
- Archive entries must not escape the source root (no `..`, absolute paths).
- Tests must not require network access except the testcontainers-based ones, which should be gated behind a feature flag or `#[ignore]` with a documented command.

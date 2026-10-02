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
| 3 | Docker stop/start, pre/post hooks | not started |
| 4 | Scheduler daemon, graceful shutdown | not started |
| 5 | Dropbox backend, `stage = "local"` | not started |

## Commands

| Command | Effect |
|---|---|
| `dvb backup <job>` | Archive the job's sources to storage, right now |
| `dvb list <job>` | List stored backups with their size and parsed timestamp |
| `dvb prune <job> [--dry-run]` | Apply the retention policy without a new backup |
| `dvb check` | Validate the config and round-trip each backend |

`dvb run` (the scheduler daemon) is not implemented yet.

Exit codes: `0` success, `1` failure. A failed run never leaves a partial object
behind, so a truncated archive cannot be mistaken for a good backup.

## Configuration

TOML, at `/etc/dvb/config.toml` by default.

```toml
[[job]]
name = "db"
source = ["/backup/pgdata"]                     # paths inside this container
filename = "pgdata-%Y%m%dT%H%M%SZ.tar.zst"      # must contain %Y and %S
compression = "zstd"                            # zstd | gzip | none
retention_days = 14
min_keep = 3

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

The S3 and SFTP tests need Docker and are `#[ignore]`d; `--all` (or
`make test-all`) runs them:

```sh
scripts/run-tests.sh --all                      # everything, serially
cargo test --test s3_minio -- --ignored         # just the SeaweedFS gateway
cargo test --test sftp -- --ignored             # just atmoz/sftp
```

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
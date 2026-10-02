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
| 2 | S3/SFTP backends, retention, `list`/`prune`/`check` | not started |
| 3 | Docker stop/start, pre/post hooks | not started |
| 4 | Scheduler daemon, graceful shutdown | not started |
| 5 | Dropbox backend, `stage = "local"` | not started |

## Usage so far

`dvb backup <job>` streams one archive of the job's `source` paths to the job's
storage backend. It holds a per-job lock, so a scheduler run and a
`docker exec dvb backup` cannot overlap.

```toml
# /etc/dvb/config.toml
[[job]]
name = "db"
source = ["/backup/pgdata"]                     # paths inside this container
filename = "pgdata-%Y%m%dT%H%M%SZ.tar.zst"      # must contain %Y and %S
compression = "zstd"                            # zstd | gzip | none
retention_days = 14
min_keep = 3

  [job.storage]
  type = "fs"                                   # fs only so far
  root = "/backups"
  prefix = "db"
```

```sh
dvb backup db
dvb --config /etc/dvb/config.toml backup db -v
```

Exit codes: `0` success, `1` failure. A failed run never leaves a partial object
behind, so a truncated archive cannot be mistaken for a good backup.

Environment: `DVB_CONFIG`, `DVB_LOG` (`EnvFilter` directives, e.g. `dvb=debug`),
`DVB_LOG_FORMAT`, `DVB_LOCK_DIR`, and `DVB__`-prefixed overrides such as
`DVB__JOB__0__NAME=db`.

## Development

```sh
make check          # cargo fmt --check + clippy -D warnings + tests
make openssl-check  # fails if openssl/native-tls sneak into the tree
make docker-build   # cargo-chef multi-stage image
```

TLS is rustls throughout. `openssl-probe` shows up in `cargo tree` as a
Windows-only helper of `rustls-native-certs` and links nothing on Linux, so
`make openssl-check` looks for `openssl-sys` and `native-tls` instead.

## License

MIT — see [LICENSE](LICENSE).
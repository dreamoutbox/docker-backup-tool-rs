# docker-backup-tool-rs

`dvb` — a Docker volume backup tool written in Rust.

It tars and compresses mounted paths, streams the archive to a storage backend
(S3, SFTP, Dropbox, local filesystem) and rotates old backups. It runs either as
a scheduler daemon (`dvb run`) or as a one-shot CLI invoked through
`docker exec <container> dvb backup <job>`.

> Implementation is in progress, see `plan.md` for the phased plan and status.

## Development

```sh
make check          # cargo fmt --check + clippy -D warnings + tests
make docker-build   # cargo-chef multi-stage image
```

## License

MIT — see [LICENSE](LICENSE).
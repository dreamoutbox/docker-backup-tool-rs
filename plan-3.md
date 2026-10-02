# dvb plan 3: `dvb init` and `dvb jobs`

Third plan for the `dvb` Docker volume backup CLI. Phase numbering restarts at 1 in this file and is independent of plans 1 and 2.

Features:

- `dvb init`: generate a basic, valid config TOML.
- `dvb jobs`: list all configured jobs (schedule, next run, storage, retention), optionally with remote backup stats.

## Agent instructions (read first)

- Implement **one phase at a time**. After each phase run `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`; all must pass before the next phase.
- Pin new dependencies. Verify every crate API against the pinned version instead of guessing.
- No `unwrap()`/`expect()` outside tests. Never print or log secrets.
- **stdout is data only** for both commands. Logs, hints, and next steps go to stderr.
- Commit per phase: `phase N: <summary>`.

**Prerequisites (existing code this plan builds on):**

- `config.rs` with serde config structs, `figment` loading, `*_FILE` secret handling, `deny_unknown_fields`.
- `cli.rs` clap skeleton, `storage.rs` `operator()`, retention listing (filename-timestamp parsing) in `retention.rs`.
- `croner` + `chrono-tz` for next-run computation.
- **Optional:** `crontext` field and `[job.restore]` section from plan 2. If the crontext parser exists, generated configs use it; otherwise generate `cron`. Include the `[job.restore]` commented block only if that section exists in the config structs.

New dependencies: `toml_edit` (comment-preserving template editing and safe string escaping), `comfy-table` (or equivalent) for table output. Dev: `insta` or golden files, `assert_cmd`, `tempfile`.

## New CLI

```
dvb init [-o, --output <PATH|->]       # default: ./dvb.toml ; "-" = stdout
         [--force]
         [--storage s3|sftp|dropbox|fs]    # default: s3
         [--job-name <NAME>]               # default: backup
         [--source <PATH>]...              # repeatable; default: /backup/data
         [--schedule <EXPR>]               # crontext or 5-field cron; default: "every day at 03:00" (or "0 3 * * *")
         [--retention-days <N>]            # default: 14
         [--min-keep <N>]                  # default: 3
         [--with-docker-socket]            # emit [docker] + stop_containers example uncommented
         [--minimal]                       # omit explanatory comments

dvb jobs [--config PATH]
         [--format table|json]             # default: table
         [--remote]                        # also query storage: last backup, count, total size
         [--remote-timeout <SECS>]         # default: 15, per job
```

Exit codes: `0` ok; `1` config missing/invalid or write error; `2` (`jobs --remote` only) config fine but at least one job's storage was unreachable.

Docker usage:

```
docker run --rm <image> init -o - > dvb.toml
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/out" <image> init -o /out/dvb.toml
docker exec backup dvb jobs
docker exec backup dvb jobs --remote --format json
```

---

## Phase 1: Shared groundwork (validation split, job summary model, CLI wiring)

Goal: the pieces both commands need, with no user-visible commands yet beyond stubs.

Tasks:

1. **Split config validation** in `config.rs`:
   - `validate_static(&Config)`: schema-level checks only, no filesystem or network (unique job names, valid cron/crontext, filename template contains a timestamp, `retention_days >= 1`, `min_keep >= 1`, valid timezone, storage type has required keys).
   - `validate_runtime(&Config)`: filesystem checks (source paths exist, secret files readable, Docker socket path present when `stop_containers` is set).
   - `Config::load(path, ValidationMode::{Static, Full})`. Existing commands keep `Full`; `jobs` uses `Static`; `init` self-checks with `Static`.
2. **Job name path-safety:** job names are used in lock file paths (`/run/dvb/<job>.lock`) and CLI args. Enforce `^[a-z0-9][a-z0-9_-]{0,62}$` in `validate_static`, with a clear error. (Add this to the existing validation if it is missing.)
3. **`summary.rs`:** a serializable, secret-free view model built from a validated job.

```rust
#[derive(Serialize)]
pub struct JobSummary {
    pub name: String,
    pub schedule: ScheduleSummary,     // { source: "cron"|"crontext", expression, resolved_cron, timezone }
    pub next_run: Option<DateTime<FixedOffset>>,  // in the job's timezone; None if cron never fires
    pub storage: StorageSummary,       // { kind, location }  location is redacted-safe (see below)
    pub sources: Vec<String>,
    pub compression: String,
    pub retention: RetentionSummary,   // { days, min_keep }
    pub stop_containers: Vec<String>,
    pub hooks: HooksSummary,           // { pre: usize, post: usize }
}
```

   `location` formats (never include keys, passwords, tokens, client secrets, refresh tokens, key paths): `s3://<bucket>/<prefix>`, `sftp://<user>@<host>:<port>/<root>`, `dropbox:/<root>`, `file://<root>`. Build via an explicit allow-list of fields per backend, **not** by serializing the config struct.
   `fn summarize(cfg: &Config, now: DateTime<Utc>) -> Vec<JobSummary>` takes `now` as a parameter for testability.
4. **CLI wiring** in `cli.rs`: add the `init` and `jobs` subcommands with the flags above; handlers return `NotImplemented` errors for now (no `todo!()`). Respect existing `--config`/`DVB_CONFIG`, `--log-format`. Make sure logging always goes to stderr.
5. Add a `TTY`/color helper: no color or fancy borders when stdout is not a terminal or `NO_COLOR` is set (`std::io::IsTerminal`).

Acceptance criteria:

- Unit tests: `validate_static` accepts a config whose source dirs do not exist; `validate_runtime` rejects it; job name rules (accept `db`, `pg-main_1`; reject `../x`, `A`, `-x`, empty, 64+ chars, names with `/`, spaces, quotes).
- **Secret leak test:** build a config with sentinel secret values (for example `SENTINEL_SECRET_xyz` in every secret field of every backend), run `summarize`, serialize to JSON and `Debug`-format it, assert none of the sentinels appear.
- `summarize` with a fixed `now` produces the expected `next_run` for a known cron in `UTC` and in `Europe/Berlin` (including the UTC offset).
- `dvb init --help` and `dvb jobs --help` list all flags.

---

## Phase 2: `dvb init`

Goal: generate a basic, well-commented, valid config that works as a starting point.

Tasks:

1. **Templates:** a base template plus one snippet per backend under `templates/` (`base.toml`, `s3.toml`, `sftp.toml`, `dropbox.toml`, `fs.toml`), embedded with `include_str!`. Each backend snippet must use **exactly the key names the config structs accept** (`deny_unknown_fields` will catch drift, see tests).
   - Active (uncommented) lines form a minimal valid config: one `[[job]]` with name, schedule, sources, filename template (`<job>-%Y%m%dT%H%M%SZ.tar.zst`), compression `zstd`, retention, and `[job.storage]` with placeholder values.
   - Optional features appear as commented-out examples with one-line explanations: `endpoint`/`force_path_style` for MinIO or R2, `stop_containers`, `stop_label`, a `[[job.pre]]` example (`pg_dump` via `container`), a `[[job.post]]` example with `run_on`, `timezone`, and the optional `[job.restore]` block if it exists.
   - `--with-docker-socket` uncomments `[docker] socket = ...` and the `stop_containers` example; otherwise `[docker]` is a commented block.
   - **Secrets are never inline.** Use the project's existing secret-file convention (look at how `config.rs` implements `*_FILE`) with placeholder paths such as `/run/secrets/<name>`, plus a comment pointing at env overrides. Placeholders for non-secret fields (bucket, host) are obviously fake (`my-backup-bucket`, `backup.example.com`).
   - `--minimal` strips comment lines (both prose and commented-out examples).
2. **Safe value injection:** parse the embedded template with `toml_edit::DocumentMut` and set values (job name, sources, schedule, retention) through its typed API so escaping is handled by the library. Do **not** use string `replace` on user input. Verify that `toml_edit` preserves comments and key order in the pinned version.
3. **Schedule handling:** `--schedule` accepts either a crontext expression (starts with `every`, only if the parser exists) or a 5-field cron. Validate it with the real parsers before writing; write it to `crontext` or `cron` accordingly. Invalid input: exit `1` with the parser's error.
4. **Self-check before writing:** deserialize the generated text into `Config` and run `validate_static`. Failure here is an internal bug: return an error that says so (and add a regression test).
5. **Output handling:**
   - `-o -` writes only the config to stdout and nothing else on stdout.
   - File output: create parent dirs if missing; create with `OpenOptions::new().write(true).create_new(true)` and mode `0o600` on Unix, so an existing file is never overwritten. If it exists and `--force` is not set, exit `1` with a message mentioning `--force`.
   - `--force`: write to a temp file in the same dir (mode `0o600`), then atomically `rename` over the target.
   - After a successful file write, print next steps to **stderr**: path written, "edit storage credentials", `dvb jobs`, `dvb check`.
6. Refuse sources that are empty strings or contain NUL. Sources need not exist at init time (config is often generated on a different machine than where it runs).

Acceptance criteria:

- Golden/snapshot tests for default output of each backend (`s3`, `sftp`, `dropbox`, `fs`), with and without `--minimal`, and with `--with-docker-socket`.
- **Matrix test:** for every backend x `{default, --minimal, --with-docker-socket}` the generated text parses into `Config` and passes `validate_static`.
- Injection tests: `--job-name 'a"b'` is rejected by name rules; `--source` values containing quotes, backslashes, unicode, `#`, and `]` round-trip exactly through parse (value equality), never breaking the TOML structure; a source containing a newline is rejected.
- Overwrite tests: existing file without `--force` fails and leaves the file byte-identical; with `--force` it is replaced; file mode is `0600` after creation.
- `init -o -` produces config on stdout and nothing else (assert stderr empty or logs only, stdout parses as TOML).
- Invalid `--schedule` (`every 5 hours` if crontext exists, `61 * * * *`) fails with a readable error and writes no file.

---

## Phase 3: `dvb jobs`

Goal: list every job in the config, offline by default, with optional remote stats.

Tasks:

1. **Load:** `Config::load(path, ValidationMode::Static)` (no filesystem checks, no Docker, no network, no locks). If the config file is missing, exit `1` with a hint: `config not found at <path>; run "dvb init" to create one`.
2. **Table output (default):** one row per job, columns:
   - `NAME`
   - `SCHEDULE`: the original `crontext` text if used, else the cron expression
   - `NEXT RUN`: in the job's timezone, e.g. `2026-10-03 03:00 +02:00`
   - `STORAGE`: `s3://bucket/prefix` style location from `JobSummary`
   - `RETENTION`: e.g. `14d, keep>=3`
   - `SOURCES`: comma-joined, truncated to the first path plus `+N` when more than one
   - with `--remote` add `LAST BACKUP`, `COUNT`, `SIZE` (human-readable)
   - No jobs: print `no jobs configured` and exit `0`.
3. **JSON output (`--format json`):** stable, documented schema, pretty-printed when stdout is a TTY, compact otherwise:

```json
{
  "schema_version": 1,
  "generated_at": "2026-10-02T12:00:00Z",
  "jobs": [
    {
      "name": "db",
      "schedule": { "source": "crontext", "expression": "every friday at 18:00", "resolved_cron": "0 18 * * 5", "timezone": "Europe/Berlin" },
      "next_run": "2026-10-02T18:00:00+02:00",
      "storage": { "kind": "s3", "location": "s3://bk/db/" },
      "sources": ["/backup/pgdata"],
      "compression": "zstd",
      "retention": { "days": 14, "min_keep": 3 },
      "stop_containers": ["postgres"],
      "hooks": { "pre": 1, "post": 1 },
      "remote": null
    }
  ]
}
```

   `remote` is `null` unless `--remote` is set; then `{ "last_backup": "<RFC3339>|null", "count": N, "total_bytes": N, "error": null }` or `{ ..., "error": "<sanitized message>" }` on failure.
4. **`--remote`:**
   - For each job build the `Operator` and reuse the retention listing (filename-timestamp parsing; only objects matching the job's pattern; ignore sidecar/checksum files if the project has them).
   - Run jobs concurrently with a limit of 4 (`futures::stream::buffer_unordered`), each wrapped in `tokio::time::timeout(--remote-timeout)`.
   - A failing or timed-out job fills `remote.error` and does not stop the others. Error messages must be sanitized (no credentials, no signed URLs, no full request dumps): map to a short message from the error kind plus the backend type.
   - Exit `2` when at least one job has `remote.error`, else `0`.
5. **Rendering details:** deterministic order (config order). `NEXT RUN` shows `-` when none. Respect `NO_COLOR`/non-TTY from Phase 1 (plain output when piped). Do not use `unwrap` on terminal width.
6. Add `now` injection (`--now` hidden flag or an internal parameter used by tests) so output is reproducible.

Acceptance criteria:

- Golden tests (fixed `now`) for table output and JSON on a fixture config with 3 jobs across backends, one with `crontext`, one with a non-UTC `timezone`, one with multiple sources.
- JSON schema test: parse the output back, assert `schema_version`, field presence/types for every job; `remote` is `null` without `--remote`.
- **Secret leak test on CLI output:** config with sentinel secrets in all backends; run `dvb jobs` (table and JSON, with and without `--remote`) and assert no sentinel in stdout or stderr.
- `--remote` against the OpenDAL `fs`/`memory` backend: correct `last_backup`, `count`, `total_bytes`; unrelated files in the prefix are not counted.
- `--remote` with one unreachable backend (invalid endpoint, short timeout) among two good ones: good jobs still populated, bad job has `error`, exit code `2`, total runtime bounded by the timeout rather than the sum.
- Missing config file: exit `1`, message includes the `dvb init` hint.
- `dvb jobs` works with source paths that do not exist (static validation only) and does not touch the Docker socket or lock files (assert no files created in the lock dir).

---

## Phase 4: End-to-end tests, docs, image check

Goal: prove the two commands work together and in the container; document usage.

Tasks:

1. **E2E test (`assert_cmd`):** `dvb init --storage fs --source <tempdir>/data -o <tempdir>/dvb.toml`, then rewrite the placeholder `root` to a temp dir (plain text replace in the test only), then:
   - `dvb jobs --config ...` lists the job;
   - `dvb check --config ...` passes;
   - `dvb backup <job> --config ...` succeeds;
   - `dvb jobs --remote --config ... --format json` shows `count == 1`.
2. **Docker check (CI or documented manual step):** build the existing multi-stage cargo-chef image and run `docker run --rm <image> init -o -`; output must parse as TOML. No Dockerfile changes are expected; if any are needed, keep the cargo-chef stages intact.
3. **README updates:**
   - "Quick start" now begins with `dvb init`, with both Docker invocations (stdout redirect and bind-mount with `--user`).
   - "Inspecting jobs" section with `dvb jobs` table and JSON examples, the `--remote` behavior, and exit codes.
   - Note that `init` never overwrites without `--force` and writes the file with mode `0600`.
   - Reminder that generated secret fields use file-based secrets and must be filled in before `dvb check`.
4. Add `docs/config-reference.md` entry (or section) noting which keys `init` emits, so docs and templates do not drift. Add a test that every key used in `templates/*.toml` (including commented examples, parsed by a simple `# key = ` scan) is a known config key, to catch template drift when config fields are renamed.

Acceptance criteria:

- E2E test passes in CI without Docker.
- Template drift test fails if a config key is renamed without updating templates (verify by temporarily renaming a field).
- README commands copy-paste correctly (manually run once and note in the PR).

---

## Cross-cutting rules

- `init` and `jobs` must never require network, the Docker socket, or write access outside the requested output path (`jobs --remote` is the only command here that touches the network, and only storage).
- Both commands must work in the existing runtime image (no new system packages).
- Keep stdout/stderr separation strictly; add a test helper that runs a command and asserts what lands on each stream.
- All user-supplied strings that end up in TOML go through `toml_edit` escaping; all user-supplied strings that end up in paths are validated first.

# dvb plan 3: `dvb init` and `dvb jobs`

Third plan for the `dvb` Docker volume backup CLI. Phase numbering restarts at 1 in this file and is independent of plans 1 and 2.

Features:

- `dvb init`: write one full, valid config TOML (optional parts commented out).
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

New dependencies: `comfy-table` (or equivalent) for table output. Dev: `toml_edit` (tests only), `insta` or golden files, `assert_cmd`, `tempfile`.

## New CLI

```
dvb init [-o, --output <PATH|->]       # default: ./dvb.toml ; "-" = stdout
         [--force]

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
3. **`summary.rs`:** a serializable, secret-free view model built from a validated job. **Its serialized shape is the TOML `[[job]]` schema** (same key names and nesting as the config file), not a new schema.

```rust
#[derive(Serialize)]
pub struct JobSummary {
    pub name: String,
    pub cron: String,                  // effective 5-field cron (resolved from crontext when used)
    pub crontext: Option<String>,      // original text; None when the job uses `cron`
    pub timezone: String,              // effective: job `timezone`, else TZ, else UTC
    pub source: Vec<String>,
    pub filename: String,
    pub compression: String,
    pub retention_days: u32,
    pub min_keep: u32,
    pub stop_containers: Vec<String>,
    pub storage: StorageSummary,       // `type` + non-secret keys, named as in TOML
    pub pre: Vec<HookSummary>,
    pub post: Vec<HookSummary>,
    pub next_run: Option<DateTime<FixedOffset>>,  // runtime field, in the job's timezone
}
```

   `StorageSummary` holds `type` plus an explicit allow-list of non-secret keys per backend (never keys, passwords, tokens, client secrets, refresh tokens, key paths). Build it field by field, **not** by serializing the config struct. A separate `location()` method (not serialized) renders the table form: `s3://<bucket>/<prefix>`, `sftp://<user>@<host>:<port>/<root>`, `dropbox:/<root>`, `file://<root>`. `HookSummary` keeps `container`, `timeout_secs`, `run_on`, and only the first element of `cmd` (arguments may carry secrets).
   `fn summarize(cfg: &Config, now: DateTime<Utc>) -> Vec<JobSummary>` takes `now` as a parameter for testability.
4. **CLI wiring** in `cli.rs`: add the `init` and `jobs` subcommands with the flags above; handlers return `NotImplemented` errors for now (no `todo!()`). Respect existing `--config`/`DVB_CONFIG`, `--log-format`. Make sure logging always goes to stderr.
5. Add a `TTY`/color helper: no color or fancy borders when stdout is not a terminal or `NO_COLOR` is set (`std::io::IsTerminal`).

Acceptance criteria:

- Unit tests: `validate_static` accepts a config whose source dirs do not exist; `validate_runtime` rejects it; job name rules (accept `db`, `pg-main_1`; reject `../x`, `A`, `-x`, empty, 64+ chars, names with `/`, spaces, quotes).
- **Secret leak test:** build a config with sentinel secret values (for example `SENTINEL_SECRET_xyz` in every secret field of every backend and in hook command arguments), run `summarize`, serialize to JSON and `Debug`-format it, assert none of the sentinels appear.
- `summarize` with a fixed `now` produces the expected `next_run` for a known cron in `UTC` and in `Europe/Berlin` (including the UTC offset).
- `dvb init --help` and `dvb jobs --help` list all flags.

---

## Phase 2: `dvb init`

Goal: write one complete, valid, well-commented config. No generation flags: the output is always the same full reference config and the user edits it.

Usage: `dvb init [-o, --output <PATH|->] [--force]`.

Tasks:

1. **Single static template** `templates/dvb.toml`, embedded with `include_str!`. It contains **every** config option and follows this convention (explain it in the file header):
   - **Uncommented lines are required options** with obviously fake placeholder values. Together they form a minimal valid config: one `[[job]]` with `name = "backup"`, the schedule, `source = ["/backup/data"]`, `filename = "backup-%Y%m%dT%H%M%SZ.tar.zst"`, `compression = "zstd"`, `retention_days = 14`, `min_keep = 3`, and an active `[job.storage]` for `s3` (`bucket = "my-backup-bucket"`, `region`, `prefix`).
   - **Optional options are commented out**, each preceded by a prose line starting with `## optional: <what it does>`. Commented-out config lines use a single `#` (`# timezone = "Europe/Berlin"`); prose uses `##`, so tests can tell them apart.
   - Optional items to include: `[docker] socket`, `timezone`, `stop_containers` / `stop_label` / `stop_timeout_secs`, `follow_symlinks`, S3 `endpoint` / `force_path_style` / credentials, a `[[job.pre]]` example (`pg_dump` via `container`), a `[[job.post]]` example with `run_on`, and `[job.restore]` only if that section exists in the config structs.
   - Alternative storage backends (`sftp`, `dropbox`, `fs`) appear as commented blocks under `## alternative storage: <type>` with all their keys. In the sftp block show `known_hosts_strategy = "strict"`.
   - Schedule: `crontext = "every day at 03:00"` active, with `# cron = "0 3 * * *"` as the commented alternative and a note that exactly one of the two must be set. If crontext is not implemented, make `cron` the active line and omit crontext.
   - **Secrets are never inline.** Use the secret-file convention exactly as `config.rs` implements it, with placeholder paths such as `/run/secrets/<name>`, plus a comment about env overrides.
   - Key names must match the config structs exactly (`deny_unknown_fields`).
2. Write the template **verbatim**: no substitution, no templating, no user-supplied values.
3. **Output handling:**
   - `-o -` writes only the config to stdout and nothing else on stdout.
   - Default output `./dvb.toml`. Create parent dirs if missing. Create with `OpenOptions::new().write(true).create_new(true)` and mode `0o600` on Unix, so an existing file is never overwritten. If it exists and `--force` is not set, exit `1` with a message mentioning `--force`.
   - `--force`: write to a temp file in the same dir (mode `0o600`), then atomically `rename` over the target.
   - After a successful file write, print next steps to **stderr**: path written, "edit storage credentials", `dvb jobs`, `dvb check`.
4. **Self-check:** a unit test (not runtime) deserializes the template into `Config` and runs `validate_static`.

Acceptance criteria:

- Golden/snapshot test of the generated output.
- Template parses into `Config` and passes `validate_static` as shipped.
- **Optional lines stay valid:** a test uncomments every optional `#` config line (except the alternative-storage blocks and the `cron` alternative) and the result still parses and passes `validate_static`.
- **Alternatives stay valid:** for each alternative storage block, a test swaps it in for the active `[job.storage]` block and the result parses and passes `validate_static`; same for `cron` replacing `crontext`.
- Renaming a config key without updating the template makes one of the tests above fail.
- Overwrite tests: existing file without `--force` fails and stays byte-identical; with `--force` it is replaced; file mode is `0600` after creation.
- `init -o -` puts the config on stdout and nothing else (stdout parses as TOML; stderr has logs only).

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
3. **JSON output (`--format json`):** pretty-printed when stdout is a TTY, compact otherwise. Each job object **keeps the TOML `[[job]]` schema**: same key names and nesting as the config file, secrets removed by allow-list. The only differences: `cron` always holds the effective 5-field expression (resolved from `crontext` when that is used) and `crontext` holds the original text or `null`; `timezone` is always the effective timezone; plus two runtime fields, `next_run` and `remote`.

```json
{
  "schema_version": 1,
  "generated_at": "2026-10-02T12:00:00Z",
  "jobs": [
    {
      "name": "db",
      "cron": "0 18 * * 5",
      "crontext": "every friday at 18:00",
      "timezone": "Europe/Berlin",
      "source": ["/backup/pgdata"],
      "filename": "pgdata-%Y%m%dT%H%M%SZ.tar.zst",
      "compression": "zstd",
      "retention_days": 14,
      "min_keep": 3,
      "stop_containers": ["postgres"],
      "storage": { "type": "s3", "bucket": "bk", "region": "eu-west-1", "prefix": "db/" },
      "pre": [{ "cmd": ["pg_dump"], "container": "postgres", "timeout_secs": 300 }],
      "post": [{ "cmd": ["/bin/notify.sh"], "run_on": "always", "timeout_secs": 60 }],
      "next_run": "2026-10-02T18:00:00+02:00",
      "remote": null
    }
  ]
}
```

   - `storage` has `type` plus the non-secret keys of that backend, named as in TOML. Secret keys are never emitted.
   - `cmd` in `pre`/`post` shows only the first element (the program); arguments may carry secrets.
   - `remote` is `null` unless `--remote` is set; then `{ "last_backup": "<RFC3339>|null", "count": N, "total_bytes": N, "error": null }`, or the same with `"error": "<sanitized message>"` on failure.
4. **`--remote`:**
   - For each job build the `Operator` and reuse the retention listing (filename-timestamp parsing; only objects matching the job's pattern; ignore sidecar/checksum files if the project has them).
   - Run jobs concurrently with a limit of 4 (`futures::stream::buffer_unordered`), each wrapped in `tokio::time::timeout(--remote-timeout)`.
   - A failing or timed-out job fills `remote.error` and does not stop the others. Error messages must be sanitized (no credentials, no signed URLs, no full request dumps): map to a short message from the error kind plus the backend type.
   - Exit `2` when at least one job has `remote.error`, else `0`.
5. **Rendering details:** deterministic order (config order). `NEXT RUN` shows `-` when none. Respect `NO_COLOR`/non-TTY from Phase 1 (plain output when piped). Do not use `unwrap` on terminal width.
6. Add `now` injection (`--now` hidden flag or an internal parameter used by tests) so output is reproducible.

Acceptance criteria:

- Golden tests (fixed `now`) for table output and JSON on a fixture config with 3 jobs across backends, one with `crontext`, one with a non-UTC `timezone`, one with multiple sources.
- JSON schema test: every non-secret key of a fixture job's TOML table appears in the JSON with the same name and nesting; a job using `cron` has `crontext: null`, a job using `crontext` has both set (`cron` = resolved); `remote` is `null` without `--remote`.
- **Secret leak test on CLI output:** config with sentinel secrets in all backends; run `dvb jobs` (table and JSON, with and without `--remote`) and assert no sentinel in stdout or stderr.
- `--remote` against the OpenDAL `fs`/`memory` backend: correct `last_backup`, `count`, `total_bytes`; unrelated files in the prefix are not counted.
- `--remote` with one unreachable backend (invalid endpoint, short timeout) among two good ones: good jobs still populated, bad job has `error`, exit code `2`, total runtime bounded by the timeout rather than the sum.
- Missing config file: exit `1`, message includes the `dvb init` hint.
- `dvb jobs` works with source paths that do not exist (static validation only) and does not touch the Docker socket or lock files (assert no files created in the lock dir).

---

## Phase 4: End-to-end tests, docs, image check

Goal: prove the two commands work together and in the container; document usage.

Tasks:

1. **E2E test (`assert_cmd`):** `dvb init -o <tempdir>/dvb.toml`, then in the test (using `toml_edit`) set `source` to a temp data dir and replace `[job.storage]` with `type = "fs"`, `root = <temp dir>`. Then:
   - `dvb jobs --config ...` lists the job;
   - `dvb check --config ...` passes;
   - `dvb backup <job> --config ...` succeeds;
   - `dvb jobs --remote --config ... --format json` shows `remote.count == 1`.
2. **Docker check (CI or documented manual step):** build the existing multi-stage cargo-chef image and run `docker run --rm <image> init -o -`; output must parse as TOML. No Dockerfile changes are expected; if any are needed, keep the cargo-chef stages intact.
3. **README updates:**
   - "Quick start" now begins with `dvb init`, with both Docker invocations (stdout redirect and bind-mount with `--user`).
   - "Inspecting jobs" section with `dvb jobs` table and JSON examples, the `--remote` behavior, and exit codes.
   - Note that `init` never overwrites without `--force` and writes the file with mode `0600`.
   - Reminder that generated secret fields use file-based secrets and must be filled in before `dvb check`.
4. Add a `docs/config-reference.md` entry listing every option. The Phase 2 template tests are what keep the template and the config structs in sync.

Acceptance criteria:

- E2E test passes in CI without Docker.
- Template drift test fails if a config key is renamed without updating templates (verify by temporarily renaming a field).
- README commands copy-paste correctly (manually run once and note in the PR).

---

## Cross-cutting rules

- `init` and `jobs` must never require network, the Docker socket, or write access outside the requested output path (`jobs --remote` is the only command here that touches the network, and only storage).
- Both commands must work in the existing runtime image (no new system packages).
- Keep stdout/stderr separation strictly; add a test helper that runs a command and asserts what lands on each stream.
- `init` writes a static template and takes no values from the user except the output path; job names that end up in paths are validated (Phase 1).

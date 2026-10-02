# dvb plan 2: `restore` command and `crontext` schedules

Continuation of `dvb-implementation-plan.md` (plan 1). This plan **supersedes** the "restore" item in plan 1's Phase 6.

## Agent instructions (read first)

- Same rules as plan 1: one phase at a time, `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` after each phase, pinned deps, no `unwrap()`/`expect()` outside tests, no secrets in logs, verify OpenDAL/tar/bollard APIs against pinned versions instead of guessing.
- **Prerequisites:** plan 1 Phases 0 to 3 complete (config, archive, S3/SFTP/fs storage, retention listing, Docker `StopGuard`, hooks, locks). Phase 8 (crontext) additionally touches the Phase 4 scheduler only through the resolved cron string.
- Reuse existing code: backup listing and filename timestamp parsing from `retention.rs`, `operator()` from `storage.rs`, `StopGuard` from `docker.rs`, lock from `lock.rs`, compression enum from `archive.rs`.
- Commit per phase: `phase N: <summary>`.

## New and changed CLI

```
dvb restore <job>
    [--name <object>]            # exact backup object name
    [--at <RFC3339|YYYY-MM-DD>]  # newest backup at or before this time
                                 # default (neither flag): newest backup
    [--to <dir>]                 # extraction dir; default: $DVB_TMP_DIR/restore-<job>-<timestamp>
    [--script <path>]            # run: <script> <abs-extracted-dir> [extra args]
    [--script-timeout <secs>]    # default 3600
    [--force]                    # allow extracting into an existing non-empty dir
    [--cleanup]                  # delete extracted dir after the script succeeds (default: true)
    [--stop-containers]          # stop the job's stop_containers while the script runs
    [--no-verify]                # skip checksum verification (Phase 9)
    [--dry-run]                  # resolve the backup and print the plan, change nothing
    [-- <extra script args>...]

dvb crontext "<expression>"      # print resolved cron + next 5 fire times (Phase 8)
```

Exit codes (extends plan 1): `0` ok, `1` failure, `2` partial (backup ok, prune/post failed), **`3` restore extracted OK but the script failed** (extracted dir is kept for inspection).

New modules: `restore.rs`, `crontext.rs`, `integrity.rs` (Phase 9).

Docker usage example:

```
docker exec backup dvb restore db --at 2026-10-01 --script /scripts/pgrestore.sh
```

Note: the script runs **inside the dvb container**, so it needs its tooling there (for example `psql`) or must call `docker exec` itself. Document this.

## Config additions

```toml
[[job]]
name = "db"
crontext = "every friday at 18:00"      # XOR with `cron` (Phase 8)
timezone = "Europe/Berlin"              # optional; overrides TZ for this job

  [job.restore]                         # optional defaults, CLI flags override
  dir = "/restore"                      # base dir for default --to
  script = "/scripts/pgrestore.sh"
  script_timeout_secs = 3600
```

---

## Phase 6: Restore core (download, decompress, extract)

Goal: `dvb restore <job> --to DIR` downloads a backup and extracts it safely. No script yet.

Tasks:

1. **Backup selection** (`restore.rs`): reuse the retention listing (filename-timestamp parsing, pattern match only). Rules:
   - default: newest backup;
   - `--at`: newest with `timestamp <= at` (accept RFC3339 or `YYYY-MM-DD` meaning end of that day in UTC);
   - `--name`: exact match; reject names containing `/`, `\`, or `..`; must match the job's filename pattern and exist under the job prefix;
   - `--name` and `--at` are mutually exclusive (clap `conflicts_with`);
   - no match: clear error listing the oldest and newest available timestamps.
2. **Compression detection:** by magic bytes, not extension (zstd `28 B5 2F FD`, gzip `1F 8B`; otherwise treat as plain tar). If the magic contradicts the job's configured compression, log a warning and trust the magic.
3. **Streaming download and extract:** OpenDAL reader, bridged to a blocking reader (`SyncIoBridge` in `spawn_blocking`), then decompress, then `tar::Archive`. No full-archive buffering and no temp file. Memory must stay bounded for multi-GiB archives.
4. **Target dir handling:**
   - `--to` absent: default `<job.restore.dir or $DVB_TMP_DIR>/restore-<job>-<archive-timestamp>`;
   - target does not exist or is empty and no `--force`: extract into a sibling staging dir `<to>.dvb-partial`, then `rename` into place on success; on any failure remove the staging dir;
   - target non-empty without `--force`: fail before downloading anything;
   - `--force`: extract in place, overwriting files, never deleting anything pre-existing; on failure leave it as is and say so.
   - Create parent dirs as needed. Print the final absolute path on stdout (and only that on stdout, logs go to stderr) so `$(dvb restore ...)` is scriptable.
5. **Extraction safety** (security critical):
   - use per-entry `entry.unpack_in(dest)` and treat a `false` return (unsafe path) as a **hard error**, not a skip;
   - allow only regular files, directories, symlinks, hardlinks; reject device nodes, fifos, and sockets;
   - reject absolute paths and `..` components; hardlink targets and symlink write-through must not escape `dest` (verify the pinned `tar` version's behavior with tests, add manual checks where it falls short);
   - preserve permissions and mtime; restore ownership only when running as root and `--preserve-owner` is passed (add the flag; default off). Verify the exact `tar::Archive` setters in the pinned version;
   - optional `--max-extracted-bytes <N>` guard against decompression bombs (abort when exceeded).
6. **Locking:** acquire the job lock (same file as backup) for the duration of the restore, so a restore never overlaps a backup or prune for the same job. Fail fast with a clear message if held.
7. **Disk space warning:** if free space on the target filesystem is less than 3x the compressed object size, log a warning (do not fail).
8. `--dry-run`: print the selected object name, size, timestamp, resolved target dir, and script command (if any). No download.
9. Update `dvb check` to warn when the extraction base dir is not writable.

Acceptance criteria:

- Round trip: back up a tree (nested dirs, symlink, executable file, 0600 file, empty dir), restore to a fresh dir, compare trees (content, modes, symlink targets).
- Selection tests with the `memory` backend: newest, `--at` boundaries (exactly equal, one second before, before all backups), `--name` valid/invalid/path-traversal attempt.
- **Malicious archive tests** (crafted tar files, must all fail and leave nothing outside `dest`): `../evil`, absolute path, symlink to outside then file written through it, hardlink to outside file, device node entry.
- Failure mid-extract (truncated archive) leaves no staging dir and no partial target.
- Target non-empty without `--force` fails before any download (assert with a storage mock/counter).
- Works against fs, MinIO, and SFTP backends from plan 1's integration setup.

---

## Phase 7: Restore script hook, container stop, cleanup

Goal: after extraction, optionally run a user script as `<script> <extracted-dir>`, for example a Postgres restore script.

Tasks:

1. **Script resolution and early validation** (before downloading): path from `--script` or `[job.restore].script`; must exist, be a file, and have an executable bit (Unix). Fail early with a clear message. Relative paths resolve against the caller's cwd.
2. **Invocation:** `tokio::process::Command::new(script).arg(<absolute extracted dir>).args(extra_args)`. **No shell.** Inherit cwd. Extra args come from after `--`. `kill_on_drop(true)` and a timeout (`--script-timeout`, default 3600s; on timeout kill the process, report it distinctly).
3. **Environment passed to the script:** `DVB_JOB`, `DVB_RESTORE_DIR` (same absolute path as arg 1), `DVB_ARCHIVE` (object name), `DVB_ARCHIVE_TIME` (RFC3339 of the backup).
4. **Output:** stream stdout and stderr to `tracing` line by line (stderr in dvb's own stdout contract is separate: dvb's single stdout line remains the extracted dir path).
5. **Failure semantics:** script exits non-zero or times out: keep the extracted dir, log its path, exit code `3`. `--cleanup` removes the extracted dir **only when the script succeeded** (and only dirs dvb created, never a pre-existing `--force` target).
6. **`--stop-containers`:** reuse `StopGuard` with the job's `stop_containers`/`stop_label`. Order: acquire lock, download+extract, stop containers, run script, **always** `restore()` containers (also on script failure, timeout, and SIGINT/SIGTERM via the existing `CancellationToken`). Fail validation if the flag is given without a Docker socket or without `stop_containers` configured.
7. **Signals:** SIGTERM during extract removes the staging dir; during the script kills it, restarts containers if stopped, then exits non-zero.
8. Add `docs/restore.md` with an example script:

```sh
#!/bin/sh
# pgrestore.sh <extracted-dir>
set -eu
DIR="$1"
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f "$DIR/dump.sql"
```

and the matching command: `dvb restore db --script ./pgrestore.sh`.

Acceptance criteria:

- Script receives the absolute extracted path as `$1` (test script writes `$1` and `DVB_*` env vars to a marker file; assert contents).
- Extra args after `--` arrive after the dir argument in order.
- Non-executable or missing script fails before any download.
- Script exit non-zero: dvb exits `3`, extracted dir still exists. Script timeout: process killed, exit `3`.
- `--cleanup` removes the dir on success, keeps it on failure.
- `--stop-containers` integration test (alpine `sleep` container): stopped while the script runs (script asserts via a marker/timestamp or a docker inspect), running again after, including when the script fails and on SIGTERM.
- Restore lock: a concurrent `dvb backup <job>` fails fast while a restore runs, and vice versa.

---

## Phase 8: `crontext` human-friendly schedules

Goal: a readable alternative to cron syntax, resolved at config load into a standard 5-field cron string. The scheduler (plan 1 Phase 4) keeps consuming only cron.

Config rules:

- Each job must set **exactly one** of `cron` or `crontext`. Both or neither is a validation error. Use `deny_unknown_fields`.
- Optional per-job `timezone` (IANA name via `chrono-tz`) overrides `TZ`/UTC.
- Resolution happens once in `config.rs`; store the resolved cron plus the original text for logging.

Implementation (`crontext.rs`): a small hand-written parser (no regex crate). Pure function:

```rust
pub fn parse(input: &str) -> Result<Schedule, CrontextError>;
pub struct Schedule { pub cron: String, pub description: String }
```

Normalize first: trim, lowercase, collapse whitespace. Must start with `every`.

Supported forms and results:

| Expression | Cron |
|---|---|
| `every minute`, `every 15 minutes` | `* * * * *`, `*/15 * * * *` |
| `every hour`, `every 12 hours` | `0 * * * *`, `0 */12 * * *` |
| `every day`, `every 1 day` | `0 0 * * *` |
| `every day at 03:30` | `30 3 * * *` |
| `every 12:00` (bare clock = daily at that time) | `0 12 * * *` |
| `every monday` | `0 0 * * 1` |
| `every friday at 18:00` | `0 18 * * 5` |
| `every mon, wed and fri at 06:30` | `30 6 * * 1,3,5` |
| `every weekday at 09:00` | `0 9 * * 1-5` |
| `every weekend at 10:00` | `0 10 * * 6,0` |
| `every month` / `every month on the 1st at 03:00` | `0 0 1 * *` / `0 3 1 * *` |

Details:

- Day names: full and 3-letter, plural accepted (`mondays`). Separators `,` and `and`. Sunday maps to `0`. Duplicates collapse, output sorted.
- `TIME`: `H:MM` or `HH:MM` (24h, `00:00` to `23:59`), and optionally `6pm`, `6:30pm` (`12am` = `00:00`, `12pm` = `12:00`).
- Singular/plural unit words both accepted (`1 hours`, `every 1 day`).
- Interval validity (cron step semantics reset at the unit boundary, so uneven steps silently misfire): minutes `N` must divide 60, hours `N` must divide 24, `every N days` is only valid for `N = 1`; for anything else return an error that explains why and suggests an alternative (for example "every 5 hours does not divide 24, use `every 6 hours` or a raw cron expression").
- Month day must be `1..=28` (avoid skipped short months); explain in the error.
- Never emit a cron with both day-of-month and day-of-week restricted (ambiguous OR/AND semantics between implementations).
- Errors list the supported forms (a short grammar hint) and point at the offending token. Use `thiserror`.

CLI:

- `dvb crontext "every friday at 18:00"` prints the resolved cron, a one-line description, and the next 5 fire times in the effective timezone.
- `dvb check` prints, per job, the resolved cron and next fire time, and marks which field it came from.
- Startup log (plan 1 Phase 4) shows `crontext` original text next to the resolved cron.

Acceptance criteria:

- Table-driven unit tests covering **every row above** plus case/whitespace variations (`  Every   FRIDAY at 18:00 `), `am/pm` edge cases, plural/singular forms.
- Negative tests with asserted error messages: `every 5 hours`, `every 7 minutes`, `every 2 days`, `every month on the 31st`, `every someday`, `every 25:00`, `every friday at 18:60`, empty string, missing `every`.
- Property test: for any successfully parsed expression, the output is accepted by `croner` and has exactly 5 fields.
- Config tests: both `cron` and `crontext` set fails; neither set fails; `timezone` invalid fails; env override (`DVB__JOB__0__CRONTEXT`) works.
- A doc/README test asserts every example in `README.md` and `docs/` parses (extract from a fenced block tagged `crontext`).
- Scheduler test (reuse plan 1 fake clock) with `every 12 hours` fires at 00:00 and 12:00 in the job's timezone, including across a DST change for a non-UTC zone.

---

## Phase 9: Integrity checksums and docs

Goal: detect corrupted or truncated backups at restore time, finish documentation.

Tasks:

1. **Sidecar checksum on backup** (`integrity.rs`): while streaming the compressed archive to storage, compute SHA-256 over the exact bytes uploaded (a hashing writer wrapper), then write `<archive-name>.sha256` containing `<hex>  <archive-name>\n` (compatible with `sha256sum -c`). Write the sidecar **after** the archive upload succeeds; on sidecar failure, log a warning and report exit code `2` (partial), do not delete the archive.
2. **Retention and list:** the filename pattern matcher must treat `*.sha256` sidecars as belonging to their archive: `dvb list` hides them, prune deletes the sidecar together with its archive (and deletes orphan sidecars older than the retention window), and sidecars never count toward `min_keep`.
3. **Verify on restore:** if the sidecar exists, hash the compressed stream while extracting (tee reader) and compare at the end. Mismatch: fail, remove the extracted dir that dvb created, exit `1`, and **do not run the script**. Missing sidecar (older backups): warn and continue. `--no-verify` skips (and skips fetching the sidecar).
4. Ordering note for docs: extraction happens before the final hash is known, so the script must never run before verification completes. Enforce in code, covered by a test.
5. **Docs:** update `README.md` with a restore section, the crontext reference table (generated from the same test table if practical), the exit code table, and the "script runs inside the dvb container" caveat. Add `docker-compose.example.yml` entries for a scripts volume (`./scripts:/scripts:ro`).

Acceptance criteria:

- Backup then `sha256sum -c` on the downloaded object and sidecar passes.
- Flip one byte of the stored archive: restore fails with a checksum error, no extracted dir remains, script is not executed.
- Prune removes archive plus sidecar together; orphan sidecars are cleaned; `list` output shows no sidecars; `min_keep` counts archives only.
- Restore of a pre-checksum backup (no sidecar) succeeds with a warning.
- `--no-verify` restores a corrupted archive without hashing (documented as unsafe).

---

## Cross-cutting rules (apply to all phases)

- Restore never writes to the original volumes by itself. It only extracts to a directory; moving data into place is the script's job. State this in docs.
- stdout contract for `dvb restore`: exactly the absolute extracted path (one line) on success; everything else goes to stderr/logs.
- All external calls (storage reads, Docker, script) have timeouts or cancellation.
- Redact secrets everywhere; the script environment must **not** include storage credentials.
- Tests needing Docker/MinIO/SFTP stay behind the same feature flag or `#[ignore]` convention as plan 1.
- Keep the Docker image build unchanged: no new system packages are needed (the cargo-chef multi-stage Dockerfile from plan 1 stays as is). Only add Rust deps: `sha2`, `hex`, and optionally `fs4` or `rustix` for the free-space check.

# Restoring Backups with `dvb restore`

`dvb restore` downloads, decompresses, and safely extracts backups from object storage directly to a target directory, and can optionally execute post-extraction restore hooks (such as database reload scripts) while orchestrating container stops and restarts.

---

## Command Overview

```sh
dvb restore <job> [OPTIONS] [-- <extra-script-args>...]
```

### Options

| Flag | Description | Default |
|---|---|---|
| `--to <DIR>` | Explicit directory to extract into | `job.<name>.post_restore.dir` (or temporary staging dir if script is used) |
| `--name <NAME>` | Exact backup object name to restore | Newest backup |
| `--at <TIMESTAMP>` | Target cutoff timestamp (RFC3339 or `YYYY-MM-DD` end of day) | Newest backup |
| `--script <PATH>` | Path to a restore script hook to execute after extraction | `job.<name>.post_restore.script` (if configured) |
| `--script-timeout <SECS>` | Timeout in seconds for the restore script hook | `job.<name>.post_restore.timeout_secs` or 3600 |
| `--force` | Extract into an existing non-empty directory | `false` |
| `--cleanup` | Delete the temporary extracted directory after a successful script hook | `false` |
| `--stop-containers` | Stop job containers before the restore script runs, and restart them after | `false` |
| `--dry-run` | Print the resolved backup object, size, timestamp, target dir, and script without downloading | `false` |
| `--preserve-owner` | Restore file ownership (requires root) | `false` |
| `--max-extracted-bytes <N>` | Maximum allowed decompressed bytes (decompression bomb protection) | unlimited |
| `-- <args>...` | Extra arguments forwarded directly to the script hook | none |

---

## Configuration (`config.toml`)

You can define the default post-restore behavior for a job under `[job.<name>.post_restore]`:

```toml
[job.db]
source = ["/var/lib/postgresql/data"]
filename = "db-%Y%m%dT%H%M%SZ.tar.zst"
stop_containers = ["postgres-prod"]

  [job.db.post_restore]
  # Default directory when restoring without a script and without --to
  dir = "/mnt/restores/db"
  # Default script hook to execute
  script = "/usr/local/bin/pg_restore.sh"
  # Step timeout in seconds
  timeout_secs = 1800
```

---

## Restore Workflow & Safety Guarantees

1. **Job Locking:** Acquires an exclusive lock on the job (using the same file as `backup` and `prune`). A restore never runs concurrently with a backup or pruning pass for the same job.
2. **Early Validation:**
   - Validates that `--script` exists, is a regular file, and has executable permissions (`chmod +x`).
   - Validates that `--stop-containers` is backed by an active Docker socket and configured containers.
   - Validates that the target directory is empty (unless `--force` is supplied).
   - Validates free disk space and logs a warning if free space is less than 3x the compressed archive size.
3. **Atomic Staging:**
   - Unless `--force` is used to unpack into an existing non-empty directory, backups are extracted into a sibling temporary staging folder: `<to>.dvb-partial`.
   - On extraction success, the staging directory is atomically renamed into place.
   - On download or decompression failure, or interruption by `SIGINT`/`SIGTERM`, the staging directory is immediately cleaned up, leaving no corrupted files behind.
4. **Tar Safety:**
   - Disallows dangerous archive entries: device nodes, fifos, and sockets are rejected.
   - Rejects path traversal attempts (`..` components or absolute path targets).
   - Hardlinks and symlinks cannot escape the destination directory.
5. **Stdout Contract:**
   - `dvb restore` prints only the absolute target path on `stdout` (`$(dvb restore db)` can be piped directly in scripts).
   - All informational logs, progress, and script output stream to `stderr`.

---

## Script Hooks

When `--script` or `[job.<name>.post_restore].script` is specified:

1. **Invocation:** The script is called directly without a shell:
   ```sh
   <script> <absolute-extracted-dir> [extra-args...]
   ```
2. **Environment Variables:**
   - `DVB_JOB`: Job name.
   - `DVB_RESTORE_DIR`: Absolute extracted directory path (identical to `$1`).
   - `DVB_ARCHIVE`: Backup object name in storage.
   - `DVB_ARCHIVE_TIME`: RFC3339 timestamp of the backup.
3. **Container Stop & Start (`--stop-containers`):**
   - Containers are stopped **after** extraction completes and **before** the script runs, minimizing database downtime.
   - Containers are **always** restarted when the script completes, times out, or when `dvb` receives `SIGINT`/`SIGTERM`.
4. **Failure Semantics:**
   - If the script exits non-zero or times out, the extracted directory is **preserved**, its location is logged, and `dvb` exits with code `3`.
   - If `--cleanup` was specified, the extracted directory is removed **only if the script succeeded** and the directory was a temporary directory created by `dvb`.

---

## Reference PostgreSQL Restore Hook (`examples/postgres_backup/scripts/pg_restore.sh`)

`examples/postgres_backup/scripts/pg_restore.sh` is provided as a reference PostgreSQL restore script. It supports both plain SQL dumps (`dump.sql`) via `psql` and custom archive dumps (`dump.dump`, `dump.tar`) via `pg_restore`.

It works in two modes:
- **Local:** If `psql` or `pg_restore` is available in PATH, it runs them directly.
- **Docker Exec:** If client tools are not installed locally but Docker is available, it pipes the dump into the running PostgreSQL container via `docker exec`.

### Usage Examples

```sh
# 1. Restore the newest backup using the custom script hook:
dvb restore db --script ./examples/postgres_backup/scripts/pg_restore.sh

# 2. Forward extra flags to pg_restore (e.g. clean existing objects):
dvb restore db --script ./examples/postgres_backup/scripts/pg_restore.sh -- --clean --if-exists

# 3. Restore to a point in time before an incident:
dvb restore db --at 2026-10-01T14:00:00Z --script ./examples/postgres_backup/scripts/pg_restore.sh

# 4. Stop the database container during restore and cleanup temporary files on success:
dvb restore db --script ./examples/postgres_backup/scripts/pg_restore.sh --stop-containers --cleanup

# 5. Extract files directly into a specific folder without running a script:
dvb restore db --to /mnt/data/postgres_restored
```

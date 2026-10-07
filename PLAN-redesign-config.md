# Redesign Config TOML Schema Plan

Redesign the configuration TOML schema to use `[job.<job_name>]` tables, explicit hook names (`pre_backup`, `post_backup`, `pre_restore`, `post_restore`), containerized or local `cmd`/`script` execution, and a unified `timeout_secs` across hooks.

### Phase 1: Core Hook Schema & Execution Engine
- Status: DONE
- Goal: Unify hook configuration types and execution logic to support `script` or `cmd`, container or local execution, and unified `timeout_secs`.
- Tasks:
  - [x] Define updated `HookConfig` and `RestoreHookConfig` structs with `container: Option<String>`, `timeout_secs: u64`, `script: Option<PathBuf>`, `cmd: Option<Vec<String>>`, and optional `dir: Option<PathBuf>` for restore.
  - [x] Update `src/hooks.rs` execution engine to handle both `cmd` (arg vector) and `script` (executable file path), dispatching via Docker exec (`client.exec`) when `container` is set or local process (`tokio::process::Command`) when `container` is `None`.
  - [x] Update `src/restore.rs` hook execution to support containerized execution (Docker exec into target container) and local execution for both `script` and `cmd`, driven by `timeout_secs`.
  - [x] Add hook validation ensuring exactly one of `cmd` or `script` is configured, arguments are non-empty, and `timeout_secs >= 1`.
  - [x] Add unit tests in `src/hooks.rs` and `src/restore.rs` for unified execution paths.
- Done when: `cargo nextest run --test-threads 1 --fail-fast -p dvb hooks` and `restore` tests pass.

### Phase 2: Job Table Mapping & Explicit Hook Integration
- Status: DONE
- Goal: Migrate config parser to `[job.<name>]` tables without `name` field, wire explicit hook sections, and support name-based environment overrides.
- Tasks:
  - [x] Update `Config` deserializer to parse `[job.<name>]` as a map/dictionary where the map key sets `JobConfig.name`, removing the `name` field from the job table.
  - [x] Replace `job.pre`, `job.post`, and `job.restore` in `JobConfig` with `pre_backup: Option<HookConfig>`, `post_backup: Option<HookConfig>`, `pre_restore: Option<HookConfig>`, and `post_restore: Option<RestoreHookConfig>`.
  - [x] Update Figment provider configuration in `src/config.rs` to support `DVB__JOB__<NAME>__...` environment overrides, replacing the indexed `[[job]]` normalization.
  - [x] Update `JobConfig::validate_static`, `JobConfig::validate_runtime`, and `JobConfig::needs_docker` to validate explicit hooks and Docker socket requirements.
  - [x] Update `src/job.rs`, `src/summary.rs`, `src/main.rs`, and `src/init.rs` to consume the new job and hook structures.
  - [x] Update unit tests in `src/config.rs` to test the new schema, schedule resolution, validation errors, and environment overrides.
- Done when: `cargo nextest run --test-threads 1 --fail-fast -p dvb --lib` passes.

### Phase 3: Templates, Examples, and Reference Documentation
- Goal: Update reference templates, example stacks, and sample configs to conform to the new schema.
- Tasks:
  - [ ] Update `templates/dvb.toml` with `[job.backup]`, `[job.backup.storage]`, `[job.backup.pre_backup]`, and `[job.backup.post_restore]`.
  - [ ] Update `examples/postgres_backup/dvb.example.toml` and `examples/postgres_backup_dropbox/dvb.example.toml` to the new schema.
  - [ ] Align `dvb.new.toml` by commenting out alternate options (`# script = ...` vs `cmd = [...]`) to ensure it is valid TOML.
  - [ ] Update CLI help and documentation referencing config structure where applicable.
- Done when: `cargo test --doc` and `dvb init` validation tests pass.

### Phase 4: Test Fixtures, Golden Outputs, and Integration Tests
- Goal: Update all test fixtures, golden files, and end-to-end integration tests to the new schema.
- Tasks:
  - [ ] Update `tests/fixtures/jobs_fixture.toml` to use `[job.<name>]` and explicit hook names.
  - [ ] Update `tests/golden/jobs_json.golden` to reflect updated summary JSON keys (`pre_backup`, `post_backup`, `post_restore`).
  - [ ] Update inline TOML fixtures in `tests/cli.rs`, `tests/jobs_cli.rs`, `tests/docker_hooks.rs`, `tests/e2e_fs.rs`, `tests/s3_test.rs`, `tests/sftp.rs`, and `tests/memory.rs`.
  - [ ] Add integration tests covering containerized hook execution for both `script` and `cmd` in `pre_backup` and `post_restore`.
  - [ ] Run full test suite and verify no regressions.
- Done when: `CI=1 cargo nextest run --test-threads 1 --fail-fast` passes on all 227+ tests.

---

### Unknowns, Risks, and Assumptions
- **Mutual Exclusivity (`cmd` vs `script`):** In `dvb.new.toml`, both `script` and `cmd` are present with an `# OR run cmd` comment. Assumption: A hook must configure either `cmd` or `script`, but not both. Validation will enforce mutual exclusivity.
- **Hook Cardinality:** `pre_backup`, `post_backup`, `pre_restore`, and `post_restore` are modeled as single optional tables (`Option<HookConfig>`), matching `dvb.new.toml`.
- **Extraction Directory:** `dir` is specified under `[job.<name>.post_restore]` (or falls back to CLI `--to` or default temporary staging).
- **Environment Overrides:** Env var overrides switch from array indices (`DVB__JOB__0__...`) to job names (`DVB__JOB__<NAME>__...`).

//! Pre and post hooks.
//!
//! Hooks are an argument vector, never a shell string: `cmd = ["pg_dump", "-Fc"]`
//! runs `pg_dump` directly. Anything that needs a shell says so itself with
//! `cmd = ["/bin/sh", "-c", "..."]`.

use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::io::AsyncBufReadExt as _;

use crate::config::{HookConfig, RunOn};
use crate::docker;
use crate::error::{Error, Result};
use crate::signal::Shutdown;

/// How long a log pump gets to flush what it already read.
///
/// Bounded so a grandchild that inherited the pipe and never exits cannot hold
/// the backup open. The task detaches and ends with the process.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Which side of the backup a hook list runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Before anything else. A failure here aborts the backup.
    Pre,
    /// After the containers are back up, filtered by `run_on`.
    Post,
}

/// How the run went, as reported through `DVB_STATUS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Pre hooks: the backup has not run yet, so there is nothing to report.
    Pending,
    /// The archive reached storage.
    Success,
    /// The archive did not.
    Failure,
}

impl Status {
    /// The literal `DVB_STATUS` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }

    /// Whether a hook with this `run_on` fires for this status.
    fn runs(self, run_on: RunOn) -> bool {
        match run_on {
            RunOn::Success => self == Self::Success,
            RunOn::Failure => self == Self::Failure,
            RunOn::Always => true,
        }
    }
}

/// What a hook is told about the run, through the environment.
#[derive(Debug, Clone)]
pub struct Context<'a> {
    /// `DVB_JOB`: the job name.
    pub job: &'a str,
    /// `DVB_STATUS`: `pending`, `success` or `failure`.
    pub status: Status,
    /// `DVB_ARCHIVE`: the object name the archive is going to.
    pub archive: &'a str,
    /// `DVB_ERROR`: why the run failed, empty when it did not.
    pub error: &'a str,
}

impl Context<'_> {
    /// The environment handed to every hook, in a stable order.
    fn env(&self) -> Vec<(String, String)> {
        [
            ("DVB_JOB", self.job),
            ("DVB_STATUS", self.status.as_str()),
            ("DVB_ARCHIVE", self.archive),
            ("DVB_ERROR", self.error),
        ]
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
    }
}

/// Run one phase's hooks in order.
///
/// Pre hooks stop at the first failure: a backup taken on top of a half-finished
/// preparation is not worth taking. Post hooks all get their turn, because they
/// are cleaning up, and the first failure is reported.
///
/// # Errors
///
/// The first hook that failed, with [`Error::HookFailed`] naming it.
pub async fn run(
    phase: Phase,
    hooks: &[HookConfig],
    ctx: &Context<'_>,
    client: Option<&docker::Client>,
) -> Result<()> {
    let mut first_failure: Option<Error> = None;

    for hook in hooks {
        if phase == Phase::Post && !ctx.status.runs(hook.run_on) {
            tracing::debug!(
                hook = %label(hook),
                status = ctx.status.as_str(),
                "post hook skipped by run_on"
            );
            continue;
        }

        if let Err(err) = run_one(hook, ctx, client).await {
            tracing::error!(hook = %label(hook), error = %err, "hook failed");
            if phase == Phase::Pre {
                return Err(err);
            }
            first_failure = first_failure.or(Some(err));
        }
    }

    match first_failure {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Run a single hook wherever it targets.
async fn run_one(
    hook: &HookConfig,
    ctx: &Context<'_>,
    client: Option<&docker::Client>,
) -> Result<()> {
    let env = ctx.env();
    let command = Command::resolve(hook.cmd.as_ref(), hook.script.as_ref(), &[], None)
        .ok_or_else(|| hook_failed(hook, "no `cmd` or `script` configured"))?;
    if command.is_empty() {
        return Err(hook_failed(hook, "empty command"));
    }

    let code = match hook.container.as_deref() {
        Some(container) => {
            let client = client
                .ok_or_else(|| hook_failed(hook, "the job has no [docker].socket configured"))?;
            run_container(&command, &env, hook.timeout_secs, container, client).await
        }
        None => run_local(&command, &env, hook.timeout_secs, None).await,
    };

    let code = code.map_err(|err| hook_failed(hook, err.describe()))?;
    if code == 0 {
        Ok(())
    } else {
        Err(hook_failed(hook, format!("exit code {code}")))
    }
}

/// A resolved hook/restore program: `cmd[0]` and its arguments, or a script
/// path plus extra arguments. Shared between backup hooks and restore so both
/// run through the same local/container machinery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Executable to run: `cmd[0]` for argument-vector hooks, else the script path.
    pub program: String,
    /// Arguments after the program, including any caller-supplied extras.
    pub args: Vec<String>,
    /// Working directory for local execution.
    pub dir: Option<PathBuf>,
}

impl Command {
    /// Whether the invocation is empty (an empty `cmd` vector).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.program.is_empty()
    }

    /// Resolve a configurable `cmd`/`script` pair plus `extra` arguments into an
    /// invocation. `None` when neither form is configured.
    #[allow(clippy::manual_map)]
    #[must_use]
    pub fn resolve(
        cmd: Option<&Vec<String>>,
        script: Option<&PathBuf>,
        extra: &[String],
        dir: Option<PathBuf>,
    ) -> Option<Self> {
        if let Some(cmd_vec) = cmd {
            // An empty vector is resolved here so the caller can report it as
            // such rather than trying to spawn an empty program.
            let Some(program) = cmd_vec.first().cloned() else {
                return Some(Self {
                    program: String::new(),
                    args: Vec::new(),
                    dir,
                });
            };
            let mut args = cmd_vec[1..].to_vec();
            args.extend(extra.iter().cloned());
            Some(Self { program, args, dir })
        } else if let Some(script) = script {
            Some(Self {
                program: script.display().to_string(),
                args: extra.iter().map(String::clone).collect::<Vec<_>>(),
                dir,
            })
        } else {
            None
        }
    }
}

/// Why a process did not finish with a clean exit code.
#[derive(Debug)]
pub enum ProcessError {
    /// The program could not be spawned.
    Spawn(String),
    /// It was killed after its timeout elapsed.
    TimedOut(u64),
    /// Waiting on the child failed.
    Wait(std::io::Error),
    /// The Docker daemon refused the exec or it timed out.
    Docker(String),
    /// A shutdown signal asked for the process to stop.
    Cancelled,
}

impl ProcessError {
    /// Human-readable description for hook error messages.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Spawn(message) | Self::Docker(message) => (*message).clone(),
            Self::TimedOut(secs) => format!("timed out after {secs}s"),
            Self::Wait(err) => format!("waiting on child failed: {err}"),
            Self::Cancelled => "cancelled".to_owned(),
        }
    }
}

/// Run `command` on this host, without a shell, forwarding its output to the log.
///
/// Returns the child's exit code, or a [`ProcessError`] if it could not be
/// started, was killed by its timeout, or its status could not be read. When a
/// `shutdown` is supplied the child is killed as soon as it fires.
/// `kill_on_drop` is the backstop: if the future is dropped (an aborted run),
/// the child does not outlive it.
///
/// # Errors
///
/// [`ProcessError::Spawn`] when the program cannot be started,
/// [`ProcessError::TimedOut`] when it outlives `timeout_secs`,
/// [`ProcessError::Cancelled`] when `shutdown` fires, and
/// [`ProcessError::Wait`] when its status cannot be read.
pub async fn run_local(
    command: &Command,
    env: &[(String, String)],
    timeout_secs: u64,
    shutdown: Option<&Shutdown>,
) -> std::result::Result<i64, ProcessError> {
    let program = command.program.clone();
    let mut proc = tokio::process::Command::new(program);
    proc.args(&command.args);
    proc.envs(env.iter().cloned());
    proc.kill_on_drop(true);
    proc.stdin(std::process::Stdio::null());
    proc.stdout(std::process::Stdio::piped());
    proc.stderr(std::process::Stdio::piped());
    if let Some(dir) = &command.dir {
        proc.current_dir(dir);
    }

    let mut child = proc.spawn().map_err(|source| {
        ProcessError::Spawn(format!("cannot start `{}`: {source}", command.program))
    })?;

    // Piped output must be drained while the child runs, or a hook that writes
    // more than a pipe's worth blocks forever waiting for us to read it.
    let stdout = tokio::spawn(pump(child.stdout.take(), "stdout"));
    let stderr = tokio::spawn(pump(child.stderr.take(), "stderr"));

    let outcome = await_child(&mut child, timeout_secs, shutdown).await;

    // Both drains are bounded, so output already read still reaches the log
    // without an inherited pipe holding the run open.
    drain(stdout).await;
    drain(stderr).await;

    outcome
}

/// Wait for a local child, honouring its timeout and, when present, a shutdown
/// signal (which kills it early).
async fn await_child(
    child: &mut tokio::process::Child,
    timeout_secs: u64,
    shutdown: Option<&Shutdown>,
) -> std::result::Result<i64, ProcessError> {
    let waited = if let Some(sig) = shutdown {
        tokio::select! {
            wait_res = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()) => {
                match wait_res {
                    Ok(inner) => Ok(inner),
                    Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out")),
                }
            }
            () = sig.cancelled() => {
                Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"))
            }
        }
    } else {
        tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait())
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"))
    };

    match waited {
        Ok(Ok(status)) => Ok(exit_code(status)),
        Ok(Err(err)) => Err(ProcessError::Wait(err)),
        Err(err) => {
            // SIGKILL: a process that ignored its deadline does not get to keep
            // running past the backup that needed it.
            let _ = child.kill().await;
            if err.kind() == std::io::ErrorKind::Interrupted {
                Err(ProcessError::Cancelled)
            } else {
                Err(ProcessError::TimedOut(timeout_secs))
            }
        }
    }
}

/// Run `command` inside `container` via the Docker daemon, returning its exit code.
///
/// The daemon wants `KEY=value` environment pairs; the local path wants tuples.
///
/// # Errors
///
/// [`ProcessError::Docker`] when the daemon refuses the exec or it times out.
pub async fn run_container(
    command: &Command,
    env: &[(String, String)],
    timeout_secs: u64,
    container: &str,
    client: &docker::Client,
) -> std::result::Result<i64, ProcessError> {
    let mut args: Vec<String> = Vec::new();
    args.push(command.program.clone());
    args.extend(command.args.iter().cloned());
    let envs: Vec<String> = env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    client
        .exec(container, &args, &envs, Duration::from_secs(timeout_secs))
        .await
        .map_err(|err| ProcessError::Docker(err.to_string()))
}

/// Translate a child's exit status into an exit code.
fn exit_code(status: ExitStatus) -> i64 {
    if status.success() {
        0
    } else {
        i64::from(status.code().unwrap_or(-1))
    }
}

/// Forward one of the child's pipes to the log, line by line.
pub(crate) async fn pump<R>(reader: Option<R>, stream: &'static str)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(reader) = reader else {
        return;
    };
    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => tracing::info!(stream, line = %String::from_utf8_lossy(&line).trim_end()),
        }
    }
}

/// Wait briefly for a pump to flush what it already read.
pub(crate) async fn drain(task: tokio::task::JoinHandle<()>) {
    // A pump that never finishes leaves a task behind until the process exits;
    // that is deliberate, and better than blocking on an inherited pipe.
    let _ = tokio::time::timeout(DRAIN_GRACE, task).await;
}

/// Wrap any hook failure with the hook's own name, so the log says which one.
fn hook_failed(hook: &HookConfig, reason: impl std::fmt::Display) -> Error {
    Error::HookFailed {
        hook: label(hook),
        reason: reason.to_string(),
    }
}

/// A hook's identity in a log line: its program or script path.
fn label(hook: &HookConfig) -> String {
    let label = hook.describe();
    if label.is_empty() {
        "<empty command>".to_owned()
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    fn hook(cmd: &[&str]) -> HookConfig {
        HookConfig {
            cmd: Some(cmd.iter().map(|s| (*s).to_owned()).collect()),
            script: None,
            run_on: RunOn::Success,
            container: None,
            timeout_secs: 30,
        }
    }

    fn script_hook(script: impl AsRef<Path>) -> HookConfig {
        HookConfig {
            cmd: None,
            script: Some(script.as_ref().to_path_buf()),
            run_on: RunOn::Success,
            container: None,
            timeout_secs: 30,
        }
    }

    fn ctx(status: Status) -> Context<'static> {
        Context {
            job: "db",
            status,
            archive: "backups/db-20260101T000000Z.tar.zst",
            error: "",
        }
    }

    #[test]
    fn status_names_match_the_documented_values() {
        assert_eq!(Status::Pending.as_str(), "pending");
        assert_eq!(Status::Success.as_str(), "success");
        assert_eq!(Status::Failure.as_str(), "failure");
    }

    #[test]
    fn run_on_selects_the_right_status() {
        assert!(Status::Success.runs(RunOn::Success));
        assert!(!Status::Failure.runs(RunOn::Success));
        assert!(Status::Failure.runs(RunOn::Failure));
        assert!(!Status::Success.runs(RunOn::Failure));
        assert!(Status::Success.runs(RunOn::Always));
        assert!(Status::Failure.runs(RunOn::Always));
    }

    #[test]
    fn context_exposes_the_documented_environment() {
        let env = ctx(Status::Failure).env();
        assert_eq!(
            env,
            vec![
                ("DVB_JOB".to_owned(), "db".to_owned()),
                ("DVB_STATUS".to_owned(), "failure".to_owned()),
                (
                    "DVB_ARCHIVE".to_owned(),
                    "backups/db-20260101T000000Z.tar.zst".to_owned()
                ),
                ("DVB_ERROR".to_owned(), String::new()),
            ]
        );
    }

    #[tokio::test]
    async fn a_successful_hook_passes_and_sees_its_environment() {
        let marker = tempfile::tempdir().expect("tempdir");
        let written = marker.path().join("seen");
        let script = format!("echo $DVB_JOB:$DVB_STATUS > {}", written.display());

        run(
            Phase::Pre,
            &[hook(&["/bin/sh", "-c", &script])],
            &ctx(Status::Pending),
            None,
        )
        .await
        .expect("hook");

        let seen = std::fs::read_to_string(&written).expect("marker");
        assert_eq!(seen.trim(), "db:pending");
    }

    #[tokio::test]
    async fn a_failing_pre_hook_aborts_before_the_next_one() {
        let marker = tempfile::tempdir().expect("tempdir");
        let never = marker.path().join("never");

        let err = run(
            Phase::Pre,
            &[
                hook(&["/bin/sh", "-c", "exit 7"]),
                hook(&["/bin/sh", "-c", &format!("touch {}", never.display())]),
            ],
            &ctx(Status::Pending),
            None,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, Error::HookFailed { .. }), "got {err:?}");
        assert!(err.to_string().contains("exit code 7"), "{err}");
        assert!(
            !never.exists(),
            "the second hook ran after the first failed"
        );
    }

    #[tokio::test]
    async fn a_timeout_kills_the_process_and_reports_it() {
        let started = std::time::Instant::now();
        let mut slow = hook(&["/bin/sleep", "30"]);
        slow.timeout_secs = 1;

        let err = run(Phase::Pre, &[slow], &ctx(Status::Pending), None)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the timeout did not kill the process: took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn post_hooks_all_run_and_the_first_failure_is_reported() {
        let marker = tempfile::tempdir().expect("tempdir");
        let ran = marker.path().join("ran");

        let mut fails = hook(&["/bin/sh", "-c", "exit 1"]);
        fails.run_on = RunOn::Always;
        let mut touches = hook(&["/bin/sh", "-c", &format!("touch {}", ran.display())]);
        touches.run_on = RunOn::Always;

        let err = run(Phase::Post, &[fails, touches], &ctx(Status::Failure), None)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("exit code 1"), "{err}");
        assert!(
            ran.exists(),
            "a later post hook was skipped by an earlier failure"
        );
    }

    #[tokio::test]
    async fn run_on_decides_which_post_hooks_fire() {
        let marker = tempfile::tempdir().expect("tempdir");
        let success_only = marker.path().join("success");
        let failure_only = marker.path().join("failure");
        let always = marker.path().join("always");

        let mut on_success = hook(&[
            "/bin/sh",
            "-c",
            &format!("touch {}", success_only.display()),
        ]);
        on_success.run_on = RunOn::Success;
        let mut on_failure = hook(&[
            "/bin/sh",
            "-c",
            &format!("touch {}", failure_only.display()),
        ]);
        on_failure.run_on = RunOn::Failure;
        let mut on_always = hook(&["/bin/sh", "-c", &format!("touch {}", always.display())]);
        on_always.run_on = RunOn::Always;

        let hooks = [on_success, on_failure, on_always];
        run(Phase::Post, &hooks, &ctx(Status::Failure), None)
            .await
            .expect("post hooks");
        assert!(!success_only.exists(), "a success hook ran on a failure");
        assert!(failure_only.exists(), "a failure hook did not run");
        assert!(always.exists(), "an always hook did not run");

        run(Phase::Post, &hooks, &ctx(Status::Success), None)
            .await
            .expect("post hooks");
        assert!(success_only.exists(), "a success hook did not run");
    }

    #[tokio::test]
    async fn an_unknown_program_is_a_hook_failure_not_a_panic() {
        let err = run(
            Phase::Pre,
            &[hook(&["/definitely/not/here"])],
            &ctx(Status::Pending),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("cannot start"), "{err}");
    }

    #[tokio::test]
    async fn an_empty_command_is_rejected() {
        let err = run(Phase::Pre, &[hook(&[])], &ctx(Status::Pending), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty command"), "{err}");
    }

    #[tokio::test]
    async fn a_script_hook_runs_the_script_file() {
        let marker = tempfile::tempdir().expect("tempdir");
        let written = marker.path().join("seen");
        let script = marker.path().join("hook.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $DVB_JOB:$DVB_STATUS > {}\n",
                written.display()
            ),
        )
        .expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        run(
            Phase::Pre,
            &[script_hook(&script)],
            &ctx(Status::Pending),
            None,
        )
        .await
        .expect("script hook");

        let seen = std::fs::read_to_string(&written).expect("marker");
        assert_eq!(seen.trim(), "db:pending");
    }

    #[tokio::test]
    async fn a_script_hook_that_fails_is_reported() {
        let marker = tempfile::tempdir().expect("tempdir");
        let script = marker.path().join("fail.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 9\n").expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let err = run(
            Phase::Pre,
            &[script_hook(&script)],
            &ctx(Status::Pending),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("exit code 9"), "{err}");
    }

    #[test]
    fn resolve_prefers_cmd_over_script_and_appends_extras() {
        let cmd = vec!["pg_dump".to_owned(), "-Fc".to_owned()];
        let command = Command::resolve(Some(&cmd), None, &["--extra".to_owned()], None)
            .expect("cmd resolves");
        assert_eq!(command.program, "pg_dump");
        assert_eq!(command.args, vec!["-Fc".to_owned(), "--extra".to_owned()]);

        let script = PathBuf::from("/scripts/restore.sh");
        let command = Command::resolve(None, Some(&script), &[], None).expect("script resolves");
        assert_eq!(command.program, "/scripts/restore.sh");
        assert!(command.args.is_empty(), "script args should be empty");
    }

    #[test]
    fn resolve_is_none_when_neither_cmd_nor_script_is_set() {
        assert_eq!(Command::resolve(None, None, &[], None), None);
    }
}

//! Pre and post hooks.
//!
//! Hooks are an argument vector, never a shell string: `cmd = ["pg_dump", "-Fc"]`
//! runs `pg_dump` directly. Anything that needs a shell says so itself with
//! `cmd = ["/bin/sh", "-c", "..."]`.

use std::process::ExitStatus;
use std::time::Duration;

use tokio::io::AsyncBufReadExt as _;

use crate::config::{HookConfig, RunOn};
use crate::docker;
use crate::error::{Error, Result};

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

    match hook.container.as_deref() {
        Some(container) => {
            let client = client
                .ok_or_else(|| hook_failed(hook, "the job has no [docker].socket configured"))?;
            // The daemon wants `KEY=value`, the local path wants pairs.
            let env: Vec<String> = env
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect();
            let code = client
                .exec(
                    container,
                    &hook.cmd,
                    &env,
                    Duration::from_secs(hook.timeout_secs),
                )
                .await
                .map_err(|err| hook_failed(hook, err))?;
            if code == 0 {
                Ok(())
            } else {
                Err(hook_failed(hook, format!("exit code {code}")))
            }
        }
        None => run_local(hook, &env).await,
    }
}

/// Run a hook on this machine, without a shell.
///
/// `kill_on_drop` is the backstop: if the future is dropped (an aborted run),
/// the child does not outlive it.
async fn run_local(hook: &HookConfig, env: &[(String, String)]) -> Result<()> {
    let (program, args) = hook
        .cmd
        .split_first()
        .ok_or_else(|| hook_failed(hook, "empty command"))?;

    let mut child = tokio::process::Command::new(program)
        .args(args)
        .envs(env.iter().cloned())
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| hook_failed(hook, format!("cannot start `{program}`: {err}")))?;

    // Piped output must be drained while the child runs, or a hook that writes
    // more than a pipe's worth blocks forever waiting for us to read it.
    let stdout = tokio::spawn(pump(child.stdout.take(), "stdout"));
    let stderr = tokio::spawn(pump(child.stderr.take(), "stderr"));

    let outcome =
        match tokio::time::timeout(Duration::from_secs(hook.timeout_secs), child.wait()).await {
            Ok(Ok(status)) => exit_result(hook, status),
            Ok(Err(err)) => Err(hook_failed(hook, err)),
            Err(_) => {
                // SIGKILL: a hook that ignored its deadline does not get to keep
                // running past the backup that needed it.
                let _ = child.kill().await;
                Err(hook_failed(
                    hook,
                    format!("timed out after {}s", hook.timeout_secs),
                ))
            }
        };

    // Both drains are bounded, so output already read still reaches the log
    // without an inherited pipe holding the run open.
    // The timeout only fires when a pump outlives its child, which is the case
    // it exists for.
    drain(stdout).await;
    drain(stderr).await;

    outcome
}

/// Translate a child's exit status into the pipeline's result type.
fn exit_result(hook: &HookConfig, status: ExitStatus) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        let reason = status.code().map_or_else(
            || "terminated by a signal".to_owned(),
            |code| format!("exit code {code}"),
        );
        Err(hook_failed(hook, reason))
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

/// A hook's identity in a log line: its program.
fn label(hook: &HookConfig) -> String {
    hook.cmd
        .first()
        .cloned()
        .unwrap_or_else(|| "<empty command>".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(cmd: &[&str]) -> HookConfig {
        HookConfig {
            cmd: cmd.iter().map(|s| (*s).to_owned()).collect(),
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
}

//! Docker control: resolve stop targets, stop and restart containers, run
//! hooks inside them.
//!
//! Everything goes through the unix socket configured under `[docker]`. When no
//! socket is configured there is simply no [`Client`], and a job that needs one
//! is rejected at validation time rather than half way through a backup.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use bollard::container::LogOutput;
use bollard::exec::{StartExecOptions, StartExecResults};
use bollard::models::{ContainerSummaryStateEnum, ExecConfig};
use bollard::query_parameters::{ListContainersOptionsBuilder, StopContainerOptionsBuilder};
use bollard::{API_DEFAULT_VERSION, Docker, errors::Error as BollardError};
use futures_util::StreamExt as _;

use crate::config::JobConfig;
use crate::error::{Error, Result};

/// Read/write timeout, in seconds, for every request to the daemon.
const REQUEST_TIMEOUT_SECS: u64 = 120;

/// Capacity of one line of exec output before the reader must be drained.
const EXEC_OUTPUT_CAPACITY: usize = 64 * 1024;

/// A container as the daemon reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    /// Full container id.
    pub id: String,
    /// Name without the leading `/`.
    pub name: String,
    /// Whether it was running when the listing was taken.
    pub running: bool,
}

/// A container plus the labels needed to match it.
struct Listed {
    container: Container,
    labels: HashMap<String, String>,
}

/// Handle to the Docker daemon.
#[derive(Debug, Clone)]
pub struct Client {
    inner: Docker,
}

impl Client {
    /// Connect over `socket`, which must already exist.
    ///
    /// # Errors
    ///
    /// When the socket is missing or the daemon refuses the connection.
    pub fn connect(socket: &Path) -> Result<Self> {
        let path = socket.to_string_lossy().into_owned();
        let inner = Docker::connect_with_socket(&path, REQUEST_TIMEOUT_SECS, API_DEFAULT_VERSION)
            .map_err(docker_err)?;
        Ok(Self { inner })
    }

    /// Connect the way the daemon's own tooling would, honouring `DOCKER_HOST`.
    ///
    /// Used by `dvb check` and the tests, where there is no configured socket.
    ///
    /// # Errors
    ///
    /// As [`Client::connect`].
    pub fn connect_default() -> Result<Self> {
        let inner = Docker::connect_with_unix_defaults().map_err(docker_err)?;
        Ok(Self { inner })
    }

    /// Round-trip the daemon, for `dvb check`.
    ///
    /// # Errors
    ///
    /// When the daemon does not answer.
    pub async fn ping(&self) -> Result<()> {
        self.inner.ping().await.map_err(docker_err)?;
        Ok(())
    }

    /// Resolve `names` (by name or id prefix) and `label` against the daemon.
    ///
    /// Resolution deliberately does not depend on Docker's own name filter,
    /// which is a substring match: a job asking for `db` must not silently
    /// pick up `db-archive`.
    ///
    /// # Errors
    ///
    /// When a configured name, or the label selector, matches nothing. That is
    /// almost always a typo, and a typo here means the container was never
    /// stopped for consistency.
    pub async fn resolve(&self, names: &[String], label: Option<&str>) -> Result<Vec<Container>> {
        let all = self.list().await?;
        let selector = label.map(LabelSelector::parse);

        let mut selected: Vec<Container> = Vec::new();
        let mut missing: Vec<&str> = Vec::new();

        for wanted in names {
            match all
                .iter()
                .find(|entry| matches_name(&entry.container, wanted))
            {
                Some(found) => push_unique(&mut selected, &found.container),
                None => missing.push(wanted),
            }
        }

        if let Some(selector) = &selector {
            let matched: Vec<&Container> = all
                .iter()
                .filter(|entry| selector.matches(&entry.labels))
                .map(|entry| &entry.container)
                .collect();
            if matched.is_empty() {
                missing.push(label.unwrap_or_default());
            }
            for found in matched {
                push_unique(&mut selected, found);
            }
        }

        if missing.is_empty() {
            Ok(selected)
        } else {
            Err(Error::Docker(format!(
                "no container matches {} ({} on the daemon); check stop_containers / stop_label",
                missing
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                all.len()
            )))
        }
    }

    /// Run `cmd` inside `container`, returning its exit code.
    ///
    /// Output is forwarded to the log as it arrives. The command's own timeout
    /// applies; when it elapses this returns an error, though the exec itself
    /// keeps running because the Docker API has no way to kill one.
    ///
    /// # Errors
    ///
    /// When the daemon refuses the exec, the timeout elapses, or the command
    /// cannot be started.
    pub async fn exec(
        &self,
        container: &str,
        cmd: &[String],
        env: &[String],
        timeout: Duration,
    ) -> Result<i64> {
        let config = ExecConfig {
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            cmd: Some(cmd.to_vec()),
            env: Some(env.to_vec()),
            ..Default::default()
        };

        let created = self
            .inner
            .create_exec(container, config)
            .await
            .map_err(docker_err)?;
        let started = self
            .inner
            .start_exec(
                &created.id,
                Some(StartExecOptions {
                    detach: false,
                    tty: false,
                    output_capacity: Some(EXEC_OUTPUT_CAPACITY),
                }),
            )
            .await
            .map_err(docker_err)?;

        let output = match started {
            StartExecResults::Attached { output, .. } => output,
            StartExecResults::Detached => {
                return Err(Error::Docker(
                    "the daemon started the exec detached, so there is no exit code".to_owned(),
                ));
            }
        };

        let drained = drain_exec(output, container, cmd);
        tokio::time::timeout(timeout, drained).await.map_err(|_| {
            Error::Docker(format!(
                "`{}` did not finish within {}s",
                cmd.first().map_or("", String::as_str),
                timeout.as_secs()
            ))
        })??;

        let inspected = self
            .inner
            .inspect_exec(&created.id)
            .await
            .map_err(docker_err)?;
        Ok(inspected.exit_code.unwrap_or(-1))
    }

    /// Every container on the daemon, running or not.
    async fn list(&self) -> Result<Vec<Listed>> {
        let options = ListContainersOptionsBuilder::new().all(true).build();
        let summaries = self
            .inner
            .list_containers(Some(options))
            .await
            .map_err(docker_err)?;

        Ok(summaries
            .into_iter()
            .filter_map(|summary| {
                let id = summary.id?;
                let name = summary
                    .names
                    .as_deref()
                    .and_then(|names| names.first())
                    .map_or_else(
                        || id.chars().take(12).collect(),
                        |name| name.trim_start_matches('/').to_owned(),
                    );
                Some(Listed {
                    container: Container {
                        running: summary.state == Some(ContainerSummaryStateEnum::RUNNING),
                        id,
                        name,
                    },
                    labels: summary.labels.unwrap_or_default(),
                })
            })
            .collect())
    }

    /// Ask the daemon directly whether a container is running.
    async fn is_running(&self, id: &str) -> Result<bool> {
        let info = self
            .inner
            .inspect_container(id, None)
            .await
            .map_err(docker_err)?;
        Ok(info.state.and_then(|state| state.running).unwrap_or(false))
    }

    /// Stop `container`, waiting up to `timeout_secs` for a clean exit.
    async fn stop(&self, container: &Container, timeout_secs: u64) -> Result<()> {
        let options = StopContainerOptionsBuilder::new()
            .t(i32::try_from(timeout_secs).unwrap_or(i32::MAX))
            .build();
        self.inner
            .stop_container(&container.id, Some(options))
            .await
            .map_err(docker_err)
    }

    /// Start `id`, unless something already started it during the backup.
    async fn start(&self, id: &str) -> Result<()> {
        if self.is_running(id).await? {
            return Ok(());
        }
        self.inner
            .start_container(id, None)
            .await
            .map_err(docker_err)
    }
}

/// Connect to the daemon only if `job` actually needs container control.
///
/// Validation guarantees `[docker].socket` is set whenever a job stops
/// containers or targets a hook at one, so this cannot silently do nothing for
/// a job that needs it.
///
/// # Errors
///
/// When the job needs Docker and no socket is configured, or the socket cannot
/// be opened.
pub fn client_for(socket: Option<&Path>, job: &JobConfig) -> Result<Option<Client>> {
    if !job.needs_docker() {
        return Ok(None);
    }
    match socket {
        Some(path) => Client::connect(path).map(Some),
        None => Err(Error::Docker(
            "this job stops containers or runs a container hook, but [docker].socket is not set"
                .to_owned(),
        )),
    }
}

/// The containers one backup run stopped.
///
/// Restoring is not left to `Drop`: an async restart cannot run in a
/// destructor, so the job pipeline calls [`StopGuard::restore`] explicitly on
/// every path out of the run.
#[derive(Debug)]
pub struct StopGuard {
    client: Client,
    stopped: Vec<Container>,
}

impl StopGuard {
    /// Resolve the job's selectors, then stop everything among them that runs.
    ///
    /// Resolution happens before the first stop, so a typo fails the backup
    /// with nothing stopped instead of stopping half of it. Containers that
    /// were already stopped are left alone and not recorded, so a restart never
    /// starts something this run did not stop.
    ///
    /// # Errors
    ///
    /// When a selector matches nothing or the daemon refuses a stop. In the
    /// latter case the containers stopped so far are in the returned guard's
    /// replacement, so the caller must still restore.
    pub async fn stop(client: Client, job: &JobConfig) -> Result<StopGuard> {
        let targets = client
            .resolve(&job.stop_containers, job.stop_label.as_deref())
            .await?;

        let mut stopped = Vec::new();
        for target in targets {
            if !target.running {
                tracing::debug!(container = %target.name, "already stopped, leaving it stopped");
                continue;
            }
            client.stop(&target, job.stop_timeout_secs).await?;
            tracing::info!(
                container = %target.name,
                timeout_secs = job.stop_timeout_secs,
                "stopped for a consistent backup"
            );
            stopped.push(target);
        }

        Ok(StopGuard { client, stopped })
    }

    /// Start every container this run stopped.
    ///
    /// Never short-circuits: each restart is attempted regardless of the
    /// others, so one broken container cannot leave the rest stopped.
    ///
    /// # Errors
    ///
    /// When any container could not be restarted; the message names all of them.
    pub async fn restore(self) -> Result<()> {
        let Self { client, stopped } = self;
        let mut failures: Vec<String> = Vec::new();

        for container in stopped {
            match client.start(&container.id).await {
                Ok(()) => tracing::info!(container = %container.name, "container restarted"),
                Err(err) => {
                    tracing::error!(container = %container.name, error = %err, "could not restart container");
                    failures.push(format!("{}: {err}", container.name));
                }
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error::Docker(format!(
                "{} container(s) left stopped: {}",
                failures.len(),
                failures.join("; ")
            )))
        }
    }
}

/// A `key` or `key=value` label selector.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LabelSelector {
    key: String,
    value: Option<String>,
}

impl LabelSelector {
    fn parse(raw: &str) -> Self {
        match raw.split_once('=') {
            Some((key, value)) => Self {
                key: key.to_owned(),
                value: Some(value.to_owned()),
            },
            None => Self {
                key: raw.to_owned(),
                value: None,
            },
        }
    }

    fn matches(&self, labels: &HashMap<String, String>) -> bool {
        match labels.get(&self.key) {
            None => false,
            Some(found) => self.value.as_deref().is_none_or(|want| want == found),
        }
    }
}

/// Whether `container` is the one the job asked for as `wanted`.
fn matches_name(container: &Container, wanted: &str) -> bool {
    container.name == wanted || container.id == wanted || container.id.starts_with(wanted)
}

/// Append `found` unless an equivalent container is already selected.
fn push_unique(selected: &mut Vec<Container>, found: &Container) {
    if !selected.iter().any(|chosen| chosen.id == found.id) {
        selected.push(found.clone());
    }
}

/// The attach stream returned when an exec is started without `detach`.
type ExecOutput = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = std::result::Result<LogOutput, BollardError>> + Send>,
>;

/// Log an exec's output until the command exits or the stream closes.
async fn drain_exec(mut output: ExecOutput, container: &str, cmd: &[String]) -> Result<()> {
    while let Some(frame) = output.next().await {
        let frame = frame.map_err(docker_err)?;
        let text = frame.to_string();
        for line in text.lines() {
            if !line.trim().is_empty() {
                tracing::info!(container, cmd = ?cmd, line, "hook output");
            }
        }
    }
    Ok(())
}

/// Wrap a bollard failure in [`Error::Docker`].
///
/// Takes the error by value because that is the only shape `map_err(docker_err)`
/// can be handed; formatting it is the whole use, and the drop is free.
#[expect(
    clippy::needless_pass_by_value,
    reason = "bollard hands errors to map_err by value"
)]
fn docker_err(err: BollardError) -> Error {
    Error::Docker(err.to_string())
}

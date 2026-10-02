//! Process-wide shutdown.
//!
//! A signal must not kill `dvb` in the middle of an upload: the containers it
//! stopped would stay stopped and the half-written object would look like a
//! backup. So SIGINT and SIGTERM instead cancel a token, and the job pipeline
//! unwinds through its normal cleanup before the process exits non-zero.

use std::sync::Arc;
use std::sync::OnceLock;

use tokio_util::sync::CancellationToken;

/// What asked for the shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Ctrl-C.
    Interrupt,
    /// `kill -TERM`, the normal way a container is stopped.
    Terminate,
    /// Not a signal: a deliberate cancellation by the caller or a test.
    Internal,
}

impl Signal {
    /// Name used in logs and in [`crate::error::Error::Cancelled`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
            Self::Internal => "cancellation",
        }
    }
}

/// A cancellation token plus the reason it was fired.
///
/// Clones share one token, so the clone handed to the upload and the clone the
/// signal handler holds are the same shutdown.
#[derive(Debug, Clone)]
pub struct Shutdown {
    token: CancellationToken,
    reason: Arc<OnceLock<Signal>>,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    /// A shutdown that has not fired.
    #[must_use]
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            reason: Arc::new(OnceLock::new()),
        }
    }

    /// Fire the shutdown, recording why.
    ///
    /// The first reason wins: a second signal does not rewrite the history.
    pub fn cancel(&self, signal: Signal) {
        let _ = self.reason.set(signal);
        self.token.cancel();
    }

    /// The recorded reason, once fired.
    #[must_use]
    pub fn signal(&self) -> Option<Signal> {
        self.reason.get().copied()
    }

    /// Completes when a shutdown fires.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    /// Install SIGINT and SIGTERM handlers.
    ///
    /// Must be called from inside a runtime. The handler task runs for the life
    /// of the process, and only the first signal is acted on - a second one
    /// cannot interrupt the cleanup it started.
    pub fn install(&self) {
        let shutdown = self.clone();
        let terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(stream) => stream,
                Err(err) => {
                    tracing::warn!(error = %err, "cannot install the SIGTERM handler");
                    return;
                }
            };

        tokio::spawn(async move {
            let mut terminate = terminate;
            let signal = tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    if let Err(err) = result {
                        tracing::warn!(error = %err, "cannot install the SIGINT handler");
                        return;
                    }
                    Signal::Interrupt
                }
                _ = terminate.recv() => Signal::Terminate,
            };
            tracing::warn!(
                signal = signal.as_str(),
                "shutdown requested; finishing the cleanup"
            );
            shutdown.cancel(signal);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unfired_shutdown_does_not_cancel() {
        let shutdown = Shutdown::new();
        assert!(shutdown.signal().is_none());

        let fired =
            tokio::time::timeout(std::time::Duration::from_millis(20), shutdown.cancelled()).await;
        assert!(fired.is_err(), "cancelled without a signal");
    }

    #[tokio::test]
    async fn the_first_reason_is_the_one_that_sticks() {
        let shutdown = Shutdown::new();
        shutdown.cancel(Signal::Terminate);
        shutdown.cancel(Signal::Interrupt);

        assert_eq!(shutdown.signal(), Some(Signal::Terminate));
        let fired =
            tokio::time::timeout(std::time::Duration::from_millis(20), shutdown.cancelled()).await;
        assert!(fired.is_ok(), "did not cancel after a signal");
    }

    #[tokio::test]
    async fn clones_share_the_same_shutdown() {
        let shutdown = Shutdown::new();
        let watcher = shutdown.clone();
        shutdown.cancel(Signal::Internal);

        let fired =
            tokio::time::timeout(std::time::Duration::from_millis(20), watcher.cancelled()).await;
        assert!(fired.is_ok(), "a clone did not see the shutdown");
        assert_eq!(watcher.signal(), Some(Signal::Internal));
    }

    #[test]
    fn signal_names_are_stable() {
        assert_eq!(Signal::Interrupt.as_str(), "SIGINT");
        assert_eq!(Signal::Terminate.as_str(), "SIGTERM");
        assert_eq!(Signal::Internal.as_str(), "cancellation");
    }
}

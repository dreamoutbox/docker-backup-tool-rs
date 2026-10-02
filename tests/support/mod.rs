//! Shared helpers for the container-backed integration tests.
//!
//! Included with `#[path = "support/mod.rs"] mod support;` from `s3_test.rs`
//! and `sftp.rs`.

use testcontainers::bollard::models::HostConfig;
use testcontainers::runners::AsyncRunner as _;
use testcontainers::{ContainerAsync, ContainerRequest, Image, ImageExt};

/// CPU cores a test container may use, overridable with `DVB_TEST_CPU`.
///
/// Defaults to 0.5. These tests spin up a real server, and leaving them
/// unconstrained starves the rest of the machine.
pub const DVB_TEST_DEFAULT_CPU: f64 = 1.0;

/// Memory limit in MiB, overridable with `DVB_TEST_MEM_MB`.
///
/// Defaults to 512 MiB, which `SeaweedFS` still runs comfortably in.
pub const DVB_TEST_DEFAULT_MEM_MB: i64 = 512;

/// CPU limit to apply, from `DVB_TEST_CPU` or [`DEFAULT_CPU`].
pub fn cpu_limit() -> f64 {
    std::env::var("DVB_TEST_CPU")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|cores| *cores > 0.0)
        .unwrap_or(DVB_TEST_DEFAULT_CPU)
}

/// Convert a CPU count to Docker's `nano_cpus` unit (1e-9 CPU).
///
/// Millicores are used internally so no float-to-int cast is needed.
#[expect(
    clippy::cast_possible_truncation,
    reason = "cores is a small positive count, far inside i64 after scaling"
)]
fn nano_cpus(cores: f64) -> i64 {
    (cores * 1_000_000_000.0).round() as i64
}

/// Memory limit in bytes, from `DVB_TEST_MEM_MB` or [`DEFAULT_MEM_MB`].
pub fn memory_limit_bytes() -> i64 {
    let mb = std::env::var("DVB_TEST_MEM_MB")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|mb| *mb > 0)
        .unwrap_or(DVB_TEST_DEFAULT_MEM_MB);
    mb * 1024 * 1024
}

/// Apply [`cpu_limit`] and [`memory_limit_bytes`] to a container request.
///
/// Without this, running the container-backed tests on a laptop or a small CI
/// runner can freeze the machine. Raise the cap when a test is genuinely too
/// slow under it:
///
/// ```sh
/// DVB_TEST_CPU=2 DVB_TEST_MEM_MB=2048 cargo test --test sftp -- --ignored
/// ```
pub fn limited<I>(request: ContainerRequest<I>) -> ContainerRequest<I>
where
    I: Image,
{
    let cpu = cpu_limit();
    let memory = memory_limit_bytes();

    request.with_host_config_modifier(move |host: &mut HostConfig| {
        // bollard's `config::HostConfig` is the flattened builder form, so the
        // resource fields sit directly on it. `nano_cpus` is an absolute limit
        // in 1e-9 CPU units.
        host.nano_cpus = Some(nano_cpus(cpu));
        host.memory = Some(memory);
    })
}

/// Start a container request with the resource limits applied.
///
/// Panics with `testcontainers`' own message when the container cannot start;
/// these are test-only helpers, so a failure is a bug rather than a condition
/// to report.
pub async fn start<I>(request: ContainerRequest<I>) -> ContainerAsync<I>
where
    I: Image + Send + Sync + 'static,
{
    limited(request)
        .start()
        .await
        .expect("start test container")
}

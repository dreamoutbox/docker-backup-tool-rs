//! `dvb` internals, exposed as a library so integration tests can drive the
//! real storage and retention code instead of duplicating it.
//!
//! The binary in `main.rs` is a thin CLI layer over these modules.

pub mod archive;
pub mod config;
pub mod docker;
pub mod error;
pub mod hooks;
pub mod init;
pub mod integrity;
pub mod job;
pub mod jobs;
pub mod lock;
pub mod restore;
pub mod retention;
pub mod scheduler;
pub mod signal;
pub mod storage;
pub mod summary;

#[cfg(test)]
mod testutil;

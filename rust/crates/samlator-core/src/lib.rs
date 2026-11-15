//! Samlator core: a dependency-free-ish runtime that turns an Android app private
//! directory into a bootable Linux/ARM64 desktop, without root, without a JVM in
//! the hot path and without an emulator.
//!
//! Layers, bottom to top:
//! * [`paths`]      – where everything lives inside the app sandbox.
//! * [`config`]     – declarative description of what to install and how to run it.
//! * [`download`]   – resumable + sha256-verified asset fetcher.
//! * [`rootfs`]     – tar.zst extraction and userland provisioning.
//! * [`proot`]      – argv builder for the ptrace-based rootfs remapper.
//! * [`supervisor`] – process lifetime, log capture, clean shutdown.

pub mod config;
pub mod download;
pub mod paths;
pub mod proot;
pub mod rootfs;
pub mod supervisor;

pub use config::{AppEntry, Config, DisplayCfg, GpuBackend, Payload};
pub use paths::Paths;
pub use proot::ProotSpec;
pub use supervisor::{Session, SessionState};

/// Progress callback contract shared by download/extract stages.
pub type ProgressFn<'a> = &'a mut dyn FnMut(Progress);

#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// Human readable stage name ("rootfs", "antigravity", ...).
    Stage(String),
    Bytes { done: u64, total: Option<u64> },
    Done,
}

/// Initialise android logging (or env_logger on the host) once.
pub fn init_logging(tag: &str) {
    #[cfg(target_os = "android")]
    {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag(tag),
        );
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = tag;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("http: {0}")]
    Http(String),
    #[error("checksum mismatch for {what}: expected {expected}, got {actual}")]
    Checksum {
        what: String,
        expected: String,
        actual: String,
    },
    #[error("archive: {0}")]
    Archive(String),
    #[error("config: {0}")]
    Config(String),
    #[error("process: {0}")]
    Process(String),
}

pub type Result<T> = std::result::Result<T, Error>;

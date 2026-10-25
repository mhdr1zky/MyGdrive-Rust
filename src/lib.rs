//! Linux offline-first Drive synchronisation. Remote mutations default to preserve-both.
mod activity;
pub mod autostart;
pub mod availability;
pub mod cache;
pub mod cloud_view;
pub mod connectivity;
pub mod dashboard;
pub mod db;
mod dehydration;
pub mod desktop;
pub mod diagnostics;
pub mod directory_sync;
pub mod drive;
#[cfg(feature = "virtual-drive")]
mod emblems;
pub mod engine;
mod keyed_lock;
pub mod local;
mod memory;
pub mod model;
pub mod oauth;
pub mod offline;
pub mod offline_controller;
pub mod onboarding;
pub mod reconcile;
pub mod scope_migration;
pub mod service;
pub mod thunar;
#[cfg(feature = "desktop-ui")]
pub mod tray;
#[cfg(feature = "virtual-drive")]
pub mod virtual_drive;

pub use engine::{SyncEngine, SyncReport};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("filesystem: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite: {0}")]
    Db(#[from] sqlx::Error),
    #[error("serialization: {0}")]
    Json(#[from] serde_json::Error),
    // Do not include request URLs/headers or OAuth response bodies in logs.
    #[error("transport failure")]
    Transport(#[source] reqwest::Error),
    #[error("Drive HTTP {status}: {reason}")]
    Http { status: u16, reason: String },
    #[error("source changed during operation; reconcile again")]
    Changed,
    #[error("invalid configuration/path: {0}")]
    Invalid(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("authentication required")]
    AuthRequired,
    #[error("remote precondition failed")]
    Precondition,
    #[error("remote page cursor was rejected; inventory must be rebuilt")]
    CursorRejected,
    #[error("watcher: {0}")]
    Watch(#[from] notify::Error),
}

impl From<reqwest::Error> for Error {
    fn from(value: reqwest::Error) -> Self {
        Self::Transport(value.without_url())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

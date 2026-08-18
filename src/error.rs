use thiserror::Error;

#[derive(Error, Debug)]
pub enum RuntimeError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("syscall {0} failed: {1}")]
    Syscall(&'static str, #[source] nix::Error),

    #[error("cgroup error: {0}")]
    Cgroup(String),

    #[error("filesystem error: {0}")]
    Filesystem(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("seccomp error: {0}")]
    Seccomp(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("snapshot error: {0}")]
    Snapshot(String),

    #[error("checkpoint error: {0}")]
    Checkpoint(String),

    #[error("restore error: {0}")]
    Restore(String),

    #[error("migration error: {0}")]
    Migration(String),
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

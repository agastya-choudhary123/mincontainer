use crate::error::{Result, RuntimeError};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

const STATE_DIR: &str = "/.mincontainer/containers";

/// Container state, persisted to disk and read back on lifecycle ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerState {
    pub id: String,
    pub pid: Option<i32>,
    pub status: String, // "created", "running", "stopped", "failed"
    pub exit_code: Option<i32>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub stopped_at: Option<String>,
}

impl ContainerState {
    fn new(id: String) -> Self {
        ContainerState {
            id,
            pid: None,
            status: "created".to_string(),
            exit_code: None,
            created_at: now_iso(),
            started_at: None,
            stopped_at: None,
        }
    }
}

pub struct ContainerStateDir {
    root: PathBuf,
}

impl ContainerStateDir {
    pub fn init() -> Result<()> {
        let root = PathBuf::from(format!("{}/.mincontainer/containers", home_dir()));
        fs::create_dir_all(&root)
            .map_err(|e| RuntimeError::Config(format!("mkdir state dir: {e}")))?;
        Ok(())
    }

    pub fn for_id(id: &str) -> Self {
        let root = PathBuf::from(format!("{}/.mincontainer/containers/{}", home_dir(), id));
        ContainerStateDir { root }
    }

    pub fn path(&self) -> &PathBuf {
        &self.root
    }

    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }

    pub fn stdout_file(&self) -> PathBuf {
        self.root.join("stdout")
    }

    pub fn stderr_file(&self) -> PathBuf {
        self.root.join("stderr")
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.json")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.root.join(".lock")
    }

    pub fn create(&self) -> Result<()> {
        fs::create_dir_all(&self.root)
            .map_err(|e| RuntimeError::Config(format!("mkdir {}: {e}", self.root.display())))?;
        Ok(())
    }

    pub fn load_state(&self) -> Result<ContainerState> {
        let content = fs::read_to_string(self.state_file())
            .map_err(|e| RuntimeError::Config(format!("read state: {e}")))?;
        serde_json::from_str(&content).map_err(RuntimeError::from)
    }

    pub fn save_state(&self, state: &ContainerState) -> Result<()> {
        let json = serde_json::to_string_pretty(state)?;
        fs::write(self.state_file(), json)
            .map_err(|e| RuntimeError::Config(format!("write state: {e}")))?;
        Ok(())
    }

    pub fn save_config(&self, config: &str) -> Result<()> {
        fs::write(self.config_file(), config)
            .map_err(|e| RuntimeError::Config(format!("write config: {e}")))?;
        Ok(())
    }

    pub fn exists(&self) -> bool {
        self.root.exists()
    }

    pub fn cleanup(&self) -> Result<()> {
        fs::remove_dir_all(&self.root)
            .map_err(|e| RuntimeError::Config(format!("cleanup {}: {e}", self.root.display())))?;
        Ok(())
    }
}

pub fn init_container(id: &str) -> Result<ContainerState> {
    ContainerStateDir::init()?;
    let dir = ContainerStateDir::for_id(id);
    dir.create()?;
    let state = ContainerState::new(id.to_string());
    dir.save_state(&state)?;
    Ok(state)
}

pub fn get_container(id: &str) -> Result<ContainerState> {
    let dir = ContainerStateDir::for_id(id);
    if !dir.exists() {
        return Err(RuntimeError::Config(format!("container {} not found", id)));
    }
    dir.load_state()
}

pub fn set_running(id: &str, pid: Pid) -> Result<()> {
    let dir = ContainerStateDir::for_id(id);
    let mut state = dir.load_state()?;
    state.status = "running".to_string();
    state.pid = Some(pid.as_raw());
    state.started_at = Some(now_iso());
    dir.save_state(&state)?;
    Ok(())
}

pub fn set_stopped(id: &str, exit_code: i32) -> Result<()> {
    let dir = ContainerStateDir::for_id(id);
    let mut state = dir.load_state()?;
    state.status = "stopped".to_string();
    state.exit_code = Some(exit_code);
    state.stopped_at = Some(now_iso());
    dir.save_state(&state)?;
    Ok(())
}

pub fn list_all() -> Result<Vec<ContainerState>> {
    let root = PathBuf::from(format!("{}/.mincontainer/containers", home_dir()));
    if !root.exists() {
        return Ok(vec![]);
    }

    let mut containers = Vec::new();
    for entry in fs::read_dir(&root)
        .map_err(|e| RuntimeError::Config(format!("list containers: {e}")))?
    {
        let entry = entry.map_err(|e| RuntimeError::Config(format!("read entry: {e}")))?;
        let path = entry.path();
        if path.is_dir() {
            if let Some(id) = path.file_name().and_then(|n| n.to_str()) {
                if !id.starts_with('.') {
                    if let Ok(state) = get_container(id) {
                        containers.push(state);
                    }
                }
            }
        }
    }
    Ok(containers)
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/root".to_string())
}

fn now_iso() -> String {
    use std::time::UNIX_EPOCH;
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        1970 + (d.as_secs() / (365 * 24 * 3600)),
        1,
        1,
        (d.as_secs() / 3600) % 24,
        (d.as_secs() / 60) % 60,
        d.as_secs() % 60
    )
}

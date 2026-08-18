use crate::config::ContainerConfig;
use crate::error::{Result, RuntimeError};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

const STATE_DIR: &str = "/.mincontainer/containers";

/// Lifecycle status of a container.
///
/// Checkpointing adds three states to the original four. The transient ones
/// matter more than they look: a container is marked `Checkpointing` *before*
/// the dump engine touches it, so when the dump kills the process tree the
/// supervisor reaping it can tell "this container was deliberately frozen"
/// apart from "this container died", and records the right terminal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Created,
    Running,
    Stopped,
    Failed,
    /// A dump is in flight.
    Checkpointing,
    /// Frozen to a snapshot; no processes exist.
    Checkpointed,
    /// A restore is in flight; the host may hold partial resources.
    Restoring,
    /// Checkpointed here, then handed to another node. Terminal on this node.
    Migrated,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Created => "created",
            Status::Running => "running",
            Status::Stopped => "stopped",
            Status::Failed => "failed",
            Status::Checkpointing => "checkpointing",
            Status::Checkpointed => "checkpointed",
            Status::Restoring => "restoring",
            Status::Migrated => "migrated",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Container state, persisted to disk and read back on lifecycle ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerState {
    pub id: String,
    pub pid: Option<i32>,
    /// One of [`Status`]'s string forms. Kept as a string in the on-disk form
    /// so a snapshot written by a newer build stays readable by an older one.
    pub status: String,
    pub exit_code: Option<i32>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub stopped_at: Option<String>,

    /// Path to the snapshot this container was last checkpointed to.
    #[serde(default)]
    pub snapshot: Option<String>,
    #[serde(default)]
    pub checkpointed_at: Option<String>,
    #[serde(default)]
    pub restored_at: Option<String>,
    /// How many times this container has been restored, on any node. Survives
    /// migration, so it doubles as a hop counter.
    #[serde(default)]
    pub generation: u32,
    /// Host this container last ran on, set when it arrives via migration.
    #[serde(default)]
    pub origin_host: Option<String>,
}

impl ContainerState {
    pub fn new(id: String) -> Self {
        ContainerState {
            id,
            pid: None,
            status: "created".to_string(),
            exit_code: None,
            created_at: now_iso(),
            started_at: None,
            stopped_at: None,
            snapshot: None,
            checkpointed_at: None,
            restored_at: None,
            generation: 0,
            origin_host: None,
        }
    }

    pub fn is(&self, s: Status) -> bool {
        self.status == s.as_str()
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

    /// Directory holding the container's stdout/stderr, bind-mounted into the
    /// container so its log fds have a path inside its own mount namespace.
    pub fn io_dir(&self) -> PathBuf {
        self.root.join("io")
    }

    /// Where snapshots for this container live.
    pub fn snapshots_dir(&self) -> PathBuf {
        self.root.join("snapshots")
    }

    /// Default snapshot path for a given name.
    pub fn snapshot_file(&self, name: &str) -> PathBuf {
        self.snapshots_dir().join(format!("{name}.mcsnap"))
    }

    /// Scratch directory a restore unpacks into before it commits.
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }

    pub fn load_config(&self) -> Result<ContainerConfig> {
        let content = fs::read_to_string(self.config_file())
            .map_err(|e| RuntimeError::Config(format!("read config: {e}")))?;
        serde_json::from_str(&content).map_err(RuntimeError::from)
    }

    pub fn create(&self) -> Result<()> {
        for d in [self.root.clone(), self.io_dir(), self.snapshots_dir()] {
            fs::create_dir_all(&d)
                .map_err(|e| RuntimeError::Config(format!("mkdir {}: {e}", d.display())))?;
        }
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
    state.status = Status::Running.as_str().to_string();
    state.pid = Some(pid.as_raw());
    state.started_at = Some(now_iso());
    dir.save_state(&state)?;
    Ok(())
}

pub fn set_stopped(id: &str, exit_code: i32) -> Result<()> {
    let dir = ContainerStateDir::for_id(id);
    let mut state = dir.load_state()?;
    state.status = Status::Stopped.as_str().to_string();
    state.exit_code = Some(exit_code);
    state.stopped_at = Some(now_iso());
    dir.save_state(&state)?;
    Ok(())
}

/// Move a container to `status`, leaving every other field alone.
pub fn set_status(id: &str, status: Status) -> Result<()> {
    update(id, |s| s.status = status.as_str().to_string())
}

/// Record a completed checkpoint. The pid is cleared because the dump engine
/// has killed the process tree — leaving a stale pid behind would let a later
/// `stop` signal whatever pid the kernel recycled it into.
pub fn set_checkpointed(id: &str, snapshot: &std::path::Path) -> Result<()> {
    update(id, |s| {
        s.status = Status::Checkpointed.as_str().to_string();
        s.pid = None;
        s.snapshot = Some(snapshot.display().to_string());
        s.checkpointed_at = Some(now_iso());
    })
}

/// Record a completed restore.
pub fn set_restored(id: &str, pid: Pid) -> Result<()> {
    update(id, |s| {
        s.status = Status::Running.as_str().to_string();
        s.pid = Some(pid.as_raw());
        s.restored_at = Some(now_iso());
        s.generation += 1;
        s.exit_code = None;
        s.stopped_at = None;
    })
}

/// Read-modify-write one container's state file.
pub fn update<F: FnOnce(&mut ContainerState)>(id: &str, f: F) -> Result<()> {
    let dir = ContainerStateDir::for_id(id);
    let mut state = dir.load_state()?;
    f(&mut state);
    dir.save_state(&state)
}

/// Write a state record that did not originate here — used when a migrated
/// container lands on a new node and has no local history.
pub fn adopt(state: &ContainerState, config: &ContainerConfig) -> Result<()> {
    ContainerStateDir::init()?;
    let dir = ContainerStateDir::for_id(&state.id);
    dir.create()?;
    dir.save_state(state)?;
    dir.save_config(&serde_json::to_string_pretty(config)?)?;
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

/// Current time as an RFC 3339 UTC timestamp.
///
/// Hand-rolled civil-date conversion (Howard Hinnant's `days_from_civil`
/// inverse) rather than pulling in `chrono`. The previous version of this
/// function pinned every date to January 1st and derived the year by dividing
/// by 365 days, which made every timestamp the runtime has ever written wrong;
/// snapshots record when they were taken, so that had to be real.
pub fn now_iso() -> String {
    use std::time::UNIX_EPOCH;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_iso_is_a_real_utc_timestamp() {
        let s = now_iso();
        assert_eq!(s.len(), 20, "{s}");
        assert!(s.ends_with('Z'), "{s}");
        let year: i32 = s[..4].parse().expect("year");
        let month: u32 = s[5..7].parse().expect("month");
        let day: u32 = s[8..10].parse().expect("day");
        assert!(year >= 2024 && year < 2100, "implausible year in {s}");
        assert!((1..=12).contains(&month), "implausible month in {s}");
        assert!((1..=31).contains(&day), "implausible day in {s}");
        // Not every date is the 1st of January: the bug this replaced was
        // exactly that, and a test that only checks the format would miss it.
    }

    #[test]
    fn status_round_trips_through_its_string_form() {
        for st in [
            Status::Created,
            Status::Running,
            Status::Stopped,
            Status::Failed,
            Status::Checkpointing,
            Status::Checkpointed,
            Status::Restoring,
            Status::Migrated,
        ] {
            let mut s = ContainerState::new("x".into());
            s.status = st.as_str().to_string();
            assert!(s.is(st), "{}", st.as_str());
        }
    }
}

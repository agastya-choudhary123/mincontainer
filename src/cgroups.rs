use crate::config::Resources;
use crate::error::{Result, RuntimeError};
use nix::unistd::Pid;
use std::fs;
use std::path::PathBuf;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// A cgroup v2 leaf that limits and measures one container.
pub struct Cgroup {
    path: PathBuf,
}

/// Enable the controllers we need for child cgroups of the root.
///
/// cgroup v2 forbids a cgroup from *both* containing processes and enabling
/// controllers in `subtree_control` (the "no internal process" rule), unless it
/// is the true root. Inside a container the mount root is only a *namespace*
/// root, so it is not exempt: our own runtime process living there blocks
/// delegation. We therefore move ourselves into a dedicated leaf first, which
/// empties the root and lets us enable the controllers.
fn ensure_delegation() -> Result<()> {
    let available = fs::read_to_string(format!("{CGROUP_ROOT}/cgroup.controllers"))
        .map_err(|e| RuntimeError::Cgroup(format!("read cgroup.controllers: {e}")))?;
    let want: Vec<String> = ["memory", "cpu", "pids"]
        .iter()
        .filter(|c| available.split_whitespace().any(|a| &a == *c))
        .map(|c| format!("+{c}"))
        .collect();
    if want.is_empty() {
        return Err(RuntimeError::Cgroup(
            "no memory/cpu/pids controllers available (need cgroup v2, privileged)".into(),
        ));
    }

    let subtree = format!("{CGROUP_ROOT}/cgroup.subtree_control");
    // Already delegated? Nothing to do.
    if let Ok(cur) = fs::read_to_string(&subtree) {
        if want.iter().all(|w| cur.split_whitespace().any(|c| format!("+{c}") == *w)) {
            return Ok(());
        }
    }

    // Move *every* process out of the root into a leaf so the root is empty
    // (the "no internal process" rule counts all members, not just ours — e.g.
    // a parent shell when the runtime is not the container's PID 1).
    let init = format!("{CGROUP_ROOT}/mc.init");
    fs::create_dir_all(&init)
        .map_err(|e| RuntimeError::Cgroup(format!("mkdir mc.init: {e}")))?;

    let root_procs = fs::read_to_string(format!("{CGROUP_ROOT}/cgroup.procs")).unwrap_or_default();
    for pid in root_procs.split_whitespace() {
        // Migrating some pids may race/fail; best effort.
        let _ = fs::write(format!("{init}/cgroup.procs"), pid);
    }

    fs::write(&subtree, want.join(" "))
        .map_err(|e| RuntimeError::Cgroup(format!("enable {}: {e}", want.join(" "))))?;
    Ok(())
}

impl Cgroup {
    /// Create the cgroup leaf and enable the controllers we need.
    pub fn create(id: &str) -> Result<Self> {
        ensure_delegation()?;

        let path = PathBuf::from(CGROUP_ROOT).join(id);
        fs::create_dir_all(&path)
            .map_err(|e| RuntimeError::Cgroup(format!("mkdir {}: {e}", path.display())))?;

        Ok(Cgroup { path })
    }

    fn write(&self, file: &str, value: &str) -> Result<()> {
        let f = self.path.join(file);
        fs::write(&f, value)
            .map_err(|e| RuntimeError::Cgroup(format!("write {}={value:?}: {e}", f.display())))
    }

    /// Apply the configured resource limits.
    pub fn apply(&self, r: &Resources) -> Result<()> {
        if r.memory_max > 0 {
            self.write("memory.max", &r.memory_max.to_string())?;
            // Disable swap so the memory ceiling is a true RSS ceiling.
            let _ = self.write("memory.swap.max", "0");
        }
        if r.cpu_quota > 0 {
            self.write("cpu.max", &format!("{} {}", r.cpu_quota, r.cpu_period))?;
        }
        if r.pids_max > 0 {
            self.write("pids.max", &r.pids_max.to_string())?;
        }
        Ok(())
    }

    /// Move a process (and thus its future children) into this cgroup.
    pub fn add_process(&self, pid: Pid) -> Result<()> {
        self.write("cgroup.procs", &pid.as_raw().to_string())
    }

    /// Peak memory used by the cgroup, in bytes.
    pub fn peak_memory(&self) -> Option<u64> {
        // memory.peak (kernel >= 5.19); fall back to current.
        for f in ["memory.peak", "memory.current"] {
            if let Ok(s) = fs::read_to_string(self.path.join(f)) {
                if let Ok(v) = s.trim().parse::<u64>() {
                    return Some(v);
                }
            }
        }
        None
    }

    /// Total CPU time consumed by the cgroup, in microseconds.
    pub fn cpu_usage_usec(&self) -> Option<u64> {
        let s = fs::read_to_string(self.path.join("cpu.stat")).ok()?;
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("usage_usec ") {
                return rest.trim().parse().ok();
            }
        }
        None
    }

    /// Remove the cgroup. Must be empty (no processes) first.
    pub fn cleanup(&self) {
        let _ = fs::remove_dir(&self.path);
    }
}

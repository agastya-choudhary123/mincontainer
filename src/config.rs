use serde::{Deserialize, Serialize};

/// A volume mount: host_path -> container_path (bind mount).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    pub host_path: String,
    pub container_path: String,
}

/// Full specification for a single container run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerConfig {
    pub id: String,
    pub hostname: String,
    pub rootfs: String,
    pub command: Vec<String>,
    pub env: Vec<String>,
    pub volumes: Vec<Volume>,
    pub resources: Resources,
    pub network: bool,
    pub seccomp: bool,
    pub drop_caps: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resources {
    /// Hard memory ceiling in bytes (memory.max). 0 = unlimited.
    pub memory_max: u64,
    /// CPU quota in microseconds per period (cpu.max). 0 = unlimited.
    pub cpu_quota: u64,
    /// CPU period in microseconds.
    pub cpu_period: u64,
    /// Max number of pids (pids.max). 0 = unlimited.
    pub pids_max: u64,
}

impl Default for Resources {
    fn default() -> Self {
        Resources {
            memory_max: 128 * 1024 * 1024, // 128 MiB
            cpu_quota: 0,
            cpu_period: 100_000,
            pids_max: 128,
        }
    }
}

impl ContainerConfig {
    pub fn new(rootfs: String, command: Vec<String>) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        let short = id[..12].to_string();
        ContainerConfig {
            id,
            hostname: format!("mc-{short}"),
            rootfs,
            command,
            env: vec![
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
                "HOME=/root".to_string(),
                "TERM=xterm".to_string(),
            ],
            volumes: Vec::new(),
            resources: Resources::default(),
            network: false,
            seccomp: true,
            drop_caps: true,
        }
    }

    /// First 12 chars of the id, used for cgroup/veth naming.
    pub fn short_id(&self) -> &str {
        &self.id[..12]
    }
}

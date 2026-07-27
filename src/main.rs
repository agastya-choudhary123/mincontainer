use clap::{Parser, Subcommand};
use mincontainer::config::{ContainerConfig, Resources, Volume};
use mincontainer::state::{self, ContainerStateDir};
use mincontainer::{container, capabilities, seccomp};
use nix::unistd::Pid;
use std::fs;

#[derive(Parser)]
#[command(name = "mincontainer", version, about = "A minimal from-scratch Linux container runtime")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a container (config only, don't start).
    Create(CreateArgs),
    /// Start an existing container.
    Start { id: String },
    /// Stop a running container.
    Stop { id: String },
    /// List all containers.
    Ps,
    /// View container logs.
    Logs { id: String },
    /// Delete a container.
    Rm { id: String },
    /// Run a container (one-shot: create + start + wait).
    Run(RunArgs),
    /// Benchmark startup latency and throughput.
    Bench(BenchArgs),
    /// Display runtime capabilities.
    Info,
}

#[derive(Parser)]
struct CreateArgs {
    /// Container ID (defaults to UUID).
    #[arg(long)]
    id: Option<String>,

    /// Path to rootfs.
    #[arg(long)]
    rootfs: String,

    /// Memory limit in bytes.
    #[arg(long, default_value_t = 128 * 1024 * 1024)]
    memory: u64,

    /// Bind mount (--bind /host:/container).
    #[arg(long)]
    bind: Vec<String>,

    /// CPU quota in microseconds.
    #[arg(long, default_value_t = 0)]
    cpu: u64,

    /// Max processes.
    #[arg(long, default_value_t = 128)]
    pids: u64,

    /// Enable networking.
    #[arg(long, default_value_t = false)]
    net: bool,

    /// Disable seccomp.
    #[arg(long, default_value_t = false)]
    no_seccomp: bool,

    /// Disable capability dropping.
    #[arg(long, default_value_t = false)]
    no_drop_caps: bool,

    /// Command to run (after `--`).
    #[arg(last = true, required = true)]
    cmd: Vec<String>,
}

#[derive(Parser)]
struct RunArgs {
    #[arg(long)]
    rootfs: String,

    #[arg(long, default_value_t = 128 * 1024 * 1024)]
    memory: u64,

    #[arg(long)]
    bind: Vec<String>,

    #[arg(long, default_value_t = 0)]
    cpu: u64,

    #[arg(long, default_value_t = 128)]
    pids: u64,

    #[arg(long, default_value_t = false)]
    net: bool,

    #[arg(long, default_value_t = false)]
    no_seccomp: bool,

    #[arg(long, default_value_t = false)]
    no_drop_caps: bool,

    #[arg(long, default_value_t = false)]
    json: bool,

    #[arg(last = true, required = true)]
    cmd: Vec<String>,
}

#[derive(Parser)]
struct BenchArgs {
    #[arg(long)]
    rootfs: String,

    #[arg(long, default_value_t = 30)]
    runs: u32,

    #[arg(long, default_value_t = 128 * 1024 * 1024)]
    memory: u64,

    #[arg(long)]
    cmd: Vec<String>,
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Commands::Create(a) => cmd_create(a),
        Commands::Start { id } => cmd_start(&id),
        Commands::Stop { id } => cmd_stop(&id),
        Commands::Ps => cmd_ps(),
        Commands::Logs { id } => cmd_logs(&id),
        Commands::Rm { id } => cmd_rm(&id),
        Commands::Run(a) => cmd_run(a),
        Commands::Bench(a) => cmd_bench(a),
        Commands::Info => {
            cmd_info();
            0
        }
    };
    std::process::exit(code);
}

fn cmd_create(a: CreateArgs) -> i32 {
    let id = a.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    if let Err(e) = state::ContainerStateDir::init() {
        eprintln!("[mincontainer] init state: {e}");
        return 1;
    }

    if let Err(e) = state::init_container(&id) {
        eprintln!("[mincontainer] create state: {e}");
        return 1;
    }

    let mut cfg = ContainerConfig::new(a.rootfs, a.cmd);
    cfg.id = id.clone();
    cfg.resources = Resources {
        memory_max: a.memory,
        cpu_quota: a.cpu,
        cpu_period: 100_000,
        pids_max: a.pids,
    };
    cfg.network = a.net;
    cfg.seccomp = !a.no_seccomp;
    cfg.drop_caps = !a.no_drop_caps;

    // Parse bind mounts.
    for bind in a.bind {
        let parts: Vec<&str> = bind.split(':').collect();
        if parts.len() != 2 {
            eprintln!("[mincontainer] bad bind format (use /host:/container)");
            return 1;
        }
        cfg.volumes.push(Volume {
            host_path: parts[0].to_string(),
            container_path: parts[1].to_string(),
        });
    }

    let dir = ContainerStateDir::for_id(&id);
    if let Err(e) = dir.save_config(&serde_json::to_string_pretty(&cfg).unwrap()) {
        eprintln!("[mincontainer] save config: {e}");
        return 1;
    }

    println!("{}", id);
    0
}

fn cmd_start(id: &str) -> i32 {
    if let Err(e) = state::ContainerStateDir::init() {
        eprintln!("[mincontainer] init state: {e}");
        return 1;
    }

    let dir = ContainerStateDir::for_id(id);
    if !dir.exists() {
        eprintln!("[mincontainer] container {id} not found");
        return 1;
    }

    let config_json = match fs::read_to_string(dir.config_file()) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("[mincontainer] read config: {e}");
            return 1;
        }
    };

    let cfg: ContainerConfig = match serde_json::from_str(&config_json) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[mincontainer] parse config: {e}");
            return 1;
        }
    };

    // Open stdout/stderr files for capture (unused in MVP; stored for logs command).
    let stdout_file = dir.stdout_file();
    let stderr_file = dir.stderr_file();

    let _stdout = match fs::File::create(&stdout_file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[mincontainer] create stdout: {e}");
            return 1;
        }
    };
    let _stderr = match fs::File::create(&stderr_file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[mincontainer] create stderr: {e}");
            return 1;
        }
    };

    // Run the container (this is blocking).
    match container::run(&cfg, 0) {
        Ok(m) => {
            // Save the final state.
            let _ = state::set_stopped(id, m.exit_code);
            eprintln!(
                "[{}] exit={} setup={:.2}ms wall={:.2}ms peak_mem={:.2}MiB",
                id,
                m.exit_code,
                m.setup_ms,
                m.wall_ms,
                m.peak_mem_bytes as f64 / (1024.0 * 1024.0),
            );
            m.exit_code
        }
        Err(e) => {
            eprintln!("[mincontainer] error: {e}");
            let _ = state::set_stopped(id, 127);
            1
        }
    }
}

fn cmd_stop(id: &str) -> i32 {
    if let Err(e) = state::ContainerStateDir::init() {
        eprintln!("[mincontainer] init state: {e}");
        return 1;
    }

    let st = match state::get_container(id) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[mincontainer] get container: {e}");
            return 1;
        }
    };

    if let Some(pid) = st.pid {
        let pid = Pid::from_raw(pid);
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
        // Wait a bit, then SIGKILL if still running.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
    }

    0
}

fn cmd_ps() -> i32 {
    if let Err(e) = state::ContainerStateDir::init() {
        eprintln!("[mincontainer] init state: {e}");
        return 1;
    }

    match state::list_all() {
        Ok(containers) => {
            println!("ID                              STATUS    PID");
            for c in containers {
                let pid_str = c.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string());
                println!(
                    "{:<32} {:<9} {}",
                    &c.id[..c.id.len().min(32)],
                    c.status,
                    pid_str
                );
            }
            0
        }
        Err(e) => {
            eprintln!("[mincontainer] list containers: {e}");
            1
        }
    }
}

fn cmd_logs(id: &str) -> i32 {
    if let Err(e) = state::ContainerStateDir::init() {
        eprintln!("[mincontainer] init state: {e}");
        return 1;
    }

    let dir = ContainerStateDir::for_id(id);
    if !dir.exists() {
        eprintln!("[mincontainer] container {id} not found");
        return 1;
    }

    println!("=== STDOUT ===");
    if let Ok(out) = fs::read_to_string(dir.stdout_file()) {
        print!("{}", out);
    }

    println!("\n=== STDERR ===");
    if let Ok(err) = fs::read_to_string(dir.stderr_file()) {
        print!("{}", err);
    }

    0
}

fn cmd_rm(id: &str) -> i32 {
    if let Err(e) = state::ContainerStateDir::init() {
        eprintln!("[mincontainer] init state: {e}");
        return 1;
    }

    let dir = ContainerStateDir::for_id(id);
    if !dir.exists() {
        eprintln!("[mincontainer] container {id} not found");
        return 1;
    }

    if let Err(e) = dir.cleanup() {
        eprintln!("[mincontainer] cleanup: {e}");
        return 1;
    }

    println!("Removed {id}");
    0
}

fn cmd_run(a: RunArgs) -> i32 {
    let res = Resources {
        memory_max: a.memory,
        cpu_quota: a.cpu,
        cpu_period: 100_000,
        pids_max: a.pids,
    };
    let mut cfg = ContainerConfig::new(a.rootfs, a.cmd);
    cfg.network = a.net;
    cfg.seccomp = !a.no_seccomp;
    cfg.drop_caps = !a.no_drop_caps;
    cfg.resources = res;

    for bind in a.bind {
        let parts: Vec<&str> = bind.split(':').collect();
        if parts.len() == 2 {
            cfg.volumes.push(Volume {
                host_path: parts[0].to_string(),
                container_path: parts[1].to_string(),
            });
        }
    }

    match container::run(&cfg, 0) {
        Ok(m) => {
            if a.json {
                println!("{}", serde_json::to_string_pretty(&m).unwrap());
            } else {
                eprintln!(
                    "\n[mincontainer] exit={} setup={:.2}ms wall={:.2}ms peak_mem={:.2}MiB cpu={}us",
                    m.exit_code, m.setup_ms, m.wall_ms,
                    m.peak_mem_bytes as f64 / (1024.0 * 1024.0),
                    m.cpu_usec,
                );
            }
            m.exit_code
        }
        Err(e) => {
            eprintln!("[mincontainer] error: {e}");
            1
        }
    }
}

fn cmd_bench(a: BenchArgs) -> i32 {
    let cmd = if a.cmd.is_empty() {
        vec!["/bin/true".to_string()]
    } else {
        a.cmd
    };

    let mut setup = Vec::new();
    let mut wall = Vec::new();
    let mut mem = Vec::new();

    eprintln!("[bench] running {} iterations of {:?}...", a.runs, cmd);
    for i in 0..a.runs {
        let res = Resources {
            memory_max: a.memory,
            ..Default::default()
        };
        let mut cfg = ContainerConfig::new(a.rootfs.clone(), cmd.clone());
        cfg.resources = res;
        match container::run(&cfg, 0) {
            Ok(m) => {
                setup.push(m.setup_ms);
                wall.push(m.wall_ms);
                mem.push(m.peak_mem_bytes as f64);
            }
            Err(e) => {
                eprintln!("[bench] run {i} failed: {e}");
                return 1;
            }
        }
    }

    print_stats("setup overhead (ms)", &setup, 1.0);
    print_stats("end-to-end wall (ms)", &wall, 1.0);
    print_stats("peak memory (MiB)", &mem, 1.0 / (1024.0 * 1024.0));
    0
}

fn print_stats(label: &str, xs: &[f64], scale: f64) {
    if xs.is_empty() {
        return;
    }
    let mut v: Vec<f64> = xs.iter().map(|x| x * scale).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    let mean = v.iter().sum::<f64>() / n as f64;
    let p50 = v[n / 2];
    let p99 = v[((n as f64 * 0.99) as usize).min(n - 1)];
    let min = v[0];
    let max = v[n - 1];
    println!(
        "{label:24} n={n:<4} mean={mean:8.3}  min={min:8.3}  p50={p50:8.3}  p99={p99:8.3}  max={max:8.3}"
    );
}

fn cmd_info() {
    println!("mincontainer — isolation applied per container:");
    println!("  namespaces : PID, mount, UTS, IPC, network (5)");
    println!("  rootfs     : pivot_root + private mounts, fresh /proc and /dev");
    println!("  cgroup v2  : memory.max, cpu.max, pids.max + peak-memory/cpu accounting");
    println!("  network    : veth pair into a bridge, per-container IP, NAT egress");
    println!("  volumes    : bind mounts (--bind /host:/container)");
    println!("  seccomp    : default-allow BPF filter, {} syscalls denied (EPERM)", seccomp::blocked_count());
    println!("  caps       : {} dangerous capabilities dropped from the bounding set", capabilities::dropped_count());
}

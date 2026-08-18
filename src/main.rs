use clap::{Parser, Subcommand};
use mincontainer::config::{ContainerConfig, Resources, Volume};
use mincontainer::state::{self, ContainerStateDir, Status};
use mincontainer::{capabilities, checkpoint, container, criu, migrate, restore, seccomp, transport};
use nix::unistd::Pid;
use std::fs;
use std::path::PathBuf;

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
    Start {
        id: String,
        /// Run in the background and return immediately. Required before a
        /// container can be checkpointed: a foreground container is owned by
        /// the terminal that launched it.
        #[arg(long, short, default_value_t = false)]
        detach: bool,
    },
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
    /// Freeze a running container into a snapshot.
    Checkpoint(CheckpointArgs),
    /// Rebuild a container from a snapshot and resume it here.
    Restore(RestoreArgs),
    /// Checkpoint a container, ship it to another node, and restore it there.
    Migrate(MigrateArgs),
    /// Accept incoming migrations.
    Serve(ServeArgs),
    /// Print a snapshot's manifest without restoring it.
    Inspect(InspectArgs),
    /// Benchmark checkpoint and restore latency.
    BenchCr(BenchCrArgs),
    /// Display runtime capabilities.
    Info,
}

#[derive(Parser)]
struct CheckpointArgs {
    id: String,

    /// Snapshot name, stored under the container's snapshots directory.
    #[arg(long, default_value = "latest")]
    name: String,

    /// Write the snapshot here instead.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Leave the container running after dumping it.
    #[arg(long, default_value_t = false)]
    leave_running: bool,

    /// Serialise established TCP connections instead of refusing to checkpoint.
    #[arg(long, default_value_t = false)]
    allow_tcp: bool,

    #[arg(long, default_value_t = false)]
    json: bool,
}

#[derive(Parser)]
struct RestoreArgs {
    id: String,

    /// Snapshot file to restore from.
    #[arg(long)]
    from: Option<PathBuf>,

    /// Index used to pick the container IP when networking is enabled.
    #[arg(long, default_value_t = 0)]
    index: u8,

    #[arg(long, default_value_t = false)]
    allow_tcp: bool,

    #[arg(long, default_value_t = false)]
    json: bool,
}

#[derive(Parser)]
struct MigrateArgs {
    id: String,

    /// Target node, `host` or `host:port`.
    host: String,

    /// Shared secret; must match the receiver's --token.
    #[arg(long, default_value = "")]
    token: String,

    /// Ship the existing snapshot instead of taking a fresh one.
    #[arg(long, default_value_t = false)]
    use_existing: bool,

    #[arg(long, default_value_t = false)]
    allow_tcp: bool,

    /// Do not release the local container after a successful handover.
    #[arg(long, default_value_t = false)]
    keep_local: bool,

    #[arg(long, default_value_t = false)]
    json: bool,
}

#[derive(Parser)]
struct ServeArgs {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:7373")]
    listen: String,

    /// Shared secret senders must present.
    #[arg(long, default_value = "")]
    token: String,

    /// Index used to pick container IPs for arrivals.
    #[arg(long, default_value_t = 0)]
    index: u8,
}

#[derive(Parser)]
struct InspectArgs {
    /// Path to a .mcsnap file.
    snapshot: PathBuf,

    /// Also verify the whole-file checksum.
    #[arg(long, default_value_t = false)]
    verify: bool,

    #[arg(long, default_value_t = false)]
    json: bool,
}

#[derive(Parser)]
struct BenchCrArgs {
    #[arg(long)]
    rootfs: String,

    #[arg(long, default_value_t = 10)]
    runs: u32,

    /// Megabytes of heap the workload touches before being checkpointed, so
    /// snapshot size can be measured against real memory rather than an idle
    /// process.
    #[arg(long, default_value_t = 0)]
    touch_mb: u32,

    #[arg(long, default_value_t = 256 * 1024 * 1024)]
    memory: u64,

    #[arg(long, default_value_t = false)]
    json: bool,
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
        Commands::Start { id, detach } => cmd_start(&id, detach),
        Commands::Stop { id } => cmd_stop(&id),
        Commands::Ps => cmd_ps(),
        Commands::Logs { id } => cmd_logs(&id),
        Commands::Rm { id } => cmd_rm(&id),
        Commands::Run(a) => cmd_run(a),
        Commands::Bench(a) => cmd_bench(a),
        Commands::Checkpoint(a) => cmd_checkpoint(a),
        Commands::Restore(a) => cmd_restore(a),
        Commands::Migrate(a) => cmd_migrate(a),
        Commands::Serve(a) => cmd_serve(a),
        Commands::Inspect(a) => cmd_inspect(a),
        Commands::BenchCr(a) => cmd_bench_cr(a),
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

fn cmd_start(id: &str, detach: bool) -> i32 {
    if let Err(e) = state::ContainerStateDir::init() {
        eprintln!("[mincontainer] init state: {e}");
        return 1;
    }

    let dir = ContainerStateDir::for_id(id);
    if !dir.exists() {
        eprintln!("[mincontainer] container {id} not found");
        return 1;
    }
    if let Err(e) = dir.create() {
        eprintln!("[mincontainer] prepare state dirs: {e}");
        return 1;
    }

    let cfg: ContainerConfig = match dir.load_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[mincontainer] read config: {e}");
            return 1;
        }
    };

    if !detach {
        // Foreground: the container inherits our stdio, as it always has.
        return match container::run(&cfg, 0) {
            Ok(m) => {
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
        };
    }

    // Detached: fork a supervisor that outlives this CLI invocation. The
    // supervisor owns the container and stays in the host PID namespace, which
    // is what lets a later `checkpoint` find the container by pid.
    //
    // Truncate the log files first, so `logs` after a restart is not a
    // confusing concatenation of two runs.
    for f in [dir.io_dir().join("stdout"), dir.io_dir().join("stderr")] {
        let _ = fs::File::create(&f);
    }

    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Parent { child: _ }) => {
            // Wait for the supervisor to publish a pid, so `start` returning
            // means the container is genuinely up and checkpointable.
            match await_running(id, std::time::Duration::from_secs(10)) {
                Some(pid) => {
                    println!("{id}");
                    eprintln!("[mincontainer] {id} running detached (pid {pid})");
                    0
                }
                None => {
                    eprintln!(
                        "[mincontainer] {id} did not come up within 10s; check `mincontainer logs {id}`"
                    );
                    1
                }
            }
        }
        Ok(nix::unistd::ForkResult::Child) => {
            supervise(id, &cfg, &dir);
        }
        Err(e) => {
            eprintln!("[mincontainer] fork supervisor: {e}");
            1
        }
    }
}

/// The detached supervisor. Owns one container for its whole life and never
/// returns.
fn supervise(id: &str, cfg: &ContainerConfig, dir: &ContainerStateDir) -> ! {
    // Detach from the launching terminal so the container survives it.
    let _ = nix::unistd::setsid();

    // Our own diagnostics go to a supervisor log; the container's stdio is
    // redirected separately, inside its own mount namespace.
    let log = dir.path().join("supervisor.log");
    if let Ok(f) = fs::OpenOptions::new().create(true).append(true).open(&log) {
        use std::os::fd::AsRawFd;
        let fd = f.as_raw_fd();
        let _ = nix::unistd::dup2(fd, 1);
        let _ = nix::unistd::dup2(fd, 2);
    }

    let io = container::Io { dir: dir.io_dir() };
    let handle = match container::spawn(cfg, 0, Some(&io)) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[mincontainer] spawn failed: {e}");
            let _ = state::update(id, |s| {
                s.status = Status::Failed.as_str().to_string();
                s.pid = None;
            });
            std::process::exit(1);
        }
    };

    let container_pid = handle.container_pid;
    // Record ourselves alongside the container: a checkpoint has to wait for
    // this process to finish tearing down before it can report success, or its
    // cleanup races the next restore's setup.
    if let Err(e) =
        state::set_running_under(id, container_pid, Some(nix::unistd::getpid()))
    {
        eprintln!("[mincontainer] record running state: {e}");
    }

    let metrics = handle.wait();

    // A checkpoint kills the container's processes on purpose. Without this
    // check the supervisor would race the checkpoint to the state file and
    // overwrite `checkpointed` with `stopped`, which would make the container
    // look dead rather than frozen.
    let frozen = state::get_container(id)
        .map(|s| s.is(Status::Checkpointing) || s.is(Status::Checkpointed))
        .unwrap_or(false);

    match metrics {
        Ok(m) => {
            if frozen {
                eprintln!("[mincontainer] {id} processes ended via checkpoint; leaving state alone");
            } else {
                let _ = state::set_stopped(id, m.exit_code);
                eprintln!(
                    "[mincontainer] {id} exited code={} wall={:.2}ms peak_mem={:.2}MiB",
                    m.exit_code,
                    m.wall_ms,
                    m.peak_mem_bytes as f64 / (1024.0 * 1024.0)
                );
            }
        }
        Err(e) => {
            eprintln!("[mincontainer] wait failed: {e}");
            if !frozen {
                let _ = state::set_stopped(id, 127);
            }
        }
    }
    std::process::exit(0);
}

/// Poll the state file until the supervisor publishes a live pid.
fn await_running(id: &str, timeout: std::time::Duration) -> Option<i32> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(st) = state::get_container(id) {
            if st.is(Status::Running) {
                if let Some(pid) = st.pid {
                    return Some(pid);
                }
            }
            if st.is(Status::Failed) || st.is(Status::Stopped) {
                return None;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    None
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

    if st.is(Status::Checkpointed) {
        eprintln!(
            "[mincontainer] {id} is checkpointed — it has no processes to stop. \
             Use `mincontainer rm {id}` to discard it, or `restore` to bring it back."
        );
        return 1;
    }

    let Some(pid) = st.pid else {
        eprintln!("[mincontainer] {id} is {} and has no pid", st.status);
        return 1;
    };

    let pid = Pid::from_raw(pid);
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
    // Give it a moment to exit on its own before insisting.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        if !checkpoint::pid_alive(pid.as_raw()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if checkpoint::pid_alive(pid.as_raw()) {
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
    }

    0
}

fn cmd_ps() -> i32 {
    if let Err(e) = state::ContainerStateDir::init() {
        eprintln!("[mincontainer] init state: {e}");
        return 1;
    }

    // Sweep up after any command that was killed mid-checkpoint or
    // mid-restore. Doing it here means the debris is cleared by the next
    // command an operator runs, instead of needing a separate repair tool.
    restore::reconcile_all();

    match state::list_all() {
        Ok(mut containers) => {
            containers.sort_by(|a, b| a.created_at.cmp(&b.created_at));
            println!(
                "{:<38} {:<14} {:<8} {:<4} {}",
                "ID", "STATUS", "PID", "GEN", "SNAPSHOT"
            );
            for c in containers {
                let pid = c.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
                let snap = c
                    .snapshot
                    .as_deref()
                    .map(|p| {
                        std::path::Path::new(p)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| p.to_string())
                    })
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{:<38} {:<14} {:<8} {:<4} {}",
                    &c.id[..c.id.len().min(38)],
                    c.status,
                    pid,
                    c.generation,
                    snap
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

    // Detached containers write into the io directory that is bind-mounted
    // into their mount namespace; that copy travels with a snapshot, so logs
    // survive a checkpoint and a migration.
    for (label, path) in [
        ("STDOUT", dir.io_dir().join("stdout")),
        ("STDERR", dir.io_dir().join("stderr")),
    ] {
        println!("=== {label} ===");
        match fs::read_to_string(&path) {
            Ok(t) => print!("{t}"),
            Err(e) => println!("({e})"),
        }
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


fn cmd_checkpoint(a: CheckpointArgs) -> i32 {
    if let Err(e) = state::ContainerStateDir::init() {
        eprintln!("[mincontainer] init state: {e}");
        return 1;
    }
    let opts = checkpoint::Options {
        name: a.name,
        dest: a.output,
        leave_running: a.leave_running,
        allow_tcp: a.allow_tcp,
    };
    match checkpoint::checkpoint(&a.id, &opts) {
        Ok(r) => {
            if a.json {
                println!("{}", serde_json::to_string_pretty(&r).unwrap());
            } else {
                println!("{}", r.snapshot.display());
                eprintln!(
                    "[mincontainer] checkpointed {} — {:.2} MiB in {} entries \
                     (preflight {:.1}ms, freeze+dump {:.1}ms, pack {:.1}ms, total {:.1}ms)",
                    r.id,
                    r.snapshot_bytes as f64 / (1024.0 * 1024.0),
                    r.entries,
                    r.preflight_ms,
                    r.dump_ms,
                    r.pack_ms,
                    r.total_ms,
                );
            }
            0
        }
        Err(e) => {
            eprintln!("[mincontainer] checkpoint failed: {e}");
            1
        }
    }
}

fn cmd_restore(a: RestoreArgs) -> i32 {
    if let Err(e) = state::ContainerStateDir::init() {
        eprintln!("[mincontainer] init state: {e}");
        return 1;
    }
    let opts = restore::Options { from: a.from, index: a.index, allow_tcp: a.allow_tcp };
    match restore::restore(&a.id, &opts) {
        Ok(r) => {
            if a.json {
                println!("{}", serde_json::to_string_pretty(&r).unwrap());
            } else {
                println!("{}", r.id);
                eprintln!(
                    "[mincontainer] restored {} as pid {} (generation {}) — \
                     verify {:.1}ms, unpack {:.1}ms, restore {:.1}ms, total {:.1}ms{}",
                    r.id,
                    r.pid,
                    r.generation,
                    r.verify_ms,
                    r.unpack_ms,
                    r.restore_ms,
                    r.total_ms,
                    r.container_ip.map(|ip| format!(", ip {ip}")).unwrap_or_default(),
                );
            }
            0
        }
        Err(e) => {
            eprintln!("[mincontainer] restore failed: {e}");
            1
        }
    }
}

fn cmd_migrate(a: MigrateArgs) -> i32 {
    if let Err(e) = state::ContainerStateDir::init() {
        eprintln!("[mincontainer] init state: {e}");
        return 1;
    }
    let opts = migrate::Options {
        token: a.token,
        use_existing: a.use_existing,
        allow_tcp: a.allow_tcp,
        keep_local: a.keep_local,
    };
    match migrate::migrate(&a.id, &a.host, &opts) {
        Ok(r) => {
            if a.json {
                println!("{}", serde_json::to_string_pretty(&r).unwrap());
            } else {
                eprintln!(
                    "[mincontainer] migrated {} to {} — {:.2} MiB, checkpoint {:.1}ms, \
                     transfer {:.1}ms ({:.1} MiB/s), remote restore {:.1}ms, total {:.1}ms; \
                     now pid {} there",
                    r.id,
                    r.target,
                    r.snapshot_bytes as f64 / (1024.0 * 1024.0),
                    r.checkpoint_ms,
                    r.transfer_ms,
                    r.throughput_mib_s,
                    r.remote_restore_ms,
                    r.total_ms,
                    r.remote_pid,
                );
            }
            0
        }
        Err(e) => {
            eprintln!("[mincontainer] migration failed: {e}");
            1
        }
    }
}

fn cmd_serve(a: ServeArgs) -> i32 {
    if let Err(e) = state::ContainerStateDir::init() {
        eprintln!("[mincontainer] init state: {e}");
        return 1;
    }
    match transport::serve(&a.listen, &a.token, a.index) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[mincontainer] serve failed: {e}");
            1
        }
    }
}

fn cmd_inspect(a: InspectArgs) -> i32 {
    match restore::inspect(&a.snapshot, a.verify) {
        Ok(m) => {
            if a.json {
                println!("{}", serde_json::to_string_pretty(&m).unwrap());
                return 0;
            }
            println!("snapshot     : {}", a.snapshot.display());
            println!("format       : v{}", m.format_version);
            println!("producer     : {}", m.producer);
            println!("engine       : {} {}", m.engine, m.engine_version);
            println!("created      : {}", m.created_at);
            println!("source       : {} ({}, kernel {})", m.source_host, m.source_arch, m.source_kernel);
            println!("container    : {}", m.container_id);
            println!("command      : {:?}", m.config.command);
            println!("rootfs       : {}", m.config.rootfs);
            println!("network      : {}", m.config.network);
            println!("dumped pid   : {} (freeze+dump {:.1}ms)", m.checkpoint_pid, m.dump_ms);
            println!(
                "cgroup       : memory.max={} cpu.max={} pids.max={} peak={:.2}MiB cpu={}us",
                m.cgroup.memory_max,
                m.cgroup.cpu_max,
                m.cgroup.pids_max,
                m.cgroup.memory_peak as f64 / (1024.0 * 1024.0),
                m.cgroup.cpu_usage_usec,
            );
            println!(
                "namespaces   : pid={:?} mnt={:?} net={:?} uts={:?} ipc={:?}",
                m.namespaces.pid, m.namespaces.mnt, m.namespaces.net, m.namespaces.uts, m.namespaces.ipc
            );
            println!("mounts       : {}", m.mounts.len());
            for mnt in &m.mounts {
                println!("               {:<24} {:<10} {}", mnt.mount_point, mnt.fs_type, mnt.source);
            }
            println!("open fds     : {}", m.open_fds.len());
            for fd in &m.open_fds {
                println!("               {:<4} -> {}", fd.fd, fd.target);
            }
            println!(
                "entries      : {} ({:.2} MiB payload)",
                m.entries.len(),
                m.payload_bytes() as f64 / (1024.0 * 1024.0)
            );
            // The ten largest entries explain where a snapshot's size went.
            let mut by_size = m.entries.clone();
            by_size.sort_by_key(|e| std::cmp::Reverse(e.len));
            for e in by_size.iter().take(10) {
                println!(
                    "               {:<28} {:>10} bytes  crc {:#010x}",
                    e.name, e.len, e.crc32
                );
            }
            if by_size.len() > 10 {
                println!("               ... and {} more", by_size.len() - 10);
            }
            if a.verify {
                println!("checksum     : OK (whole file and every entry)");
            }
            0
        }
        Err(e) => {
            eprintln!("[mincontainer] {e}");
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

/// Measure checkpoint and restore cost against a workload with a known,
/// controllable memory footprint.
///
/// The workload allocates `touch_mb` of heap and *writes to every page* before
/// idling. Touching matters: an untouched allocation is never faulted in, so a
/// checkpoint of it would measure nothing but the runtime's own overhead and
/// report a flatteringly small snapshot.
fn cmd_bench_cr(a: BenchCrArgs) -> i32 {
    if let Err(e) = state::ContainerStateDir::init() {
        eprintln!("[mincontainer] init state: {e}");
        return 1;
    }

    // Fill `touch_mb` MiB with a shell string, one MiB at a time, then idle.
    // Deliberately built from shell builtins so the rootfs needs nothing but
    // /bin/sh, and so the pages are anonymous private memory — the case a
    // checkpoint actually has to serialise.
    let script = format!(
        "chunk=$(awk 'BEGIN{{while(i++<1024) printf \"x\"}}'); \
         i=0; while [ $i -lt {} ]; do j=0; part=; while [ $j -lt 1024 ]; do \
         part=\"$part$chunk\"; j=$((j+1)); done; eval \"blk$i=\\$part\"; i=$((i+1)); done; \
         echo filled; while true; do sleep 1; done",
        a.touch_mb
    );
    let cmd = vec!["/bin/sh".to_string(), "-c".to_string(), script];

    let mut ckpt_ms = Vec::new();
    let mut dump_ms = Vec::new();
    let mut rest_ms = Vec::new();
    let mut sizes = Vec::new();

    eprintln!(
        "[bench-cr] {} iterations, workload touches {} MiB",
        a.runs, a.touch_mb
    );

    for i in 0..a.runs {
        let id = format!("bench-cr-{}", uuid::Uuid::new_v4());
        if let Err(e) = state::init_container(&id) {
            eprintln!("[bench-cr] init {id}: {e}");
            return 1;
        }
        let mut cfg = ContainerConfig::new(a.rootfs.clone(), cmd.clone());
        cfg.id = id.clone();
        cfg.resources = Resources { memory_max: a.memory, ..Default::default() };
        // seccomp blocks nothing this workload needs, but leaving it on keeps
        // the benchmark representative of a real container.
        let dir = ContainerStateDir::for_id(&id);
        let _ = dir.create();
        if let Err(e) = dir.save_config(&serde_json::to_string_pretty(&cfg).unwrap()) {
            eprintln!("[bench-cr] save config: {e}");
            return 1;
        }

        if cmd_start(&id, true) != 0 {
            eprintln!("[bench-cr] run {i}: container did not start");
            return 1;
        }
        // Wait for the workload to finish allocating.
        if !wait_for_log(&dir, "filled", std::time::Duration::from_secs(60)) {
            eprintln!("[bench-cr] run {i}: workload never reported ready");
            let _ = cmd_stop(&id);
            return 1;
        }

        let c = match checkpoint::checkpoint(&id, &checkpoint::Options::default()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[bench-cr] run {i}: checkpoint failed: {e}");
                return 1;
            }
        };
        let r = match restore::restore(&id, &restore::Options::default()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[bench-cr] run {i}: restore failed: {e}");
                return 1;
            }
        };

        ckpt_ms.push(c.total_ms);
        dump_ms.push(c.dump_ms);
        rest_ms.push(r.total_ms);
        sizes.push(c.snapshot_bytes as f64);

        let _ = cmd_stop(&id);
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = ContainerStateDir::for_id(&id).cleanup();
    }

    if a.json {
        let out = serde_json::json!({
            "runs": a.runs,
            "touch_mb": a.touch_mb,
            "checkpoint_total_ms": ckpt_ms,
            "checkpoint_dump_ms": dump_ms,
            "restore_total_ms": rest_ms,
            "snapshot_bytes": sizes,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        print_stats("checkpoint total (ms)", &ckpt_ms, 1.0);
        print_stats("  freeze+dump (ms)", &dump_ms, 1.0);
        print_stats("restore total (ms)", &rest_ms, 1.0);
        print_stats("snapshot size (MiB)", &sizes, 1.0 / (1024.0 * 1024.0));
    }
    0
}

/// Block until `needle` shows up in the container's stdout.
fn wait_for_log(dir: &ContainerStateDir, needle: &str, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    let path = dir.io_dir().join("stdout");
    while std::time::Instant::now() < deadline {
        if let Ok(t) = fs::read_to_string(&path) {
            if t.contains(needle) {
                return true;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
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
    println!("  snapshots  : .mcsnap format v{}, CRC32 per entry and whole-file", mincontainer::snapshot::FORMAT_VERSION);
    println!("  migration  : raw TCP, length-prefixed frames, protocol v{}", transport::PROTO_VERSION);

    print!("  engine     : ");
    match criu::version() {
        Ok(v) => {
            println!("criu {v}");
            match criu::check() {
                Ok((true, _)) => println!("  c/r ready  : yes (criu check --all passed)"),
                Ok((false, out)) => {
                    println!("  c/r ready  : PARTIAL — criu check --all reported:");
                    for line in out.lines().filter(|l| l.contains("Error") || l.contains("Warn")) {
                        println!("               {line}");
                    }
                }
                Err(e) => println!("  c/r ready  : unknown ({e})"),
            }
        }
        Err(e) => {
            println!("MISSING");
            println!("  c/r ready  : no — {e}");
        }
    }
}

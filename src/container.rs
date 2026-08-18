use crate::capabilities;
use crate::cgroups::Cgroup;
use crate::config::ContainerConfig;
use crate::error::{Result, RuntimeError};
use crate::network::Network;
use crate::seccomp;

use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{chdir, execvpe, fork, pivot_root, sethostname, ForkResult, Pid};
use serde::Serialize;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Measurements captured for one container run.
#[derive(Debug, Clone, Serialize)]
pub struct Metrics {
    pub id: String,
    /// Runtime setup overhead: time until the container is cleared to exec.
    pub setup_ms: f64,
    /// Wall-clock time from launch to container exit.
    pub wall_ms: f64,
    /// Peak memory attributed to the container cgroup, in bytes.
    pub peak_mem_bytes: u64,
    /// CPU time consumed by the container, in microseconds.
    pub cpu_usec: u64,
    pub exit_code: i32,
    pub container_ip: Option<String>,
}

/// Where a detached container's stdio is redirected.
///
/// The directory is bind-mounted to [`IO_MOUNT`] *inside* the container's root
/// before `pivot_root`, and the container opens its log files through that
/// path. This is not cosmetic: a checkpoint engine records an open regular file
/// by its path relative to the mount namespace root, so a log file living only
/// on the host — outside the container's root — is not a dumpable fd. Binding
/// it in gives fd 1 and 2 a path that exists in both the original and the
/// restored mount namespace.
#[derive(Debug, Clone)]
pub struct Io {
    /// Host directory holding `stdout` and `stderr`.
    pub dir: PathBuf,
}

/// Mount point of [`Io::dir`] inside the container.
pub const IO_MOUNT: &str = ".mcio";

/// A container that has been started but not yet waited on.
///
/// Splitting spawn from wait is what makes checkpointing possible at all: the
/// original `run()` owned the container for its whole lifetime, so there was
/// never a moment where a caller held a live container and could do something
/// else with it.
pub struct Handle {
    pub id: String,
    /// Host-visible pid of the container's PID 1. This is the pid a checkpoint
    /// engine dumps, and the reason the parent must stay in the host PID
    /// namespace to learn it.
    pub container_pid: Pid,
    /// The middle process, which waits on the container and exits with its code.
    pub supervisor_pid: Pid,
    pub cgroup: Cgroup,
    pub network: Option<Network>,
    pub setup_ms: f64,
    t0: Instant,
}

impl Handle {
    pub fn container_ip(&self) -> Option<&str> {
        self.network.as_ref().map(|n| n.container_ip())
    }

    /// Block until the container exits, then tear down and report metrics.
    pub fn wait(self) -> Result<Metrics> {
        let status = waitpid(self.supervisor_pid, None)
            .map_err(|e| RuntimeError::Syscall("waitpid", e))?;
        let wall_ms = self.t0.elapsed().as_secs_f64() * 1000.0;

        let exit_code = match status {
            WaitStatus::Exited(_, code) => code,
            WaitStatus::Signaled(_, sig, _) => 128 + sig as i32,
            _ => -1,
        };

        Ok(self.finish(exit_code, wall_ms))
    }

    /// Tear down cgroup and networking and produce metrics, without waiting.
    /// Used when the container's processes are already gone — after a
    /// checkpoint, the dump engine has killed the tree and there is nothing
    /// left to reap.
    pub fn finish(self, exit_code: i32, wall_ms: f64) -> Metrics {
        let peak_mem_bytes = self.cgroup.peak_memory().unwrap_or(0);
        let cpu_usec = self.cgroup.cpu_usage_usec().unwrap_or(0);
        let container_ip = self.network.as_ref().map(|n| n.container_ip().to_string());

        if let Some(n) = self.network {
            n.cleanup();
        }
        self.cgroup.cleanup();

        Metrics {
            id: self.id,
            setup_ms: self.setup_ms,
            wall_ms,
            peak_mem_bytes,
            cpu_usec,
            exit_code,
            container_ip,
        }
    }

    pub fn elapsed_ms(&self) -> f64 {
        self.t0.elapsed().as_secs_f64() * 1000.0
    }
}

/// A raw pipe used to synchronise parent and child around fork.
struct Sync {
    read: i32,
    write: i32,
}

fn make_pipe() -> Result<Sync> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(RuntimeError::Io(std::io::Error::last_os_error()));
    }
    Ok(Sync { read: fds[0], write: fds[1] })
}

fn notify(fd: i32) {
    let b = [1u8];
    unsafe { libc::write(fd, b.as_ptr() as *const _, 1) };
}

fn wait_for(fd: i32) {
    let mut b = [0u8];
    unsafe { libc::read(fd, b.as_mut_ptr() as *mut _, 1) };
}

fn write_pid(fd: i32, pid: i32) {
    let b = pid.to_ne_bytes();
    unsafe { libc::write(fd, b.as_ptr() as *const _, 4) };
}

fn read_pid(fd: i32) -> i32 {
    let mut b = [0u8; 4];
    unsafe { libc::read(fd, b.as_mut_ptr() as *mut _, 4) };
    i32::from_ne_bytes(b)
}

fn close(fd: i32) {
    unsafe { libc::close(fd) };
}

/// Run a container to completion, returning its metrics.
///
/// `index` gives each concurrently-run container a distinct IP.
///
/// Process structure (two-level fork):
///
/// ```text
///   parent (host PID ns)  ── forks ──▶  middle  ── forks ──▶  grandchild = container PID 1
///     • cgroup + network                  • unshares all         • pivot_root, drop caps,
///       (helpers must see                   namespaces             seccomp, exec
///       host PIDs, so it must              • relays grandchild's
///       NOT be in the new PID ns)           host PID up, waits
/// ```
///
/// The parent must stay in the host PID namespace: `ip`/`nsenter` resolve the
/// target PID in *their own* PID namespace, so a networking helper forked from
/// inside the container's PID namespace could never see the container's host
/// PID. Only the grandchild (created after `unshare(CLONE_NEWPID)`) actually
/// enters the new PID namespace as PID 1.
pub fn run(cfg: &ContainerConfig, index: u8) -> Result<Metrics> {
    spawn(cfg, index, None)?.wait()
}

/// Start a container and return a [`Handle`] without waiting for it.
///
/// `io`, when given, redirects the container's stdio into files (see [`Io`]);
/// with `None` the container inherits the caller's stdio, which is what the
/// foreground `run` path wants.
pub fn spawn(cfg: &ContainerConfig, index: u8, io: Option<&Io>) -> Result<Handle> {
    let t0 = Instant::now();

    let cgroup = Cgroup::create(&cfg.id)?;
    cgroup.apply(&cfg.resources)?;

    let go = make_pipe()?; // parent -> grandchild: "you may exec"
    let pidp = make_pipe()?; // middle -> parent: grandchild's host PID

    match unsafe { fork() }.map_err(|e| RuntimeError::Syscall("fork", e))? {
        ForkResult::Child => {
            close(go.write);
            close(pidp.read);
            // Never returns; execs (grandchild) or _exit (middle).
            middle(cfg, io, go.read, pidp.write)
        }

        ForkResult::Parent { child: middle_pid } => {
            close(go.read);
            close(pidp.write);

            // Learn the container's host PID from the middle process.
            let gc_pid = Pid::from_raw(read_pid(pidp.read));
            close(pidp.read);

            // Only the container (grandchild) goes in the resource cgroup.
            cgroup.add_process(gc_pid)?;

            let network = if cfg.network {
                Some(Network::setup(cfg.short_id(), gc_pid, index)?)
            } else {
                None
            };

            let setup_ms = t0.elapsed().as_secs_f64() * 1000.0;

            // Release the container to exec.
            notify(go.write);
            close(go.write);

            Ok(Handle {
                id: cfg.id.clone(),
                container_pid: gc_pid,
                supervisor_pid: middle_pid,
                cgroup,
                network,
                setup_ms,
                t0,
            })
        }
    }
}

/// The middle process: creates the namespaces and forks the real container.
/// Stays in the host PID namespace itself (only its children enter the new PID
/// namespace). Never returns.
fn middle(cfg: &ContainerConfig, io: Option<&Io>, go_r: i32, pidp_w: i32) -> ! {
    // NEWNS/UTS/IPC/NET take effect on this process immediately; NEWPID takes
    // effect on the next fork, making the grandchild PID 1.
    if let Err(e) = unshare(
        CloneFlags::CLONE_NEWPID
            | CloneFlags::CLONE_NEWNS
            | CloneFlags::CLONE_NEWUTS
            | CloneFlags::CLONE_NEWIPC
            | CloneFlags::CLONE_NEWNET,
    ) {
        eprintln!("mincontainer: unshare failed: {e}");
        unsafe { libc::_exit(127) };
    }

    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            close(pidp_w);
            if let Err(e) = grandchild(cfg, io, go_r) {
                eprintln!("mincontainer: container setup failed: {e}");
                unsafe { libc::_exit(127) };
            }
            unreachable!("grandchild execs");
        }
        Ok(ForkResult::Parent { child: gc }) => {
            // gc is the grandchild's PID in *our* (still host) PID namespace,
            // i.e. its host-visible PID — exactly what the parent needs.
            write_pid(pidp_w, gc.as_raw());
            let code = match waitpid(gc, None) {
                Ok(WaitStatus::Exited(_, c)) => c,
                Ok(WaitStatus::Signaled(_, sig, _)) => 128 + sig as i32,
                _ => 1,
            };
            unsafe { libc::_exit(code) };
        }
        Err(e) => {
            eprintln!("mincontainer: fork failed: {e}");
            unsafe { libc::_exit(127) };
        }
    }
}

/// The container process (PID 1 in its namespace). Sets up its root filesystem,
/// applies hardening, and execs the command. Returns only on error.
fn grandchild(cfg: &ContainerConfig, io: Option<&Io>, go_r: i32) -> Result<()> {
    // Wait until the parent has attached us to the cgroup and wired networking,
    // then close the pipe. Leaving it open would hand the container an fd it
    // never asked for — visible in `inspect` as a stray `pipe:[...]`, and one
    // more thing a checkpoint has to serialise and a restore reproduce.
    wait_for(go_r);
    close(go_r);

    // A detached container must lead its own session.
    //
    // We are PID 1 of a new PID namespace, but `setsid` was never called, so
    // our session id is still the launching shell's — a session leader that
    // lives in the *host* PID namespace and is therefore not part of the
    // process tree a checkpoint would dump. CRIU refuses that outright:
    //
    //     Error (criu/cr-dump.c:1618): A session leader of 11(1) is outside
    //     of its pid namespace
    //
    // Calling `setsid` makes this process its own session and process-group
    // leader, so the whole session is contained within the namespace and the
    // tree is self-contained.
    //
    // This is done only for detached containers. `setsid` also drops the
    // controlling terminal, and a foreground `mincontainer run -- /bin/sh`
    // needs to keep it or the shell is unusable and Ctrl-C stops working. That
    // is the tradeoff behind requiring `start --detach` before `checkpoint`.
    if io.is_some() {
        nix::unistd::setsid().map_err(|e| RuntimeError::Syscall("setsid", e))?;
    }

    sethostname(&cfg.hostname).map_err(|e| RuntimeError::Syscall("sethostname", e))?;

    // Mount volumes *before* pivot_root, so paths are still in the host namespace.
    // Volumes are bound into the rootfs directory.
    let rootfs_path = Path::new(&cfg.rootfs);
    for vol in &cfg.volumes {
        mount_volume_into_rootfs(rootfs_path, &vol.host_path, &vol.container_path)?;
    }

    // Bind the io directory in *before* pivot_root, for the same reason as
    // volumes: after the pivot its host path is gone.
    if let Some(io) = io {
        mount_volume_into_rootfs(rootfs_path, &io.dir.to_string_lossy(), IO_MOUNT)?;
    }

    setup_rootfs(rootfs_path)?;

    chdir("/").map_err(|e| RuntimeError::Syscall("chdir(/)", e))?;

    // Redirect stdio only now: the target paths exist solely inside the new
    // root, and /dev had to be mounted first for /dev/null to resolve.
    if io.is_some() {
        redirect_stdio()?;
    }

    // Hardening — order matters: drop caps and install seccomp last, after all
    // privileged mount work is done (mount/pivot_root are on the seccomp
    // deny-list themselves).
    if cfg.drop_caps {
        capabilities::drop_dangerous()?;
    }
    if cfg.seccomp {
        seccomp::apply()?;
    }

    // Replace this process with the container command.
    let prog = CString::new(cfg.command[0].as_str())
        .map_err(|_| RuntimeError::Config("command contains NUL".into()))?;
    let argv: Vec<CString> = cfg
        .command
        .iter()
        .map(|s| CString::new(s.as_str()).unwrap())
        .collect();
    let envp: Vec<CString> = cfg
        .env
        .iter()
        .map(|s| CString::new(s.as_str()).unwrap())
        .collect();

    execvpe(&prog, &argv, &envp).map_err(|e| RuntimeError::Syscall("execvpe", e))?;
    unreachable!("execvpe returned without error");
}

/// Point fd 0 at /dev/null and fds 1 and 2 at the bound-in log files.
///
/// Uses raw `open`/`dup2` rather than `std::fs::File` so the descriptors are
/// exactly 0, 1 and 2 with no stray extras left open — every additional open fd
/// is one more thing the checkpoint engine has to describe and the restore has
/// to reproduce.
fn redirect_stdio() -> Result<()> {
    use nix::fcntl::{open, OFlag};
    use nix::sys::stat::Mode;
    use nix::unistd::{close as nix_close, dup2};

    let devnull = open("/dev/null", OFlag::O_RDWR, Mode::empty())
        .map_err(|e| RuntimeError::Syscall("open(/dev/null)", e))?;
    dup2(devnull, 0).map_err(|e| RuntimeError::Syscall("dup2(stdin)", e))?;
    if devnull > 2 {
        let _ = nix_close(devnull);
    }

    for (fd, name) in [(1, "stdout"), (2, "stderr")] {
        let path = format!("/{IO_MOUNT}/{name}");
        let f = open(
            path.as_str(),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_APPEND,
            Mode::from_bits_truncate(0o644),
        )
        .map_err(|e| RuntimeError::Syscall("open(container log)", e))?;
        dup2(f, fd).map_err(|e| RuntimeError::Syscall("dup2(container log)", e))?;
        if f > 2 {
            let _ = nix_close(f);
        }
    }
    Ok(())
}

/// pivot_root into `rootfs` and mount a fresh /proc and /dev.
fn setup_rootfs(rootfs: &Path) -> Result<()> {
    // Make the whole mount tree private so nothing propagates back to the host.
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .map_err(|e| RuntimeError::Syscall("mount(/ private)", e))?;

    // pivot_root requires the new root to be a mount point: bind it onto itself.
    mount(
        Some(rootfs),
        rootfs,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| RuntimeError::Syscall("mount(bind rootfs)", e))?;

    chdir(rootfs).map_err(|e| RuntimeError::Syscall("chdir(rootfs)", e))?;

    // put_old lives inside the new root; "." trick keeps paths relative.
    let put_old = rootfs.join(".oldroot");
    std::fs::create_dir_all(&put_old)
        .map_err(|e| RuntimeError::Filesystem(format!("mkdir .oldroot: {e}")))?;

    pivot_root(".", ".oldroot").map_err(|e| RuntimeError::Syscall("pivot_root", e))?;
    chdir("/").map_err(|e| RuntimeError::Syscall("chdir(/) post-pivot", e))?;

    // Detach and remove the old root.
    umount2("/.oldroot", MntFlags::MNT_DETACH).map_err(|e| RuntimeError::Syscall("umount(.oldroot)", e))?;
    let _ = std::fs::remove_dir("/.oldroot");

    // Fresh /proc reflecting the new PID namespace.
    let _ = std::fs::create_dir_all("/proc");
    mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    )
    .map_err(|e| RuntimeError::Syscall("mount(/proc)", e))?;

    // /dev: prefer devtmpfs, fall back to a tmpfs with the essential nodes.
    let _ = std::fs::create_dir_all("/dev");
    if mount(Some("devtmpfs"), "/dev", Some("devtmpfs"), MsFlags::empty(), None::<&str>).is_err() {
        mount(Some("tmpfs"), "/dev", Some("tmpfs"), MsFlags::empty(), None::<&str>)
            .map_err(|e| RuntimeError::Syscall("mount(/dev tmpfs)", e))?;
        make_dev_nodes()?;
    }

    Ok(())
}

/// Create the minimal set of device nodes a shell needs.
fn make_dev_nodes() -> Result<()> {
    use nix::sys::stat::{mknod, Mode, SFlag};
    let nodes: &[(&str, u64, u64)] = &[
        ("/dev/null", 1, 3),
        ("/dev/zero", 1, 5),
        ("/dev/full", 1, 7),
        ("/dev/random", 1, 8),
        ("/dev/urandom", 1, 9),
        ("/dev/tty", 5, 0),
    ];
    for (path, major, minor) in nodes {
        let dev = nix::sys::stat::makedev(*major, *minor);
        let _ = mknod(*path, SFlag::S_IFCHR, Mode::from_bits_truncate(0o666), dev);
    }
    Ok(())
}

/// Mount a volume (bind mount) from host into the rootfs, before pivot_root.
/// container_path is relative to the container's root (e.g., "/mnt"),
/// and we bind it at rootfs/container_path.
fn mount_volume_into_rootfs(rootfs: &Path, host_path: &str, container_path: &str) -> Result<()> {
    // The target is container_path relative to the rootfs.
    // Strip leading '/' from container_path if present.
    let rel_path = container_path.trim_start_matches('/');
    let target = rootfs.join(rel_path);

    // Create the target directory if it doesn't exist.
    std::fs::create_dir_all(&target)
        .map_err(|e| RuntimeError::Filesystem(format!("mkdir {}: {e}", target.display())))?;

    // Perform the bind mount. Both source and target must exist.
    mount(
        Some(host_path),
        target.as_os_str(),
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| RuntimeError::Syscall("mount(volume bind)", e))
}

/// Best-effort reap of a leaked child (used by the CLI on error paths).
pub fn reap(pid: Pid) {
    let _ = waitpid(pid, None);
}

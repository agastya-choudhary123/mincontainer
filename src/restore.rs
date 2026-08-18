//! Rebuilding a container from a snapshot.
//!
//! Restore is the half that has to be paranoid. A checkpoint that fails leaves
//! a container running and a snapshot unwritten — annoying, but nothing is
//! half-built. A restore that fails partway has already created a cgroup, moved
//! a veth, and possibly spawned processes, and the host is left holding pieces
//! of a container that does not exist.
//!
//! Two properties make that recoverable:
//!
//! 1. **The snapshot is never consumed.** Nothing in this module writes to,
//!    moves, or deletes the snapshot file. A restore that fails for any reason
//!    — including the process being killed outright — can simply be run again.
//!    Everything is unpacked into a scratch directory that is safe to delete.
//!
//! 2. **Every host resource is registered before it is created.** [`Rollback`]
//!    holds the undo list. On the error path it runs; on success it is
//!    disarmed. A `SIGKILL` outruns it, which is what [`reconcile`] is for.
//!
//! The ordering rule throughout: *verify everything, then build*. The whole
//! snapshot is checksummed and unpacked before a single host resource is
//! touched, so the overwhelmingly common failure (a bad snapshot) never gets
//! far enough to need rolling back.

use crate::cgroups::Cgroup;
use crate::checkpoint::{self, pid_alive};
use crate::config::ContainerConfig;
use crate::criu;
use crate::error::{Result, RuntimeError};
use crate::network::Network;
use crate::snapshot::{Manifest, SnapshotReader};
use crate::state::{self, ContainerStateDir, Status};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Knobs for one restore.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Snapshot to restore from. Defaults to the container's recorded snapshot,
    /// then to `snapshots/latest.mcsnap`.
    pub from: Option<PathBuf>,
    /// Give each restored container a distinct IP.
    pub index: u8,
    /// Serialise/deserialise established TCP connections. Must match how the
    /// snapshot was taken.
    pub allow_tcp: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub id: String,
    pub pid: i32,
    pub generation: u32,
    pub snapshot: PathBuf,
    pub snapshot_bytes: u64,
    /// Whole-file checksum verification.
    pub verify_ms: f64,
    /// Unpacking entries, each checksummed as it lands.
    pub unpack_ms: f64,
    /// Rebuilding namespaces and processes. This is the window that matters
    /// for how long a migration is visible as downtime.
    pub restore_ms: f64,
    pub total_ms: f64,
    pub container_ip: Option<String>,
}

/// Restore `id` from a snapshot and resume it on this host.
pub fn restore(id: &str, opts: &Options) -> Result<Report> {
    let t0 = Instant::now();

    // Clean up after any previous restore that was killed mid-flight, so a
    // retry starts from a known state rather than tripping over its own debris.
    reconcile(id)?;

    let dir = ContainerStateDir::for_id(id);
    if !dir.exists() {
        return Err(RuntimeError::Restore(format!("container {id} not found")));
    }
    let st = dir.load_state()?;

    if st.is(Status::Running) {
        if let Some(pid) = st.pid {
            if pid_alive(pid) {
                return Err(RuntimeError::Restore(format!(
                    "container {id} is already running as pid {pid} — \
                     restoring would give you two copies of the same container"
                )));
            }
        }
    }

    let snapshot_path = resolve_snapshot(&dir, &st, opts)?;

    // --- verify, entirely before touching the host --------------------------
    let t_verify = Instant::now();
    let mut reader = SnapshotReader::open(&snapshot_path)?;
    reader.verify()?;
    let manifest = reader.manifest().clone();
    check_compatible(&manifest)?;
    let verify_ms = t_verify.elapsed().as_secs_f64() * 1000.0;

    let t_unpack = Instant::now();
    let staging = dir.staging_dir();
    let _ = std::fs::remove_dir_all(&staging);
    reader.extract_all(&staging)?;
    let images_dir = staging.join("images");
    if !images_dir.is_dir() {
        return Err(RuntimeError::Restore(
            "snapshot contains no images/ entries — nothing to restore".into(),
        ));
    }
    let unpack_ms = t_unpack.elapsed().as_secs_f64() * 1000.0;

    let cfg = manifest.config.clone();
    if !Path::new(&cfg.rootfs).is_dir() {
        return Err(RuntimeError::Restore(format!(
            "rootfs {} does not exist on this host — a snapshot carries the container's \
             memory, not its filesystem; stage the rootfs first",
            cfg.rootfs
        )));
    }

    // --- build ---------------------------------------------------------------
    let t_restore = Instant::now();
    state::set_status(id, Status::Restoring)?;
    let mut rb = Rollback::new(id);

    // The container's own logs come back with it, so a migrated container's
    // output is continuous rather than restarting empty on the new node.
    restore_io_files(&mut reader, &staging, &dir)?;

    let cgroup = Cgroup::create(&cfg.id)?;
    cgroup.apply(&cfg.resources)?;
    rb.cgroup = Some(cfg.id.clone());

    let layout = checkpoint::layout_for(&cfg, &dir);
    let pidfile = staging.join("restored.pid");
    let restore_opts = criu::RestoreOptions {
        images_dir: images_dir.clone(),
        layout,
        pidfile,
        tcp_established: opts.allow_tcp,
    };

    let pid = match criu::restore(&restore_opts) {
        Ok(p) => p,
        Err(e) => {
            rb.run();
            let _ = state::set_status(id, Status::Checkpointed);
            return Err(RuntimeError::Restore(format!(
                "{e}\n\nThe snapshot is untouched; fix the cause and retry."
            )));
        }
    };
    rb.pid = Some(pid);

    // From here every failure must kill the tree we just brought back to life.
    let built = (|| -> Result<Option<String>> {
        if !pid_alive(pid) {
            return Err(RuntimeError::Restore(format!(
                "restored pid {pid} died immediately after restore"
            )));
        }

        cgroup.add_process(Pid::from_raw(pid))?;

        // A restore that somehow landed back in the *original* namespaces would
        // mean we are sharing state with a container that no longer exists.
        assert_fresh_namespaces(pid, &manifest)?;

        let ip = if cfg.network {
            let net = Network::setup(cfg.short_id(), Pid::from_raw(pid), opts.index)?;
            let ip = net.container_ip().to_string();
            rb.network = Some(net);
            Some(ip)
        } else {
            None
        };
        Ok(ip)
    })();

    let container_ip = match built {
        Ok(ip) => ip,
        Err(e) => {
            rb.run();
            let _ = state::set_status(id, Status::Checkpointed);
            return Err(e);
        }
    };

    // --- commit --------------------------------------------------------------
    state::set_restored(id, Pid::from_raw(pid))?;
    if manifest.source_host != checkpoint::hostname() {
        state::update(id, |s| s.origin_host = Some(manifest.source_host.clone()))?;
    }
    rb.disarm();

    let restore_ms = t_restore.elapsed().as_secs_f64() * 1000.0;
    let _ = std::fs::remove_dir_all(&staging);

    let generation = dir.load_state().map(|s| s.generation).unwrap_or(0);
    let snapshot_bytes = std::fs::metadata(&snapshot_path).map(|m| m.len()).unwrap_or(0);
    Ok(Report {
        id: id.to_string(),
        pid,
        generation,
        snapshot: snapshot_path,
        snapshot_bytes,
        verify_ms,
        unpack_ms,
        restore_ms,
        total_ms: t0.elapsed().as_secs_f64() * 1000.0,
        container_ip,
    })
}

/// Undo list for a restore in progress.
///
/// Deliberately explicit rather than a `Drop` impl: rollback kills processes
/// and tears down networking, and doing that implicitly from a destructor makes
/// it far too easy to trigger on a path that did not intend it (an early
/// `return Ok`, a moved value). `run()` is called at each failure site;
/// `disarm()` marks success. A leaked `Rollback` warns in debug builds.
struct Rollback {
    id: String,
    pid: Option<i32>,
    cgroup: Option<String>,
    network: Option<Network>,
    armed: bool,
}

impl Rollback {
    fn new(id: &str) -> Self {
        Rollback { id: id.to_string(), pid: None, cgroup: None, network: None, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Tear down everything this restore created, in reverse order.
    fn run(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;

        if let Some(net) = self.network.take() {
            net.cleanup();
        }

        if let Some(pid) = self.pid.take() {
            // The restored tree is PID 1 of a fresh pid namespace: killing it
            // takes the whole namespace with it, so there is nothing to walk.
            let p = Pid::from_raw(pid);
            let _ = kill(p, Signal::SIGKILL);
            // Reap if it is our child; if it was re-parented to init this is a
            // no-op and the namespace teardown still happens.
            let _ = nix::sys::wait::waitpid(p, Some(nix::sys::wait::WaitPidFlag::WNOHANG));
            wait_for_exit(pid, std::time::Duration::from_secs(2));
        }

        if let Some(cg) = self.cgroup.take() {
            // The cgroup can only be removed once it is empty, which is why the
            // kill above waits.
            Cgroup::open(&cg).cleanup();
        }

        let _ = std::fs::remove_dir_all(ContainerStateDir::for_id(&self.id).staging_dir());
    }
}

impl Drop for Rollback {
    fn drop(&mut self) {
        debug_assert!(
            !self.armed,
            "restore rollback for {} was neither run nor disarmed — a failure path is missing a \
             call to run()",
            self.id
        );
    }
}

fn wait_for_exit(pid: i32, timeout: std::time::Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Clean up after a restore or checkpoint that was killed mid-flight.
///
/// A `SIGKILL` beats the rollback guard, so the state file can be left saying
/// `restoring` with host resources dangling. Recovery leans on the snapshot
/// being immutable: there is nothing to repair, only debris to sweep, after
/// which the container is back to `checkpointed` and can simply be restored
/// again.
///
/// Called at the start of every restore and by `ps`, so the debris is cleaned
/// up by the next command the operator runs rather than needing a repair tool.
pub fn reconcile(id: &str) -> Result<()> {
    let dir = ContainerStateDir::for_id(id);
    if !dir.exists() {
        return Ok(());
    }
    let st = dir.load_state()?;

    let stale = match st.status.as_str() {
        // Interrupted mid-restore.
        "restoring" => true,
        // Interrupted mid-checkpoint: the dump either finished (processes gone)
        // or did not (processes still there); either way the label is wrong.
        "checkpointing" => true,
        // Marked running but the process is gone — the supervisor died without
        // recording the exit.
        "running" => st.pid.map(|p| !pid_alive(p)).unwrap_or(true),
        _ => false,
    };
    if !stale {
        return Ok(());
    }

    // Kill anything still alive under the recorded pid before removing the
    // cgroup, or the removal fails and leaves the leaf behind forever.
    if let Some(pid) = st.pid {
        if pid_alive(pid) {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            wait_for_exit(pid, std::time::Duration::from_secs(2));
        }
    }

    if let Ok(cfg) = dir.load_config() {
        Cgroup::open(&cfg.id).cleanup();
        if cfg.network {
            Network::cleanup_by_id(cfg.short_id());
        }
    }
    let _ = std::fs::remove_dir_all(dir.staging_dir());

    // A snapshot on disk means the container is recoverable; without one, the
    // interruption lost it.
    let has_snapshot = st
        .snapshot
        .as_ref()
        .map(|p| Path::new(p).is_file())
        .unwrap_or(false)
        || dir.snapshot_file("latest").is_file();

    state::update(id, |s| {
        s.pid = None;
        s.status = if has_snapshot {
            Status::Checkpointed.as_str().to_string()
        } else {
            Status::Failed.as_str().to_string()
        };
    })?;

    Ok(())
}

/// Run [`reconcile`] over every container, ignoring individual failures.
pub fn reconcile_all() {
    if let Ok(all) = state::list_all() {
        for c in all {
            let _ = reconcile(&c.id);
        }
    }
}

fn resolve_snapshot(
    dir: &ContainerStateDir,
    st: &crate::state::ContainerState,
    opts: &Options,
) -> Result<PathBuf> {
    if let Some(p) = &opts.from {
        if !p.is_file() {
            return Err(RuntimeError::Restore(format!(
                "snapshot {} does not exist",
                p.display()
            )));
        }
        return Ok(p.clone());
    }
    if let Some(p) = &st.snapshot {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }
    let latest = dir.snapshot_file("latest");
    if latest.is_file() {
        return Ok(latest);
    }
    Err(RuntimeError::Restore(format!(
        "no snapshot found for this container; checkpoint it first or pass --from <file>. \
         Looked in {}",
        dir.snapshots_dir().display()
    )))
}

/// Refuse a snapshot this host cannot honestly restore.
///
/// Architecture is the hard one: a snapshot is a register dump and a set of
/// page images, so an aarch64 checkpoint on an x86_64 host is not a
/// compatibility problem to work around, it is meaningless. Better to say so
/// than to hand the kernel garbage.
fn check_compatible(m: &Manifest) -> Result<()> {
    if m.source_arch != std::env::consts::ARCH {
        return Err(RuntimeError::Restore(format!(
            "snapshot was taken on {} but this host is {} — process images are \
             architecture-specific and cannot be translated",
            m.source_arch,
            std::env::consts::ARCH
        )));
    }

    let local = criu::version()?;
    if local != m.engine_version {
        // Not fatal: CRIU image formats are stable across patch releases, and
        // refusing here would make a fleet impossible to upgrade incrementally.
        // Worth saying out loud, though, because it is the first thing to
        // suspect if the restore then fails.
        eprintln!(
            "[mincontainer] note: snapshot made with {} {}, restoring with {local}",
            m.engine, m.engine_version
        );
    }
    Ok(())
}

/// Namespaces must be new. Matching inode numbers would mean the restored
/// processes rejoined the namespaces of the container we checkpointed, which on
/// a same-host restore is a real possibility if the original never died.
fn assert_fresh_namespaces(pid: i32, m: &Manifest) -> Result<()> {
    let read = |kind: &str| -> Option<u64> {
        let link = std::fs::read_link(format!("/proc/{pid}/ns/{kind}")).ok()?;
        let s = link.to_string_lossy();
        let inner = s.split_once('[')?.1.strip_suffix(']')?;
        inner.parse().ok()
    };

    for (kind, old) in [
        ("pid", m.namespaces.pid),
        ("mnt", m.namespaces.mnt),
        ("uts", m.namespaces.uts),
        ("ipc", m.namespaces.ipc),
    ] {
        let (Some(old), Some(new)) = (old, read(kind)) else {
            continue;
        };
        if old == new {
            return Err(RuntimeError::Restore(format!(
                "restored process is in the *original* {kind} namespace (inode {old}) — \
                 the checkpointed container is apparently still alive, and continuing would \
                 give two containers one namespace"
            )));
        }
    }
    Ok(())
}

/// Put the container's stdout/stderr back where its restored fds expect them.
///
/// The restored process holds open descriptors onto `<io>/stdout` and
/// `<io>/stderr` by path. On a migration those files do not exist on the new
/// node yet, and the engine would refuse the restore; on a same-host restore
/// they exist but may have been truncated. Either way the snapshot's copy is
/// the authoritative one.
fn restore_io_files(
    _reader: &mut SnapshotReader,
    staging: &Path,
    dir: &ContainerStateDir,
) -> Result<()> {
    std::fs::create_dir_all(dir.io_dir())
        .map_err(|e| RuntimeError::Restore(format!("mkdir io dir: {e}")))?;

    for name in ["stdout", "stderr"] {
        let src = staging.join("io").join(name);
        let dst = dir.io_dir().join(name);
        if src.is_file() {
            std::fs::copy(&src, &dst).map_err(|e| {
                RuntimeError::Restore(format!("restore {}: {e}", dst.display()))
            })?;
        } else if !dst.exists() {
            // The engine reopens these by path; they must exist even if empty.
            std::fs::File::create(&dst).map_err(|e| {
                RuntimeError::Restore(format!("create {}: {e}", dst.display()))
            })?;
        }
    }
    Ok(())
}

/// Inspect a snapshot without restoring it. The format is self-describing, so
/// this needs nothing but the file.
pub fn inspect(path: &Path, verify: bool) -> Result<Manifest> {
    let mut reader = SnapshotReader::open(path)?;
    if verify {
        reader.verify()?;
    }
    Ok(reader.manifest().clone())
}

/// Adopt a snapshot that arrived from another node, creating local state for a
/// container this host has never seen.
pub fn adopt_snapshot(path: &Path) -> Result<(String, ContainerConfig)> {
    let mut reader = SnapshotReader::open(path)?;
    reader.verify()?;
    let m = reader.manifest();

    let mut lifecycle = m.lifecycle.clone();
    lifecycle.status = Status::Checkpointed.as_str().to_string();
    lifecycle.pid = None;
    lifecycle.snapshot = Some(path.display().to_string());
    lifecycle.origin_host = Some(m.source_host.clone());

    state::adopt(&lifecycle, &m.config)?;
    Ok((m.container_id.clone(), m.config.clone()))
}

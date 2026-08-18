//! Freezing a running container into a snapshot.
//!
//! The sequence is deliberately ordered so that everything which can fail
//! cheaply fails *before* the container is frozen:
//!
//! ```text
//!   1. resolve + validate      is this container actually running, and ours?
//!   2. pre-flight              is its state checkpointable at all?
//!   3. collect metadata        namespaces, mounts, fds, cgroup — from procfs
//!   4. mark Checkpointing      so a concurrent reaper reads the right story
//!   5. dump                    engine freezes and serialises the tree
//!   6. pack                    engine images + logs + metadata -> one file
//!   7. mark Checkpointed       pid cleared; snapshot path recorded
//! ```
//!
//! Steps 1–3 are pure reads. By the time the container stops running, the only
//! remaining failure modes are disk-full and engine bugs, and both leave the
//! snapshot marked incomplete rather than the container marked healthy.

use crate::config::ContainerConfig;
use crate::criu;
use crate::error::{Result, RuntimeError};
use crate::snapshot::{
    CgroupSnapshot, EntryMeta, FdEntry, Manifest, MountEntry, NamespaceSnapshot, SnapshotWriter,
    FORMAT_VERSION,
};
use crate::state::{self, now_iso, ContainerState, ContainerStateDir, Status};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Knobs for one checkpoint.
#[derive(Debug, Clone)]
pub struct Options {
    /// Snapshot name; the file lands at `<state>/snapshots/<name>.mcsnap`.
    pub name: String,
    /// Explicit destination, overriding the default location.
    pub dest: Option<PathBuf>,
    /// Leave the container running after dumping.
    pub leave_running: bool,
    /// Serialise established TCP connections instead of refusing them.
    pub allow_tcp: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            name: "latest".to_string(),
            dest: None,
            leave_running: false,
            allow_tcp: false,
        }
    }
}

/// What a checkpoint produced, with the timings broken out so the cost can be
/// attributed rather than quoted as one opaque number.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub id: String,
    pub snapshot: PathBuf,
    /// Size of the finished `.mcsnap` file.
    pub snapshot_bytes: u64,
    /// Raw engine image bytes before framing.
    pub image_bytes: u64,
    pub entries: usize,
    /// Pre-flight and metadata collection, container still running.
    pub preflight_ms: f64,
    /// Freeze + serialise. This is the window the container is unavailable.
    pub dump_ms: f64,
    /// Packing engine output into the snapshot file.
    pub pack_ms: f64,
    pub total_ms: f64,
}

/// Freeze `id` and write a snapshot.
pub fn checkpoint(id: &str, opts: &Options) -> Result<Report> {
    let t0 = Instant::now();

    let engine_version = criu::version()?;

    // --- 1. resolve and validate -------------------------------------------
    let dir = ContainerStateDir::for_id(id);
    if !dir.exists() {
        return Err(RuntimeError::Checkpoint(format!("container {id} not found")));
    }
    let st = dir.load_state()?;
    let cfg = dir.load_config()?;

    if !st.is(Status::Running) {
        return Err(RuntimeError::Checkpoint(format!(
            "container {id} is {}, not running — only a running container can be checkpointed",
            st.status
        )));
    }
    let pid = st.pid.ok_or_else(|| {
        RuntimeError::Checkpoint(format!("container {id} is marked running but has no pid"))
    })?;
    if !pid_alive(pid) {
        return Err(RuntimeError::Checkpoint(format!(
            "container {id} is marked running but pid {pid} is gone — state is stale, \
             run `mincontainer ps` after the supervisor reaps it"
        )));
    }

    // --- 2. pre-flight ------------------------------------------------------
    preflight(pid, &cfg, opts)?;

    // --- 3. metadata --------------------------------------------------------
    let namespaces = read_namespaces(pid);
    let mounts = read_mounts(pid)?;
    let open_fds = read_fds(pid)?;
    let cgroup = read_cgroup(&cfg.id);
    let preflight_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // --- 4. mark, then dump -------------------------------------------------
    state::set_status(id, Status::Checkpointing)?;

    let images_dir = dir.staging_dir().join("dump-images");
    // A previous attempt's images must not be mistaken for this one's.
    let _ = std::fs::remove_dir_all(&images_dir);
    std::fs::create_dir_all(&images_dir)
        .map_err(|e| RuntimeError::Checkpoint(format!("mkdir images: {e}")))?;

    let layout = layout_for(&cfg, &dir);
    let dump_opts = criu::DumpOptions {
        pid,
        images_dir: images_dir.clone(),
        layout,
        leave_running: opts.leave_running,
        tcp_established: opts.allow_tcp,
    };

    let t_dump = Instant::now();
    if let Err(e) = criu::dump(&dump_opts) {
        // The dump failed; the container may or may not still be running.
        // Report which, so the operator is not left guessing.
        let still = pid_alive(pid);
        let _ = state::set_status(id, if still { Status::Running } else { Status::Failed });
        return Err(RuntimeError::Checkpoint(format!(
            "{e}\n\ncontainer {id} is {} after the failed dump",
            if still { "still running" } else { "NOT running — its processes were lost" }
        )));
    }
    let dump_ms = t_dump.elapsed().as_secs_f64() * 1000.0;

    // The dump killed the container's processes, so the detached supervisor is
    // now waking up to tear down its cgroup and veth. Let it finish.
    //
    // Skipping this is a race with a long fuse: the supervisor's `remove_dir`
    // on the cgroup lands *after* a subsequent restore has recreated it, and
    // the restore then fails moving the restored process into a cgroup that
    // just vanished ("write .../cgroup.procs: No such file or directory"). It
    // showed up only with --net, because deleting a veth makes the supervisor
    // slow enough to lose the race reliably.
    if !opts.leave_running {
        await_supervisor_exit(&st, std::time::Duration::from_secs(5));
        // Whether or not the supervisor got there, the cgroup must be gone
        // before we report success, so the next restore starts from clean.
        crate::cgroups::Cgroup::open(&cfg.id).cleanup();
    }

    // --- 5. pack ------------------------------------------------------------
    let t_pack = Instant::now();
    let dest = opts
        .dest
        .clone()
        .unwrap_or_else(|| dir.snapshot_file(&opts.name));

    let lifecycle = ContainerState {
        status: Status::Checkpointed.as_str().to_string(),
        pid: None,
        checkpointed_at: Some(now_iso()),
        ..st.clone()
    };

    let meta = SnapshotMeta {
        engine: criu::BINARY.to_string(),
        engine_version,
        config: cfg.clone(),
        lifecycle,
        cgroup,
        namespaces,
        mounts,
        open_fds,
        checkpoint_pid: pid,
        dump_ms,
    };

    let (manifest, snapshot_bytes, image_bytes) = pack(&dest, &images_dir, &dir, meta)?;
    let pack_ms = t_pack.elapsed().as_secs_f64() * 1000.0;

    // Staging has served its purpose; the snapshot is self-contained.
    let _ = std::fs::remove_dir_all(dir.staging_dir());

    // --- 6. commit ----------------------------------------------------------
    if opts.leave_running {
        // The container never stopped, so it goes back to Running — not left
        // in the transient Checkpointing state, which reconciliation would
        // later mistake for an interrupted dump and clean up by killing the
        // very processes we deliberately kept alive.
        state::update(id, |s| {
            s.status = Status::Running.as_str().to_string();
            s.snapshot = Some(dest.display().to_string());
            s.checkpointed_at = Some(now_iso());
        })?;
    } else {
        state::set_checkpointed(id, &dest)?;
    }

    Ok(Report {
        id: id.to_string(),
        snapshot: dest,
        snapshot_bytes,
        image_bytes,
        entries: manifest.entries.len(),
        preflight_ms,
        dump_ms,
        pack_ms,
        total_ms: t0.elapsed().as_secs_f64() * 1000.0,
    })
}

/// Wait for a container's detached supervisor to exit.
///
/// Best effort: a supervisor that outlives the timeout is reported rather than
/// waited on forever, since the caller can still make progress and the cgroup
/// is removed explicitly either way.
fn await_supervisor_exit(st: &ContainerState, timeout: std::time::Duration) {
    let Some(sup) = st.supervisor_pid else {
        return;
    };
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !pid_alive(sup) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    eprintln!(
        "[mincontainer] warning: supervisor {sup} still running {}s after the dump; \
         its cleanup may race a later restore",
        timeout.as_secs()
    );
}

/// Metadata gathered before the dump, handed to the packer.
struct SnapshotMeta {
    engine: String,
    engine_version: String,
    config: ContainerConfig,
    lifecycle: ContainerState,
    cgroup: CgroupSnapshot,
    namespaces: NamespaceSnapshot,
    mounts: Vec<MountEntry>,
    open_fds: Vec<FdEntry>,
    checkpoint_pid: i32,
    dump_ms: f64,
}

/// Which mounts the engine must be told are supplied from outside.
///
/// Shared with the restore path so the `--external` keys declared at dump time
/// and the `--ext-mount-map` entries supplied at restore time cannot drift.
pub fn layout_for(cfg: &ContainerConfig, dir: &ContainerStateDir) -> criu::Layout {
    let mut external_mounts = vec![criu::ExternalMount {
        container_path: format!("/{}", crate::container::IO_MOUNT),
        key: "mcio".to_string(),
        host_path: dir.io_dir().display().to_string(),
    }];

    for (i, v) in cfg.volumes.iter().enumerate() {
        external_mounts.push(criu::ExternalMount {
            container_path: v.container_path.clone(),
            key: format!("mcvol{i}"),
            host_path: v.host_path.clone(),
        });
    }

    criu::Layout {
        root: Some(PathBuf::from(&cfg.rootfs)),
        external_mounts,
        // Unconditional: every container gets its own network namespace,
        // whether or not `--net` wired anything into it.
        empty_net_ns: true,
    }
}

// ---------------------------------------------------------------------------
// Pre-flight
// ---------------------------------------------------------------------------

/// Refuse to checkpoint state the engine cannot honestly reproduce.
///
/// The rule this enforces: a checkpoint either captures the container's real
/// state or it fails. It never silently drops part of it. An established TCP
/// connection is the sharp case — the engine *can* serialise the socket, but
/// the peer on the other end knows nothing about the freeze, so the connection
/// is only still valid if the container comes back at the same address before
/// the peer gives up. That is a judgement about the deployment, not something
/// this runtime can decide, so it is refused unless asked for explicitly.
fn preflight(pid: i32, cfg: &ContainerConfig, opts: &Options) -> Result<()> {
    let conns = established_connections(pid)?;
    if !conns.is_empty() && !opts.allow_tcp {
        let list = conns
            .iter()
            .map(|c| format!("      {} -> {}", c.local, c.remote))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(RuntimeError::Checkpoint(format!(
            "container has {} established TCP connection(s):\n{list}\n\n\
             Freezing drops these on the floor: the peer is not part of the checkpoint and \
             will keep its half of the connection open until it times out.\n\
             Either close them first, or pass --allow-tcp to serialise them — which is only \
             correct if the container resumes at the same address, soon, and the peer's \
             timeout is longer than the freeze. It is never correct across a migration to a \
             different address.",
            conns.len()
        )));
    }

    // A rootfs that has gone missing produces a baffling engine error several
    // seconds later; catch it here.
    if !Path::new(&cfg.rootfs).is_dir() {
        return Err(RuntimeError::Checkpoint(format!(
            "rootfs {} does not exist — cannot checkpoint a container whose root is gone",
            cfg.rootfs
        )));
    }

    Ok(())
}

#[derive(Debug)]
struct Connection {
    local: String,
    remote: String,
}

/// Established TCP connections in the container's network namespace.
///
/// Read through `/proc/<pid>/net/tcp`, which is namespace-scoped: reading it
/// through the container's pid gives that container's sockets without having to
/// enter the namespace.
fn established_connections(pid: i32) -> Result<Vec<Connection>> {
    const TCP_ESTABLISHED: &str = "01";
    let mut out = Vec::new();

    for (file, v6) in [("tcp", false), ("tcp6", true)] {
        let path = format!("/proc/{pid}/net/{file}");
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            // tcp6 is absent when IPv6 is compiled out; that is not an error.
            Err(_) => continue,
        };
        for line in content.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 4 || f[3] != TCP_ESTABLISHED {
                continue;
            }
            out.push(Connection {
                local: format_addr(f[1], v6),
                remote: format_addr(f[2], v6),
            });
        }
    }
    Ok(out)
}

/// Decode procfs's hex `ADDRESS:PORT` form.
///
/// The address is little-endian hex, so the octets come out reversed; the port
/// is big-endian hex. Getting this wrong only shows up as a nonsense IP in an
/// error message, which is exactly when it matters most.
fn format_addr(raw: &str, v6: bool) -> String {
    let (addr, port) = match raw.split_once(':') {
        Some(p) => p,
        None => return raw.to_string(),
    };
    let port = u16::from_str_radix(port, 16).unwrap_or(0);

    if v6 {
        let mut groups = Vec::new();
        for chunk in addr.as_bytes().chunks(8) {
            // Each 32-bit word is little-endian.
            let word = std::str::from_utf8(chunk).unwrap_or("0");
            if let Ok(v) = u32::from_str_radix(word, 16) {
                let be = v.swap_bytes();
                groups.push(format!("{:x}", (be >> 16) as u16));
                groups.push(format!("{:x}", (be & 0xFFFF) as u16));
            }
        }
        return format!("[{}]:{port}", groups.join(":"));
    }

    match u32::from_str_radix(addr, 16) {
        Ok(v) => {
            // procfs prints the address as a host-order u32, so on a
            // little-endian machine 127.0.0.1 appears as "0100007F". Taking the
            // little-endian bytes recovers the octets already in network order.
            let b = v.to_le_bytes();
            format!("{}.{}.{}.{}:{port}", b[0], b[1], b[2], b[3])
        }
        Err(_) => format!("{addr}:{port}"),
    }
}

// ---------------------------------------------------------------------------
// Metadata collection
// ---------------------------------------------------------------------------

pub fn pid_alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Inode number behind each `/proc/<pid>/ns/*` link.
///
/// These are the namespaces' identities. Recording them lets a restore assert
/// it built *new* namespaces rather than accidentally rejoining the old ones.
fn read_namespaces(pid: i32) -> NamespaceSnapshot {
    let one = |kind: &str| -> Option<u64> {
        let link = std::fs::read_link(format!("/proc/{pid}/ns/{kind}")).ok()?;
        // The link reads as e.g. "pid:[4026532281]".
        let s = link.to_string_lossy();
        let inner = s.split_once('[')?.1.strip_suffix(']')?;
        inner.parse().ok()
    };
    NamespaceSnapshot {
        pid: one("pid"),
        mnt: one("mnt"),
        net: one("net"),
        uts: one("uts"),
        ipc: one("ipc"),
    }
}

/// Parse `/proc/<pid>/mountinfo` into the fields a restore cares about.
///
/// The format is positional up to a variable-length optional-fields section
/// terminated by a lone `-`, so the split has to be done on that separator
/// rather than by counting columns.
fn read_mounts(pid: i32) -> Result<Vec<MountEntry>> {
    let path = format!("/proc/{pid}/mountinfo");
    let content = std::fs::read_to_string(&path)
        .map_err(|e| RuntimeError::Checkpoint(format!("read {path}: {e}")))?;

    let mut out = Vec::new();
    for line in content.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let l: Vec<&str> = left.split_whitespace().collect();
        let r: Vec<&str> = right.split_whitespace().collect();
        if l.len() < 6 || r.len() < 2 {
            continue;
        }
        out.push(MountEntry {
            mount_point: l[4].to_string(),
            options: l[5].to_string(),
            fs_type: r[0].to_string(),
            source: r[1].to_string(),
        });
    }
    Ok(out)
}

/// Every open descriptor and what it points at.
///
/// Recorded for diagnosis rather than for replay — the engine serialises the
/// descriptors itself. When a restore fails with "can't open file", this table
/// is what tells you which fd it meant.
fn read_fds(pid: i32) -> Result<Vec<FdEntry>> {
    let dir = format!("/proc/{pid}/fd");
    let rd = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) => return Err(RuntimeError::Checkpoint(format!("read {dir}: {e}"))),
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let Some(fd) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let target = std::fs::read_link(entry.path())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "<unreadable>".to_string());
        out.push(FdEntry { fd, target });
    }
    out.sort_by_key(|e| e.fd);
    Ok(out)
}

/// Read the cgroup's limits and counters back as they actually are.
fn read_cgroup(id: &str) -> CgroupSnapshot {
    let base = PathBuf::from("/sys/fs/cgroup").join(id);
    let read = |f: &str| -> String {
        std::fs::read_to_string(base.join(f))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let peak = ["memory.peak", "memory.current"]
        .iter()
        .find_map(|f| read(f).parse::<u64>().ok())
        .unwrap_or(0);
    let cpu_usage = std::fs::read_to_string(base.join("cpu.stat"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("usage_usec ").and_then(|v| v.trim().parse().ok()))
        })
        .unwrap_or(0);

    CgroupSnapshot {
        memory_max: read("memory.max"),
        memory_peak: peak,
        cpu_max: read("cpu.max"),
        pids_max: read("pids.max"),
        cpu_usage_usec: cpu_usage,
    }
}

// ---------------------------------------------------------------------------
// Packing
// ---------------------------------------------------------------------------

/// Assemble engine images, container logs and metadata into one snapshot file.
fn pack(
    dest: &Path,
    images_dir: &Path,
    dir: &ContainerStateDir,
    meta: SnapshotMeta,
) -> Result<(Manifest, u64, u64)> {
    let mut w = SnapshotWriter::new(dest)?;

    let mut image_bytes = 0u64;
    for path in criu::image_files(images_dir)? {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| RuntimeError::Snapshot(format!("bad image name {}", path.display())))?;
        image_bytes += path.metadata().map(|m| m.len()).unwrap_or(0);
        w.add_file(&format!("images/{name}"), &path)?;
    }
    if image_bytes == 0 {
        return Err(RuntimeError::Checkpoint(
            "engine produced no images — refusing to write an empty snapshot".into(),
        ));
    }

    // The container's own output travels with it. Without this, a migrated
    // container's logs would silently restart at empty on the new node.
    for name in ["stdout", "stderr"] {
        let p = dir.io_dir().join(name);
        if p.is_file() {
            w.add_file(&format!("io/{name}"), &p)?;
        }
    }

    let entries_preview: Vec<EntryMeta> = w.entries().to_vec();
    let _ = entries_preview;

    let manifest = w.finish(dest, |entries| Manifest {
        format_version: FORMAT_VERSION,
        producer: format!("mincontainer/{}", env!("CARGO_PKG_VERSION")),
        engine: meta.engine,
        engine_version: meta.engine_version,
        created_at: now_iso(),
        source_host: hostname(),
        source_arch: std::env::consts::ARCH.to_string(),
        source_kernel: kernel_release(),
        container_id: meta.config.id.clone(),
        config: meta.config,
        lifecycle: meta.lifecycle,
        cgroup: meta.cgroup,
        namespaces: meta.namespaces,
        mounts: meta.mounts,
        open_fds: meta.open_fds,
        checkpoint_pid: meta.checkpoint_pid,
        dump_ms: meta.dump_ms,
        entries,
    })?;

    let snapshot_bytes = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    Ok((manifest, snapshot_bytes, image_bytes))
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_procfs_ipv4_addresses() {
        // 0100007F:0050 is 127.0.0.1:80 (address little-endian, port big-endian).
        assert_eq!(format_addr("0100007F:0050", false), "127.0.0.1:80");
        // 10.66.0.2:8080
        assert_eq!(format_addr("0200420A:1F90", false), "10.66.0.2:8080");
    }

    #[test]
    fn decodes_procfs_ipv6_addresses() {
        // Loopback ::1, port 443. Each 32-bit word is little-endian.
        assert_eq!(
            format_addr("00000000000000000000000001000000:01BB", true),
            "[0:0:0:0:0:0:0:1]:443"
        );
    }
}

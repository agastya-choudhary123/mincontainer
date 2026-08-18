//! Adapter around the CRIU binary — the only place this runtime knows what a
//! checkpoint *engine* is.
//!
//! Everything above this module (the snapshot format, the restore sequencing,
//! the migration transport) is engine-agnostic; swapping CRIU for something
//! else means rewriting this file and nothing above it. That boundary is
//! deliberate, and it is why the snapshot manifest records `engine` and
//! `engine_version` rather than assuming CRIU.
//!
//! CRIU is driven as a subprocess rather than through libcriu/RPC. The CLI is
//! the interface CRIU documents and stabilises, it keeps the dependency at
//! "a binary on PATH", and — the part that actually mattered in practice — a
//! failed dump leaves a verbose log file we can quote back to the user
//! verbatim instead of an opaque error code.

use crate::error::{Result, RuntimeError};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const BINARY: &str = "criu";

/// Verbosity passed to CRIU. Level 4 is what makes a failure diagnosable; the
/// log is only read on the error path, so the cost is a few hundred KB of
/// writes on a path that is already doing disk I/O.
const VERBOSITY: &str = "-v4";

/// Where CRIU is invoked and what it produced.
#[derive(Debug)]
pub struct EngineError {
    pub stage: &'static str,
    pub status: Option<i32>,
    pub log_tail: String,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "criu {} failed (exit {}):\n{}",
            self.stage,
            self.status.map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
            self.log_tail
        )
    }
}

/// Check CRIU is present and usable, returning its version string.
///
/// Called before every dump. `criu check` is deliberately *not* run here: it
/// takes ~200ms and probes features we do not use, and a real dump gives a
/// better error than a generic feature probe would.
pub fn version() -> Result<String> {
    let out = Command::new(BINARY).arg("--version").output().map_err(|e| {
        RuntimeError::Checkpoint(format!(
            "cannot run `{BINARY}`: {e}. Install CRIU (the dev image ships it)."
        ))
    })?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v = text
        .lines()
        .find_map(|l| l.strip_prefix("Version: "))
        .unwrap_or("unknown")
        .trim()
        .to_string();
    Ok(v)
}

/// Run `criu check --all` and return its complaints. Used by `mincontainer
/// info` to report honestly what this host can and cannot checkpoint.
pub fn check() -> Result<(bool, String)> {
    let out = Command::new(BINARY)
        .args(["check", "--all"])
        .output()
        .map_err(|e| RuntimeError::Checkpoint(format!("run criu check: {e}")))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok((out.status.success(), text))
}

/// Arguments shared by dump and restore that describe *this* container's shape.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// The container's root directory on this host.
    pub root: Option<PathBuf>,
    /// Mounts CRIU must treat as supplied from outside rather than recreate.
    ///
    /// Every bind mount we made from the host — volumes and the io directory —
    /// is external by definition: its source lives outside the container's
    /// root, so there is nothing inside the snapshot that could recreate it.
    /// Dump declares them with `--external mnt[<path>]:<key>`; restore supplies
    /// them back with `--ext-mount-map <key>:<host path>`.
    pub external_mounts: Vec<ExternalMount>,
    /// Create the network namespace but restore nothing inside it.
    ///
    /// Always true for our containers, and it is the single most important
    /// flag in this file. See [`Layout::net_note`].
    pub empty_net_ns: bool,
}

impl Layout {
    /// Why the engine never touches the network namespace.
    ///
    /// This runtime owns networking end to end: it unshares the namespace,
    /// creates the veth pair, attaches it to the bridge, assigns the address
    /// and installs the default route — all from configuration it already
    /// holds. Asking the engine to serialise and rebuild that would duplicate
    /// logic we have, and make a restore depend on the engine reproducing our
    /// addressing scheme rather than on us re-running the code that created it.
    ///
    /// `--empty-ns net` says exactly that: give the container a fresh, bare
    /// network namespace and leave its contents to the caller. The restore
    /// path then calls [`crate::network::Network::setup`] — the same function
    /// the original start used.
    ///
    /// It also sidesteps an environment problem that would otherwise be fatal.
    /// A new network namespace on this kernel is not empty: the loaded tunnel
    /// modules seed every namespace with fallback devices (`tunl0`, `gre0`,
    /// `sit0`, `ip6tnl0`, …). The engine cannot serialise an `ipip` link and
    /// refuses the whole dump with `Unsupported link 2 (type 768 kind ipip)`,
    /// and the devices cannot be deleted — the kernel recreates them
    /// immediately. Any design that asks the engine to dump the network
    /// namespace is simply not viable here.
    pub fn net_note() -> &'static str {
        "network namespace is created empty and rewired by the runtime"
    }
}

#[derive(Debug, Clone)]
pub struct ExternalMount {
    /// Mount point inside the container, e.g. `/data`.
    pub container_path: String,
    /// Stable key used to pair dump and restore.
    pub key: String,
    /// Host directory to bind back on restore.
    pub host_path: String,
}

fn tail_log(path: &Path, lines: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            // CRIU puts the useful "Error (file.c:NN)" lines at the end, but a
            // -v4 log ends with hundreds of routine lines too. Prefer the error
            // lines when there are any; otherwise show the tail.
            let errs: Vec<&str> = s
                .lines()
                .filter(|l| l.contains("Error (") || l.contains("Warn (") && l.contains("unsupported"))
                .collect();
            if !errs.is_empty() {
                let start = errs.len().saturating_sub(lines);
                return errs[start..].join("\n");
            }
            let all: Vec<&str> = s.lines().collect();
            let start = all.len().saturating_sub(lines);
            all[start..].join("\n")
        }
        Err(e) => format!("(could not read {}: {e})", path.display()),
    }
}

fn layout_args(layout: &Layout, args: &mut Vec<String>, dumping: bool) {
    if let Some(root) = &layout.root {
        args.push("--root".into());
        args.push(root.display().to_string());
    }
    for m in &layout.external_mounts {
        if dumping {
            args.push("--external".into());
            args.push(format!("mnt[{}]:{}", m.container_path, m.key));
        } else {
            args.push("--ext-mount-map".into());
            args.push(format!("{}:{}", m.key, m.host_path));
        }
    }
    if layout.empty_net_ns {
        // Symmetric: passed to both dump and restore. See `Layout::net_note`.
        args.push("--empty-ns".into());
        args.push("net".into());
    }
}

/// Options for a dump.
#[derive(Debug, Clone)]
pub struct DumpOptions {
    pub pid: i32,
    pub images_dir: PathBuf,
    pub layout: Layout,
    /// Keep the process tree running after the dump. Turns the checkpoint into
    /// a pure snapshot rather than a freeze-and-move.
    pub leave_running: bool,
    /// Allow established TCP connections to be serialised. Off by default:
    /// see the pre-flight in `checkpoint.rs` for why this is opt-in.
    pub tcp_established: bool,
}

/// Freeze and dump the process tree rooted at `pid`.
///
/// On success the container's processes are gone (unless `leave_running`), and
/// `images_dir` holds the engine's image set.
pub fn dump(opts: &DumpOptions) -> std::result::Result<(), EngineError> {
    std::fs::create_dir_all(&opts.images_dir).ok();
    let log = opts.images_dir.join("dump.log");

    let mut args: Vec<String> = vec![
        "dump".into(),
        "--tree".into(),
        opts.pid.to_string(),
        "--images-dir".into(),
        opts.images_dir.display().to_string(),
        "--log-file".into(),
        "dump.log".into(),
        VERBOSITY.into(),
        // The container lives in a cgroup we created; CRIU must record the
        // membership rather than assume it owns the hierarchy.
        "--manage-cgroups=ignore".into(),
        // Our containers hold ordinary files open; without this a file that is
        // unlinked-but-open, or shared between processes, is a hard error.
        "--file-locks".into(),
        "--link-remap".into(),
    ];

    if opts.leave_running {
        args.push("--leave-running".into());
    }
    if opts.tcp_established {
        args.push("--tcp-established".into());
    }
    layout_args(&opts.layout, &mut args, true);

    run(BINARY, &args, "dump", &log)
}

/// Options for a restore.
#[derive(Debug, Clone)]
pub struct RestoreOptions {
    pub images_dir: PathBuf,
    pub layout: Layout,
    /// File CRIU writes the restored root pid into.
    pub pidfile: PathBuf,
    pub tcp_established: bool,
}

/// Rebuild the process tree from an image set, detached from this process.
///
/// Returns the restored root pid, read back from the pidfile CRIU writes.
pub fn restore(opts: &RestoreOptions) -> std::result::Result<i32, EngineError> {
    let log = opts.images_dir.join("restore.log");
    let _ = std::fs::remove_file(&opts.pidfile);

    let mut args: Vec<String> = vec![
        "restore".into(),
        "--images-dir".into(),
        opts.images_dir.display().to_string(),
        "--log-file".into(),
        "restore.log".into(),
        VERBOSITY.into(),
        // Detach: CRIU forks the restored tree and returns, rather than
        // becoming its parent and blocking. We record the pid ourselves.
        "--restore-detached".into(),
        "--pidfile".into(),
        opts.pidfile.display().to_string(),
        "--manage-cgroups=ignore".into(),
        "--file-locks".into(),
        "--link-remap".into(),
    ];

    if opts.tcp_established {
        args.push("--tcp-established".into());
    }
    layout_args(&opts.layout, &mut args, false);

    run(BINARY, &args, "restore", &log)?;

    let raw = std::fs::read_to_string(&opts.pidfile).map_err(|e| EngineError {
        stage: "restore",
        status: None,
        log_tail: format!(
            "criu reported success but wrote no pidfile at {}: {e}",
            opts.pidfile.display()
        ),
    })?;
    raw.trim().parse::<i32>().map_err(|e| EngineError {
        stage: "restore",
        status: None,
        log_tail: format!("pidfile {:?} is not a pid: {e}", raw.trim()),
    })
}

fn run(bin: &str, args: &[String], stage: &'static str, log: &Path) -> std::result::Result<(), EngineError> {
    let out = Command::new(bin).args(args).output().map_err(|e| EngineError {
        stage,
        status: None,
        log_tail: format!("cannot execute {bin}: {e}"),
    })?;

    if out.status.success() {
        return Ok(());
    }

    // CRIU's own stderr is usually a one-line summary; the log has the cause.
    let mut tail = tail_log(log, 12);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if tail.trim().is_empty() && !stderr.trim().is_empty() {
        tail = stderr.trim().to_string();
    }
    Err(EngineError { stage, status: out.status.code(), log_tail: tail })
}

/// Every file CRIU wrote into an image directory, sorted for a stable snapshot
/// layout. The log is included: a snapshot that cannot explain why a later
/// restore failed is worth less than the bytes it costs to carry it.
pub fn image_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let rd = std::fs::read_dir(dir).map_err(|e| {
        RuntimeError::Checkpoint(format!("read image dir {}: {e}", dir.display()))
    })?;
    for entry in rd {
        let entry =
            entry.map_err(|e| RuntimeError::Checkpoint(format!("read image entry: {e}")))?;
        let path = entry.path();
        if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

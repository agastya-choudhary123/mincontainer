//! Live migration: checkpoint here, restore there, hand over ownership.
//!
//! This module is only sequencing — [`crate::checkpoint`] does the freezing and
//! [`crate::transport`] moves the bytes. What lives here is the part that is
//! easy to get subtly wrong: *when* each node believes it owns the container.
//!
//! ```text
//!   running here
//!        │  checkpoint          ── container stops. Downtime starts.
//!        ▼
//!   checkpointed here
//!        │  send + remote restore
//!        ▼
//!   running there, checkpointed here   ── briefly true, and safe: this node
//!        │                                still has a snapshot but no processes
//!        │  mark migrated
//!        ▼
//!   running there                      ── ownership released
//! ```
//!
//! The invariant is that the container is never *running* in two places. It
//! passes through a window where two nodes hold a valid snapshot, which is
//! recoverable; it never passes through a window where two nodes hold live
//! processes, which is not.
//!
//! If anything fails after the freeze, this node keeps its snapshot and stays
//! authoritative — `mincontainer restore <id>` brings it back locally. The
//! failure mode is downtime, never a lost container.

use crate::checkpoint;
use crate::error::{Result, RuntimeError};
use crate::state::{self, ContainerStateDir, Status};
use crate::transport::{self, MigrationReport};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Shared secret the receiver must match.
    pub token: String,
    /// Migrate an existing snapshot instead of taking a fresh one. Useful for
    /// retrying a transfer that failed after a successful freeze.
    pub use_existing: bool,
    /// Serialise established TCP connections. Refused by default, and a bad
    /// idea across hosts: see the pre-flight in `checkpoint.rs`.
    pub allow_tcp: bool,
    /// Keep the local container after a successful migration rather than
    /// releasing ownership. For testing the transport without losing the
    /// container.
    pub keep_local: bool,
}

/// Move `id` to `target`.
pub fn migrate(id: &str, target: &str, opts: &Options) -> Result<MigrationReport> {
    let t0 = Instant::now();
    let dir = ContainerStateDir::for_id(id);
    if !dir.exists() {
        return Err(RuntimeError::Migration(format!("container {id} not found")));
    }

    // --- freeze --------------------------------------------------------------
    let (snapshot, checkpoint_ms) = if opts.use_existing {
        let st = dir.load_state()?;
        let path = st
            .snapshot
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .ok_or_else(|| {
                RuntimeError::Migration(
                    "--use-existing was given but this container has no snapshot on disk".into(),
                )
            })?;
        (path, 0.0)
    } else {
        let st = dir.load_state()?;
        if !st.is(Status::Running) {
            return Err(RuntimeError::Migration(format!(
                "container {id} is {} — migrate freezes a running container. \
                 To ship an existing snapshot, pass --use-existing.",
                st.status
            )));
        }
        let rep = checkpoint::checkpoint(
            id,
            &checkpoint::Options {
                name: "migrate".to_string(),
                dest: None,
                leave_running: false,
                allow_tcp: opts.allow_tcp,
            },
        )?;
        (rep.snapshot, rep.total_ms)
    };

    // --- ship ----------------------------------------------------------------
    // A failure here leaves the container checkpointed locally, which is a
    // recoverable state: the operator restores it here, or retries the
    // migration with --use-existing. Nothing is lost.
    let (result, transfer_ms, bytes) =
        transport::send_snapshot(id, &snapshot, target, &opts.token).map_err(|e| {
            RuntimeError::Migration(format!(
                "{e}\n\nThe container is checkpointed on this node and was NOT handed over. \
                 Recover with:\n    mincontainer restore {id}\nor retry with:\n    \
                 mincontainer migrate {id} {target} --use-existing"
            ))
        })?;

    // --- release -------------------------------------------------------------
    // Only now, with the far side confirming a running process, does this node
    // give up its claim.
    if !opts.keep_local {
        state::update(id, |s| {
            s.status = Status::Migrated.as_str().to_string();
            s.pid = None;
        })?;
    }

    let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let throughput_mib_s = if transfer_ms > 0.0 {
        (bytes as f64 / (1024.0 * 1024.0)) / (transfer_ms / 1000.0)
    } else {
        0.0
    };

    Ok(MigrationReport {
        id: id.to_string(),
        target: target.to_string(),
        snapshot_bytes: bytes,
        checkpoint_ms,
        transfer_ms,
        remote_restore_ms: result.restore_ms,
        total_ms,
        throughput_mib_s,
        remote_pid: result.pid,
        remote_ip: result.container_ip,
    })
}

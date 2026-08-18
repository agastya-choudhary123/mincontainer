//! Partial-failure behaviour: what happens when a snapshot is damaged, or a
//! restore is killed halfway through.
//!
//! These are the tests that justify the design. The claim being checked is not
//! "restore works" but "a restore that goes wrong leaves the host and the
//! container recoverable, and says so" — which is only believable if something
//! actually damages a snapshot and actually kills a restore mid-flight.

mod common;

use common::*;
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Flip one bit inside a snapshot's payload, well past the manifest.
///
/// Aimed at the middle of the file so it lands in engine image data rather than
/// in the header — corrupting the header would be caught by parsing alone,
/// which is a much weaker claim than catching silent bit rot in the payload.
fn corrupt_payload(path: &std::path::Path) -> u64 {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open snapshot");
    let len = f.metadata().unwrap().len();
    let offset = len / 2;

    f.seek(SeekFrom::Start(offset)).unwrap();
    let mut byte = [0u8; 1];
    f.read_exact(&mut byte).unwrap();
    byte[0] ^= 0x01;
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(&byte).unwrap();
    f.flush().unwrap();
    offset
}

/// A single flipped bit must be caught, and caught *before* anything on the
/// host has been built.
#[test]
fn a_corrupt_snapshot_is_refused_and_changes_nothing() {
    require_support!();

    let c = TestContainer::start("corrupt", TICKER);
    c.wait_for_output("tick 3", Duration::from_secs(10));
    mc_ok(&["checkpoint", &c.id]);

    let snap = c.snapshot();
    let before_status = c.status();
    let offset = corrupt_payload(&snap);

    // Reading the manifest still works — the damage is in the payload, so a
    // reader that only parsed the header would see nothing wrong.
    let inspect_ok = mc(&["inspect", snap.to_str().unwrap()]);
    assert!(
        inspect_ok.status.success(),
        "the manifest itself should still parse; the corruption is at byte {offset}"
    );

    // Verification must catch it.
    let err = mc_err(&["inspect", snap.to_str().unwrap(), "--verify"]);
    assert!(
        err.contains("checksum mismatch") && err.contains("corrupt"),
        "verify did not report a checksum failure: {err}"
    );

    // And so must restore, which verifies before building anything.
    let err = mc_err(&["restore", &c.id]);
    assert!(
        err.contains("checksum mismatch") || err.contains("corrupt"),
        "restore accepted a corrupt snapshot, or failed for the wrong reason: {err}"
    );

    // Nothing was half-built: the container is exactly as it was, with no
    // process and no leftover cgroup.
    assert_eq!(
        c.status(),
        before_status,
        "a refused restore changed the container's state"
    );
    assert!(c.pid().is_none(), "a refused restore left a pid behind");
    assert!(
        !std::path::Path::new(&format!("/sys/fs/cgroup/{}", c.id)).exists(),
        "a refused restore left a cgroup behind"
    );
    assert!(
        !c.dir().join("staging").exists(),
        "a refused restore left its staging directory behind"
    );
}

/// Truncation is the other shape corruption takes — an interrupted copy or a
/// full disk — and it must not be mistaken for a valid short snapshot.
#[test]
fn a_truncated_snapshot_is_refused() {
    require_support!();

    let c = TestContainer::start("truncate", TICKER);
    c.wait_for_output("tick 2", Duration::from_secs(10));
    mc_ok(&["checkpoint", &c.id]);

    let snap = c.snapshot();
    let len = std::fs::metadata(&snap).unwrap().len();
    let f = std::fs::OpenOptions::new().write(true).open(&snap).unwrap();
    f.set_len(len - 4096).unwrap();
    drop(f);

    let err = mc_err(&["restore", &c.id]);
    assert!(
        err.contains("truncat") || err.contains("trailer") || err.contains("corrupt"),
        "a truncated snapshot was not recognised as damaged: {err}"
    );
    assert!(c.pid().is_none());
}

/// A file that is not a snapshot at all must be rejected on its magic, not by
/// crashing somewhere deeper.
#[test]
fn a_file_that_is_not_a_snapshot_is_rejected_immediately() {
    require_support!();

    let path = std::env::temp_dir().join("not-a-snapshot.mcsnap");
    std::fs::write(&path, b"this is just some text, definitely not a container").unwrap();

    let err = mc_err(&["inspect", path.to_str().unwrap()]);
    assert!(
        err.contains("not a mincontainer snapshot"),
        "wrong rejection reason: {err}"
    );
    let _ = std::fs::remove_file(&path);
}

/// Kill a restore while it is running and prove the system recovers.
///
/// This is the case the [`crate::restore::Rollback`] guard cannot handle —
/// `SIGKILL` leaves no chance to unwind — so recovery has to come from
/// reconciliation on the next command. The properties being checked:
///
///   1. the snapshot survives (it is never consumed, so a retry is possible)
///   2. the container does not get stuck in `restoring`
///   3. no cgroup or process debris is left behind
///   4. a plain retry then succeeds
#[test]
fn a_restore_killed_midway_recovers_and_can_be_retried() {
    require_support!();

    let c = TestContainer::start("killrestore", TICKER);
    c.wait_for_output("tick 3", Duration::from_secs(10));
    mc_ok(&["checkpoint", &c.id]);
    let frozen_at = c.last_tick();
    let snapshot_len = std::fs::metadata(c.snapshot()).unwrap().len();

    // A restore takes on the order of 100-300ms; land the kill inside that
    // window, after the build phase has started but before it commits.
    let mut child = Command::new(BIN)
        .args(["restore", &c.id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn restore");

    std::thread::sleep(Duration::from_millis(60));
    child.kill().expect("kill the restore mid-flight");
    let status = child.wait().expect("reap the killed restore");
    assert!(!status.success(), "the restore was supposed to be killed");

    // (1) The snapshot is untouched — this is what makes retrying possible.
    assert!(c.snapshot().is_file(), "the killed restore destroyed the snapshot");
    assert_eq!(
        std::fs::metadata(c.snapshot()).unwrap().len(),
        snapshot_len,
        "the killed restore modified the snapshot"
    );

    // (2) Reconciliation happens on the next command an operator would run.
    // The container must not be left claiming to be mid-restore.
    let _ = mc(&["ps"]);
    let status_now = c.status();
    assert!(
        status_now == "checkpointed" || status_now == "running",
        "container stuck in {status_now:?} after a killed restore"
    );

    // (3) If reconciliation decided the restore did not complete, no debris
    // may remain.
    if status_now == "checkpointed" {
        assert!(c.pid().is_none(), "stale pid after a killed restore");
        assert!(
            !c.dir().join("staging").exists(),
            "staging directory survived reconciliation"
        );

        // (4) And a plain retry works.
        mc_ok(&["restore", &c.id]);
        assert_eq!(c.status(), "running");
        assert!(
            wait_until(Duration::from_secs(10), || c.last_tick() > frozen_at),
            "the retried restore did not resume the container"
        );
    } else {
        // The restore had already committed before the kill landed. That is a
        // legitimate outcome — the work was done and only the reporting was
        // lost — but the container must genuinely be alive.
        assert!(
            wait_until(Duration::from_secs(10), || c.last_tick() > frozen_at),
            "container marked running after a killed restore but is not ticking"
        );
    }
}

/// Killing a checkpoint mid-dump must not leave the container in limbo either.
#[test]
fn a_checkpoint_killed_midway_leaves_a_reconcilable_state() {
    require_support!();

    let c = TestContainer::start("killckpt", TICKER);
    c.wait_for_output("tick 3", Duration::from_secs(10));

    let mut child = Command::new(BIN)
        .args(["checkpoint", &c.id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn checkpoint");

    std::thread::sleep(Duration::from_millis(30));
    let _ = child.kill();
    let _ = child.wait();

    // Reconcile, then require a state that is actually actionable rather than
    // a permanent "checkpointing".
    let _ = mc(&["ps"]);
    let s = c.status();
    assert!(
        matches!(s.as_str(), "checkpointed" | "running" | "stopped" | "failed"),
        "container stuck in transient state {s:?} after a killed checkpoint"
    );
    assert_ne!(s, "checkpointing", "transient state was never reconciled");
}

/// The container's cgroup must not survive a container that is gone.
#[test]
fn removing_a_checkpointed_container_cleans_up() {
    require_support!();

    let c = TestContainer::start("cleanup", TICKER);
    c.wait_for_output("tick 2", Duration::from_secs(10));
    mc_ok(&["checkpoint", &c.id]);

    let id = c.id.clone();
    let dir = c.dir();
    drop(c); // runs stop + rm

    assert!(!dir.exists(), "state directory survived rm");
    assert!(
        !std::path::Path::new(&format!("/sys/fs/cgroup/{id}")).exists(),
        "cgroup survived rm"
    );
}

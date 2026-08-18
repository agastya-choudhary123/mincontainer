//! Checkpoint and restore, end to end, against real containers.

mod common;

use common::*;
use std::time::Duration;

/// The property that matters: a restored container continues from where it was
/// frozen, rather than starting over.
///
/// A test that only asserted "a process is running afterwards" would pass on a
/// runtime that quietly relaunched the container from scratch, which is
/// precisely the fake-success this must rule out. Comparing tick counts proves
/// the *memory* came back, not just the process.
#[test]
fn restore_resumes_execution_where_the_checkpoint_froze_it() {
    require_support!();

    let c = TestContainer::start("resume", TICKER);
    c.wait_for_output("tick 3", Duration::from_secs(10));

    mc_ok(&["checkpoint", &c.id]);
    assert_eq!(c.status(), "checkpointed");
    assert!(c.pid().is_none(), "a checkpointed container must not keep a pid");

    let frozen_at = c.last_tick();

    // Nothing may advance while frozen.
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        c.last_tick(),
        frozen_at,
        "the container kept running after being checkpointed"
    );

    mc_ok(&["restore", &c.id]);
    assert_eq!(c.status(), "running");
    assert_eq!(c.state()["generation"].as_u64(), Some(1));

    assert!(
        wait_until(Duration::from_secs(10), || c.last_tick() > frozen_at),
        "the container did not resume ticking after restore"
    );

    // The counter must have carried on, not restarted. A relaunched container
    // would come back at tick 1.
    let resumed = c.last_tick();
    assert!(
        resumed > frozen_at,
        "expected the counter to continue past {frozen_at}, got {resumed}"
    );
    assert!(
        c.stdout().matches("tick 1\n").count() == 1,
        "the container restarted from the beginning instead of resuming:\n{}",
        c.stdout()
    );
}

/// A snapshot is a file, and `inspect` must be able to describe it without any
/// out-of-band knowledge — that is what "self-describing" has to mean.
#[test]
fn a_snapshot_describes_itself() {
    require_support!();

    let c = TestContainer::start("inspect", TICKER);
    c.wait_for_output("tick 2", Duration::from_secs(10));
    mc_ok(&["checkpoint", &c.id]);

    let snap = c.snapshot();
    assert!(snap.is_file(), "no snapshot at {}", snap.display());

    let json = mc_ok(&["inspect", snap.to_str().unwrap(), "--verify", "--json"]);
    let m: serde_json::Value = serde_json::from_str(&json).expect("manifest is JSON");

    assert_eq!(m["format_version"], 1);
    assert_eq!(m["container_id"], c.id.as_str());
    assert_eq!(m["engine"], "criu");
    assert_eq!(m["source_arch"], std::env::consts::ARCH);
    assert!(
        m["created_at"].as_str().unwrap().starts_with("20"),
        "implausible timestamp {}",
        m["created_at"]
    );

    // The metadata a restore actually depends on must be present and non-empty.
    assert!(!m["mounts"].as_array().unwrap().is_empty(), "no mounts recorded");
    assert!(!m["open_fds"].as_array().unwrap().is_empty(), "no fds recorded");
    assert!(m["namespaces"]["pid"].is_number(), "no pid namespace recorded");
    assert!(m["namespaces"]["mnt"].is_number(), "no mount namespace recorded");
    assert_ne!(m["cgroup"]["memory_max"], "", "no cgroup limits recorded");

    // The container's own log travels inside the snapshot.
    let names: Vec<&str> = m["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"io/stdout"), "stdout not carried: {names:?}");
    assert!(
        names.iter().any(|n| n.starts_with("images/")),
        "no engine images: {names:?}"
    );
}

/// Checkpointing twice and restoring the older snapshot must work: a snapshot
/// is immutable and is never consumed by a restore.
#[test]
fn restoring_from_an_explicit_older_snapshot_works() {
    require_support!();

    let c = TestContainer::start("older", TICKER);
    c.wait_for_output("tick 3", Duration::from_secs(10));

    // First checkpoint, left running so the container can advance further.
    mc_ok(&["checkpoint", &c.id, "--name", "early", "--leave-running"]);
    let early_tick = c.last_tick();
    let early = c.dir().join("snapshots/early.mcsnap");
    assert!(early.is_file());

    assert!(
        wait_until(Duration::from_secs(10), || c.last_tick() > early_tick + 3),
        "--leave-running did not leave the container running"
    );

    mc_ok(&["checkpoint", &c.id, "--name", "late"]);
    let late_tick = c.last_tick();
    assert!(late_tick > early_tick);

    // Restore the *earlier* snapshot; the container must come back at the
    // earlier point in its life.
    mc_ok(&["restore", &c.id, "--from", early.to_str().unwrap()]);
    assert!(
        wait_until(Duration::from_secs(10), || c.last_tick() > early_tick),
        "restored container did not resume"
    );

    // And the snapshot is still there afterwards — restore does not consume it.
    assert!(early.is_file(), "restoring deleted the snapshot it read");
}

/// Only a running container can be checkpointed, and the error has to say so.
#[test]
fn checkpointing_a_checkpointed_container_is_refused() {
    require_support!();

    let c = TestContainer::start("double", TICKER);
    c.wait_for_output("tick 2", Duration::from_secs(10));
    mc_ok(&["checkpoint", &c.id]);

    let err = mc_err(&["checkpoint", &c.id]);
    assert!(
        err.contains("not running"),
        "unhelpful error for double checkpoint: {err}"
    );
    // The first snapshot must survive the refused second attempt.
    assert!(c.snapshot().is_file());
}

/// Restoring a container that is already running would produce two copies of
/// it sharing one identity.
#[test]
fn restoring_a_running_container_is_refused() {
    require_support!();

    let c = TestContainer::start("dup", TICKER);
    c.wait_for_output("tick 2", Duration::from_secs(10));
    mc_ok(&["checkpoint", &c.id, "--leave-running"]);

    let err = mc_err(&["restore", &c.id]);
    assert!(
        err.contains("already running"),
        "expected a refusal to duplicate a running container, got: {err}"
    );
}

/// With no snapshot at all, restore must fail with something actionable.
#[test]
fn restoring_without_a_snapshot_says_what_to_do() {
    require_support!();

    let c = TestContainer::start("nosnap", TICKER);
    c.wait_for_output("tick 1", Duration::from_secs(10));

    // Stop it first: a *running* container is refused earlier, on the stronger
    // grounds that restoring it would duplicate it, and that check has to come
    // before we go looking for a snapshot.
    mc_ok(&["stop", &c.id]);
    assert!(
        wait_until(Duration::from_secs(5), || c.status() == "stopped"),
        "container did not stop, status is {:?}",
        c.status()
    );

    let err = mc_err(&["restore", &c.id]);
    assert!(
        err.contains("no snapshot found") && err.contains("checkpoint it first"),
        "unhelpful error: {err}"
    );
}

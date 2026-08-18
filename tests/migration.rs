//! Migration transport: loopback migration, and the failure paths that decide
//! who owns the container when something goes wrong.
//!
//! A true cross-host test needs two machines; these run the receiver on
//! loopback, which exercises the entire protocol — framing, handshake,
//! checksums, remote restore, ownership handover — with only the network path
//! shortened. `scripts/bench-migrate.sh` does the genuinely cross-host run
//! between two containers on a Docker network.

mod common;

use common::*;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const TOKEN: &str = "test-token";

/// A receiver process that is shut down when the test ends.
struct Receiver {
    child: Child,
    port: u16,
}

impl Receiver {
    fn start(port: u16) -> Self {
        let child = Command::new(BIN)
            .args([
                "serve",
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--token",
                TOKEN,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn receiver");

        // Wait for the port to accept connections.
        let up = wait_until(Duration::from_secs(5), || {
            std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
        });
        assert!(up, "receiver never started listening on {port}");
        Receiver { child, port }
    }

    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Pick a port that is free right now.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().unwrap().port()
}

/// A wrong token must be refused during the handshake, before any snapshot
/// bytes are sent.
#[test]
fn a_bad_token_is_refused_at_the_handshake() {
    require_support!();

    let rx = Receiver::start(free_port());
    let c = TestContainer::start("badtoken", TICKER);
    c.wait_for_output("tick 2", Duration::from_secs(10));
    mc_ok(&["checkpoint", &c.id]);

    let err = mc_err(&[
        "migrate",
        &c.id,
        &rx.addr(),
        "--token",
        "definitely-wrong",
        "--use-existing",
    ]);
    assert!(
        err.contains("token mismatch") || err.contains("refused"),
        "a bad token was not rejected clearly: {err}"
    );

    // Ownership must not have moved.
    assert_eq!(
        c.status(),
        "checkpointed",
        "a refused migration gave away the container"
    );
    assert!(c.snapshot().is_file(), "a refused migration destroyed the snapshot");
}

/// The container must stay ours when the target is unreachable, and the error
/// must name the commands that recover it.
#[test]
fn an_unreachable_target_leaves_the_container_recoverable() {
    require_support!();

    let c = TestContainer::start("unreachable", TICKER);
    c.wait_for_output("tick 3", Duration::from_secs(10));
    let frozen_at = c.last_tick();

    // Nothing is listening on this port.
    let dead = format!("127.0.0.1:{}", free_port());
    let err = mc_err(&["migrate", &c.id, &dead]);

    assert!(
        err.contains("connect") || err.contains("Connection refused"),
        "unexpected failure for an unreachable target: {err}"
    );
    // The failure must tell the operator how to get the container back.
    assert!(
        err.contains(&format!("mincontainer restore {}", c.id)),
        "the error does not say how to recover: {err}"
    );
    assert!(
        err.contains("--use-existing"),
        "the error does not offer a retry: {err}"
    );

    // The container was frozen (migrate always checkpoints first) but is still
    // ours, and the snapshot is intact — so it can be brought back locally.
    assert_eq!(c.status(), "checkpointed");
    mc_ok(&["restore", &c.id]);
    assert!(
        wait_until(Duration::from_secs(10), || c.last_tick() > frozen_at),
        "the container could not be recovered after a failed migration"
    );
}

/// A full migration over the real protocol.
///
/// Sender and receiver share a state directory here, since both run as the
/// same user on one host — so this checks the protocol, the checksums, the
/// remote restore and the handover, and `scripts/bench-migrate.sh` checks the
/// genuinely two-host case.
#[test]
fn a_snapshot_survives_a_round_trip_through_the_transport() {
    require_support!();

    let rx = Receiver::start(free_port());
    let c = TestContainer::start("wire", TICKER);
    c.wait_for_output("tick 3", Duration::from_secs(10));
    let frozen_at = c.last_tick();

    let out = mc(&[
        "migrate",
        &c.id,
        &rx.addr(),
        "--token",
        TOKEN,
        "--keep-local",
        "--json",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "migration over loopback failed: {stderr}"
    );

    let rep: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("report is JSON");
    assert!(rep["snapshot_bytes"].as_u64().unwrap() > 0);
    assert!(rep["remote_pid"].as_i64().unwrap() > 0, "no remote pid reported");
    assert!(
        rep["remote_restore_ms"].as_f64().unwrap() > 0.0,
        "receiver reported no restore time"
    );

    // The receiver restored it into the shared state directory, so the
    // container is running again and its counter carried on.
    assert!(
        wait_until(Duration::from_secs(10), || c.last_tick() > frozen_at),
        "the migrated container is not running on the receiving side"
    );
}

/// A receiver must survive a client that connects and says nothing useful.
/// One bad peer taking down the migration endpoint would be a trivial
/// denial of service.
#[test]
fn the_receiver_survives_a_garbage_client() {
    require_support!();

    let rx = Receiver::start(free_port());

    // Speak nonsense at it, several ways.
    for junk in [
        &b"GET / HTTP/1.1\r\n\r\n"[..],
        &b"\xff\xff\xff\xff\xff"[..],
        &b""[..],
    ] {
        use std::io::Write;
        if let Ok(mut s) = std::net::TcpStream::connect(rx.addr()) {
            let _ = s.write_all(junk);
            let _ = s.flush();
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // A well-formed migration must still work afterwards.
    let c = TestContainer::start("aftergarbage", TICKER);
    c.wait_for_output("tick 2", Duration::from_secs(10));
    let out = mc(&[
        "migrate",
        &c.id,
        &rx.addr(),
        "--token",
        TOKEN,
        "--keep-local",
    ]);
    assert!(
        out.status.success(),
        "the receiver stopped working after garbage input: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

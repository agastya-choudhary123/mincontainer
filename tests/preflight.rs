//! Pre-flight: state that cannot be checkpointed cleanly must fail loudly.

mod common;

use common::*;
use std::io::Read;
use std::net::TcpListener;
use std::time::Duration;

/// A container holding an established TCP connection must not be silently
/// frozen.
///
/// The connection's peer is not part of the checkpoint and cannot be told to
/// wait, so a checkpoint that "succeeded" would be lying: the restored
/// container would come back holding a socket whose other end has long since
/// given up. The runtime refuses, and the error explains the one situation in
/// which overriding is actually correct.
#[test]
fn established_tcp_connections_are_refused_by_default() {
    require_support!();

    // A listener on the host bridge address, reachable from inside the
    // container's network namespace.
    let listener = TcpListener::bind("10.66.0.1:0").or_else(|_| TcpListener::bind("0.0.0.0:0"));
    let Ok(listener) = listener else {
        eprintln!("SKIP: could not bind a test listener");
        return;
    };
    let port = listener.local_addr().unwrap().port();

    // Hold the accepted connections open for the duration of the test.
    let accepter = std::thread::spawn(move || {
        let mut held = Vec::new();
        listener
            .set_nonblocking(false)
            .expect("blocking listener");
        for stream in listener.incoming().take(1) {
            match stream {
                Ok(s) => held.push(s),
                Err(_) => break,
            }
        }
        std::thread::sleep(Duration::from_secs(20));
        drop(held);
    });

    // A networked container that opens a connection and keeps it open.
    let id = format!("test-tcp-{}", std::process::id());
    let _ = std::process::Command::new(BIN).args(["rm", &id]).output();
    let script = format!(
        "(echo hello; sleep 30) | nc 10.66.0.1 {port} & \
         i=0; while true; do i=$((i+1)); echo \"tick $i\"; sleep 0.2; done"
    );
    mc_ok(&[
        "create",
        "--rootfs",
        &rootfs(),
        "--id",
        &id,
        "--net",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);

    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::process::Command::new(BIN).args(["stop", &self.0]).output();
            std::thread::sleep(Duration::from_millis(80));
            let _ = std::process::Command::new(BIN).args(["rm", &self.0]).output();
        }
    }
    let _cleanup = Cleanup(id.clone());

    if !mc(&["start", "-d", &id]).status.success() {
        eprintln!("SKIP: networked container would not start (bridge unavailable?)");
        return;
    }

    // Give the connection time to establish. If it never does — no `nc` in the
    // rootfs, or no bridge — there is nothing to assert, and pretending
    // otherwise would make this test lie.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let state = format!("{home}/.mincontainer/containers/{id}/state.json");
    let pid = {
        let mut pid = None;
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&state) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                    if let Some(p) = v["pid"].as_i64() {
                        pid = Some(p);
                        break;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        match pid {
            Some(p) => p,
            None => {
                eprintln!("SKIP: container never reported a pid");
                return;
            }
        }
    };

    let established = wait_until(Duration::from_secs(8), || {
        std::fs::File::open(format!("/proc/{pid}/net/tcp"))
            .map(|mut f| {
                let mut s = String::new();
                let _ = f.read_to_string(&mut s);
                s.lines().skip(1).any(|l| {
                    l.split_whitespace().nth(3).map(|st| st == "01").unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    if !established {
        eprintln!("SKIP: no established connection appeared inside the container");
        return;
    }

    // The actual assertion.
    let err = mc_err(&["checkpoint", &id]);
    assert!(
        err.contains("established TCP connection"),
        "checkpoint did not refuse a container with a live connection: {err}"
    );
    assert!(
        err.contains("--allow-tcp"),
        "the refusal does not say how to override it: {err}"
    );
    assert!(
        err.contains("never correct across a migration"),
        "the refusal does not explain when overriding is wrong: {err}"
    );

    drop(accepter);
}

/// A container whose rootfs vanished cannot be checkpointed, and should say so
/// rather than producing a baffling engine error seconds later.
#[test]
fn a_missing_rootfs_is_caught_in_preflight() {
    require_support!();

    let id = format!("test-norootfs-{}", std::process::id());
    let _ = std::process::Command::new(BIN).args(["rm", &id]).output();

    let tmp = std::env::temp_dir().join(format!("mc-rootfs-{}", std::process::id()));
    // Build a throwaway rootfs by copying the real one's essentials.
    std::fs::create_dir_all(&tmp).unwrap();
    let status = std::process::Command::new("cp")
        .args(["-a", &format!("{}/.", rootfs()), tmp.to_str().unwrap()])
        .status()
        .expect("copy rootfs");
    if !status.success() {
        eprintln!("SKIP: could not stage a throwaway rootfs");
        return;
    }

    mc_ok(&[
        "create",
        "--rootfs",
        tmp.to_str().unwrap(),
        "--id",
        &id,
        "--",
        "/bin/sh",
        "-c",
        TICKER,
    ]);
    mc_ok(&["start", "-d", &id]);
    std::thread::sleep(Duration::from_millis(400));

    // Pull the rootfs out from under it.
    std::fs::remove_dir_all(&tmp).ok();

    let err = mc_err(&["checkpoint", &id]);
    assert!(
        err.contains("rootfs") && err.contains("does not exist"),
        "a missing rootfs produced an unhelpful error: {err}"
    );

    let _ = std::process::Command::new(BIN).args(["stop", &id]).output();
    std::thread::sleep(Duration::from_millis(80));
    let _ = std::process::Command::new(BIN).args(["rm", &id]).output();
}

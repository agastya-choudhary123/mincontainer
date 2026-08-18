//! Shared helpers for the checkpoint/restore integration tests.
//!
//! These drive the real binary against real containers. There is no mock
//! engine and no in-process shortcut: a test that does not actually freeze a
//! process would not tell us anything about whether checkpointing works.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_mincontainer");

/// Rootfs the dev image pre-extracts.
pub fn rootfs() -> String {
    std::env::var("MC_TEST_ROOTFS").unwrap_or_else(|_| "/rootfs".to_string())
}

/// Whether this host can run the tests at all.
///
/// Returns the reason it cannot, so a skip is always explained rather than
/// silently passing.
pub fn unsupported() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("not Linux".into());
    }
    if !nix::unistd::Uid::effective().is_root() {
        return Some("not root (checkpointing needs privileges)".into());
    }
    if Command::new("criu").arg("--version").output().is_err() {
        return Some("criu is not installed".into());
    }
    if !Path::new(&rootfs()).is_dir() {
        return Some(format!("rootfs {} does not exist", rootfs()));
    }
    None
}

/// Skip the calling test if the host cannot support it, printing why.
///
/// `MC_TEST_STRICT=1` turns a skip into a failure, which is what CI and the
/// test script set: a suite that quietly skips everything is worse than one
/// that fails.
#[macro_export]
macro_rules! require_support {
    () => {
        if let Some(reason) = $crate::common::unsupported() {
            if std::env::var("MC_TEST_STRICT").as_deref() == Ok("1") {
                panic!("MC_TEST_STRICT=1 but this host cannot run the test: {reason}");
            }
            eprintln!("SKIP {}: {reason}", module_path!());
            return;
        }
    };
}

pub fn mc(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run {BIN} {args:?}: {e}"))
}

/// Run the binary and require success, showing both streams on failure.
pub fn mc_ok(args: &[&str]) -> String {
    let out = mc(args);
    assert!(
        out.status.success(),
        "`mincontainer {}` failed with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        args.join(" "),
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Run the binary and require failure, returning stderr so the test can assert
/// on *why* it failed. A test that only checks a non-zero exit would pass on
/// the wrong error.
pub fn mc_err(args: &[&str]) -> String {
    let out = mc(args);
    assert!(
        !out.status.success(),
        "`mincontainer {}` unexpectedly succeeded\n{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
    );
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// A container that cleans itself up when the test ends, however it ends.
pub struct TestContainer {
    pub id: String,
}

impl TestContainer {
    /// Create and start a detached container running `script` under /bin/sh.
    pub fn start(name: &str, script: &str) -> Self {
        let id = format!("test-{name}-{}", std::process::id());
        let _ = Command::new(BIN).args(["rm", &id]).output();

        mc_ok(&[
            "create",
            "--rootfs",
            &rootfs(),
            "--id",
            &id,
            "--",
            "/bin/sh",
            "-c",
            script,
        ]);
        mc_ok(&["start", "-d", &id]);
        TestContainer { id }
    }

    pub fn dir(&self) -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        PathBuf::from(home).join(".mincontainer/containers").join(&self.id)
    }

    pub fn stdout(&self) -> String {
        std::fs::read_to_string(self.dir().join("io/stdout")).unwrap_or_default()
    }

    pub fn snapshot(&self) -> PathBuf {
        self.dir().join("snapshots/latest.mcsnap")
    }

    pub fn state(&self) -> serde_json::Value {
        let raw = std::fs::read_to_string(self.dir().join("state.json"))
            .unwrap_or_else(|e| panic!("read state.json for {}: {e}", self.id));
        serde_json::from_str(&raw).expect("state.json is valid JSON")
    }

    pub fn status(&self) -> String {
        self.state()["status"].as_str().unwrap_or("?").to_string()
    }

    pub fn pid(&self) -> Option<i64> {
        self.state()["pid"].as_i64()
    }

    /// Block until the container's stdout contains `needle`.
    pub fn wait_for_output(&self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.stdout().contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "{}: {needle:?} never appeared in stdout within {timeout:?}. Got:\n{}",
            self.id,
            self.stdout()
        );
    }

    /// The last `tick N` value the workload printed.
    pub fn last_tick(&self) -> u64 {
        self.stdout()
            .lines()
            .filter_map(|l| l.strip_prefix("tick ").and_then(|n| n.trim().parse().ok()))
            .next_back()
            .unwrap_or_else(|| panic!("{}: no tick lines in stdout:\n{}", self.id, self.stdout()))
    }
}

impl Drop for TestContainer {
    fn drop(&mut self) {
        let _ = Command::new(BIN).args(["stop", &self.id]).output();
        std::thread::sleep(Duration::from_millis(80));
        let _ = Command::new(BIN).args(["rm", &self.id]).output();
    }
}

/// A workload that counts, prints, and sleeps — so a checkpoint's effect is
/// directly observable: the counter must freeze while dumped and resume from
/// where it stopped, not from zero.
pub const TICKER: &str = "i=0; while true; do i=$((i+1)); echo \"tick $i\"; sleep 0.2; done";

pub fn wait_until<F: Fn() -> bool>(timeout: Duration, f: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

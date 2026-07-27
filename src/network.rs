use crate::error::{Result, RuntimeError};
use nix::unistd::Pid;
use std::process::Command;

const BRIDGE: &str = "mc0";
const BRIDGE_ADDR: &str = "10.66.0.1";
const SUBNET: &str = "10.66.0.0/24";

/// Per-container network handle. The host-side veth is named from the short id;
/// the container side is renamed to eth0 inside its netns.
pub struct Network {
    host_veth: String,
    container_ip: String,
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| RuntimeError::Network(format!("spawn {cmd}: {e}")))?;
    if !out.status.success() {
        return Err(RuntimeError::Network(format!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Create the shared bridge if it does not already exist, and enable NAT so
/// containers can reach the outside world.
fn ensure_bridge() -> Result<()> {
    let exists = Command::new("ip")
        .args(["link", "show", BRIDGE])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !exists {
        run("ip", &["link", "add", BRIDGE, "type", "bridge"])?;
        run("ip", &["addr", "add", &format!("{BRIDGE_ADDR}/24"), "dev", BRIDGE])?;
        run("ip", &["link", "set", BRIDGE, "up"])?;
        // Enable forwarding + masquerade so container traffic can egress.
        let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");
        let _ = run(
            "iptables",
            &["-t", "nat", "-C", "POSTROUTING", "-s", SUBNET, "!", "-o", BRIDGE, "-j", "MASQUERADE"],
        )
        .or_else(|_| {
            run(
                "iptables",
                &["-t", "nat", "-A", "POSTROUTING", "-s", SUBNET, "!", "-o", BRIDGE, "-j", "MASQUERADE"],
            )
        });
    }
    Ok(())
}

impl Network {
    /// Wire a freshly-created container (identified by its host-visible pid)
    /// into the bridge. `index` gives each container a distinct /24 address.
    pub fn setup(short_id: &str, pid: Pid, index: u8) -> Result<Self> {
        ensure_bridge()?;

        let host_veth = format!("mch{short_id}");
        let host_veth = host_veth[..host_veth.len().min(15)].to_string(); // IFNAMSIZ
        let peer = format!("mcp{short_id}");
        let peer = peer[..peer.len().min(15)].to_string();
        let container_ip = format!("10.66.0.{}", 1 + index as u16 + 1); // .2, .3, ...

        // veth pair; move the peer end into the container's netns.
        run("ip", &["link", "add", &host_veth, "type", "veth", "peer", "name", &peer])?;
        run("ip", &["link", "set", &host_veth, "master", BRIDGE])?;
        run("ip", &["link", "set", &host_veth, "up"])?;
        run("ip", &["link", "set", &peer, "netns", &pid.to_string()])?;

        // Configure the container side from the host, via nsenter into its netns.
        let p = pid.to_string();
        run("nsenter", &["-t", &p, "-n", "ip", "link", "set", &peer, "name", "eth0"])?;
        run("nsenter", &["-t", &p, "-n", "ip", "addr", "add", &format!("{container_ip}/24"), "dev", "eth0"])?;
        run("nsenter", &["-t", &p, "-n", "ip", "link", "set", "eth0", "up"])?;
        run("nsenter", &["-t", &p, "-n", "ip", "link", "set", "lo", "up"])?;
        run("nsenter", &["-t", &p, "-n", "ip", "route", "add", "default", "via", BRIDGE_ADDR])?;

        Ok(Network { host_veth, container_ip })
    }

    pub fn container_ip(&self) -> &str {
        &self.container_ip
    }

    /// Tear down the host-side veth (the peer disappears with the netns).
    pub fn cleanup(&self) {
        let _ = run("ip", &["link", "delete", &self.host_veth]);
    }
}

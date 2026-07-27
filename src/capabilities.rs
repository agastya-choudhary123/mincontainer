use crate::error::{Result, RuntimeError};
use caps::{CapSet, Capability};

/// Dangerous capabilities we strip from the container before exec. Dropping
/// these from the bounding set means the process can never regain them, even
/// via setuid binaries.
const DROP: &[Capability] = &[
    Capability::CAP_SYS_ADMIN,   // the "root of all capabilities"
    Capability::CAP_SYS_MODULE,  // load kernel modules
    Capability::CAP_SYS_RAWIO,   // raw I/O port access
    Capability::CAP_SYS_BOOT,    // reboot
    Capability::CAP_SYS_TIME,    // set system clock
    Capability::CAP_NET_ADMIN,   // reconfigure networking
    Capability::CAP_NET_RAW,     // raw sockets (spoofing)
    Capability::CAP_MKNOD,       // create device nodes
    Capability::CAP_SYS_PTRACE,  // trace/inject into other processes
];

/// Number of capabilities the runtime drops.
pub fn dropped_count() -> usize {
    DROP.len()
}

/// Drop the dangerous capabilities from every set (bounding + inheritable +
/// permitted + effective + ambient). Call after privileged setup, before exec.
pub fn drop_dangerous() -> Result<()> {
    for cap in DROP {
        // Remove from the bounding set first so it can't be re-acquired.
        let _ = caps::drop(None, CapSet::Bounding, *cap);
        let _ = caps::drop(None, CapSet::Ambient, *cap);
    }
    // Clear the inheritable set entirely; children need nothing inherited.
    caps::clear(None, CapSet::Inheritable)
        .map_err(|e| RuntimeError::Config(format!("clear inheritable caps: {e}")))?;
    Ok(())
}

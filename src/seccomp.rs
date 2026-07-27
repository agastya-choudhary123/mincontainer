use crate::error::{Result, RuntimeError};
use seccompiler::{SeccompAction, SeccompFilter};
use std::collections::BTreeMap;

/// Syscalls we refuse to let containers make. Denying these blocks whole
/// classes of container escapes / kernel-surface attacks while leaving normal
/// workloads untouched. Returning EPERM (rather than killing) keeps behaviour
/// close to Docker's default profile.
fn blocked_syscalls() -> Vec<libc::c_long> {
    vec![
        libc::SYS_ptrace,          // debugging / process injection
        libc::SYS_keyctl,          // kernel keyring
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_mount,           // remount tricks
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_init_module,     // load kernel modules
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_perf_event_open,
    ]
}

/// Number of syscalls the active profile denies.
pub fn blocked_count() -> usize {
    blocked_syscalls().len()
}

/// Install a seccomp-bpf filter: default ALLOW, explicit deny-list -> EPERM.
///
/// Must be called after all privileged setup (mount/pivot_root are themselves
/// on the deny-list) and just before exec.
pub fn apply() -> Result<()> {
    let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();
    for sc in blocked_syscalls() {
        rules.insert(sc as i64, vec![]); // empty rule vec = unconditional match
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,                                 // default: allow
        SeccompAction::Errno(libc::EPERM as u32),             // matched: EPERM
        std::env::consts::ARCH.try_into().map_err(|e| {
            RuntimeError::Seccomp(format!("unsupported arch: {e:?}"))
        })?,
    )
    .map_err(|e| RuntimeError::Seccomp(format!("build filter: {e}")))?;

    let program: seccompiler::BpfProgram = filter
        .try_into()
        .map_err(|e| RuntimeError::Seccomp(format!("compile bpf: {e}")))?;

    seccompiler::apply_filter(&program)
        .map_err(|e| RuntimeError::Seccomp(format!("apply filter: {e}")))?;

    Ok(())
}

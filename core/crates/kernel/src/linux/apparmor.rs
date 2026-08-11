//! AppArmor `change_onexec`
//!
//! The profile is parameterised by sandbox UUID so each request gets its own
//! `/tmp/sandbox-<uuid>` rule. The profile must already be loaded into the
//! kernel (`apparmor_parser -r /etc/apparmor.d/...`).
//!
//! The `changeprofile <profile>` line is formatted in the **parent**;
//! `write_attr_exec` only opens, writes and closes via raw syscalls so the
//! fork child never allocates.

use crate::error::{SandboxError, SandboxResult};
use std::ffi::{CStr, CString};

/// Build the `changeprofile` command line in the parent (allocation allowed).
pub fn change_command(profile_name: &str) -> SandboxResult<CString> {
    CString::new(format!("changeprofile {profile_name}\n"))
        .map_err(|e| SandboxError::AppArmor(format!("changeprofile cmd: {e}")))
}

/// Stage the prebuilt command on the next `execve`. Raw syscalls only.
pub fn write_attr_exec(cmd: &CStr) -> SandboxResult<()> {
    // SAFETY: open/write/close are stable syscalls; the path is a static
    // NUL-terminated byte string.
    let fd = unsafe {
        libc::open(
            b"/proc/self/attr/exec\0".as_ptr() as *const libc::c_char,
            libc::O_WRONLY,
        )
    };
    if fd < 0 {
        return Err(SandboxError::AppArmor("open exec".into()));
    }
    // SAFETY: cmd is NUL-terminated; write is a stable syscall.
    let n = unsafe {
        libc::write(
            fd,
            cmd.as_ptr() as *const libc::c_void,
            cmd.to_bytes().len(),
        )
    };
    unsafe { libc::close(fd) };
    if n < 0 {
        return Err(SandboxError::AppArmor("write exec".into()));
    }
    Ok(())
}

/// Returns true if AppArmor is enforcing on the host. Checked once per
/// engine lifetime — the kernel's securityfs does not come and go while the
/// process runs, and a stat() per launch is wasted work in the hot path.
pub fn is_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| std::path::Path::new("/sys/kernel/security/apparmor").exists())
}

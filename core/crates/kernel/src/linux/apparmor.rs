//! AppArmor `change_onexec`
//!
//! The profile is parameterised by sandbox UUID so each request gets its own
//! `/tmp/sandbox-<uuid>` rule. The profile must already be loaded into the
//! kernel (`apparmor_parser -r /etc/apparmor.d/...`).

use crate::error::{SandboxError, SandboxResult};
use std::io::Write;

/// Stage the profile to apply on the next `execve`.
pub fn change_onexec(profile_name: &str) -> SandboxResult<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open("/proc/self/attr/exec")
        .map_err(|e| SandboxError::AppArmor(format!("open exec: {e}")))?;
    let cmd = format!("changeprofile {profile_name}\n");
    f.write_all(cmd.as_bytes())
        .map_err(|e| SandboxError::AppArmor(format!("write: {e}")))?;
    Ok(())
}

/// returns true if AppArmor is enforcing on the host.
pub fn is_available() -> bool {
    std::path::Path::new("/sys/kernel/security/apparmor").exists()
}

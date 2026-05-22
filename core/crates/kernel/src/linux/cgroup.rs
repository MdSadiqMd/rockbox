//! Cgroup v2 + `cgroup.kill` (FIX PERF-10).
//!
//! We use direct sysfs writes instead of `cgroups-rs` because v2 is simple
//! enough that adding the dependency surface isn't worth it, and we want
//! atomic teardown via `cgroup.kill` (kernel 5.14+) which the crate didn't
//! expose on older versions.

use crate::error::{SandboxError, SandboxResult};
use crate::spec::ResourceLimits;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

#[derive(Debug, Clone, Copy)]
pub struct CgroupConfig {
    pub limits: ResourceLimits,
}

#[derive(Debug)]
pub struct Cgroup {
    path: PathBuf,
    dirfd: OwnedFd,
}

impl Cgroup {
    /// Create a fresh cgroup directory under [`CGROUP_ROOT`]. Returns an
    /// `O_PATH | O_DIRECTORY` fd suitable for `clone3 + CLONE_INTO_CGROUP`.
    pub fn create(name: &str, cfg: CgroupConfig) -> SandboxResult<Self> {
        let path = Path::new(CGROUP_ROOT).join(name);
        std::fs::create_dir_all(&path).map_err(|e| io_cg("create_dir", e))?;
        write_in(&path, "memory.max", &cfg.limits.memory_bytes.to_string())?;
        write_in(&path, "pids.max", &cfg.limits.pids_max.to_string())?;
        write_in(
            &path,
            "cpu.max",
            &format!("{} {}", cfg.limits.cpu_quota_us, cfg.limits.cpu_period_us),
        )?;
        // We do NOT enable subtree_control here: cgroup v2 forbids placing
        // processes in a domain cgroup with controllers delegated to children.
        // The parent cgroup (above this one) should have already enabled
        // subtree_control via the deployment's init container.
        let dirfd = open_path_dir(&path)?;
        Ok(Self { path, dirfd })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn fd(&self) -> RawFd {
        self.dirfd.as_raw_fd()
    }

    /// Move an existing process into this cgroup. Used when CLONE_INTO_CGROUP
    /// isn't available (nested container setups). Equivalent to
    /// `echo $pid > cgroup.procs`.
    pub fn add_pid(&self, pid: i32) -> SandboxResult<()> {
        write_in(&self.path, "cgroup.procs", &pid.to_string())
    }

    /// Atomic SIGKILL of every task in the cgroup (FIX PERF-10).
    pub fn kill_all(&self) -> SandboxResult<()> {
        write_in(&self.path, "cgroup.kill", "1")
    }

    /// Reset peak counters so the cgroup can be re-used from the pool.
    pub fn reset_peaks(&self) -> SandboxResult<()> {
        // `memory.peak` and `pids.peak` accept "0" to reset on kernel >= 6.0.
        let _ = write_in(&self.path, "memory.peak", "0");
        let _ = write_in(&self.path, "pids.peak", "0");
        Ok(())
    }

    pub fn current_memory_peak(&self) -> SandboxResult<u64> {
        read_u64(&self.path, "memory.peak")
    }

    pub fn wait_empty(&self) -> SandboxResult<()> {
        // Poll `cgroup.events` for `populated 0`.
        let events = self.path.join("cgroup.events");
        for _ in 0..200 {
            let s = std::fs::read_to_string(&events).map_err(|e| io_cg("events", e))?;
            if s.lines().any(|l| l.trim() == "populated 0") {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Err(SandboxError::Cgroup {
            op: "wait_empty",
            source: std::io::Error::other("cgroup did not drain"),
        })
    }

    pub fn remove(&self) -> SandboxResult<()> {
        std::fs::remove_dir(&self.path).map_err(|e| io_cg("rmdir", e))
    }
}

fn write_in(root: &Path, file: &str, value: &str) -> SandboxResult<()> {
    let p = root.join(file);
    std::fs::write(&p, value).map_err(|e| SandboxError::Cgroup {
        op: leak_op(file),
        source: e,
    })
}

fn read_u64(root: &Path, file: &str) -> SandboxResult<u64> {
    let s = std::fs::read_to_string(root.join(file)).map_err(|e| SandboxError::Cgroup {
        op: leak_op(file),
        source: e,
    })?;
    s.trim().parse().map_err(|_| SandboxError::Cgroup {
        op: leak_op(file),
        source: std::io::Error::other("parse u64"),
    })
}

fn open_path_dir(path: &Path) -> SandboxResult<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| io_cg("open dirfd", e))?;
    Ok(f.into())
}

fn io_cg(op: &'static str, source: std::io::Error) -> SandboxError {
    SandboxError::Cgroup { op, source }
}

/// `op` lives forever — the strings here are interned via `Box::leak`. We
/// only ever call this with short cgroup-file names, so the leak is bounded.
fn leak_op(name: &str) -> &'static str {
    Box::leak(name.to_string().into_boxed_str())
}

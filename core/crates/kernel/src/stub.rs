//! No-op stubs for non-Linux dev machines. Every entrypoint returns
//! `SandboxError::Unsupported`. The shapes match the Linux module so callers
//! compile on macOS.

#![allow(dead_code)]

use crate::error::{SandboxError, SandboxResult};
use crate::spec::{ChildSpec, SeccompProfileId};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct BpfProfile;

#[derive(Debug, Default, Clone)]
pub struct SeccompResolver;
impl SeccompResolver {
    pub fn new() -> Self {
        Self
    }
    pub fn compile(&self, _p: SeccompProfileId) -> SandboxResult<BpfProfile> {
        Err(SandboxError::Unsupported)
    }
}

#[derive(Debug)]
pub struct CgroupConfig;

#[derive(Debug)]
pub struct Cgroup;
impl Cgroup {
    pub fn create(_name: &str, _cfg: CgroupConfig) -> SandboxResult<Self> {
        Err(SandboxError::Unsupported)
    }
    pub fn kill_all(&self) -> SandboxResult<()> {
        Err(SandboxError::Unsupported)
    }
    pub fn current_memory_peak(&self) -> SandboxResult<u64> {
        Ok(0)
    }
    pub fn reset_peaks(&self) -> SandboxResult<()> {
        Ok(())
    }
    pub fn wait_empty(&self) -> SandboxResult<()> {
        Ok(())
    }
    pub fn remove(&self) -> SandboxResult<()> {
        Ok(())
    }
    pub fn fd(&self) -> i32 {
        -1
    }
}

#[derive(Debug)]
pub struct MountPlan;

/// Shape-mirror of the Linux `ChildHandle` so dependent crates compile on
/// non-Linux dev hosts. Never constructed: `SandboxLauncher::launch`
/// always returns `Err(Unsupported)` here.
#[derive(Debug)]
pub struct ChildHandle {
    pub pid: i32,
    pub pidfd: std::os::fd::OwnedFd,
    pub cgroup: Cgroup,
    pub stdout: std::os::fd::OwnedFd,
    pub stderr: std::os::fd::OwnedFd,
    pub stdin_w: std::os::fd::OwnedFd,
    pub proto_fd: Option<std::os::fd::OwnedFd>,
    pub wall: Duration,
}
impl ChildHandle {
    pub fn pid(&self) -> i32 {
        self.pid
    }
}

#[derive(Debug)]
pub enum ChildExit {
    Normal {
        status: i32,
        memory_peak_mb: u64,
        cpu_time_ms: u64,
    },
    Signal {
        signo: i32,
    },
    Timeout,
    OomKilled,
    OutputCap,
}

#[derive(Debug)]
pub struct Drainer;
impl Drainer {
    pub fn new() -> SandboxResult<Self> {
        Err(SandboxError::Unsupported)
    }
    pub fn run<F1, F2>(
        &mut self,
        _cg: &Cgroup,
        _on_stdout: F1,
        _on_stderr: F2,
    ) -> SandboxResult<ChildExit>
    where
        F1: FnMut(&[u8]),
        F2: FnMut(&[u8]),
    {
        Err(SandboxError::Unsupported)
    }
    pub fn drain(&mut self, _: Duration) -> SandboxResult<ChildExit> {
        Err(SandboxError::Unsupported)
    }
}

#[derive(Debug)]
pub struct SandboxLauncher;
impl SandboxLauncher {
    pub fn new() -> SandboxResult<Self> {
        Err(SandboxError::Unsupported)
    }
    pub fn launch(&self, _spec: &ChildSpec) -> SandboxResult<ChildHandle> {
        Err(SandboxError::Unsupported)
    }
    pub fn make_drainer(&self, _h: ChildHandle, _cap: u64) -> SandboxResult<(Cgroup, Drainer)> {
        Err(SandboxError::Unsupported)
    }
    pub fn release_cgroup(&self, _cg: Cgroup) {}
}

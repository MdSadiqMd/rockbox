//! Linux implementation of the sandbox primitives.
//!
//! Each submodule implements one layer of the 10-layer stack defined in
//! `docs/nix_sandbox_flowchart.md`:
//!
//! - [`clone3`] — fused clone + pidfd + cgroup placement (PERF-06 + SEC-17)
//! - [`userns`] — uid_map + setgroups deny + sync pipe (SEC-02)
//! - [`mounts`] — pivot_root + tmpfs + bind mounts (SEC-10 + PERF-11)
//! - [`apparmor`] — `change_onexec` to per-uuid profile (SEC-09 + SEC-25)
//! - [`cgroup`] — v2 + `cgroup.kill` (PERF-10)
//! - [`drain`] — io_uring stdout/stderr/pidfd/timer (PERF-08)
//! - [`pidfd`] — pidfd_open helpers for callers that don't use clone3-PIDFD
//! - [`caps`] — PR_SET_NO_NEW_PRIVS, MDWE, capability dropping, rlimits
//! - [`seccomp`] — BPF profile and syscall filtering
//! - [`launcher`] — top-level orchestrator wiring all layers together
//!
//! Most modules are stand-alone testable; integration happens in [`launcher`].

pub mod apparmor;
pub mod cgroup;
pub mod clone3;
pub mod drain;
pub mod launcher;
pub mod mounts;
pub mod pidfd;
pub mod seccomp;
pub mod userns;

pub use apparmor::{change_onexec, is_available as apparmor_is_available};
pub use cgroup::{Cgroup, CgroupConfig, CGROUP_ROOT};
pub use clone3::{clone3, CloneRequest, Cloned};
pub use drain::{ChildExit, Drainer};
pub use launcher::{ChildHandle, SandboxLauncher};
pub use mounts::MountPlan;
pub use pidfd::{open as pidfd_open, send_signal as pidfd_send_signal};
pub use seccomp::{BpfProfile, SeccompResolver};
pub use userns::{
    child_wait_for_release, make_sync_pipe, release_child, write_id_maps, SyncPipe, NOBODY_GID,
    NOBODY_UID,
};

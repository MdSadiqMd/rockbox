//! Translate `settings.limits` + capability deltas into [`ResourceLimits`].

use kernel::spec::ResourceLimits;
use protocol::{Capability, Mode, Settings};

/// Long-lived workers (RL episodes, sessions) are latency-bound per step;
/// one-shot exec is throughput work. 4:1 keeps a stepping worker responsive
/// while a burst of exec sandboxes saturates the node (noisy-neighbour
/// bench: 24 attackers degraded RL batch p50 3.9× at equal weight).
pub const CPU_WEIGHT_INTERACTIVE: u32 = 400;
pub const CPU_WEIGHT_BATCH: u32 = 100;

pub fn resolve(settings: &Settings) -> ResourceLimits {
    let l = &settings.limits;

    // +large_fs raises fsize/tmpfs caps (already applied in tmpfs mount).
    let fsize_bytes = if settings.has_capability(Capability::LargeFs) {
        (l.fsize_mb.max(2048)).saturating_mul(1024 * 1024)
    } else {
        l.fsize_mb.saturating_mul(1024 * 1024)
    };

    let pids_max = if settings.has_capability(Capability::Subprocess) {
        l.pids_max.max(500)
    } else {
        l.pids_max
    };

    // CPU quota = cores × period. Default period 100ms.
    let cpu_period_us: u64 = 100_000;
    let cpu_quota_us = ((l.cpu_cores as f64) * cpu_period_us as f64).max(1000.0) as u64;

    let cpu_weight = match settings.mode {
        Mode::RlStep | Mode::RlEpisode | Mode::Session => CPU_WEIGHT_INTERACTIVE,
        Mode::Exec => CPU_WEIGHT_BATCH,
    };

    ResourceLimits {
        cpu_weight,
        memory_bytes: l.memory_mb.saturating_mul(1024 * 1024),
        cpu_quota_us,
        cpu_period_us,
        pids_max,
        nofile: l.fd_max,
        fsize_bytes,
        address_space_bytes: (l.memory_mb.saturating_add(128)).saturating_mul(1024 * 1024),
        stack_bytes: l.stack_mb.saturating_mul(1024 * 1024),
    }
}

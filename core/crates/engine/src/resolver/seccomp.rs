//! Pick the seccomp profile id for a request. Capabilities can promote a
//! profile (e.g. `+subprocess` adds an "execve+fork" overlay applied by the
//! `core` resolver before BPF compilation)

use kernel::spec::SeccompProfileId;
use protocol::{Capability, Mode, Settings};

pub fn resolve(settings: &Settings) -> SeccompProfileId {
    if matches!(settings.mode, Mode::RlStep | Mode::RlEpisode) {
        return SeccompProfileId::RlStep;
    }
    let _ = settings.has_capability(Capability::Subprocess); // overlay handled in core
    SeccompProfileId::for_language(settings.language)
}

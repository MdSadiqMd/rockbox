//! Pick the AppArmor profile name. We use parameterised profile names so each
//! sandbox UUID gets its own /tmp path rule

use protocol::{Capability, Mode, Settings};

pub fn resolve(settings: &Settings) -> String {
    let base = if matches!(settings.mode, Mode::RlStep | Mode::RlEpisode)
        || settings.has_capability(Capability::Gpu)
    {
        "sandbox-rl"
    } else {
        "sandbox-executor"
    };
    format!("{base}//&{}", settings.request_id)
}

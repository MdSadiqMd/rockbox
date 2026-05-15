//! Environment variable filtering + merging.
//!
//! Precedence (last wins):
//! 1. Runtime baseline (from [`crate::runtime_catalog`])
//! 2. User env (already SEC-28 prefix-filtered by Elixir SettingsValidator)
//! 3. Resolved secrets (vault-fetched, never logged)
//!
//! SEC-18 deny-list is a final post-filter: any key matching the deny list
//! is dropped regardless of where it came from.

use protocol::Settings;
use std::collections::BTreeMap;

const DENYLIST_PREFIXES: &[&str] = &[
    "LD_",
    "NODE_OPTIONS",
    "NODE_PATH",
    "PYTHONSTARTUP",
    "PYTHONPATH",
    "PYTHONHOME",
    "RUBYOPT",
    "RUBYLIB",
    "PERL5OPT",
    "PERL5LIB",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "GIT_",
    "SSH_",
    "HISTFILE",
    "BASH_ENV",
];

const DENYLIST_EXACT: &[&str] = &["PATH", "HOME", "SHELL", "USER", "LOGNAME"];

pub fn resolve(settings: &Settings, runtime_env: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();

    for (k, v) in runtime_env {
        out.insert((*k).into(), (*v).into());
    }
    for (k, v) in &settings.env {
        if is_safe(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in &settings.resolved_secrets {
        // Secrets bypass user-set deny rules but are themselves screened upstream.
        out.insert(k.clone(), v.clone());
    }

    // Deterministic seed propagation.
    if let Some(seed) = settings.determinism.seed {
        out.insert("PYTHONHASHSEED".into(), seed.to_string());
        out.insert("RUST_LOG_SEED".into(), seed.to_string());
        out.insert("NODE_RANDOM_SEED".into(), seed.to_string());
    }

    out.into_iter().collect()
}

fn is_safe(key: &str) -> bool {
    if DENYLIST_EXACT.contains(&key) {
        return false;
    }
    if DENYLIST_PREFIXES.iter().any(|p| key.starts_with(p)) {
        return false;
    }
    // ARCH-10 + SEC-28 — user keys must match `[A-Z][A-Z0-9_]*`.
    let bytes = key.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let head_ok = bytes[0].is_ascii_uppercase();
    let rest_ok = bytes[1..]
        .iter()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_');
    head_ok && rest_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_keys() {
        assert!(!is_safe("LD_PRELOAD"));
        assert!(!is_safe("NODE_OPTIONS"));
        assert!(!is_safe("PATH"));
        assert!(!is_safe("home")); // lowercase
        assert!(!is_safe(""));
    }

    #[test]
    fn accepts_user_keys() {
        assert!(is_safe("HF_HOME"));
        assert!(is_safe("MY_API_KEY"));
        assert!(is_safe("A1_B2"));
    }
}

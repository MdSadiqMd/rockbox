//! Top-level resolver: `Settings` → [`ChildSpec`].
//!
//! Layering: each sub-resolver owns ONE concern; this module just wires them
//! together. No business logic lives here — keep it that way.
//!
//! Per-language exec strategy:
//!
//! | Lang       | Pre-launch work       | argv inside sandbox                              |
//! |------------|-----------------------|--------------------------------------------------|
//! | Python     | none                  | `[python3, /sandbox/main.py]`                    |
//! | Typescript | none                  | `[node, /sandbox/index.ts]`                      |
//! | Go         | `go build -CGO=0`     | `[/sandbox/main]` (static binary, no toolchain)  |
//! | Rust       | `rustc -O` in engine  | `[/sandbox/main]` (binary written into workdir)  |
//! | C++        | `g++ -O2` in engine   | `[/sandbox/main]` (binary written into workdir)  |
//!
//! Compiled langs are pre-built **outside** the sandbox by the engine
//! process. The resulting binary lands in `work_dir/main`, which is then
//! visible at `/sandbox/main` once the mount-plan binds `work_dir → /sandbox`.
//! Production replaces this in-process compile with the privileged
//! `compile-helper` (see §22.1 of the flowchart spec); the contract — a
//! verified binary at `/sandbox/main` — is identical.

use crate::resolver::{apparmor, env, limits, mount, seccomp};
use crate::runtime_catalog;
use anyhow::{Result, anyhow, bail};
use kernel::spec::{ChildSpec, SandboxLayers};
use protocol::{Capability, Language, Settings};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug)]
pub struct Resolved {
    pub spec: ChildSpec,
    pub work_dir: PathBuf,
}

pub fn resolve(settings: &Settings, work_root: &PathBuf) -> Result<Resolved> {
    settings.validate().map_err(|e| anyhow!(e))?;

    let runtime_name = settings
        .runtime
        .as_deref()
        .unwrap_or_else(|| match settings.language {
            Language::Python => "python-base",
            Language::Typescript => "ts-modern",
            Language::Go => "go-std",
            Language::Rust => "rust-tokio",
            Language::Cpp => "cpp-modern",
        });
    let runtime = runtime_catalog::lookup(runtime_name)
        .ok_or_else(|| anyhow!("unknown runtime {runtime_name}"))?;

    if runtime.language != settings.language {
        bail!(
            "runtime {} is for {:?}, request says {:?}",
            runtime_name,
            runtime.language,
            settings.language
        );
    }

    let work_dir = work_root.join(&settings.request_id);
    std::fs::create_dir_all(&work_dir)?;
    for f in &settings.files {
        let target = work_dir.join(&f.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &f.content)?;
    }

    let env_pairs = env::resolve(settings, runtime.baseline_env);
    let limits = limits::resolve(settings);
    let seccomp_id = seccomp::resolve(settings);
    let apparmor_profile = apparmor::resolve(settings);
    let layers = SandboxLayers {
        trace_syscalls: settings.observability.trace_syscalls,
        enforce_w_xor_x: matches!(seccomp_id, kernel::spec::SeccompProfileId::NativeJit),
    };

    // Resolve runtime binary via $PATH (then canonicalized through symlinks
    // to a real `/nix/store/...` path) — see `RuntimeEntry::resolve_bin`.
    let bin_path = runtime
        .resolve_bin()
        .map_err(|e| anyhow!("runtime {runtime_name}: {e}"))?;

    // Pre-compile (Go / Rust / C++) BEFORE sandbox launch. Output binary
    // lands in `work_dir/main`, which the mount-plan surfaces at
    // `/sandbox/main`. The sandbox execs that binary directly — no compile
    // inside the sandbox, no toolchain needed at run time.
    //
    // Go produces statically-linked binaries for pure-Go code, so `/sandbox`
    // is the only mount the running binary needs.
    let argv = match settings.language {
        Language::Go | Language::Rust | Language::Cpp => {
            precompile(
                settings.language,
                &bin_path,
                &work_dir,
                &settings.entrypoint,
            )?;
            vec!["/sandbox/main".into()]
        }
        Language::Python | Language::Typescript => vec![
            bin_path.to_string_lossy().into_owned(),
            format!("/sandbox/{}", settings.entrypoint),
        ],
    };

    let mounts = mount::resolve(settings, &work_dir, &bin_path);

    if settings.has_capability(Capability::Gpu) && settings.gpu.count == 0 {
        bail!("+gpu capability without gpu.count > 0");
    }

    let spec = ChildSpec {
        request_id: settings.request_id.clone(),
        language: settings.language,
        mounts,
        argv,
        env: env_pairs,
        limits,
        seccomp_profile: seccomp_id,
        layers,
        apparmor_profile,
        binary_fd_path: None,
        wall_timeout: Duration::from_millis(settings.limits.wall_ms),
    };
    Ok(Resolved { spec, work_dir })
}

/// Compile a single-file program with the runtime's compiler. Synchronous —
/// the resolver is the only caller and runs on the request-handler task.
///
/// The output is `work_dir/main` (mode 0755) — already inside `work_dir`,
/// which the mount step bind-mounts at `/sandbox`. No extra mount entry
/// needed; the child execve's `/sandbox/main`.
///
/// **Go** produces a statically-linked binary for pure-Go code, so at run
/// time `/sandbox/main` needs no toolchain or `/nix/store` — execution is
/// as fast as any other native binary.
///
/// **GOCACHE** is shared across requests at `/tmp/rockbox-go-cache` so the
/// standard-library packages are compiled only once. First call: 3-30 s.
/// All subsequent calls for the same source: < 1 s (only the user code
/// needs to relink).
fn precompile(lang: Language, compiler: &Path, work_dir: &Path, entrypoint: &str) -> Result<()> {
    use std::process::Command;
    let src = work_dir.join(entrypoint);
    let out = work_dir.join("main");

    let mut cmd = Command::new(compiler);
    match lang {
        Language::Go => {
            // `go build -o output src.go` compiles a single-file package.
            // We clear GOFLAGS (which carries "-mod=mod" from the catalog)
            // because there is no go.mod here. GOCACHE is persistent so the
            // stdlib objects are cached across requests.
            let cache_dir = "/tmp/rockbox-go-cache";
            std::fs::create_dir_all(cache_dir).ok();
            cmd.args(["build", "-o"])
                .arg(&out)
                .arg(&src)
                .env("GOFLAGS", "")
                .env("GOCACHE", cache_dir)
                .env("GOPATH", work_dir.join(".gopath"))
                .env("CGO_ENABLED", "0");
        }
        Language::Rust => {
            cmd.args(["-O", "--edition", "2021"])
                .arg("-o")
                .arg(&out)
                .arg(&src);
        }
        Language::Cpp => {
            cmd.args(["-O2", "-std=c++23"])
                .arg("-o")
                .arg(&out)
                .arg(&src);
        }
        _ => bail!("precompile called for non-compiled language {:?}", lang),
    }

    let output = cmd
        .output()
        .map_err(|e| anyhow!("spawn {compiler:?}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "compile failed ({:?}): {stderr}",
            output.status.code().unwrap_or(-1)
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&out)?;
        let mut perm = meta.permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&out, perm)?;
    }
    Ok(())
}

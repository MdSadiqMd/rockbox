//! Top-level resolver: `Settings` → [`ChildSpec`]
//!
//! Layering: each sub-resolver owns ONE concern; this module just wires them
//! together. No business logic lives here — keep it that way
//!
//! Per-language exec strategy:
//!
//! | Lang       | Pre-launch work       | argv inside sandbox                              |
//! |------------|-----------------------|--------------------------------------------------|
//! | Python     | none                  | `[python3, -S, /sandbox/main.py]`                |
//! | Typescript | amaro strip → main.js | `[node, /sandbox/main.js]`                       |
//! | Go         | `go build -ldflags=-s -w` | `[/sandbox/main]` (static binary, no toolchain)  |
//! | Rust       | `rustc -O` + mold   | `[/sandbox/main]` (binary written into workdir)  |
//! | C++        | `g++ -O2` + mold    | `[/sandbox/main]` (binary written into workdir)  |
//!
//! The TS strip runs once per distinct program — the work dir is
//! content-addressed — so warm requests exec plain JS instead of paying
//! node's per-run amaro type-stripping (~25 ms)
//!
//! Compiled langs are pre-built **outside** the sandbox by the engine
//! process. The resulting binary lands in `work_dir/main`, which is then
//! visible at `/sandbox/main` once the mount-plan binds `work_dir → /sandbox`
//! Production replaces this in-process compile with the privileged
//! `compile-helper` the contract — a
//! verified binary at `/sandbox/main` — is identical

use crate::resolver::{apparmor, env, limits, mount, seccomp};
use crate::runtime_catalog::{self, RuntimeEntry};
use anyhow::{Result, anyhow, bail};
use cache::{BinaryCache, BinaryKey};
use dashmap::DashMap;
use kernel::spec::{ChildSpec, SandboxLayers};
use protocol::settings::{DnsMode, MountMode};
use protocol::{Capability, Language, Mode, NetworkTier, OutputAction, Settings};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Resolved {
    pub spec: ChildSpec,
    pub work_dir: PathBuf,
}

static RESOLVED_CACHE: LazyLock<DashMap<[u8; 32], Resolved>> =
    LazyLock::new(|| DashMap::with_capacity(256));

fn resolved_cache() -> &'static DashMap<[u8; 32], Resolved> {
    &RESOLVED_CACHE
}

/// SOTA Loop20: structured hash, zero `format!("{:?}")` allocs. The old key
/// built SIX debug strings per resolve (each walks + allocates: language,
/// mode, limits, capabilities, network, filesystem, env, determinism,
/// observability) plus a 64-char hex `String` key. The new key feeds
/// discriminants + LE ints + length-prefixed str bytes straight into blake3
/// and uses the raw 32B digest as the `DashMap` key — no hex, no `String`.
/// Same field set as before (no semantic change); `BTreeMap`/vec order is
/// iteration order in both (pipeline output is deterministic).
fn resolved_cache_key(
    settings: &Settings,
    input_digest: &[u8; 32],
    runtime_name: &str,
) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(input_digest);
    h.update(&(runtime_name.len() as u64).to_le_bytes());
    h.update(runtime_name.as_bytes());
    h.update(&[match settings.language {
        Language::Python => 0,
        Language::Typescript => 1,
        Language::Go => 2,
        Language::Rust => 3,
        Language::Cpp => 4,
    }]);
    h.update(&[match settings.mode {
        Mode::Exec => 0,
        Mode::Session => 1,
        Mode::RlStep => 2,
        Mode::RlEpisode => 3,
    }]);
    // Limits (all numeric fields LE; Options as tag + payload).
    let l = &settings.limits;
    h.update(&l.wall_ms.to_le_bytes());
    match l.cpu_ms {
        Some(v) => {
            h.update(&[1]);
            h.update(&v.to_le_bytes());
        }
        None => {
            h.update(&[0]);
        }
    }
    h.update(&l.compile_ms.to_le_bytes());
    h.update(&l.memory_mb.to_le_bytes());
    h.update(&l.cpu_cores.to_bits().to_le_bytes());
    h.update(&l.pids_max.to_le_bytes());
    h.update(&l.fd_max.to_le_bytes());
    h.update(&l.fsize_mb.to_le_bytes());
    h.update(&l.tmpfs_mb.to_le_bytes());
    h.update(&l.stack_mb.to_le_bytes());
    h.update(&l.output_bytes.to_le_bytes());
    h.update(&[match l.output_action {
        OutputAction::Truncate => 0,
        OutputAction::KillOnOverflow => 1,
    }]);
    match l.step_ms {
        Some(v) => {
            h.update(&[1]);
            h.update(&v.to_le_bytes());
        }
        None => {
            h.update(&[0]);
        }
    }
    match l.episode_ms {
        Some(v) => {
            h.update(&[1]);
            h.update(&v.to_le_bytes());
        }
        None => {
            h.update(&[0]);
        }
    }
    // Capabilities in pipeline order.
    h.update(&(settings.capabilities.len() as u64).to_le_bytes());
    for c in &settings.capabilities {
        h.update(&[match c {
            Capability::Concurrency => 0,
            Capability::Subprocess => 1,
            Capability::LargeFs => 2,
            Capability::PersistentSession => 3,
            Capability::Gpu => 4,
            Capability::Install => 5,
            Capability::RawSockets => 6,
        }]);
    }
    // Network.
    let n = &settings.network;
    h.update(&[match n.tier {
        NetworkTier::None => 0,
        NetworkTier::Loopback => 1,
        NetworkTier::EgressAllowlist => 2,
        NetworkTier::EgressOpen => 3,
    }]);
    match &n.allowlist {
        Some(list) => {
            h.update(&[1]);
            h.update(&(list.len() as u64).to_le_bytes());
            for v in list {
                h.update(&(v.len() as u64).to_le_bytes());
                h.update(v.as_bytes());
            }
        }
        None => {
            h.update(&[0]);
        }
    }
    match n.bandwidth_mbps {
        Some(v) => {
            h.update(&[1]);
            h.update(&v.to_le_bytes());
        }
        None => {
            h.update(&[0]);
        }
    }
    match n.max_conns {
        Some(v) => {
            h.update(&[1]);
            h.update(&v.to_le_bytes());
        }
        None => {
            h.update(&[0]);
        }
    }
    h.update(&[u8::from(n.block_metadata)]);
    h.update(&[match n.dns.mode {
        DnsMode::Proxied => 0,
        DnsMode::None => 1,
    }]);
    h.update(&n.dns.cache_s.to_le_bytes());
    // Filesystem.
    let f = &settings.filesystem;
    h.update(&(f.writable_paths.len() as u64).to_le_bytes());
    for v in &f.writable_paths {
        h.update(&(v.len() as u64).to_le_bytes());
        h.update(v.as_bytes());
    }
    h.update(&(f.mounts.len() as u64).to_le_bytes());
    for m in &f.mounts {
        h.update(&(m.source.len() as u64).to_le_bytes());
        h.update(m.source.as_bytes());
        h.update(&(m.target.len() as u64).to_le_bytes());
        h.update(m.target.as_bytes());
        h.update(&[match m.mode {
            MountMode::Ro => 0,
            MountMode::Rw => 1,
        }]);
    }
    h.update(&[u8::from(f.preserve_on_exit)]);
    // Env (BTreeMap iterates sorted — canonical).
    h.update(&(settings.env.len() as u64).to_le_bytes());
    for (k, v) in &settings.env {
        h.update(&(k.len() as u64).to_le_bytes());
        h.update(k.as_bytes());
        h.update(&(v.len() as u64).to_le_bytes());
        h.update(v.as_bytes());
    }
    // Determinism.
    let d = &settings.determinism;
    match d.seed {
        Some(v) => {
            h.update(&[1]);
            h.update(&v.to_le_bytes());
        }
        None => {
            h.update(&[0]);
        }
    }
    h.update(&[u8::from(d.deterministic_time)]);
    h.update(&[u8::from(d.freeze_random)]);
    match &d.pin_runtime_hash {
        Some(v) => {
            h.update(&[1]);
            h.update(&(v.len() as u64).to_le_bytes());
            h.update(v.as_bytes());
        }
        None => {
            h.update(&[0]);
        }
    }
    // Observability.
    let o = &settings.observability;
    h.update(&[u8::from(o.capture_metrics)]);
    h.update(&[u8::from(o.trace_syscalls)]);
    match &o.webhook {
        Some(w) => {
            h.update(&[1]);
            h.update(&(w.url.len() as u64).to_le_bytes());
            h.update(w.url.as_bytes());
            h.update(&(w.events.len() as u64).to_le_bytes());
            for e in &w.events {
                h.update(&(e.len() as u64).to_le_bytes());
                h.update(e.as_bytes());
            }
            match &w.hmac_key {
                Some(k) => {
                    h.update(&[1]);
                    h.update(&(k.len() as u64).to_le_bytes());
                    h.update(k.as_bytes());
                }
                None => {
                    h.update(&[0]);
                }
            }
        }
        None => {
            h.update(&[0]);
        }
    }
    *h.finalize().as_bytes()
}

pub fn resolve(
    settings: &Settings,
    work_root: &Path,
    binary_cache: Option<&BinaryCache>,
) -> Result<Resolved> {
    settings.validate().map_err(|e| anyhow!(e))?;

    let runtime_name = settings.runtime.as_deref().unwrap_or_else(|| {
        runtime_catalog::default_for(settings.language)
            .name
            .as_str()
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

    // Work dirs are keyed by the content digest of the program rather than
    // the unique request_id: identical programs (repeated RL steps, retries,
    // deduped CI runs) share one directory, so the per-request file writes
    // and mkdir chains only happen once per distinct program.
    // Work dirs are keyed by the content digest of the program rather than
    // the unique request_id: identical programs (repeated RL steps, retries,
    // deduped CI runs) share one directory, so the per-request file writes
    // and mkdir chains only happen once per distinct program.
    // SOTA Loop20: cache lookup BEFORE `work_dir` hex+join — hits (the hot
    // path for repeated programs) skip the 64-char hex `String` + `PathBuf`
    // alloc entirely; the cached `Resolved` already carries `work_dir`.
    let t_digest = std::time::Instant::now();
    let input_digest = files_digest(settings);
    let digest_ms = t_digest.elapsed().as_micros() as u64;
    let cache_key = resolved_cache_key(settings, &input_digest, runtime_name);
    if let Some(cached) = resolved_cache().get(&cache_key) {
        let mut hit = cached.clone();
        hit.spec.request_id = settings.request_id.clone();
        hit.spec.wall_timeout = Duration::from_millis(settings.limits.wall_ms);
        tracing::debug!(cache_hit = true, "resolved_cache_hit");
        return Ok(hit);
    }
    let work_dir = work_root.join(hex_digest(&input_digest));
    let t_mat = std::time::Instant::now();
    materialize_work_dir(settings, &work_dir, &input_digest)?;
    let mat_ms = t_mat.elapsed().as_micros() as u64;

    let t_env = std::time::Instant::now();
    let baseline_env: Vec<(&str, &str)> = runtime
        .baseline_env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let env_pairs = env::resolve(settings, &baseline_env);
    let limits = limits::resolve(settings);
    let seccomp_id = seccomp::resolve(settings);
    let apparmor_profile = apparmor::resolve(settings);
    let env_ms = t_env.elapsed().as_micros() as u64;
    let layers = SandboxLayers {
        trace_syscalls: settings.observability.trace_syscalls,
        enforce_w_xor_x: matches!(seccomp_id, kernel::spec::SeccompProfileId::NativeJit),
    };

    // Resolve runtime binary via $PATH (then canonicalized through symlinks
    // to a real `/nix/store/...` path) — see `RuntimeEntry::resolve_bin`.
    // Cached per runtime for the engine's lifetime.
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
    //
    // The compile result is content-addressed in the BinaryCache: warm hits
    // skip the compiler entirely and execve the verified cached fd.
    let t_bin = std::time::Instant::now();
    let mut binary_fd_path = None;
    let mut compile_ms: Option<u64> = None;
    let mut cache_hit = false;
    let argv = match settings.language {
        Language::Go => {
            // Single-file pure-Go programs run under the yaegi interpreter
            // INSIDE the sandbox (no compiler involved, ~10ms). Everything
            // else compiles to a static binary through the content-addressed
            // BinaryCache: warm hits execve the verified cached fd without
            // ever invoking the compiler.
            if settings.files.len() == 1
                && !settings.files.iter().any(|f| f.path.contains(".."))
                && Path::new("/usr/local/bin/yaegi").exists()
                && !settings.files[0]
                    .content
                    .windows(9)
                    .any(|w| w == b"import \"C\"")
                && !settings.files[0].content.windows(6).any(|w| w == b"unsafe")
            {
                vec![
                    "/usr/local/bin/yaegi".into(),
                    "run".into(),
                    format!("/sandbox/{}", settings.entrypoint),
                ]
            } else {
                let key = compile_key(&runtime, &input_digest);
                let t_lookup = std::time::Instant::now();
                let lookup = binary_cache.and_then(|c| c.lookup(&key).ok());
                let lookup_ms = t_lookup.elapsed().as_micros() as u64;
                match lookup {
                    Some(handle) => {
                        binary_fd_path = Some(handle.fd_path().to_string());
                        cache_hit = true;
                        tracing::debug!(%cache_hit, lookup_ms, "binary_cache_lookup");
                    }
                    None => {
                        let t_pre = std::time::Instant::now();
                        precompile(settings.language, bin_path, &work_dir, &settings.entrypoint)?;
                        compile_ms = Some(t_pre.elapsed().as_micros() as u64);
                        if let Some(c) = binary_cache {
                            let out = work_dir.join("main");
                            if out.exists() {
                                let t_store = std::time::Instant::now();
                                let _ = c.store(&key, &out);
                                tracing::debug!(
                                    store_ms = t_store.elapsed().as_micros() as u64,
                                    "binary_cache_store"
                                );
                            }
                        }
                    }
                }
                vec!["/sandbox/main".into()]
            }
        }
        Language::Rust | Language::Cpp => {
            // Content-addressed BinaryCache: warm hits execve the verified
            // cached fd (the compiler never runs); misses compile once with
            // the toolchain and store the artifact for later requests.
            let key = compile_key(&runtime, &input_digest);
            let t_lookup = std::time::Instant::now();
            let lookup = binary_cache.and_then(|c| c.lookup(&key).ok());
            let lookup_ms = t_lookup.elapsed().as_micros() as u64;
            match lookup {
                Some(handle) => {
                    binary_fd_path = Some(handle.fd_path().to_string());
                    cache_hit = true;
                    tracing::debug!(%cache_hit, lookup_ms, "binary_cache_lookup");
                }
                None => {
                    let t_pre = std::time::Instant::now();
                    precompile(settings.language, bin_path, &work_dir, &settings.entrypoint)?;
                    compile_ms = Some(t_pre.elapsed().as_micros() as u64);
                    if let Some(c) = binary_cache {
                        // Populate the shared cache so the next identical
                        // request skips the compiler (best-effort: the cache
                        // dir may be unwritable without the privileged
                        // compiler helper).
                        let out = work_dir.join("main");
                        if out.exists() {
                            let t_store = std::time::Instant::now();
                            let _ = c.store(&key, &out);
                            tracing::debug!(
                                store_ms = t_store.elapsed().as_micros() as u64,
                                "binary_cache_store"
                            );
                        }
                    }
                }
            }
            vec!["/sandbox/main".into()]
        }
        Language::Python | Language::Typescript => {
            let mut argv = vec![bin_path.to_string_lossy().into_owned()];
            argv.extend(runtime.interpreter_flags.iter().map(|f| f.to_string()));
            let entry = settings.entrypoint.clone();
            let entry_path = work_dir.join(&entry);
            // TypeScript is run through node's built-in amaro type-stripper.
            // Stripping costs ~25ms per run, so pre-strip once per program and
            // exec plain JS on every subsequent request (the work dir is
            // content-addressed by program digest, so the artifact persists).
            if settings.language == Language::Typescript {
                if runtime.executable == "bun" {
                    // Bun strips TypeScript natively at load time — no
                    // pre-strip pass, no cached JS artifact, and ~0.6ms
                    // interpreter boot vs node's ~13ms.
                    argv.push(format!("/sandbox/{entry}"));
                } else if entry_path.extension().is_some_and(|e| e == "ts") {
                    transpile_typescript(bin_path, &work_dir, &entry)?;
                    argv.push("/sandbox/main.js".into());
                } else {
                    argv.push(format!("/sandbox/{entry}"));
                }
            } else {
                argv.push(format!("/sandbox/{entry}"));
            }
            argv
        }
    };

    let t_mount = std::time::Instant::now();
    let mounts = mount::resolve(settings, &work_dir, bin_path);
    let mount_ms = t_mount.elapsed().as_micros() as u64;
    let bin_total_ms = t_bin.elapsed().as_micros() as u64;
    let _ = std::fs::write(
        "/tmp/resolve_timing.log",
        format!(
            "RESOLVE_TIMING digest_us={} mat_us={} env_us={} compile_us={} mount_us={} total_us={} hit={}\n",
            digest_ms,
            mat_ms,
            env_ms,
            compile_ms.unwrap_or(0),
            mount_ms,
            bin_total_ms,
            cache_hit
        ),
    );

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
        binary_fd_path,
        wall_timeout: Duration::from_millis(settings.limits.wall_ms),
        protocol_fd: matches!(settings.mode, Mode::RlStep | Mode::RlEpisode),
    };
    let resolved = Resolved { spec, work_dir };
    resolved_cache().insert(cache_key, resolved.clone());
    Ok(resolved)
}

/// Content digest over entrypoint + every file (path + bytes). Identical
/// programs — including across users, since user code is untrusted and
/// content-addressing is purely a caching concern — map to one work dir.
fn files_digest(settings: &Settings) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(settings.entrypoint.as_bytes());
    h.update(&[0]);
    for f in &settings.files {
        h.update(f.path.as_bytes());
        h.update(&[0]);
        h.update(&f.content);
        h.update(&[0]);
    }
    *h.finalize().as_bytes()
}

fn hex_digest(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// Write the program files into the (content-addressed) work dir, skipping
/// the writes when the dir already holds the identical program. The marker
/// file makes the check O(read) instead of stat-per-file.
fn materialize_work_dir(settings: &Settings, work_dir: &Path, digest: &[u8; 32]) -> Result<()> {
    let marker = work_dir.join(".rockbox-input-hash");
    if std::fs::read(&marker)
        .map(|m| m.as_slice() == digest)
        .unwrap_or(false)
    {
        return Ok(());
    }
    std::fs::create_dir_all(work_dir)?;
    for f in &settings.files {
        let target = work_dir.join(&f.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &f.content)?;
    }
    std::fs::write(&marker, digest)?;
    Ok(())
}

/// Cache key for a compile. The frozen `flake.lock` pins the toolchain, so
/// code + lock + arch uniquely identify the artifact. `digest` is the
/// already-computed `files_digest` of the program.
fn compile_key(runtime: &RuntimeEntry, digest: &[u8; 32]) -> BinaryKey {
    BinaryKey::from_parts(
        digest,
        &runtime.executable,
        std::env::consts::ARCH,
        runtime.flake_lock_bytes(),
    )
}

/// Strip TypeScript types from the entrypoint and write the plain-JS result
/// to `work_dir/main.js`. Runs once per distinct program: the work dir is
/// content-addressed, so the transpiled artifact persists across requests
/// and the child just execs `node /sandbox/main.js` (~13 ms) instead of
/// paying node's amaro type-stripping on every run (~37 ms).
///
/// The strip runs through node's own `node:module.stripTypeScriptTypes`, so
/// the output is byte-for-byte what node's native `.ts` loader would run —
/// no dialect drift between the transpile and the runtime.
fn transpile_typescript(node: &Path, work_dir: &Path, entrypoint: &str) -> Result<()> {
    use std::process::{Command, Stdio};

    let src = work_dir.join(entrypoint);
    let out = work_dir.join("main.js");
    if out.exists() {
        return Ok(());
    }

    let script = r#"const { stripTypeScriptTypes } = require("node:module");
const fs = require("fs");
const src = process.argv[1];
fs.writeFileSync(process.argv[2], stripTypeScriptTypes(fs.readFileSync(src, "utf8")));"#;

    let child = Command::new(node)
        .args(["-e", script, "--no-warnings"])
        .arg(&src)
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("spawn node (transpile): {e}"))?;
    let output = child
        .wait_with_output()
        .map_err(|e| anyhow!("wait node (transpile): {e}"))?;
    if !output.status.success() {
        // Clear the stale artifact so the next request retries the transpile
        // instead of running an empty main.js.
        let _ = std::fs::remove_file(&out);
        bail!(
            "typescript transpile failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Compile a single-file program with the runtime's compiler. Synchronous —/// the resolver is the only caller and runs on the request-handler task.
///
/// The output is `work_dir/main` (mode 0755) — already inside `work_dir`,
/// which the mount step bind-mounts at `/sandbox`. No extra mount entry
/// needed; the child execve's `/sandbox/main`.
///
/// **Go** produces a statically-linked binary for pure-Go code, so at run
/// time `/sandbox/main` needs no toolchain or `/nix/store` — execution is
/// as fast as any other native binary.
///
/// **GOCACHE** is shared across requests at `/var/cache/sandbox/go-cache` so
/// the standard-library packages are compiled only once. First call on a
/// fresh volume: 3-30 s. All subsequent calls for the same source: < 1 s
/// (only the user code needs to relink).
///
/// Rust and C++ link through `mold` when available (much faster than GNU
/// `ld` for aarch64 symbol merging); a compile that fails because mold is
/// missing is retried without it.
fn precompile(lang: Language, compiler: &Path, work_dir: &Path, entrypoint: &str) -> Result<()> {
    let src = work_dir.join(entrypoint);
    let out = work_dir.join("main");

    let (args, envs, linker_flag): (
        Vec<std::ffi::OsString>,
        Vec<(String, String)>,
        Option<&[&str]>,
    ) = match lang {
        Language::Go => {
            // `go build -o output src.go` compiles a single-file package.
            // We clear GOFLAGS (which carries "-mod=mod" from the catalog)
            // because there is no go.mod here. GOCACHE is persistent so the
            // stdlib objects are cached across requests. `-s -w` skips
            // DWARF/symbol emission, `-buildvcs=false` skips vcs stamping.
            let cache_dir = "/var/cache/sandbox/go-cache";
            std::fs::create_dir_all(cache_dir).ok();
            (
                vec![
                    "build".into(),
                    "-buildvcs=false".into(),
                    "-ldflags=-s -w".into(),
                    "-o".into(),
                    out.as_os_str().to_os_string(),
                    src.as_os_str().to_os_string(),
                ],
                vec![
                    ("GOFLAGS".into(), String::new()),
                    ("GOCACHE".into(), cache_dir.into()),
                    (
                        "GOPATH".into(),
                        work_dir.join(".gopath").to_string_lossy().into_owned(),
                    ),
                    ("CGO_ENABLED".into(), "0".into()),
                    ("GOTOOLCHAIN".into(), "local".into()),
                ],
                None,
            )
        }
        Language::Rust => (
            vec![
                "-C".into(),
                "opt-level=0".into(),
                "--edition".into(),
                "2021".into(),
                "-o".into(),
                out.as_os_str().to_os_string(),
                src.as_os_str().to_os_string(),
            ],
            Vec::new(),
            Some(&["-C", "link-arg=-fuse-ld=mold"]),
        ),
        Language::Cpp => {
            // mold (and lld) drop the toolchain rpath that gcc's specs
            // emit for the GNU linker, so the sandboxed binary can't find
            // libstdc++ at runtime. Re-add it explicitly from the
            // toolchain's own file-name lookups (memoized per process).
            // `-O0` is fastest for tiny programs; `-O2` adds passes with
            // no benefit for hello-world.
            let mut args = vec![
                "-O0".into(),
                "-std=c++23".into(),
                "-o".into(),
                out.as_os_str().to_os_string(),
                src.as_os_str().to_os_string(),
            ];
            for dir in cpp_toolchain_lib_dirs(compiler) {
                args.push(format!("-Wl,-rpath,{dir}").into());
            }
            (args, Vec::new(), Some(&["-fuse-ld=mold"]))
        }
        _ => bail!("precompile called for non-compiled language {:?}", lang),
    };

    let output = run_compile(compiler, &args, &envs, linker_flag)?;
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

/// (e.g. images without mold installed).
fn run_compile(
    compiler: &Path,
    args: &[std::ffi::OsString],
    envs: &[(String, String)],
    linker_flag: Option<&[&str]>,
) -> Result<std::process::Output> {
    let run = |flag: Option<&[&str]>| {
        let mut cmd = std::process::Command::new(compiler);
        cmd.args(args);
        if let Some(f) = flag {
            cmd.args(f);
        }
        for (k, v) in envs {
            cmd.env(k, v);
        }
        cmd.output()
    };

    let with_flag = run(linker_flag);
    if linker_flag.is_some() {
        if let Ok(output) = &with_flag {
            if output.status.success() {
                return Ok(output.clone());
            }
            return run(None).map_err(|e| anyhow!("spawn {compiler:?}: {e}"));
        }
    }
    with_flag.map_err(|e| anyhow!("spawn {compiler:?}: {e}"))
}

/// Library directories a C++ binary needs at runtime (libstdc++, libgcc_s,
/// libc) as reported by the compiler itself. Memoized: the toolchain never
/// moves within a running engine.
fn cpp_toolchain_lib_dirs(compiler: &Path) -> &'static [String] {
    use std::sync::OnceLock;
    static DIRS: OnceLock<Vec<String>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut dirs: Vec<String> = Vec::new();
        for probe in ["libstdc++.so.6", "libgcc_s.so.1", "libc.so.6"] {
            let Ok(output) = std::process::Command::new(compiler)
                .arg(format!("-print-file-name={probe}"))
                .output()
            else {
                continue;
            };
            if !output.status.success() {
                continue;
            }
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() || stdout == probe {
                continue;
            }
            if let Some(dir) = Path::new(&stdout).parent() {
                let dir = dir.to_string_lossy().into_owned();
                if !dirs.contains(&dir) {
                    dirs.push(dir);
                }
            }
        }
        dirs
    })
}

#[cfg(test)]
mod cache_key_tests {
    use super::*;
    use protocol::settings::{
        CostSettings, Determinism, DnsSettings, FileEntry, FilesystemSettings, GpuSettings,
        LifecyclePolicy, LifecycleSettings, Limits, NetworkSettings, ObservabilitySettings,
        OutputAction, OutputSettings,
    };

    fn minimal_settings() -> Settings {
        Settings {
            schema: "v1".to_string(),
            request_id: "req_test".to_string(),
            labels: Default::default(),
            language: Language::Python,
            runtime: Some("python3".to_string()),
            files: vec![],
            entrypoint: "main.py".to_string(),
            mode: Mode::Exec,
            session_id: None,
            limits: Limits {
                wall_ms: 5000,
                cpu_ms: None,
                compile_ms: 30_000,
                memory_mb: 512,
                cpu_cores: 1.0,
                pids_max: 64,
                fd_max: 1024,
                fsize_mb: 100,
                tmpfs_mb: 100,
                stack_mb: 8,
                output_bytes: 1_000_000,
                output_action: OutputAction::Truncate,
                step_ms: None,
                episode_ms: None,
            },
            lifecycle: LifecycleSettings {
                idle_ttl_s: 0,
                max_lifetime_s: 3600,
                auto_destroy: true,
                keep_alive_on_error: false,
                restart_policy: LifecyclePolicy::None,
            },
            capabilities: vec![],
            network: NetworkSettings {
                tier: NetworkTier::None,
                allowlist: None,
                bandwidth_mbps: None,
                max_conns: None,
                block_metadata: true,
                dns: DnsSettings {
                    mode: DnsMode::Proxied,
                    cache_s: 60,
                },
            },
            filesystem: FilesystemSettings {
                writable_paths: vec![],
                mounts: vec![],
                preserve_on_exit: false,
            },
            env: Default::default(),
            resolved_secrets: Default::default(),
            stdin: None,
            determinism: Determinism::default(),
            gpu: GpuSettings::default(),
            output: OutputSettings {
                stream: false,
                binary_safe: true,
                merge_streams: false,
                include_metrics: false,
            },
            observability: ObservabilitySettings {
                capture_metrics: false,
                trace_syscalls: false,
                webhook: None,
            },
            cost: CostSettings::default(),
        }
    }

    fn key_of(s: &Settings) -> [u8; 32] {
        let digest = files_digest(s);
        resolved_cache_key(s, &digest, "python3")
    }

    #[test]
    fn same_settings_same_key() {
        assert_eq!(key_of(&minimal_settings()), key_of(&minimal_settings()));
    }

    #[test]
    fn request_id_does_not_affect_key() {
        // Per-request patching on cache hit (request_id, wall_timeout) is
        // only sound because the key ignores per-request fields.
        let mut a = minimal_settings();
        let mut b = minimal_settings();
        a.request_id = "req_aaa".to_string();
        b.request_id = "req_bbb".to_string();
        assert_eq!(key_of(&a), key_of(&b));
    }

    #[test]
    fn language_mode_env_determinism_affect_key() {
        let base = key_of(&minimal_settings());
        let mut s = minimal_settings();
        s.language = Language::Go;
        assert_ne!(base, key_of(&s));
        let mut s = minimal_settings();
        s.mode = Mode::RlEpisode;
        assert_ne!(base, key_of(&s));
        let mut s = minimal_settings();
        s.env.insert("FOO".to_string(), "bar".to_string());
        assert_ne!(base, key_of(&s));
        let mut s = minimal_settings();
        s.determinism.seed = Some(42);
        assert_ne!(base, key_of(&s));
    }

    #[test]
    fn files_digest_stable_and_content_sensitive() {
        let a = minimal_settings();
        assert_eq!(files_digest(&a), files_digest(&minimal_settings()));
        let mut b = minimal_settings();
        b.files.push(FileEntry {
            path: "main.py".to_string(),
            content: b"print(1)".to_vec(),
            mode: 0o644,
        });
        let mut c = minimal_settings();
        c.files.push(FileEntry {
            path: "main.py".to_string(),
            content: b"print(2)".to_vec(),
            mode: 0o644,
        });
        assert_ne!(files_digest(&b), files_digest(&c));
        // ... and the content difference propagates into the cache key.
        assert_ne!(key_of(&b), key_of(&c));
    }

    #[test]
    fn resolved_clone_cost_bench() {
        // Sizes mirror a warm python exec: 6 mounts, 3 argv words, 25 env
        // pairs, short profile/id strings, interpreted (no binary fd).
        use kernel::spec::{MountKind, ResourceLimits, SeccompProfileId};
        let spec = ChildSpec {
            request_id: "req_bench0123456789".to_string(),
            language: Language::Python,
            mounts: vec![
                MountKind::BindRo {
                    src: "/nix/store/abc123-python3-minimal".into(),
                    target: "/nix/store".into(),
                },
                MountKind::BindRo {
                    src: "/nix/store/def456-dev-min".into(),
                    target: "/dev-min".into(),
                },
                MountKind::BindRw {
                    src: "/tmp/rockbox-work/9f8e7d6c".into(),
                    target: "/sandbox".into(),
                },
                MountKind::Proc {
                    target: "/proc".into(),
                },
                MountKind::Tmpfs {
                    target: "/tmp".into(),
                    size_bytes: 512 << 20,
                    mode: 0o1777,
                },
                MountKind::Tmpfs {
                    target: "/run".into(),
                    size_bytes: 64 << 20,
                    mode: 0o755,
                },
            ],
            argv: vec!["python3".into(), "-S".into(), "/sandbox/main.py".into()],
            env: (0..25)
                .map(|i| {
                    (
                        format!("ROCKBOX_BASE_VAR_{i:02}"),
                        format!("value-{i}-with-padding"),
                    )
                })
                .collect(),
            limits: ResourceLimits {
                memory_bytes: 512 << 20,
                cpu_quota_us: 100_000,
                cpu_period_us: 100_000,
                pids_max: 200,
                nofile: 256,
                fsize_bytes: 50 << 20,
                address_space_bytes: 1 << 32,
                stack_bytes: 8 << 20,
            },
            seccomp_profile: SeccompProfileId::InterpAot,
            layers: SandboxLayers::default(),
            apparmor_profile: "rockbox-python-base".to_string(),
            binary_fd_path: None,
            wall_timeout: Duration::from_millis(5000),
            protocol_fd: false,
        };
        let resolved = Resolved {
            spec,
            work_dir: "/tmp/rockbox-work/9f8e7d6c5b4a".into(),
        };
        const N: usize = 50_000;
        for _ in 0..1_000 {
            std::hint::black_box(resolved.clone());
        }
        let t = std::time::Instant::now();
        for _ in 0..N {
            std::hint::black_box(resolved.clone());
        }
        eprintln!(
            "resolved clone: {:.3}µs/op",
            t.elapsed().as_micros() as f64 / N as f64
        );
    }
}

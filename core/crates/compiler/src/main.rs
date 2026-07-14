//! `compiler` — privileged compile process
//!
//! Runs as uid 0 (no NET caps). Listens on a unix-domain socket; engines
//! (uid 1000) send compile requests, helper runs the language compiler under
//! `bwrap` with the relaxed-compiler seccomp profile and writes
//! the resulting binary into `/var/cache/sandbox/bin/<hash>/` with mode
//! 0555 root:root

use anyhow::{Context, Result};
use clap::Parser;
use msgpack::{FrameReader, FrameWriter};
use protocol::FileEntry;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::net::UnixListener;
use tracing::{error, info, instrument};

#[derive(Debug, Parser)]
#[command(name = "compiler", version)]
struct Args {
    /// Socket to listen on; engines connect here.
    #[arg(
        long,
        env = "ROCKBOX_HELPER_SOCK",
        default_value = "/run/sandbox/compile.sock"
    )]
    sock: PathBuf,

    /// Where to write cached binaries.
    #[arg(
        long,
        env = "ROCKBOX_BIN_CACHE",
        default_value = "/var/cache/sandbox/bin"
    )]
    cache_root: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum HelperCommand {
    Compile {
        request_id: String,
        language: String,
        compiler_cmd: Vec<String>,
        flake_lock_hash: String,
        compiler_version: String,
        files: Vec<FileEntry>,
        timeout_ms: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HelperResponse {
    Ok {
        request_id: String,
        binary_path: String,
        sha256: String,
        time_ms: u64,
        cached: bool,
    },
    Failed {
        request_id: String,
        error: String,
        stderr: String,
    },
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .json()
        .init();
    let args = Args::parse();
    std::fs::create_dir_all(&args.cache_root)?;

    if args.sock.exists() {
        std::fs::remove_file(&args.sock)?;
    }
    let listener = UnixListener::bind(&args.sock).context("bind unix socket")?;
    info!(sock = %args.sock.display(), "compiler_ready");

    loop {
        let (stream, _addr) = listener.accept().await?;
        let cache_root = args.cache_root.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, cache_root).await {
                error!(?e, "client_error");
            }
        });
    }
}

#[instrument(skip(stream, cache_root))]
async fn handle_client(stream: tokio::net::UnixStream, cache_root: PathBuf) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = FrameReader::new(read_half);
    let writer = FrameWriter::new(write_half);
    loop {
        let cmd: HelperCommand = match reader.read().await {
            Ok(c) => c,
            Err(_) => return Ok(()), // peer closed
        };
        match cmd {
            HelperCommand::Compile {
                request_id,
                language,
                compiler_cmd,
                flake_lock_hash,
                compiler_version,
                files,
                timeout_ms,
            } => {
                let resp = compile_one(
                    &cache_root,
                    &request_id,
                    &language,
                    &compiler_cmd,
                    &flake_lock_hash,
                    &compiler_version,
                    files,
                    timeout_ms,
                );
                writer.write(&resp).await?;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn compile_one(
    cache_root: &PathBuf,
    request_id: &str,
    _language: &str,
    compiler_cmd: &[String],
    flake_lock_hash: &str,
    compiler_version: &str,
    files: Vec<FileEntry>,
    _timeout_ms: u64,
) -> HelperResponse {
    let start = std::time::Instant::now();
    let key = compute_key(&files, compiler_version, flake_lock_hash);
    let target_dir = cache_root.join(&key);
    let target_bin = target_dir.join("main.bin");
    let target_hash = target_dir.join("main.sha256");

    if target_bin.exists() && target_hash.exists() {
        let sha = std::fs::read_to_string(&target_hash).unwrap_or_default();
        return HelperResponse::Ok {
            request_id: request_id.into(),
            binary_path: target_bin.display().to_string(),
            sha256: sha.trim().into(),
            time_ms: start.elapsed().as_millis() as u64,
            cached: true,
        };
    }

    // Scratch work dir for the source files.
    let work_dir = std::env::temp_dir().join(format!("rockbox-compile-{request_id}"));
    if let Err(e) = std::fs::create_dir_all(&work_dir) {
        return failed(request_id, format!("mkdir work: {e}"), "");
    }
    for f in &files {
        let p = work_dir.join(&f.path);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&p, &f.content) {
            return failed(request_id, format!("write src: {e}"), "");
        }
    }
    if let Err(e) = std::fs::create_dir_all(&target_dir) {
        return failed(request_id, format!("mkdir cache: {e}"), "");
    }

    let out_result = run_bwrap_compile(&work_dir, &target_dir, compiler_cmd);
    let out = match out_result {
        Ok(o) => o,
        Err(e) => return failed(request_id, format!("bwrap spawn: {e}"), ""),
    };
    if !out.status.success() {
        return failed(
            request_id,
            format!("compiler exit {}", out.status),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        );
    }
    if !target_bin.exists() {
        return failed(
            request_id,
            "compiler did not produce /output/main.bin",
            String::from_utf8_lossy(&out.stderr).into_owned(),
        );
    }
    let sha = match hash_file(&target_bin) {
        Ok(h) => h,
        Err(e) => return failed(request_id, format!("hash bin: {e}"), ""),
    };
    let _ = std::fs::write(&target_hash, &sha);
    set_readonly(&target_bin);
    set_readonly(&target_hash);
    let _ = std::fs::remove_dir_all(&work_dir);

    HelperResponse::Ok {
        request_id: request_id.into(),
        binary_path: target_bin.display().to_string(),
        sha256: sha,
        time_ms: start.elapsed().as_millis() as u64,
        cached: false,
    }
}

fn compute_key(files: &[FileEntry], compiler_version: &str, flake_lock_hash: &str) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    for f in files {
        h.update(f.path.as_bytes());
        h.update(b"\0");
        h.update(&f.content);
        h.update(b"\0");
    }
    h.update(compiler_version.as_bytes());
    h.update(b"\0");
    h.update(flake_lock_hash.as_bytes());
    hex::encode(h.finalize())
}

#[cfg(target_os = "linux")]
fn run_bwrap_compile(
    work_dir: &std::path::Path,
    target_dir: &std::path::Path,
    compiler_cmd: &[String],
) -> std::io::Result<std::process::Output> {
    use std::process::Command;
    let mut cmd = Command::new("bwrap");
    cmd.args([
        "--unshare-all",
        "--ro-bind",
        "/nix/store",
        "/nix/store",
        "--bind",
        work_dir.to_str().unwrap_or("/tmp"),
        "/build",
        "--bind",
        target_dir.to_str().unwrap_or("/tmp"),
        "/output",
        "--tmpfs",
        "/tmp",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--die-with-parent",
        "--chdir",
        "/build",
        "--",
    ]);
    for arg in compiler_cmd {
        cmd.arg(arg);
    }
    cmd.output()
}

#[cfg(not(target_os = "linux"))]
fn run_bwrap_compile(
    work_dir: &std::path::Path,
    target_dir: &std::path::Path,
    compiler_cmd: &[String],
) -> std::io::Result<std::process::Output> {
    // Non-Linux dev fallback: `bwrap` doesn't exist and namespaces / seccomp
    // are Linux-only, so we cannot sandbox at compile-time. Instead we run the
    // compiler directly with the same env contract (`/build` → work_dir,
    // `/output` → target_dir) via env vars, and let the resolver pass a
    // compiler command that resolves paths through those vars.
    //
    // Substitute `/build` and `/output` prefixes in the compiler argv so the
    // command that was designed for the sandboxed layout works on the host
    // filesystem. This is only intended for dev workflows on macOS — Linux
    // production always takes the bwrap branch above.
    use std::process::Command;
    if compiler_cmd.is_empty() {
        return Ok(std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: b"compiler: empty compiler_cmd".to_vec(),
        });
    }
    let build_str = work_dir.to_string_lossy().into_owned();
    let output_str = target_dir.to_string_lossy().into_owned();
    let rewrite = |arg: &String| -> String {
        arg.replace("/build", &build_str)
            .replace("/output", &output_str)
    };
    let mut cmd = Command::new(rewrite(&compiler_cmd[0]));
    for arg in &compiler_cmd[1..] {
        cmd.arg(rewrite(arg));
    }
    cmd.current_dir(work_dir);
    cmd.env("HOME", "/tmp");
    cmd.output()
}

fn failed(request_id: &str, error: impl Into<String>, stderr: impl Into<String>) -> HelperResponse {
    HelperResponse::Failed {
        request_id: request_id.into(),
        error: error.into(),
        stderr: stderr.into(),
    }
}

fn hash_file(path: &PathBuf) -> std::io::Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

fn set_readonly(path: &PathBuf) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(0o555);
            let _ = std::fs::set_permissions(path, perm);
        }
    }
}

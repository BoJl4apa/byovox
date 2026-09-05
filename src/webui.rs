//! Lifecycle of the optional upload-and-process web app: a Python process the daemon spawns
//! and owns, never a dictation-path dependency. Everything past "is `[webui]` enabled" and
//! "start/stop this child" is `pywebui`'s job; this module knows no HTTP, no ffmpeg, no LLM.
//!
//! Kept deliberately thin, mirroring `daemon_exe`/`cli_exe` in `src/daemon.rs`: a handful of
//! path lookups and a `Command::spawn`, so the Rust side stays "trigger the web server" and
//! nothing more.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::WebuiConfig;

/// Where `pywebui` lives: beside the running executable first (an installed copy ships it
/// there), else the repository's `pywebui` beside `Cargo.toml` (a `cargo run` checkout).
/// `cfg.app_dir` overrides both when set.
pub fn resolve_app_dir(cfg: &WebuiConfig, exe: &Path) -> Result<PathBuf> {
    if !cfg.app_dir.trim().is_empty() {
        let dir = crate::config::expand_home(&cfg.app_dir);
        return dir
            .is_dir()
            .then_some(dir.clone())
            .with_context(|| format!("webui.app_dir {} is not a directory", dir.display()));
    }
    let beside_exe = exe.with_file_name("pywebui");
    if beside_exe.is_dir() {
        return Ok(beside_exe);
    }
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("pywebui");
    if repo.is_dir() {
        return Ok(repo);
    }
    bail!(
        "pywebui not found beside {} or at {}: set webui.app_dir",
        exe.display(),
        repo.display()
    );
}

/// The interpreter to launch: `cfg.python` verbatim if set, else the first of `python3`,
/// `python` found on PATH. A bare name is handed to `Command` as given — PATH lookup is the
/// OS's job, not this function's — so "found" here only means "not empty".
fn interpreter(cfg: &WebuiConfig) -> Result<String> {
    if !cfg.python.trim().is_empty() {
        return Ok(cfg.python.clone());
    }
    for candidate in ["python3", "python"] {
        if Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return Ok(candidate.into());
        }
    }
    bail!("no python3 or python found on PATH: install Python 3.11+, or set webui.python")
}

/// Spawns `pywebui/server.py` as a child process, log file already open so its own stdout
/// and stderr land next to the daemon's rather than a console it does not have. `pywebui`
/// owns venv creation and dependency install on first run — this only launches it and hands
/// it the same config file and the host/port/retention it needs to serve.
pub fn spawn(
    cfg: &WebuiConfig,
    exe: &Path,
    config_path: &Path,
    data_dir: &Path,
    log_dir: &Path,
) -> Result<Child> {
    let app_dir = resolve_app_dir(cfg, exe)?;
    let python = interpreter(cfg)?;
    std::fs::create_dir_all(log_dir).with_context(|| format!("creating {}", log_dir.display()))?;
    let log_file = log_dir.join("webui.log");
    let out = std::fs::File::create(&log_file)
        .with_context(|| format!("creating {}", log_file.display()))?;
    let err = out
        .try_clone()
        .with_context(|| format!("cloning handle for {}", log_file.display()))?;
    Command::new(python)
        .arg(app_dir.join("server.py"))
        .arg("--config")
        .arg(config_path)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--host")
        .arg(&cfg.host)
        .arg("--port")
        .arg(cfg.port.to_string())
        .current_dir(&app_dir)
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {}", app_dir.join("server.py").display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_dir_override_must_exist() {
        let cfg = WebuiConfig {
            app_dir: "/does/not/exist/pywebui".into(),
            ..Default::default()
        };
        let err = resolve_app_dir(&cfg, Path::new("/tmp/byovox")).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");
    }

    #[test]
    fn repo_app_dir_is_found_without_override() {
        let cfg = WebuiConfig::default();
        let dir = resolve_app_dir(&cfg, Path::new("/tmp/byovox")).unwrap();
        assert!(dir.ends_with("pywebui"), "{}", dir.display());
    }

    #[test]
    fn explicit_python_is_used_verbatim() {
        let cfg = WebuiConfig {
            python: "/usr/bin/python3.11".into(),
            ..Default::default()
        };
        assert_eq!(interpreter(&cfg).unwrap(), "/usr/bin/python3.11");
    }
}

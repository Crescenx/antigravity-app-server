//! Local Codex discovery. Never starts, stops or reconfigures the shared daemon.
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

pub fn home() -> Result<PathBuf> {
    let path = if let Some(value) = std::env::var_os("CODEX_HOME") {
        PathBuf::from(value)
    } else {
        PathBuf::from(std::env::var_os("HOME").context("HOME unavailable; set CODEX_HOME")?)
            .join(".codex")
    };
    Ok(std::path::absolute(path)?)
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    true
}

fn find_named(name: &OsStr, search: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(search)
        // Do not execute a repository's ./codex through an empty/relative PATH entry.
        .filter(|p| p.is_absolute())
        .map(|p| p.join(name))
        .find(|p| executable(p))
}

fn resolve(
    explicit: Option<OsString>,
    search: &OsStr,
    user_home: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(value) = explicit {
        let path = PathBuf::from(&value);
        let resolved = if path.components().count() == 1 && !path.is_absolute() {
            find_named(&value, search)
        } else {
            Some(std::path::absolute(path)?)
        };
        let path = resolved.context("configured Codex executable was not found on PATH")?;
        ensure!(
            executable(&path),
            "configured Codex executable is missing or not executable: {}",
            path.display()
        );
        return Ok(path);
    }
    if let Some(path) = find_named(OsStr::new("codex"), search) {
        return Ok(path);
    }
    if let Some(home) = user_home {
        for relative in [
            ".local/bin/codex",
            ".cargo/bin/codex",
            ".npm-global/bin/codex",
        ] {
            let candidate = home.join(relative);
            if executable(&candidate) {
                return Ok(candidate);
            }
        }
        let versions = home.join(".nvm/versions/node");
        if let Ok(entries) = std::fs::read_dir(versions) {
            let mut candidates = entries.flatten().map(|e| e.path()).collect::<Vec<_>>();
            candidates.sort_by_cached_key(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .trim_start_matches('v')
                    .split('.')
                    .map(|n| n.parse::<u64>().unwrap_or(0))
                    .collect::<Vec<_>>()
            });
            for candidate in candidates.into_iter().rev().map(|p| p.join("bin/codex")) {
                if executable(&candidate) {
                    return Ok(candidate);
                }
            }
        }
    }
    bail!(
        "Codex was not found. Install Codex, run `codex login`, or set AGY_CODEX_BINARY to its executable; run antigravity-app-server --doctor afterward"
    )
}

pub fn binary(override_key: &str) -> Result<PathBuf> {
    resolve(
        std::env::var_os(override_key).or_else(|| std::env::var_os("AGY_CODEX_BINARY")),
        &std::env::var_os("PATH").unwrap_or_default(),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )
}

/// npm's launcher may use `/usr/bin/env node`; retain its original bin directory.
pub fn command(binary: &Path) -> Result<tokio::process::Command> {
    let mut command = tokio::process::Command::new(binary);
    let mut paths = vec![
        binary
            .parent()
            .context("Codex binary has no parent")?
            .to_owned(),
    ];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    command.env("PATH", std::env::join_paths(paths)?);
    Ok(command)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCodex {
    pub binary: PathBuf,
    pub home: PathBuf,
    pub connection: String,
    pub socket: Option<PathBuf>,
    pub fallback_allowed: bool,
}
impl LocalCodex {
    pub fn discover() -> Result<Self> {
        let binary = binary("AGY_CODEX_REVIEW_BINARY")?;
        let home = home()?;
        let mode = std::env::var("AGY_CODEX_CONNECTION").unwrap_or_else(|_| "auto".into());
        ensure!(
            ["auto", "daemon", "stdio"].contains(&mode.as_str()),
            "AGY_CODEX_CONNECTION must be auto, daemon or stdio"
        );
        let explicit_socket = std::env::var_os("AGY_CODEX_SOCKET");
        let socket = explicit_socket
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("app-server-control/app-server-control.sock"));
        ensure!(socket.is_absolute(), "AGY_CODEX_SOCKET must be absolute");
        let use_daemon =
            mode != "stdio" && (mode == "daemon" || explicit_socket.is_some() || socket.exists());
        Ok(Self {
            binary,
            home,
            connection: if use_daemon { "daemon" } else { "stdio" }.into(),
            socket: use_daemon.then_some(socket),
            fallback_allowed: mode == "auto" && explicit_socket.is_none(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_precedence_and_service_path() {
        let root = std::env::current_dir()
            .unwrap()
            .join(".tmp/discovery")
            .join(uuid::Uuid::new_v4().to_string());
        let first = root.join("bin/codex");
        let nvm = root.join(".nvm/versions/node/v24.1.0/bin/codex");
        for p in [&first, &nvm] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        let search = first.parent().unwrap().as_os_str();
        assert_eq!(resolve(None, search, Some(&root)).unwrap(), first);
        assert_eq!(resolve(None, OsStr::new(""), Some(&root)).unwrap(), nvm);
        assert!(
            resolve(
                Some(root.join("missing").into_os_string()),
                search,
                Some(&root)
            )
            .is_err()
        );
        assert_eq!(
            resolve(Some(nvm.clone().into_os_string()), search, None).unwrap(),
            nvm
        );
        assert!(find_named(OsStr::new("codex"), OsStr::new(".:relative")).is_none());
    }
}

//! Locating and probing the Claude Code binary.
//!
//! Order: the configured path, then the native installer's default location,
//! then PATH. Only a real executable is accepted. On Windows the npm package's
//! `claude.cmd` shim is never used, because a `.cmd` needs a console shell and
//! this app is a GUI process.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;

use crate::error::RuntimeError;

pub fn exe_name() -> &'static str {
    if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    }
}

pub fn locate(explicit: Option<&Path>) -> Result<PathBuf, RuntimeError> {
    if let Some(p) = explicit {
        return if p.is_file() {
            Ok(p.to_path_buf())
        } else {
            Err(RuntimeError::ClaudeNotFound(format!(
                "configured Claude Code path {} does not exist",
                p.display()
            )))
        };
    }
    if let Some(base) = directories::BaseDirs::new() {
        let candidate = base.home_dir().join(".local").join("bin").join(exe_name());
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(exe_name());
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(RuntimeError::ClaudeNotFound(
        "Claude Code was not found. Install it, or set its path in Settings.".into(),
    ))
}

/// Output of `claude auth status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct AuthStatus {
    #[serde(default, rename = "loggedIn")]
    pub logged_in: bool,
    #[serde(default, rename = "authMethod")]
    pub auth_method: String,
}

/// Runs the binary with `args` and returns stdout. The API key variables are
/// removed so the answer reflects the login the app will actually use.
async fn run_capture(binary: &Path, args: &[&str]) -> Result<String, RuntimeError> {
    let mut cmd = tokio::process::Command::new(binary);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("DISABLE_AUTOUPDATER", "1");
    cmd.env_remove("ANTHROPIC_API_KEY");
    cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .map_err(|_| RuntimeError::Utility(format!("`claude {}` timed out", args.join(" "))))?
        .map_err(|e| RuntimeError::Utility(format!("could not run {}: {e}", binary.display())))?;
    // `auth status` exits non-zero when logged out but still prints its JSON.
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub async fn auth_status(binary: &Path) -> Result<AuthStatus, RuntimeError> {
    let out = run_capture(binary, &["auth", "status"]).await?;
    serde_json::from_str(out.trim())
        .map_err(|e| RuntimeError::Utility(format!("could not read auth status: {e}")))
}

pub async fn version(binary: &Path) -> Result<String, RuntimeError> {
    run_capture(binary, &["--version"])
        .await
        .map(|s| s.trim().to_string())
}

/// What the Settings screen shows about Claude Code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeProbe {
    pub binary: Option<PathBuf>,
    pub error: Option<String>,
    pub version: Option<String>,
    pub logged_in: Option<bool>,
    pub auth_method: Option<String>,
}

impl ClaudeProbe {
    pub fn is_ready(&self) -> bool {
        self.binary.is_some() && self.logged_in == Some(true)
    }
}

/// Locates the binary and asks it for its version and login state. Never
/// fails; every problem is reported in the returned struct.
pub async fn probe(explicit: Option<&Path>) -> ClaudeProbe {
    let binary = match locate(explicit) {
        Ok(b) => b,
        Err(e) => {
            return ClaudeProbe {
                error: Some(e.to_string()),
                ..Default::default()
            }
        }
    };
    let version = version(&binary).await.ok();
    let (logged_in, auth_method, error) = match auth_status(&binary).await {
        Ok(s) => (Some(s.logged_in), Some(s.auth_method), None),
        Err(e) => (None, None, Some(e.to_string())),
    };
    ClaudeProbe {
        binary: Some(binary),
        error,
        version,
        logged_in,
        auth_method,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_path_must_exist() {
        let err = locate(Some(Path::new("Z:/definitely/not/here/claude.exe"))).unwrap_err();
        assert!(matches!(err, RuntimeError::ClaudeNotFound(_)));
    }

    #[test]
    fn explicit_existing_file_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join(exe_name());
        std::fs::write(&fake, b"").unwrap();
        assert_eq!(locate(Some(&fake)).unwrap(), fake);
    }

    #[tokio::test]
    async fn probe_reports_a_missing_binary_instead_of_failing() {
        let p = probe(Some(Path::new("Z:/nope/claude.exe"))).await;
        assert!(p.binary.is_none());
        assert!(p.error.unwrap().contains("does not exist"));
        assert!(!ClaudeProbe::default().is_ready());
    }

    #[test]
    fn auth_status_json_parses() {
        let s: AuthStatus =
            serde_json::from_str(r#"{"loggedIn": true, "authMethod": "claude.ai", "x": 1}"#)
                .unwrap();
        assert!(s.logged_in);
        assert_eq!(s.auth_method, "claude.ai");
    }
}

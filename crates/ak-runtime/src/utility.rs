//! Utility calls: Claude Code as a plain model call, with no tools, no MCP,
//! no hooks and no session. Used by Refine and Integrate.
//!
//! The instruction goes on the command line and the material goes through
//! stdin, which sidesteps the Windows command-line length limit. Each call
//! gets its own directory under the home directory holding the system prompt,
//! the input and the captured output, for diagnostics.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use ak_core::config::GlobalSettings;
use ak_core::ids::CallId;

use crate::claude;
use crate::error::RuntimeError;
use crate::paths::HomePaths;

#[derive(Debug, Clone, PartialEq)]
pub struct UtilityRequest {
    /// Replaces Claude Code's default system prompt.
    pub system_prompt: String,
    /// The short instruction passed as the prompt argument.
    pub instruction: String,
    /// The material, passed through stdin.
    pub input: String,
    pub model: Option<String>,
    /// For logs and the call directory.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UtilityOutput {
    pub text: String,
    pub cost_usd: Option<f64>,
    pub duration: Duration,
    /// Where the call's files were written, when a real process ran.
    pub call_dir: Option<PathBuf>,
}

#[async_trait]
pub trait UtilityRunner: Send + Sync {
    async fn call(&self, request: UtilityRequest) -> Result<UtilityOutput, RuntimeError>;

    /// A short description of what will run, for the UI.
    fn describe(&self) -> String;
}

/// The real thing: spawns the Claude Code binary.
pub struct ClaudeCodeRunner {
    binary: Result<PathBuf, String>,
    calls_dir: PathBuf,
    pass_api_key: bool,
    timeout: Duration,
}

impl ClaudeCodeRunner {
    pub fn new(binary: Result<PathBuf, String>, calls_dir: PathBuf) -> Self {
        Self {
            binary,
            calls_dir,
            pass_api_key: false,
            timeout: Duration::from_secs(240),
        }
    }

    pub fn from_settings(settings: &GlobalSettings, paths: &HomePaths) -> Self {
        let binary = claude::locate(settings.claude_binary.as_deref()).map_err(|e| e.to_string());
        Self::new(binary, paths.calls_dir())
    }

    pub fn binary(&self) -> Result<&Path, &str> {
        self.binary.as_deref().map_err(|e| e.as_str())
    }

    /// Asks the binary whether it is logged in, so a missing login fails fast
    /// with a useful message instead of a confusing result. If the status
    /// command itself cannot run, the check is skipped.
    pub async fn ensure_logged_in(&self, binary: &Path) -> Result<(), RuntimeError> {
        match claude::auth_status(binary).await {
            Ok(status) if status.logged_in => Ok(()),
            Ok(status) => Err(RuntimeError::Utility(format!(
                "{LOGIN_HINT} (auth method: {})",
                if status.auth_method.is_empty() {
                    "none"
                } else {
                    &status.auth_method
                }
            ))),
            // If the status command itself cannot run, let the call proceed
            // and report whatever happens.
            Err(_) => Ok(()),
        }
    }
}

/// The argument list for a utility call. Separate so it can be checked
/// without spawning anything.
pub fn build_args(request: &UtilityRequest, system_prompt_file: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-p".into(),
        request.instruction.clone().into(),
        "--output-format".into(),
        "json".into(),
        "--tools".into(),
        "".into(),
        "--strict-mcp-config".into(),
        "--settings".into(),
        r#"{"disableAllHooks": true}"#.into(),
        "--system-prompt-file".into(),
        system_prompt_file.as_os_str().to_os_string(),
        "--no-session-persistence".into(),
    ];
    if let Some(model) = &request.model {
        args.push("--model".into());
        args.push(model.clone().into());
    }
    args
}

/// The `result` message of `--output-format json`.
#[derive(Debug, Deserialize)]
struct ResultMessage {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    subtype: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    errors: Option<Vec<String>>,
    /// Why the turn ended, e.g. `api_error`. Present on newer versions.
    #[serde(default)]
    terminal_reason: Option<String>,
}

pub const LOGIN_HINT: &str =
    "Claude Code is not logged in. Open a terminal, run `claude`, and use /login, then try again.";

/// Finds the result object in the process output. Normally the whole stdout
/// is one JSON object; be lenient and accept the last line that parses.
fn parse_result(stdout: &str) -> Option<ResultMessage> {
    if let Ok(m) = serde_json::from_str::<ResultMessage>(stdout.trim()) {
        if m.kind == "result" {
            return Some(m);
        }
    }
    stdout
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<ResultMessage>(line.trim()).ok())
        .find(|m| m.kind == "result")
}

#[async_trait]
impl UtilityRunner for ClaudeCodeRunner {
    async fn call(&self, request: UtilityRequest) -> Result<UtilityOutput, RuntimeError> {
        let binary = self.binary.clone().map_err(RuntimeError::ClaudeNotFound)?;
        self.ensure_logged_in(&binary).await?;
        let id = CallId::new();
        let dir = self.calls_dir.join(id.to_string());
        std::fs::create_dir_all(&dir).map_err(|e| RuntimeError::io(&dir, e))?;
        let system_file = dir.join("system.md");
        std::fs::write(&system_file, &request.system_prompt)
            .map_err(|e| RuntimeError::io(&system_file, e))?;
        let input_file = dir.join("input.md");
        std::fs::write(&input_file, &request.input)
            .map_err(|e| RuntimeError::io(&input_file, e))?;

        let mut cmd = tokio::process::Command::new(&binary);
        cmd.args(build_args(&request, &system_file))
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("DISABLE_AUTOUPDATER", "1")
            .kill_on_drop(true);
        if !self.pass_api_key {
            // An ambient key silently overrides the user's login in print mode.
            cmd.env_remove("ANTHROPIC_API_KEY");
            cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
        }
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let started = Instant::now();
        tracing::info!(label = %request.label, dir = %dir.display(), "utility call starting");
        let mut child = cmd.spawn().map_err(|e| {
            RuntimeError::Utility(format!("could not start {}: {e}", binary.display()))
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            // A broken pipe here means the process died; the wait below reports it.
            let _ = stdin.write_all(request.input.as_bytes()).await;
            drop(stdin);
        }
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let stdout_task = tokio::spawn(async move {
            let mut buf = String::new();
            if let Some(p) = stdout_pipe.as_mut() {
                let _ = p.read_to_string(&mut buf).await;
            }
            buf
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = String::new();
            if let Some(p) = stderr_pipe.as_mut() {
                let _ = p.read_to_string(&mut buf).await;
            }
            buf
        });

        let status = tokio::select! {
            s = child.wait() => s.map_err(|e| RuntimeError::Utility(format!("wait failed: {e}")))?,
            _ = tokio::time::sleep(self.timeout) => {
                let _ = child.kill().await;
                return Err(RuntimeError::Utility(format!(
                    "Claude Code did not finish within {} seconds",
                    self.timeout.as_secs()
                )));
            }
        };
        let stdout = stdout_task.await.unwrap_or_default();
        let stderr = stderr_task.await.unwrap_or_default();
        let _ = std::fs::write(dir.join("stdout.json"), &stdout);
        let _ = std::fs::write(dir.join("stderr.log"), &stderr);
        let duration = started.elapsed();

        let Some(result) = parse_result(&stdout) else {
            let tail: String = stderr
                .lines()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            return Err(RuntimeError::Utility(format!(
                "Claude Code exited with {status} and no result. {}",
                if tail.is_empty() {
                    "No diagnostics.".to_string()
                } else {
                    tail
                }
            )));
        };
        if result.is_error || result.subtype != "success" {
            // On failure the message usually sits in `result`, with `subtype`
            // still "success" and `terminal_reason` saying why.
            let mut parts: Vec<String> = Vec::new();
            if let Some(text) = result.result.as_deref().filter(|t| !t.trim().is_empty()) {
                parts.push(text.trim().to_string());
            }
            if let Some(errors) = &result.errors {
                parts.extend(errors.iter().map(|e| e.trim().to_string()));
            }
            let why = if parts.is_empty() {
                format!(
                    "{} ({})",
                    result.subtype,
                    result.terminal_reason.as_deref().unwrap_or("no detail")
                )
            } else {
                parts.join("; ")
            };
            let lower = why.to_lowercase();
            let hint = if lower.contains("authenticat")
                || lower.contains("oauth")
                || lower.contains("log in")
            {
                format!(" {LOGIN_HINT}")
            } else {
                String::new()
            };
            return Err(RuntimeError::Utility(format!(
                "Claude Code failed: {why}.{hint}"
            )));
        }
        tracing::info!(label = %request.label, secs = duration.as_secs_f32(), cost = ?result.total_cost_usd, "utility call finished");
        Ok(UtilityOutput {
            text: result.result.unwrap_or_default(),
            cost_usd: result.total_cost_usd,
            duration,
            call_dir: Some(dir),
        })
    }

    fn describe(&self) -> String {
        match &self.binary {
            Ok(p) => format!("Claude Code at {}", p.display()),
            Err(e) => format!("Claude Code unavailable: {e}"),
        }
    }
}

type Responder = Box<dyn Fn(&UtilityRequest) -> Result<String, String> + Send + Sync>;

/// A runner for tests and offline work: answers from a closure.
pub struct FakeRunner {
    respond: Responder,
}

impl FakeRunner {
    pub fn new(
        respond: impl Fn(&UtilityRequest) -> Result<String, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            respond: Box::new(respond),
        }
    }

    /// Echoes the input back, which for Refine means "no changes".
    pub fn echo() -> Self {
        Self::new(|req| Ok(req.input.clone()))
    }
}

#[async_trait]
impl UtilityRunner for FakeRunner {
    async fn call(&self, request: UtilityRequest) -> Result<UtilityOutput, RuntimeError> {
        match (self.respond)(&request) {
            Ok(text) => Ok(UtilityOutput {
                text,
                cost_usd: Some(0.0),
                duration: Duration::from_millis(1),
                call_dir: None,
            }),
            Err(e) => Err(RuntimeError::Utility(e)),
        }
    }

    fn describe(&self) -> String {
        "fake runner".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_disable_every_capability_and_pass_the_system_prompt() {
        let req = UtilityRequest {
            system_prompt: "sys".into(),
            instruction: "do it".into(),
            input: "material".into(),
            model: Some("sonnet".into()),
            label: "test".into(),
        };
        let args = build_args(&req, Path::new("C:/calls/1/system.md"));
        let as_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let joined = as_str.join(" ");
        for expected in [
            "-p do it",
            "--output-format json",
            "--tools ",
            "--strict-mcp-config",
            "--settings {\"disableAllHooks\": true}",
            "--system-prompt-file C:/calls/1/system.md",
            "--no-session-persistence",
            "--model sonnet",
        ] {
            assert!(
                joined.contains(expected),
                "missing {expected:?} in {joined}"
            );
        }
        assert!(
            as_str.contains(&String::new()),
            "--tools takes an empty argument"
        );
        assert!(!joined.contains("bypassPermissions"));
    }

    #[test]
    fn result_parsing_accepts_whole_object_or_last_line() {
        let whole = r#"{"type":"result","subtype":"success","is_error":false,"result":"hello","total_cost_usd":0.01}"#;
        let m = parse_result(whole).unwrap();
        assert_eq!(m.result.as_deref(), Some("hello"));
        assert_eq!(m.total_cost_usd, Some(0.01));

        let noisy = "warning: something\n{\"type\":\"system\",\"subtype\":\"init\"}\n{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"is_error\":true,\"errors\":[\"too many\"]}\n";
        let m = parse_result(noisy).unwrap();
        assert!(m.is_error);
        assert_eq!(m.subtype, "error_max_turns");
        assert!(parse_result("nothing here").is_none());
    }

    #[tokio::test]
    async fn fake_runner_round_trips() {
        let runner = FakeRunner::new(|req| Ok(format!("refined: {}", req.input)));
        let out = runner
            .call(UtilityRequest {
                system_prompt: String::new(),
                instruction: String::new(),
                input: "draft".into(),
                model: None,
                label: "t".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.text, "refined: draft");
    }
}

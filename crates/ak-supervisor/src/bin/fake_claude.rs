//! A stand-in for the Claude Code binary, used by the supervisor tests.
//!
//! It records its arguments and stdin to `fake_claude_args.json` in the
//! working directory, then prints stream-json like the real thing. The
//! `FAKE_CLAUDE_MODE` environment variable picks the behaviour: `ok`
//! (default), `hang`, `crash`, `error`, or `noinit`.

use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut stdin = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin);
    let cwd = std::env::current_dir().unwrap_or_default();
    let record = serde_json::json!({
        "args": args,
        "stdin": stdin,
        "cwd": cwd.to_string_lossy(),
        "env_api_key": std::env::var("ANTHROPIC_API_KEY").ok(),
    });
    let _ = std::fs::write(cwd.join("fake_claude_args.json"), record.to_string());

    let session_id = args
        .iter()
        .position(|a| a == "--session-id")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "fake-session".into());
    // The mode comes from a file in the working directory so parallel tests
    // do not share it through the process environment.
    let mode = std::fs::read_to_string(cwd.join("fake_claude_mode.txt"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| std::env::var("FAKE_CLAUDE_MODE").unwrap_or_else(|_| "ok".into()));

    if mode == "noinit" {
        std::thread::sleep(std::time::Duration::from_secs(60));
        return;
    }

    println!(
        "{}",
        serde_json::json!({
            "type": "system", "subtype": "init", "session_id": session_id, "model": "fake-model",
            "cwd": cwd.to_string_lossy(), "tools": ["Read", "mcp__board__inbox_list"],
            "mcp_servers": [{"name": "board", "status": "connected"}]
        })
    );

    match mode.as_str() {
        "hang" => {
            std::thread::sleep(std::time::Duration::from_secs(120));
        }
        "crash" => {
            eprintln!("fatal: something went wrong");
            std::process::exit(3);
        }
        "error" => {
            println!(
                "{}",
                serde_json::json!({
                    "type": "result", "subtype": "success", "is_error": true,
                    "result": "Failed to authenticate", "total_cost_usd": 0.0, "num_turns": 1,
                    "session_id": session_id
                })
            );
        }
        _ => {
            println!(
                "{}",
                serde_json::json!({
                    "type": "assistant",
                    "message": {"role": "assistant", "content": [
                        {"type": "text", "text": "Checking the inbox."},
                        {"type": "tool_use", "id": "t1", "name": "mcp__board__inbox_list", "input": {}}
                    ]}
                })
            );
            println!(
                "{}",
                serde_json::json!({
                    "type": "user",
                    "message": {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "t1", "content": "inbox empty"}
                    ]}
                })
            );
            println!(
                "{}",
                serde_json::json!({
                    "type": "result", "subtype": "success", "is_error": false, "result": "Nothing to do.",
                    "total_cost_usd": 0.0123, "num_turns": 2, "duration_ms": 10,
                    "usage": {"input_tokens": 120, "output_tokens": 30},
                    "session_id": session_id
                })
            );
        }
    }
}

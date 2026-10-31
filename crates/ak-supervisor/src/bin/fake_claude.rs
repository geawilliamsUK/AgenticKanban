//! A stand-in for the Claude Code binary, used by the supervisor tests.
//!
//! It records its arguments and stdin to `fake_claude_args.json` in the
//! working directory, then prints stream-json like the real thing. The
//! `FAKE_CLAUDE_MODE` environment variable picks the behaviour: `ok`
//! (default), `hang`, `crash`, `error`, or `noinit`.

use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--input-format") {
        return conversation(&args);
    }
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

    // Claude Code 2.1.314 reports the subscription's windows before init.
    // `fake_claude_usage.txt` sets the 5-hour utilization.
    let five_hour: f64 = std::fs::read_to_string(cwd.join("fake_claude_usage.txt"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0.25);
    println!(
        "{}",
        serde_json::json!({
            "type": "rate_limit_event", "session_id": session_id,
            "rate_limit_info": {
                "status": "allowed", "resetsAt": 1893456000, "rateLimitType": "five_hour",
                "utilization": five_hour,
                "unifiedWindows": {
                    "five_hour": {"utilization": five_hour, "resetsAt": 1893456000},
                    "seven_day": {"utilization": 0.5, "resetsAt": 1893800000}
                }
            }
        })
    );

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

/// A Desk session: stream-json in and out, one turn per user line, until
/// stdin closes. Each spawn appends its arguments to
/// `fake_claude_desk_args.jsonl` in the working directory.
///
/// Every message is echoed back as "Echo: <text>" (with deltas and, on the
/// first turn, a Read tool call). `/context` answers like a local command,
/// replay included. `slow` streams part of a reply and then waits for an
/// interrupt, which it honours like the real thing.
fn conversation(args: &[String]) {
    use std::io::{BufRead, Write};
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cwd.join("fake_claude_desk_args.jsonl"))
    {
        let _ = writeln!(f, "{}", serde_json::json!(args));
    }
    let session_id = args
        .iter()
        .position(|a| a == "--session-id" || a == "--resume")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "fake-session".into());
    let out = |v: serde_json::Value| {
        println!("{v}");
        let _ = std::io::stdout().flush();
    };
    out(serde_json::json!({
        "type": "rate_limit_event", "session_id": session_id,
        "rate_limit_info": {"status": "allowed", "unifiedWindows": {
            "five_hour": {"utilization": 0.3, "resetsAt": 1893456000}}}
    }));
    let mut total = 0.0;
    let mut turn = 0;
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    while let Some(Ok(line)) = lines.next() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if msg["type"] != "user" {
            continue;
        }
        let text = msg["message"]["content"].as_str().unwrap_or("").to_string();
        turn += 1;
        out(serde_json::json!({
            "type": "system", "subtype": "init", "session_id": session_id,
            "model": "fake-model", "slash_commands": ["compact", "context", "review"],
            "mcp_servers": [{"name": "board", "status": "connected"}]
        }));
        let result = |total: f64, subtype: &str, terminal: &str, result: &str| {
            serde_json::json!({
                "type": "result", "subtype": subtype, "is_error": subtype != "success",
                "result": result, "total_cost_usd": total, "terminal_reason": terminal,
                "duration_ms": 5, "session_id": session_id,
                "modelUsage": {"fake-model": {"contextWindow": 200000, "costUSD": total}}
            })
        };
        if text == "/context" {
            // A replay of an older local command's output, as after /compact.
            out(serde_json::json!({
                "type": "assistant", "local_command_source": "<local-command-stdout>old",
                "message": {"role": "assistant", "content": [{"type": "text", "text": "old output"}]}
            }));
            let mut r = result(
                total,
                "success",
                "completed",
                "## Context Usage\n42k / 200k",
            );
            r["local_command"] = serde_json::json!("context");
            out(r);
            continue;
        }
        let delta = |t: &str| {
            serde_json::json!({"type": "stream_event", "event": {
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": t}}})
        };
        if text == "slow" {
            out(delta("Working"));
            // Wait for the interrupt.
            for next in lines.by_ref() {
                let Ok(next) = next else { break };
                let v: serde_json::Value = serde_json::from_str(&next).unwrap_or_default();
                if v["type"] == "control_request" {
                    out(serde_json::json!({"type": "control_response", "response": {
                        "subtype": "success", "request_id": v["request_id"]}}));
                    out(
                        serde_json::json!({"type": "user", "message": {"role": "user",
                        "content": [{"type": "text", "text": "[Request interrupted by user]"}]}}),
                    );
                    out(result(
                        total,
                        "error_during_execution",
                        "aborted_streaming",
                        "",
                    ));
                    break;
                }
            }
            continue;
        }
        if turn == 1 {
            out(
                serde_json::json!({"type": "assistant", "message": {"role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "README.md"}}],
                "usage": {"input_tokens": 10, "cache_read_input_tokens": 1000, "output_tokens": 5}}}),
            );
            out(
                serde_json::json!({"type": "user", "message": {"role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "# Demo"}]}}),
            );
        }
        out(delta("Echo: "));
        out(delta(&text));
        out(
            serde_json::json!({"type": "assistant", "message": {"role": "assistant",
            "content": [{"type": "text", "text": format!("Echo: {text}")}],
            "usage": {"input_tokens": 20, "cache_read_input_tokens": 1200, "output_tokens": 8}}}),
        );
        total += 0.01;
        out(result(
            total,
            "success",
            "completed",
            &format!("Echo: {text}"),
        ));
    }
}

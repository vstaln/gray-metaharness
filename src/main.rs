//! gray-metaharness — end-to-end search over the context-injection
//! harness, after arXiv:2603.28052 (Meta-Harness).
//!
//! The search space is `policy.py`: the code that decides what lands in
//! every turn's `prompt/context`. Candidates live as plain files under
//! `~/.gray/metaharness/candidates/<NNN>/` because the paper's proposer is
//! a filesystem agent — it reads prior policies, scores, and full eval
//! traces, then writes the next candidate. The inner loop scores a
//! candidate by running each eval task under an isolated GRAY_HOME whose
//! `live.py` IS the candidate, so every score is attributable to the
//! candidate rather than the incumbent.
//!
//! Wire surface: `prompt/context` runs `live.py` (fail-open, ~10s cap),
//! `command/run` serves `/metaharness <verb>`, and the `metaharness_status`
//! tool inlines best + history to the agent. `run`/`eval` always spawn
//! detached — a sidecar can't block on a multi-hour loop.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

mod cli;
mod domain;
mod evals;
mod paths;
mod policy;
mod proc;
mod propose;
mod store;

/// Policy wall-clock cap inside `prompt/context` — generous for stdlib
/// python, tight enough that a hung candidate costs one pause per turn.
const POLICY_TIMEOUT: Duration = Duration::from_secs(10);

fn manifest() -> Value {
    json!({
        "name": "metaharness",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "description": "End-to-end search over the context-injection harness \
            (arXiv:2603.28052): candidates are policy.py files deciding what \
            context each turn sees; a proposer reads scores + traces and \
            writes the next one.",
        "tools": [{
            "name": "metaharness_status",
            "description": "Meta-harness search state: candidates, scores, \
                which policy is live, and the current best.",
            "parameters": {"type": "object", "properties": {}}
        }],
        "commands": ["/metaharness"],
        "hooks": ["prompt/context"],
        "subcommands": ["metaharness"],
    })
}

/// Run `live.py` for one turn. Fail-open to empty text: a broken policy
/// must never inject garbage or break the host's context build.
fn context(params: &Value) -> Value {
    let cwd = params["session"]["cwd"]
        .as_str()
        .or_else(|| params["cwd"].as_str())
        .unwrap_or("")
        .to_string();
    let sid = params["session"]["id"].as_str().unwrap_or("").to_string();
    let empty = json!({ "text": "" });
    let Ok(home) = paths::gray_home() else {
        return empty;
    };
    let meta = paths::meta_dir(&home);
    let live = paths::live_path(&meta);
    if !live.is_file() {
        return empty;
    }
    let payload = policy::stdin_payload(&cwd, &sid, "", &home);
    match policy::run(&live, &payload, POLICY_TIMEOUT) {
        Ok(v) => {
            let text = policy::render_inject(&v);
            json!({ "text": text })
        }
        Err(e) => {
            eprintln!("metaharness: live policy failed, injecting nothing: {e:#}");
            empty
        }
    }
}

/// The wire loop: one JSON object per line on stdin, one reply per
/// request on stdout. Human output and logs go to stderr only.
fn sidecar() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let method = req["method"].as_str().unwrap_or("");
        let id = req.get("id").cloned();
        let params = req.get("params").cloned().unwrap_or(json!({}));
        match method {
            "plugin/shutdown" => return,
            "event/notify" => continue,
            _ => {}
        }
        let Some(id) = id else { continue };
        let reply = match method {
            "plugin/manifest" => json!({ "id": id, "result": manifest() }),
            "plugin/tools" => json!({
                "id": id,
                "result": { "tools": manifest()["tools"].clone() }
            }),
            "prompt/context" => json!({ "id": id, "result": context(&params) }),
            "command/run" => {
                let argv: Vec<String> = params
                    .get("argv")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                match cli::wire_command(&argv) {
                    Ok(v) => json!({ "id": id, "result": v }),
                    Err(e) => json!({
                        "id": id,
                        "error": {
                            "code": "invalid",
                            "message": format!("{e:#}"),
                            "retryable": false,
                            "terminal": true,
                        }
                    }),
                }
            }
            "tool/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let result = match name {
                    "metaharness_status" => {
                        let text = paths::gray_home()
                            .map(|h| store::status_text(&paths::meta_dir(&h)))
                            .unwrap_or_else(|e| format!("metaharness: {e:#}"));
                        json!({ "content": text })
                    }
                    _ => json!({
                        "content": format!("unknown tool: {name}"),
                        "is_error": true
                    }),
                };
                json!({ "id": id, "result": result })
            }
            _ => json!({
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("method not found: {method}")
                }
            }),
        };
        let _ = writeln!(out, "{}", serde_json::to_string(&reply).unwrap_or_default());
        let _ = out.flush();
    }
}

/// `gray metaharness <verb>` — the direct CLI path (long-running verbs
/// like `run` block here; the wire path detaches instead).
fn cli_main(args: &[String]) -> i32 {
    let Some(verb) = args.first() else {
        println!("{}", cli::usage());
        return 0;
    };
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    let out = match verb.as_str() {
        "init" => cli::init(),
        "status" => cli::status(),
        "apply" => match rest.first() {
            Some(id) => cli::apply(id),
            None => Err(anyhow::anyhow!("apply needs a candidate id")),
        },
        "eval" => match rest.first() {
            Some(id) => cli::eval_one(id),
            None => Err(anyhow::anyhow!("eval needs a candidate id")),
        },
        "run" => cli::run(&rest),
        "manifest" => {
            println!("{}", manifest());
            return 0;
        }
        _ => {
            println!("{}", cli::usage());
            return 0;
        }
    };
    match out {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(e) => {
            eprintln!("metaharness {verb}: {e:#}");
            1
        }
    }
}

fn main() {
    use anyhow::Context;
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Piped stdin is the host's spawn signal — enter the wire loop even on
    // bare invocation. A TTY with no args prints usage instead.
    let stdin_is_host = !stdin_is_tty();
    match args.first().map(String::as_str) {
        Some("manifest") => {
            println!("{}", manifest());
        }
        Some("sidecar") | None if stdin_is_host => sidecar(),
        None => println!("{}", cli::usage()),
        _ => std::process::exit(cli_main(&args)),
    }
    let _ = std::io::stdout().flush().context("flush");
}

/// stdin not a terminal means the host spawned us — the wire signal.
fn stdin_is_tty() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdin())
}

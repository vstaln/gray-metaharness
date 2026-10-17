//! The candidate contract — the search space the proposer edits.
//!
//! A candidate is a `policy.py`: the code that decides what gets injected
//! into every turn's context. The plugin invokes it per `prompt/context`:
//!
//!   stdin : {"cwd","session_id","recent_user_text","available_sources","gray_home"}
//!   stdout: {"inject":[{"title","text"},…], "notes"?:str}
//!
//! Contract enforcement is deliberately narrow: valid JSON out, `inject` a
//! list of {title,text}. Everything else — which sources, how it ranks,
//! whether it calls an LLM — is what the search is *for*. The 10s timeout
//! and fail-open exist because a harness hook must never break a turn
//! (a crashing policy would take the whole session's context with it).

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// One policy invocation: python3 <policy> with `payload` on stdin.
/// Returns the parsed stdout object.
pub fn run(policy: &Path, payload: &Value, timeout: Duration) -> Result<Value> {
    let mut cmd = Command::new("python3");
    cmd.arg(policy);
    let out = crate::proc::run(
        cmd,
        Some(payload.to_string().into_bytes()),
        timeout,
    )
    .context("spawn python3")?;
    if out.timed_out {
        bail!("policy timed out after {}s", timeout.as_secs());
    }
    let text = out.stdout_text();
    let v: Value = serde_json::from_str(text.trim()).with_context(|| {
        format!(
            "policy stdout is not a JSON object: {:.200}",
            text.trim()
        )
    })?;
    if !v.is_object() {
        bail!("policy stdout must be a JSON object");
    }
    Ok(v)
}

/// Contract smoke test: run the candidate against a representative stdin
/// and require `inject` to be a list of {title,text} objects. This is the
/// "validate_candidates" step of the reference loop — cheap, syntactic,
/// catches policies that crash or emit garbage before they waste an eval.
pub fn validate(policy: &Path, home: &Path, timeout: Duration) -> Result<()> {
    if !policy.is_file() {
        bail!("{} does not exist", policy.display());
    }
    let payload = stdin_payload("/tmp", "validate", "what is the codeword", home);
    let v = run(policy, &payload, timeout)?;
    check_output(&v)
}

/// Structural check on a policy's stdout value.
pub fn check_output(v: &Value) -> Result<()> {
    let inject = v
        .get("inject")
        .and_then(Value::as_array)
        .context("\"inject\" must be a list")?;
    for (i, item) in inject.iter().enumerate() {
        if item.get("title").and_then(Value::as_str).is_none()
            || item.get("text").and_then(Value::as_str).is_none()
        {
            bail!("inject[{i}] needs string \"title\" and \"text\"");
        }
    }
    Ok(())
}

/// The stdin payload every policy sees. `gray_home` is an extension over
/// the paper-minimal contract: policies need it to find the stores they
/// rank (eval homes intentionally differ from the real home).
pub fn stdin_payload(cwd: &str, session_id: &str, recent: &str, home: &Path) -> Value {
    json!({
        "cwd": cwd,
        "session_id": session_id,
        "recent_user_text": recent,
        "available_sources": available_sources(home),
        "gray_home": home,
    })
}

/// Probe which injection sources exist under `home`. Kept honest — a
/// policy can check this instead of blindly opening paths that may not
/// exist in a fresh eval home.
pub fn available_sources(home: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let has_files = |dir: &Path, pred: fn(&Path) -> bool| -> bool {
        std::fs::read_dir(dir)
            .map(|it| it.flatten().any(|e| pred(&e.path())))
            .unwrap_or(false)
    };
    let mem = home.join("memory");
    if has_files(&mem, |p| p.extension().is_some_and(|e| e == "md")) {
        out.push("memory".to_string());
    }
    if home.join("harness/state.json").is_file()
        || has_files(&home.join("learn"), |p| {
            p.extension().is_some_and(|e| e == "md")
        })
    {
        out.push("learn".to_string());
    }
    if home.join("recall").is_dir() {
        out.push("recall".to_string());
    }
    // Files are always available — the policy runs with filesystem access.
    out.push("files".to_string());
    out
}

/// Merge a policy's inject list into the `{text}` we hand the host. One
/// titled block per item under a shared header; empty list → no context.
pub fn render_inject(v: &Value) -> String {
    let Some(items) = v.get("inject").and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for item in items {
        let title = item.get("title").and_then(Value::as_str).unwrap_or("context");
        let text = item.get("text").and_then(Value::as_str).unwrap_or("");
        if text.trim().is_empty() {
            continue;
        }
        if out.is_empty() {
            out.push_str("## Injected context\n");
        }
        out.push_str("\n### ");
        out.push_str(title);
        out.push('\n');
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::io::Write;

    fn write_policy(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("policy.py");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn good_policy_parses() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_policy(
            dir.path(),
            r#"import sys, json
inp = json.load(sys.stdin)
print(json.dumps({"inject":[{"title":"t","text":"hello"}],"notes":"ok"}))
"#,
        );
        let payload = stdin_payload("/tmp", "s", "hi", dir.path());
        let v = run(&p, &payload, Duration::from_secs(10)).unwrap();
        assert_eq!(v["inject"][0]["text"], "hello");
        assert!(check_output(&v).is_ok());
        let rendered = render_inject(&v);
        assert!(rendered.contains("### t"));
        assert!(rendered.contains("hello"));
    }

    #[test]
    fn empty_inject_renders_nothing() {
        let v = json!({"inject":[]});
        assert_eq!(render_inject(&v), "");
    }

    #[test]
    fn bad_json_fails() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_policy(dir.path(), "print('not json')\n");
        let payload = stdin_payload("/tmp", "s", "", dir.path());
        assert!(run(&p, &payload, Duration::from_secs(10)).is_err());
    }

    #[test]
    fn wrong_shape_fails_validation() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_policy(
            dir.path(),
            r#"import json
print(json.dumps({"inject":[{"title":"t"}]}))  # missing text
"#,
        );
        assert!(validate(&p, dir.path(), Duration::from_secs(10)).is_err());
    }

    #[test]
    fn crash_fails_validation() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_policy(dir.path(), "import sys; sys.exit(3)\n");
        // exit!=0 still prints whatever; contract only sees stdout — a
        // crashing policy that prints nothing fails the JSON parse.
        assert!(validate(&p, dir.path(), Duration::from_secs(10)).is_err());
    }

    #[test]
    fn hang_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_policy(dir.path(), "import time; time.sleep(30)\n");
        let payload = stdin_payload("/tmp", "s", "", dir.path());
        let r = run(&p, &payload, Duration::from_millis(300));
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("timed out"));
    }

    #[test]
    fn sources_detected() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(available_sources(dir.path()), vec!["files"]);
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        std::fs::write(dir.path().join("memory/project-x.md"), "- a: b\n").unwrap();
        let s = available_sources(dir.path());
        assert!(s.contains(&"memory".to_string()));
        assert!(s.contains(&"files".to_string()));
    }
}

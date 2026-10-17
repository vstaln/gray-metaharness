//! The proposer step: one `gray -p` session pointed at the metaharness
//! directory. In the paper the proposer is a strong coding agent with
//! filesystem access to every prior artifact — ours is gray itself, run in
//! the user's real home so its full tool set applies, with cwd pinned to
//! the search root. The prompt (proposer_prompt.md, user-editable in the
//! meta dir) tells it what to read and where to write; the loop only
//! checks that candidates/<next>/policy.py appeared.

use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;

use crate::domain::Domain;

/// Fallback template when the user hasn't customized their copy.
pub const TEMPLATE: &str = include_str!("../proposer_prompt.md");

/// User-editable copy wins; embedded template is the default.
pub fn template(meta: &Path) -> String {
    std::fs::read_to_string(meta.join("proposer_prompt.md")).unwrap_or_else(|_| TEMPLATE.to_string())
}

/// Render the proposer prompt for one iteration.
pub fn render(meta: &Path, next_id: &str, iteration: u32) -> String {
    let cand_dir = crate::paths::candidate_dir(meta, next_id);
    template(meta)
        .replace("{META_DIR}", &meta.display().to_string())
        .replace("{NEXT_ID}", next_id)
        .replace("{CANDIDATE_DIR}", &cand_dir.display().to_string())
        .replace("{ITERATION}", &iteration.to_string())
}

/// Spawn the proposer session. Blocks until it exits or hits the timeout.
/// Returns Ok(true) when the new policy file exists afterwards.
pub async fn propose(domain: &Domain, meta: &Path, main_home: &Path, prompt: &str, log_name: &str) -> Result<bool> {
    let mut cmd = tokio::process::Command::new(&domain.gray_bin);
    cmd.arg("-p")
        .arg(prompt)
        .arg("--max-requests")
        .arg(domain.proposer_max_requests.to_string());
    if let Some(m) = &domain.proposer_model {
        cmd.arg("--model").arg(m);
    }
    cmd.current_dir(meta)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // The proposer is a *user* session: real home, full tools. Only the
    // home is pinned — everything else inherits like a normal `gray -p`.
    for (k, _) in std::env::vars() {
        if k.starts_with("GRAY_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("GRAY_HOME", main_home);

    let timeout = Duration::from_secs(domain.proposer_timeout_secs);
    let child = cmd.spawn().context("spawn proposer")?;
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            anyhow::bail!("proposer timed out after {}s", timeout.as_secs())
        }
    };
    // Keep a log per proposer session — when a proposal is bad you want to
    // see what it actually said, not just the file it left behind.
    let logdir = meta.join("proposer_logs");
    let _ = std::fs::create_dir_all(&logdir);
    let _ = std::fs::write(
        logdir.join(log_name),
        format!(
            "=== stdout ===\n{}\n=== stderr ===\n{}\n=== exit {:?} ===\n",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            out.status.code()
        ),
    );
    Ok(true)
}

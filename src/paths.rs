//! Filesystem layout for the search. Every artifact lives under
//! `<GRAY_HOME>/metaharness/` as plain files and directories — the paper's
//! proposer is a filesystem agent: it learns by reading prior candidates,
//! scores and traces, not through an API (arXiv:2603.28052). Ordinary files
//! are also the cheapest thing to debug when the loop goes wrong.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// `~/.gray`, overridable with `GRAY_HOME` (same rule as the host).
pub fn gray_home() -> Result<PathBuf> {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".gray")))
        .ok_or_else(|| {
            anyhow::anyhow!("cannot resolve home: set GRAY_HOME or the platform user profile")
        })
}

/// The search root: `<home>/metaharness`.
pub fn meta_dir(home: &Path) -> PathBuf {
    home.join("metaharness")
}

pub fn candidates_dir(meta: &Path) -> PathBuf {
    meta.join("candidates")
}

/// One candidate's directory: `candidates/<id>/` holding `policy.py`,
/// `score.json`, `hypothesis.txt`, and `trace/`.
pub fn candidate_dir(meta: &Path, id: &str) -> PathBuf {
    candidates_dir(meta).join(id)
}

/// Per-(candidate, task) isolated GRAY_HOMEs for eval turns.
pub fn eval_root(meta: &Path) -> PathBuf {
    meta.join("evalhome")
}

/// The policy serving `prompt/context` in the real home: a *copy* of the
/// applied candidate, so editing/applying candidates never mutates a live
/// file mid-turn.
pub fn live_path(meta: &Path) -> PathBuf {
    meta.join("live.py")
}

/// `live.id` records which candidate id `live.py` was copied from (content
/// comparison breaks when two candidates are byte-identical).
pub fn live_id_path(meta: &Path) -> PathBuf {
    meta.join("live.id")
}

pub fn best_path(meta: &Path) -> PathBuf {
    meta.join("best.json")
}

/// Append-only score log — the `evolution_summary.jsonl` analog.
pub fn history_path(meta: &Path) -> PathBuf {
    meta.join("history.jsonl")
}

/// Serialize `run` invocations — two loops writing candidates/ at once
/// corrupts the numbering.
pub fn run_lock_path(meta: &Path) -> PathBuf {
    meta.join("run.lock")
}

/// Resolve a candidate id arg: a bare number ("3" → "003") or a directory
/// name that already exists.
pub fn resolve_id(meta: &Path, arg: &str) -> Result<String> {
    let id = if arg.chars().all(|c| c.is_ascii_digit()) {
        format!("{arg:0>3}")
    } else {
        arg.to_string()
    };
    if candidate_dir(meta, &id).is_dir() {
        Ok(id)
    } else {
        anyhow::bail!(
            "no candidate {arg:?} under {}",
            candidates_dir(meta).display()
        )
    }
}

/// The gray binary used for eval/proposer spawns.
pub fn gray_bin() -> String {
    std::env::var("GRAY_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "gray".to_string())
}

/// Path to this binary, for eval-home launcher scripts.
pub fn self_exe() -> Result<PathBuf> {
    std::env::current_exe().context("cannot resolve current exe")
}

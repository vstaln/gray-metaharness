//! Candidate bookkeeping: numbered dirs, per-candidate score.json, the
//! append-only history log, best pointer, and the live copy. Scores append
//! to `history.jsonl` rather than a database — the proposer greps it like
//! every other artifact, and a log survives partial runs.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub struct Candidate {
    pub id: String,
    pub dir: PathBuf,
}

impl Candidate {
    pub fn policy(&self) -> PathBuf {
        self.dir.join("policy.py")
    }
    pub fn score_path(&self) -> PathBuf {
        self.dir.join("score.json")
    }
    pub fn trace_dir(&self) -> PathBuf {
        self.dir.join("trace")
    }
    pub fn hypothesis(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join("hypothesis.txt"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
    pub fn score(&self) -> Option<Value> {
        std::fs::read(&self.score_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
    }
    pub fn score_f64(&self) -> Option<f64> {
        self.score().and_then(|v| v["score"].as_f64())
    }
}

/// All `candidates/<id>/` dirs that contain a policy.py, sorted by id.
pub fn list(meta: &Path) -> Vec<Candidate> {
    let dir = crate::paths::candidates_dir(meta);
    let mut out: Vec<Candidate> = std::fs::read_dir(&dir)
        .map(|it| {
            it.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| Candidate {
                    id: e.file_name().to_string_lossy().into_owned(),
                    dir: e.path(),
                })
                .filter(|c| c.policy().is_file())
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Candidates with no score.json yet — what `run` picks up first.
pub fn unscored(meta: &Path) -> Vec<Candidate> {
    list(meta)
        .into_iter()
        .filter(|c| !c.score_path().is_file())
        .collect()
}

/// Next candidate id: max numeric id + 1, zero-padded.
pub fn next_id(meta: &Path) -> String {
    let max = std::fs::read_dir(crate::paths::candidates_dir(meta))
        .map(|it| {
            it.flatten()
                .filter_map(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .parse::<u32>()
                        .ok()
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    // Seed lives at 000; if candidates/ is empty the first proposer
    // iteration writes 001.
    if max == 0 && !crate::paths::candidate_dir(meta, "000").is_dir() {
        "000".to_string()
    } else {
        format!("{:03}", max + 1)
    }
}

/// Persist one candidate's score record and append the history row.
pub fn record_score(meta: &Path, cand: &Candidate, rec: &Value) -> Result<()> {
    std::fs::create_dir_all(&cand.dir)?;
    let mut v = rec.clone();
    v["candidate"] = json!(cand.id);
    std::fs::write(
        cand.score_path(),
        serde_json::to_string_pretty(&v)?,
    )?;
    let mut row = json!({
        "candidate": cand.id,
        "score": v["score"],
        "at": v["evaluated_at"],
    });
    if let Some(h) = cand.hypothesis() {
        row["hypothesis"] = json!(h);
    }
    if let Some(t) = v.get("tasks") {
        row["tasks"] = t.clone();
    }
    let mut line = serde_json::to_string(&row)?;
    line.push('\n');
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::history_path(meta))?;
    f.write_all(line.as_bytes())?;
    Ok(())
}

/// Recompute best.json from all scored candidates; returns (id, score).
pub fn update_best(meta: &Path) -> Option<(String, f64)> {
    let best = list(meta)
        .into_iter()
        .filter_map(|c| c.score_f64().map(|s| (c.id, s)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    if let Some((id, score)) = &best {
        let rec = json!({
            "candidate": id,
            "score": score,
            "updated_at": now_iso(),
        });
        let _ = std::fs::write(
            crate::paths::best_path(meta),
            serde_json::to_string_pretty(&rec).unwrap_or_default(),
        );
    }
    best
}

/// Which candidate live.py currently serves (from the live.id marker).
pub fn live_id(meta: &Path) -> Option<String> {
    std::fs::read_to_string(crate::paths::live_id_path(meta))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Copy a candidate's policy to live.py + record the id.
pub fn apply(meta: &Path, id: &str) -> Result<()> {
    let cand = crate::paths::candidate_dir(meta, id);
    let src = cand.join("policy.py");
    if !src.is_file() {
        anyhow::bail!("candidate {id} has no policy.py");
    }
    std::fs::copy(&src, crate::paths::live_path(meta))
        .with_context(|| format!("cannot copy {}", src.display()))?;
    std::fs::write(crate::paths::live_id_path(meta), format!("{id}\n"))?;
    Ok(())
}

pub fn now_iso() -> String {
    // Seconds-since-epoch ISO is fine for logs; keep dep-free.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

/// Status table text shared by `status`, `/metaharness status`, and the
/// `metaharness_status` tool.
pub fn status_text(meta: &Path) -> String {
    let cands = list(meta);
    if cands.is_empty() {
        return format!(
            "metaharness: no candidates under {} — run `gray metaharness init`",
            crate::paths::candidates_dir(meta).display()
        );
    }
    let live = live_id(meta);
    let best = std::fs::read(crate::paths::best_path(meta))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["candidate"].as_str().map(str::to_string));
    let mut out = String::from("candidate  score   marks    hypothesis\n");
    for c in &cands {
        let score = c
            .score_f64()
            .map(|s| format!("{s:.3}"))
            .unwrap_or_else(|| {
                if c.score().is_some() {
                    "err".to_string()
                } else {
                    "—".to_string()
                }
            });
        let mut marks = String::new();
        if live.as_deref() == Some(c.id.as_str()) {
            marks.push_str("LIVE ");
        }
        if best.as_deref() == Some(c.id.as_str()) {
            marks.push_str("BEST");
        }
        let hyp = c.hypothesis().unwrap_or_default();
        let hyp_short: String = hyp.chars().take(52).collect();
        out.push_str(&format!(
            "{:<10} {:<7} {:<8} {hyp_short}\n",
            c.id, score, marks.trim_end()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_candidate(meta: &Path, id: &str, policy: &str) {
        let d = crate::paths::candidate_dir(meta, id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("policy.py"), policy).unwrap();
    }

    #[test]
    fn list_and_next_id() {
        let t = tempfile::tempdir().unwrap();
        let meta = t.path();
        assert_eq!(next_id(meta), "000");
        mk_candidate(meta, "000", "# seed\n");
        mk_candidate(meta, "002", "# b\n");
        let ids: Vec<_> = list(meta).into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["000", "002"]);
        assert_eq!(next_id(meta), "003");
    }

    #[test]
    fn record_score_appends_history_and_best() {
        let t = tempfile::tempdir().unwrap();
        let meta = t.path();
        mk_candidate(meta, "000", "# a\n");
        mk_candidate(meta, "001", "# b\n");
        let c0 = &list(meta)[0];
        let c1 = &list(meta)[1];
        record_score(meta, c0, &json!({"score":0.5,"tasks":{"t":0.5}})).unwrap();
        record_score(meta, c1, &json!({"score":0.9,"tasks":{"t":0.9}})).unwrap();
        assert_eq!(unscored(meta).len(), 0);
        let hist = std::fs::read_to_string(crate::paths::history_path(meta)).unwrap();
        assert_eq!(hist.lines().count(), 2);
        let (id, s) = update_best(meta).unwrap();
        assert_eq!((id.as_str(), s), ("001", 0.9));
        // Re-scoring 000 lower doesn't move best.
        record_score(meta, c0, &json!({"score":0.4,"tasks":{"t":0.4}})).unwrap();
        let (id, _) = update_best(meta).unwrap();
        assert_eq!(id, "001");
    }

    #[test]
    fn apply_marks_live() {
        let t = tempfile::tempdir().unwrap();
        let meta = t.path();
        mk_candidate(meta, "000", "print(1)\n");
        apply(meta, "000").unwrap();
        assert_eq!(live_id(meta).as_deref(), Some("000"));
        assert_eq!(
            std::fs::read_to_string(crate::paths::live_path(meta)).unwrap(),
            "print(1)\n"
        );
        assert!(apply(meta, "099").is_err());
    }

    #[test]
    fn resolve_ids() {
        let t = tempfile::tempdir().unwrap();
        let meta = t.path();
        mk_candidate(meta, "007", "# x\n");
        assert_eq!(crate::paths::resolve_id(meta, "7").unwrap(), "007");
        assert!(crate::paths::resolve_id(meta, "8").is_err());
    }
}

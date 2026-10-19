//! The `gray metaharness <verb>` surface. Handlers return text so the same
//! verbs serve the CLI and the `/metaharness` wire command. The outer loop
//! itself is the paper's shape: score pending candidates (the inner loop)
//! → proposer reads all artifacts and writes the next candidate → repeat.

use anyhow::{Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::domain::Domain;
use crate::{evals, paths, policy, propose, store};

const SEED_POLICY: &str = include_str!("../seed/policy.py");

/// `--iterations N` parsing shared by `run` and the wire path.
pub fn parse_iterations(args: &[String]) -> Result<u32> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--iterations" | "-n" => {
                let v = it.next().context("--iterations needs a value")?;
                return v
                    .parse::<u32>()
                    .with_context(|| format!("bad --iterations {v:?}"));
            }
            s if s.starts_with("--iterations=") => {
                return s[13..]
                    .parse::<u32>()
                    .with_context(|| format!("bad --iterations {s:?}"));
            }
            other => anyhow::bail!("unknown flag {other:?} (expected --iterations N)"),
        }
    }
    Ok(1)
}

/// `init` — lay down the search root with a seed candidate, example tasks,
/// and an editable proposer prompt. Never overwrites: a second init only
/// fills in what's missing.
pub fn init() -> Result<String> {
    let home = paths::gray_home()?;
    let meta = paths::meta_dir(&home);
    let mut made: Vec<String> = Vec::new();
    let mut put = |path: &Path, contents: &str, mode_700: bool| -> Result<()> {
        if path.exists() {
            return Ok(());
        }
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(path, contents)?;
        #[cfg(unix)]
        if mode_700 {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(not(unix))]
        let _ = mode_700;
        made.push(path.display().to_string());
        Ok(())
    };

    std::fs::create_dir_all(paths::candidates_dir(&meta))?;
    put(&meta.join("domain.toml"), Domain::TEMPLATE, false)?;
    put(&meta.join("proposer_prompt.md"), propose::TEMPLATE, false)?;

    // Seed candidate 000 — the baseline the search starts from.
    let seed_dir = paths::candidate_dir(&meta, "000");
    put(&seed_dir.join("policy.py"), SEED_POLICY, false)?;
    put(&seed_dir.join("hypothesis.txt"),
        "seed baseline: inject all learn/memory entries, ranked by keyword overlap with recent user text",
        false)?;

    // Two example tasks proving injection works end to end.
    for (name, task_md, setup, check) in example_tasks() {
        let d = meta.join("tasks").join(name);
        put(&d.join("task.md"), task_md, false)?;
        put(&d.join("setup.sh"), setup, true)?;
        put(&d.join("check.sh"), check, true)?;
    }

    // live = the seed until the search earns something better.
    if !paths::live_path(&meta).exists() {
        store::apply(&meta, "000")?;
        made.push(paths::live_path(&meta).display().to_string());
    }

    let mut out = format!("metaharness initialized at {}\n", meta.display());
    for m in made {
        out.push_str(&format!("  + {m}\n"));
    }
    out.push_str(
        "\nNext: `gray metaharness run --iterations 1` scores the seed, \
         proposes candidate 001, and scores it. `gray metaharness status` shows the table.",
    );
    Ok(out)
}

fn example_tasks() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    vec![
        (
            "recall-fact",
            "What is this project's codeword? Reply with only the codeword, nothing else.\n",
            r#"#!/bin/sh
# Plant a fact only the candidate's injection can surface. Without it the
# eval agent can only guess — that's what makes the score attributable.
mkdir -p "$GRAY_HOME/harness"
cat > "$GRAY_HOME/harness/state.json" <<'JSON'
{"version":1,"scope":"global","entries":{"memory":{"codeword":{"id":"codeword","kind":"memory","title":"Project codeword","content":"The project codeword is ZEPHYR-42.","path":"lessons","source":"eval-seed","created_at":0,"updated_at":0,"version":1}},"prompt":{},"skill":{},"subagent":{}},"refinements":[]}
JSON
"#,
            r#"#!/bin/sh
# Last stdout line is the score. GRAY_ANSWER holds the agent's final text.
if grep -qi "ZEPHYR-42" "$GRAY_ANSWER" 2>/dev/null; then
  echo "found codeword in answer"
  echo 1.0
else
  echo "codeword missing"
  echo 0.0
fi
"#,
        ),
        (
            "prefer-convention",
            "Which package manager should be used to install dependencies in this workspace? Answer with one word only.\n",
            r#"#!/bin/sh
mkdir -p "$GRAY_HOME/harness"
cat > "$GRAY_HOME/harness/state.json" <<'JSON'
{"version":1,"scope":"global","entries":{"memory":{"pkg-manager":{"id":"pkg-manager","kind":"memory","title":"Package manager convention","content":"In this workspace the package manager is pnpm — never npm or yarn.","path":"lessons","source":"eval-seed","created_at":0,"updated_at":0,"version":1}},"prompt":{},"skill":{},"subagent":{}},"refinements":[]}
JSON
"#,
            r#"#!/bin/sh
if grep -qiw "pnpm" "$GRAY_ANSWER" 2>/dev/null; then
  echo "agent picked pnpm"
  echo 1.0
else
  echo "expected pnpm"
  echo 0.0
fi
"#,
        ),
    ]
}

/// `status` — the candidate table plus what's live/best.
pub fn status() -> Result<String> {
    let home = paths::gray_home()?;
    let meta = paths::meta_dir(&home);
    Ok(store::status_text(&meta))
}

/// `apply <id>` — promote a candidate to live.py.
pub fn apply(id: &str) -> Result<String> {
    let home = paths::gray_home()?;
    let meta = paths::meta_dir(&home);
    let id = paths::resolve_id(&meta, id)?;
    store::apply(&meta, &id)?;
    Ok(format!("candidate {id} is now live ({})", paths::live_path(&meta).display()))
}

/// Score every unscored candidate; returns how many were evaluated.
async fn score_unscored(
    meta: &Path,
    home: &Path,
    domain: &Domain,
) -> Result<u32> {
    let pending = store::unscored(meta);
    let mut n = 0;
    for cand in pending {
        // A candidate without a contract-valid policy can't be meaningfully
        // benchmarked — score it 0 with the reason, so the proposer sees
        // the failure in artifacts instead of an invisible skip.
        if let Err(e) = policy::validate(
            &cand.policy(),
            home,
            Duration::from_secs(domain.policy_timeout_secs),
        ) {
            eprintln!("  {} fails contract: {e:#} — scoring 0", cand.id);
            let rec = json!({
                "score": 0.0,
                "tasks": {},
                "error": format!("policy contract: {e:#}"),
                "evaluated_at": store::now_iso(),
            });
            store::record_score(meta, &cand, &rec)?;
            n += 1;
            continue;
        }
        eprintln!("  evaluating candidate {} on all tasks...", cand.id);
        let rec = evals::eval_candidate(meta, home, domain, &cand, |name, score| {
            eprintln!("    {name}: {score:.3}");
        })
        .await?;
        let s = rec["score"].as_f64().unwrap_or(0.0);
        eprintln!("  candidate {} score = {s:.3}", cand.id);
        store::record_score(meta, &cand, &rec)?;
        n += 1;
    }
    Ok(n)
}

/// `eval <id>` — re-score one candidate without proposing (manual iteration).
pub fn eval_one(id: &str) -> Result<String> {
    let home = paths::gray_home()?;
    let meta = paths::meta_dir(&home);
    let domain = Domain::load(&meta)?;
    let id = paths::resolve_id(&meta, id)?;
    let cand = store::list(&meta)
        .into_iter()
        .find(|c| c.id == id)
        .context("candidate vanished")?;
    let meta2 = meta.clone();
    let home2 = home.clone();
    let rec = runtime()?.block_on(evals::eval_candidate(
        &meta2,
        &home2,
        &domain,
        &cand,
        |name, score| eprintln!("    {name}: {score:.3}"),
    ))?;
    store::record_score(&meta, &cand, &rec)?;
    let best = store::update_best(&meta);
    Ok(format!(
        "candidate {id} score = {:.3}{}",
        rec["score"].as_f64().unwrap_or(0.0),
        best.map(|(b, s)| format!("  (best: {b} @ {s:.3})"))
            .unwrap_or_default()
    ))
}

/// `run [--iterations N]` — the outer loop.
pub fn run(args: &[String]) -> Result<String> {
    let iterations = parse_iterations(args)?;
    let home = paths::gray_home()?;
    let meta = paths::meta_dir(&home);
    let domain = Domain::load(&meta)?;

    // One loop at a time; a stale lock is reported, not silently honored.
    let lock = paths::run_lock_path(&meta);
    if lock.exists() {
        anyhow::bail!(
            "{} exists — another run is active (or a stale lock; remove it to proceed)",
            lock.display()
        );
    }
    std::fs::write(&lock, format!("pid {}\n", std::process::id()))?;
    let guard = LockGuard(lock.clone());
    let out = runtime()?.block_on(run_loop(&meta, &home, &domain, iterations));
    drop(guard);
    out
}

struct LockGuard(PathBuf);
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The loop proper: catch up on unscored candidates (baseline phase), then
/// per iteration: proposer → score the new candidate. Faithful to the
/// paper where it matters: artifacts on the filesystem, proposer outside
/// the scoring path, one evaluated candidate per iteration.
async fn run_loop(
    meta: &Path,
    home: &Path,
    domain: &Domain,
    iterations: u32,
) -> Result<String> {
    let start = Instant::now();
    let budget = Duration::from_secs(domain.budget_secs);
    let mut log = String::new();

    let caught = score_unscored(meta, home, domain).await?;
    if caught > 0 {
        log.push_str(&format!("scored {caught} pending candidate(s)\n"));
        store::update_best(meta);
    }

    for i in 0..iterations {
        if start.elapsed() > budget {
            log.push_str("budget exhausted; stopping before next proposal\n");
            break;
        }
        let next = store::next_id(meta);
        let prompt = propose::render(meta, &next, i + 1);
        eprintln!("proposing candidate {next} (iteration {})...", i + 1);
        match propose::propose(domain, meta, home, &prompt, &format!("iter{:03}.log", i + 1)).await {
            Ok(_) => {
                let pol = paths::candidate_dir(meta, &next).join("policy.py");
                if pol.is_file() {
                    eprintln!("  proposer wrote {}", pol.display());
                } else {
                    eprintln!("  proposer did not write {next}/policy.py — nothing to score");
                    log.push_str(&format!("iteration {}: proposer produced no policy.py\n", i + 1));
                    continue;
                }
            }
            Err(e) => {
                eprintln!("  proposer failed: {e:#}");
                log.push_str(&format!("iteration {}: proposer error: {e:#}\n", i + 1));
                continue;
            }
        }
        score_unscored(meta, home, domain).await?;
        store::update_best(meta);
    }

    log.push_str(&store::status_text(meta));
    Ok(log)
}

/// Detached `run`/`eval` for the wire path — a sidecar can't block on a
/// multi-hour loop. Output lands in meta/run.log.
pub fn spawn_detached(verb: &str, args: &[String]) -> Result<String> {
    let home = paths::gray_home()?;
    let meta = paths::meta_dir(&home);
    let exe = paths::self_exe()?;
    let log_path = meta.join("run.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let err = log.try_clone()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(verb)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(err);
    // Detach from the sidecar's process group so plugin shutdown doesn't
    // take the loop with it.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP.
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let child = cmd.spawn().context("spawn detached")?;
    Ok(format!(
        "metaharness {verb} started detached (pid {}) — progress in {}",
        child.id(),
        log_path.display()
    ))
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("tokio runtime")
}

/// Wire `command/run` entry: argv without the command name → reply {text}.
pub fn wire_command(argv: &[String]) -> Result<serde_json::Value> {
    let (verb, rest) = match argv.split_first() {
        None => return Ok(json!({ "text": usage() })),
        Some((v, r)) => (v.as_str(), r),
    };
    let text = match verb {
        "status" => status()?,
        "apply" => {
            let id = rest.first().context("apply needs a candidate id")?;
            apply(id)?
        }
        "init" => init()?,
        "eval" => {
            let _ = rest.first().context("eval needs a candidate id")?;
            return spawn_detached("eval", rest).map(|t| json!({ "text": t }));
        }
        "run" => return spawn_detached("run", rest).map(|t| json!({ "text": t })),
        _ => usage(),
    };
    Ok(json!({ "text": text }))
}

pub fn usage() -> String {
    "metaharness — outer-loop search over the context-injection harness\n\
     verbs: init · run [--iterations N] · status · apply <id> · eval <id>"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iterations_parse() {
        assert_eq!(parse_iterations(&[]).unwrap(), 1);
        assert_eq!(
            parse_iterations(&["--iterations".into(), "5".into()]).unwrap(),
            5
        );
        assert_eq!(parse_iterations(&["--iterations=3".into()]).unwrap(), 3);
        assert_eq!(parse_iterations(&["-n".into(), "2".into()]).unwrap(), 2);
        assert!(parse_iterations(&["--bogus".into()]).is_err());
        assert!(parse_iterations(&["--iterations".into()]).is_err());
    }
}

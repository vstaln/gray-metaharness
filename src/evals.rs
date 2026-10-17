//! The inner loop: score one candidate by running each eval task under it.
//!
//! The key trick (and the thing that makes scores attributable to the
//! candidate rather than the incumbent): every eval turn spawns
//! `gray -p --json` with `GRAY_HOME=<metaharness>/evalhome/<cand>/<task>`,
//! an isolated home whose `metaharness/live.py` is a symlink to *the
//! candidate's* policy.py. The eval profile carries only
//! `builtin: tools-minimal` + this sidecar, so the only context injection
//! the eval agent sees is what the candidate chose. Provider creds come
//! from a config.json snapshot of the real home — same pattern the
//! telegram/matrix adapters use for per-conversation homes.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::domain::Domain;
use crate::paths;
use crate::store::Candidate;

/// One eval task: `tasks/<name>/task.md` + optional `setup.sh` + `check.sh`.
pub struct Task {
    pub name: String,
    pub dir: PathBuf,
}

impl Task {
    pub fn prompt(&self) -> Result<String> {
        std::fs::read_to_string(self.dir.join("task.md"))
            .with_context(|| format!("no task.md in {}", self.dir.display()))
    }
    pub fn setup(&self) -> PathBuf {
        self.dir.join("setup.sh")
    }
    pub fn check(&self) -> PathBuf {
        self.dir.join("check.sh")
    }
}

/// Discover tasks: subdirs of tasks_dir containing task.md, sorted.
pub fn discover(tasks_dir: &Path) -> Vec<Task> {
    let mut out: Vec<Task> = std::fs::read_dir(tasks_dir)
        .map(|it| {
            it.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter(|e| e.path().join("task.md").is_file())
                .map(|e| Task {
                    name: e.file_name().to_string_lossy().into_owned(),
                    dir: e.path(),
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Build `<evalhome>`: config snapshot, candidate live.py symlink, work
/// dir with the eval gray.yml and the sidecar launcher.
fn prepare_home(
    eval_home: &Path,
    main_home: &Path,
    policy: &Path,
    exe: &Path,
    domain: &Domain,
) -> Result<PathBuf> {
    std::fs::create_dir_all(eval_home)?;
    // Provider creds: snapshot the real home's config (+ auth.json when the
    // provider keeps OAuth tokens there). Never write creds into traces.
    for f in ["config.json", "auth.json"] {
        let src = main_home.join(f);
        if src.is_file() {
            std::fs::copy(&src, eval_home.join(f))
                .with_context(|| format!("cannot copy {}", src.display()))?;
        }
    }
    // A plugin-backed selection can't resolve here — no sidecars exist in
    // the eval home, so its provider_id fails the turn before the model
    // runs. Strip the plugin identity (and pin the eval endpoint/model
    // when configured); api_key + base_url then drive a plain HTTP turn.
    let cfg_path = eval_home.join("config.json");
    if let Ok(mut cfg) =
        serde_json::from_str::<serde_json::Map<String, Value>>(
            &std::fs::read_to_string(&cfg_path).unwrap_or_default(),
        )
    {
        for k in ["provider_id", "credential_source", "auth_ref"] {
            cfg.remove(k);
        }
        cfg.insert("credential_source".to_string(), json!(""));
        if let Some(url) = &domain.inner_base_url {
            cfg.insert("base_url".to_string(), json!(url));
        }
        if let Some(m) = &domain.inner_model {
            cfg.insert("model".to_string(), json!(m));
        }
        std::fs::write(&cfg_path, serde_json::to_string_pretty(&cfg)?)?;
    }
    // The candidate serves as this home's live policy.
    let meta = eval_home.join("metaharness");
    std::fs::create_dir_all(&meta)?;
    let live = meta.join("live.py");
    let _ = std::fs::remove_file(&live);
    std::os::unix::fs::symlink(policy, &live)
        .with_context(|| format!("cannot symlink {}", live.display()))?;

    // Dedicated workdir: bounds project discovery and gives check.sh a cwd.
    let work = eval_home.join("work");
    std::fs::create_dir_all(&work)?;
    let _ = std::fs::create_dir(work.join(".git"));

    // Launcher script forwards args so the host can probe `manifest` and
    // then spawn `sidecar` (same launcher pattern as the channel adapters).
    let launcher = eval_home.join("metaharness-sidecar");
    std::fs::write(&launcher, format!("#!/bin/sh\nexec {} \"$@\"\n", exe.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::write(
        work.join("gray.yml"),
        format!(
            "plugins:\n  - builtin: tools-minimal\n  - sidecar: {}\n",
            launcher.display()
        ),
    )?;
    Ok(work)
}

/// Env shared by setup.sh / check.sh: where the home, trace, and answer live.
fn task_env(
    cmd: &mut Command,
    eval_home: &Path,
    task: &Task,
    cand: &Candidate,
    work: &Path,
    answer: &Path,
    trace: &Path,
) {
    cmd.env("GRAY_HOME", eval_home)
        .env("TASK_DIR", &task.dir)
        .env("CAND_DIR", &cand.dir)
        .env("WORK", work)
        .env("GRAY_ANSWER", answer)
        .env("GRAY_TRACE", trace)
        .current_dir(work);
}

/// Run the task's scorer (its check.sh, or domain.scorer_cmd when set) and
/// parse the last stdout line as a score in [0,1]. Unparseable or failed
/// scorer = 0.0 — the paper counts broken evals as 0 rather than dropping
/// them, which keeps the history honest.
fn score_task(
    task: &Task,
    domain: &Domain,
    eval_home: &Path,
    cand: &Candidate,
    work: &Path,
    answer: &Path,
    trace: &Path,
) -> (f64, String) {
    let mut cmd = if let Some(sc) = &domain.scorer_cmd {
        let mut c = Command::new("sh");
        c.arg("-c").arg(sc);
        c
    } else {
        if !task.check().is_file() {
            return (0.0, "no check.sh and no scorer_cmd".to_string());
        }
        let mut c = Command::new("sh");
        c.arg(task.check());
        c
    };
    task_env(&mut cmd, eval_home, task, cand, work, answer, trace);
    match crate::proc::run(cmd, None, Duration::from_secs(120)) {
        Ok(out) => {
            let text = out.stdout_text();
            let last = text
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string();
            let score = last
                .parse::<f64>()
                .map(|s| s.clamp(0.0, 1.0))
                .unwrap_or(0.0);
            (score, text)
        }
        Err(e) => (0.0, format!("scorer spawn failed: {e}")),
    }
}

/// Extract the final answer text from `gray -p --json` NDJSON: the last
/// row with type=="result". Empty string when the turn errored.
fn extract_answer(raw: &str) -> String {
    let mut text = String::new();
    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) == Some("result") {
            text = v
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
    }
    text
}

/// Score `cand` on every task; write trace files under the candidate's
/// trace/ and return the score record. Never fails the whole candidate on
/// one task's error — that task scores 0 and the trace shows why.
pub async fn eval_candidate(
    meta: &Path,
    main_home: &Path,
    domain: &Domain,
    cand: &Candidate,
    on_task: impl Fn(&str, f64),
) -> Result<Value> {
    let tasks_dir = domain.tasks_path(meta);
    let tasks = discover(&tasks_dir);
    if tasks.is_empty() {
        anyhow::bail!("no eval tasks under {}", tasks_dir.display());
    }
    let trace_dir = cand.trace_dir();
    std::fs::create_dir_all(&trace_dir)?;
    let exe = paths::self_exe()?;
    let policy = cand
        .policy()
        .canonicalize()
        .unwrap_or_else(|_| cand.policy());

    // One auth.json read per candidate: the eval turns' GRAY_API_KEY.
    let api_key = domain
        .inner_auth_key
        .as_deref()
        .and_then(|name| read_auth_key(main_home, name));

    let mut per_task = serde_json::Map::new();
    for task in &tasks {
        let eval_home = paths::eval_root(meta).join(&cand.id).join(&task.name);
        if eval_home.exists() {
            std::fs::remove_dir_all(&eval_home)?;
        }
        let score = match prepare_home(&eval_home, main_home, &policy, &exe, domain) {
            Ok(work) => {
                let trace_file = trace_dir.join(format!("{}.ndjson", task.name));
                let answer_file = trace_dir.join(format!("{}.answer.txt", task.name));
                // Optional per-task fixture seeding (e.g. plant a fact the
                // policy must surface).
                if task.setup().is_file() {
                    let mut c = Command::new("sh");
                    c.arg(task.setup());
                    task_env(
                        &mut c,
                        &eval_home,
                        task,
                        cand,
                        &work,
                        &answer_file,
                        &trace_file,
                    );
                    let _ = crate::proc::run(c, None, Duration::from_secs(120));
                }
                match run_turn(task, domain, &eval_home, &work, api_key.as_deref()).await {
                    Ok(raw) => {
                        std::fs::write(&trace_file, &raw)?;
                        let answer = extract_answer(&raw);
                        std::fs::write(&answer_file, &answer)?;
                        let (score, check_out) = score_task(
                            task, domain, &eval_home, cand, &work, &answer_file, &trace_file,
                        );
                        std::fs::write(
                            trace_dir.join(format!("{}.check.txt", task.name)),
                            &check_out,
                        )?;
                        score
                    }
                    Err(e) => {
                        std::fs::write(&trace_file, format!("turn failed: {e:#}\n"))?;
                        std::fs::write(&answer_file, "")?;
                        0.0
                    }
                }
            }
            Err(e) => {
                eprintln!("  task {}: prepare_home failed: {e:#}", task.name);
                0.0
            }
        };
        on_task(&task.name, score);
        per_task.insert(task.name.clone(), json!(score));
    }

    let n = per_task.len().max(1) as f64;
    let mean = per_task
        .values()
        .filter_map(Value::as_f64)
        .sum::<f64>()
        / n;
    Ok(json!({
        "score": (mean * 1000.0).round() / 1000.0,
        "tasks": Value::Object(per_task),
        "inner_model": domain.inner_model,
        "evaluated_at": crate::store::now_iso(),
    }))
}

/// Read `auth.json[name]` — flat `{"<name>": "<key>"}` entries only;
/// envelope values (plugin creds) don't apply to api-key eval turns.
fn read_auth_key(home: &Path, name: &str) -> Option<String> {
    let auth: serde_json::Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(home.join("auth.json")).ok()?).ok()?;
    auth.get(name)?.as_str().map(str::to_string)
}

/// One eval turn: `gray -p <task.md> --json` under the isolated home.
async fn run_turn(
    task: &Task,
    domain: &Domain,
    eval_home: &Path,
    work: &Path,
    api_key: Option<&str>,
) -> Result<String> {
    let prompt = task.prompt()?;
    let mut cmd = tokio::process::Command::new(&domain.gray_bin);
    cmd.arg("-p")
        .arg(&prompt)
        .arg("--json")
        .arg("--max-requests")
        .arg(domain.max_requests.to_string());
    if let Some(m) = &domain.inner_model {
        cmd.arg("--model").arg(m);
    }
    cmd.current_dir(work)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Inherit the environment but strip GRAY_* so nothing leaks the caller's
    // home/session into the eval (same env hygiene as the adapters).
    for (k, _) in std::env::vars() {
        if k.starts_with("GRAY_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("GRAY_HOME", eval_home)
        .env("GRAY_SHOW_REASONING", "0")
        .env("GRAY_MAX_WALL_SECS", domain.eval_timeout_secs.to_string());
    // The snapshot's provider_id may name a plugin sidecar this home lacks;
    // explicit base/key env beats the copied config outright.
    if let Some(url) = &domain.inner_base_url {
        cmd.env("GRAY_BASE_URL", url);
    }
    if let Some(key) = api_key {
        cmd.env("GRAY_API_KEY", key);
    }

    let timeout = Duration::from_secs(domain.eval_timeout_secs + 60);
    let child = cmd.spawn().context("spawn gray")?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => anyhow::bail!("eval turn timed out after {}s", timeout.as_secs()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_finds_task_dirs() {
        let t = tempfile::tempdir().unwrap();
        let tasks = t.path().join("tasks");
        std::fs::create_dir_all(tasks.join("b-task")).unwrap();
        std::fs::create_dir_all(tasks.join("a-task")).unwrap();
        std::fs::create_dir_all(tasks.join("not-a-task")).unwrap();
        std::fs::write(tasks.join("b-task/task.md"), "b").unwrap();
        std::fs::write(tasks.join("a-task/task.md"), "a").unwrap();
        let found = discover(&tasks);
        assert_eq!(
            found.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["a-task", "b-task"]
        );
    }

    #[test]
    fn answer_extracts_last_result_row() {
        let raw = "{\"protocol\":1,\"type\":\"progress\",\"text\":\"working\"}\n\
                   {\"protocol\":1,\"type\":\"result\",\"text\":\"the answer is ZEPHYR-42\"}\n";
        assert_eq!(extract_answer(raw), "the answer is ZEPHYR-42");
        assert_eq!(extract_answer("garbage\n"), "");
    }

    #[test]
    fn scorer_parses_last_line_and_clamps() {
        let t = tempfile::tempdir().unwrap();
        let meta = t.path();
        let home = t.path().join("home");
        let cand_dir = paths::candidate_dir(meta, "000");
        std::fs::create_dir_all(&cand_dir).unwrap();
        std::fs::write(cand_dir.join("policy.py"), "x").unwrap();
        let cand = Candidate {
            id: "000".into(),
            dir: cand_dir,
        };
        let task_dir = t.path().join("task");
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("check.sh"), "echo noise\necho 1.5\n").unwrap();
        let task = Task {
            name: "t".into(),
            dir: task_dir,
        };
        let domain = Domain::default();
        let (score, _) = score_task(
            &task,
            &domain,
            &home,
            &cand,
            t.path(),
            &t.path().join("a.txt"),
            &t.path().join("tr"),
        );
        assert_eq!(score, 1.0);

        std::fs::write(task.dir.join("check.sh"), "echo garbage\n").unwrap();
        let (score, _) = score_task(
            &task,
            &domain,
            &home,
            &cand,
            t.path(),
            &t.path().join("a.txt"),
            &t.path().join("tr"),
        );
        assert_eq!(score, 0.0);
    }

    #[test]
    fn prepare_home_links_candidate_and_profile() {
        let t = tempfile::tempdir().unwrap();
        let main_home = t.path().join("main");
        std::fs::create_dir_all(&main_home).unwrap();
        std::fs::write(main_home.join("config.json"), "{\"model\":\"m\"}").unwrap();
        let policy = t.path().join("policy.py");
        std::fs::write(&policy, "print('{}')").unwrap();
        let eval_home = t.path().join("evalhome/000/t");
        let exe = Path::new("/bin/true");
        let work = prepare_home(&eval_home, &main_home, &policy, exe, &Domain::default()).unwrap();
        assert!(eval_home.join("config.json").is_file());
        let live = eval_home.join("metaharness/live.py");
        assert!(live.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(live.canonicalize().unwrap(), policy.canonicalize().unwrap());
        let profile = std::fs::read_to_string(work.join("gray.yml")).unwrap();
        assert!(profile.contains("builtin: tools-minimal"));
        assert!(profile.contains("sidecar:"));
        assert!(eval_home.join("metaharness-sidecar").is_file());
    }
}

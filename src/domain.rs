//! `domain.toml` — the eval configuration. The proposer may read it but the
//! loop never lets it write here: fixed evaluation code is what keeps
//! scores comparable across candidates (ONBOARDING.md: "Keep evaluation
//! code fixed").

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Domain {
    /// Task directory, relative to the metaharness dir unless absolute.
    pub tasks_dir: String,
    /// Model for eval turns (`--model` on the inner `gray -p`). Empty = the
    /// home's configured default.
    pub inner_model: Option<String>,
    /// Base URL for eval turns (env `GRAY_BASE_URL`). Pin this with
    /// inner_model or a plugin-backed config snapshot's provider_id can
    /// hijack provider resolution and the turn dies before the model runs.
    #[serde(default)]
    pub inner_base_url: Option<String>,
    /// auth.json entry the eval turns' `GRAY_API_KEY` reads from — the
    /// name, never the secret. Paired with inner_base_url.
    #[serde(default)]
    pub inner_auth_key: Option<String>,
    /// Model for the proposer `gray -p`. Empty = the home's default.
    pub proposer_model: Option<String>,
    /// Wall-clock budget for one `run` invocation.
    pub budget_secs: u64,
    /// Timeout per eval turn (per task per candidate).
    pub eval_timeout_secs: u64,
    /// Timeout for one proposer session.
    pub proposer_timeout_secs: u64,
    /// Timeout for one `policy.py` invocation inside `prompt/context`.
    pub policy_timeout_secs: u64,
    /// --max-requests cap for eval turns.
    pub max_requests: u32,
    /// --max-requests cap for the proposer.
    pub proposer_max_requests: u32,
    /// Optional per-task scorer override; when unset each task's own
    /// `check.sh` is used.
    pub scorer_cmd: Option<String>,
    /// gray binary for child spawns.
    pub gray_bin: String,
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            tasks_dir: "tasks".to_string(),
            inner_model: None,
            inner_base_url: None,
            inner_auth_key: None,
            proposer_model: None,
            budget_secs: 4 * 3600,
            eval_timeout_secs: 600,
            proposer_timeout_secs: 2400,
            policy_timeout_secs: 10,
            max_requests: 60,
            proposer_max_requests: 200,
            scorer_cmd: None,
            gray_bin: crate::paths::gray_bin(),
        }
    }
}

impl Domain {
    /// Absolute tasks dir (relative paths anchor at the metaharness dir).
    pub fn tasks_path(&self, meta: &Path) -> PathBuf {
        let p = PathBuf::from(&self.tasks_dir);
        if p.is_absolute() {
            p
        } else {
            meta.join(p)
        }
    }

    pub fn load(meta: &Path) -> Result<Self> {
        let path = meta.join("domain.toml");
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "no {} — run `gray metaharness init` first",
                path.display()
            )
        })?;
        toml::from_str(&text).with_context(|| format!("bad {}", path.display()))
    }

    /// The template written by `init`. Models stay commented out so the
    /// home's configured defaults apply until the user opts into a split
    /// inner/proposer setup.
    pub const TEMPLATE: &'static str = r#"# gray-metaharness eval configuration.
# The proposer may READ this file; the loop never lets it write.

# Directory of eval tasks (relative to ~/.gray/metaharness).
tasks_dir = "tasks"

# Model for eval turns and for the proposer. Uncomment to pin; otherwise
# the home's configured default model is used.
#inner_model = "openai/gpt-5-mini"
#proposer_model = "anthropic/claude-opus-4-6"
# Eval turns spawn `gray -p` in an isolated home: pin the endpoint and the
# auth.json entry name (never the key itself) so a plugin-backed selection
# in the copied config can't shadow them.
#inner_base_url = "https://api.stepfun.ai/step_plan/v1"
#inner_auth_key = "stepfun-ai-step-plan"

# Budgets (seconds).
budget_secs = 14400            # wall clock for one `run`
eval_timeout_secs = 600        # per eval turn (candidate x task)
proposer_timeout_secs = 2400   # per proposer session
policy_timeout_secs = 10       # per policy.py call inside prompt/context

# Request caps for the spawned `gray -p` turns.
max_requests = 60
proposer_max_requests = 200

# Optional: a single scorer command used INSTEAD of each task's check.sh.
# Run per task with GRAY_HOME / GRAY_ANSWER / GRAY_TRACE / TASK_DIR / CAND_DIR
# in the environment; last stdout line must be a score in [0,1].
#scorer_cmd = "python3 ~/.gray/metaharness/score.py"

# Which gray binary the loop spawns.
#gray_bin = "gray"
"#;
}

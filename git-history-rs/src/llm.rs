//! An optional narrator, backed by a local model.
//!
//! The digest in `model.rs` is exact: it counts what the log states and can
//! be checked against it. A language model cannot make that promise, so it
//! never replaces the digest -- it is offered alongside it, on request, and
//! labelled as generated.
//!
//! # Why HTTP and not bindings
//!
//! Every serious local runtime -- Ollama, llama.cpp's `llama-server`, LM
//! Studio, Jan -- exposes the same OpenAI-shaped `/v1/chat/completions`
//! endpoint. Speaking that over HTTP costs nothing here (`ureq` is already a
//! dependency), keeps the binary small, and lets the model change without a
//! recompile. In-process bindings (`llama-cpp-2`, `candle`, `mistral.rs`)
//! would drag a C++/CUDA toolchain and hundreds of megabytes into a tool
//! whose entire premise is that it starts instantly.
//!
//! Loopback only. A project's commit subjects are not somebody else's
//! business, and the tool stays usable with the network unplugged.
//!
//! # Partitioning: what is actually possible
//!
//! A dense transformer cannot be partitioned per-inference. Every token
//! passes through every layer; there is no subset of a 7B model that answers
//! "what happened Tuesday". Claims otherwise usually mean one of four real
//! things, and only some of them are ours to implement:
//!
//! 1. **Mixture-of-experts models** genuinely activate a fraction of their
//!    parameters per token -- Qwen3-30B-A3B runs ~3B active of 30B total.
//!    That is real intra-model partitioning, but it is a property of the
//!    weights you choose, not something a client can impose. It is a model
//!    recommendation, and it composes freely with everything below.
//! 2. **Layer offload** (`--n-gpu-layers`) splits *where* layers run, not
//!    whether they run. It changes speed, not work.
//! 3. **Prefix / KV caching** avoids recomputing a shared prompt head. Real,
//!    and free: keep the system prompt byte-identical across calls and every
//!    runtime here reuses its cache.
//! 4. **Partitioning the work rather than the weights** -- which is the one
//!    that fits day-end logs, and what this module does.
//!
//! ## The day partition
//!
//! A week of commits decomposes cleanly by day, and this is what makes it
//! worth doing: **a past day is immutable**. Once Tuesday is over, Tuesday's
//! commits never change, so Tuesday's summary never needs recomputing.
//!
//! So narration is a map-reduce:
//!
//! ```text
//!   Sun..Sat commits ─┬─ day pass (small model) ─┐
//!                     ├─ day pass (small model) ─┼─→ week pass (main model)
//!                     └─ day pass (small model) ─┘
//! ```
//!
//! At day end only the newest day is unseen; the rest are served from cache.
//! The marginal cost of asking "where is this project at" each evening is
//! therefore one short inference over one day of subjects, not one long
//! inference over the whole week.
//!
//! The two passes may use different models (`GIT_HISTORY_MODEL_SMALL` for
//! the per-day work, `GIT_HISTORY_MODEL` for the synthesis) -- a cascade, so
//! the cheap repetitive pass need not pay for the capable model.
//!
//! ## Why this cache is allowed to exist
//!
//! The deterministic digest is deliberately never cached: it is a claim about
//! the present and a stored one ages into a lie. This cache is different in
//! kind. Its key is the **content** -- a hash of the exact commit hashes
//! summarised, plus the model and prompt version. It cannot go stale, because
//! any change to the input produces a different key. Time never invalidates
//! it; only different facts do.

use crate::model::{Commit, WeekDigest};
use chrono::Datelike;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Bump when a prompt changes, so old cached text is not reused for a new
/// question.
const PROMPT_VERSION: &str = "v1";

/// Loopback only, in the order a machine is likely to be running one.
const ENDPOINTS: &[(&str, &str)] = &[
    ("ollama", "http://127.0.0.1:11434/v1/chat/completions"),
    ("llama.cpp", "http://127.0.0.1:8080/v1/chat/completions"),
    ("lm studio", "http://127.0.0.1:1234/v1/chat/completions"),
    ("jan", "http://127.0.0.1:1337/v1/chat/completions"),
];

/// Kept byte-identical across every call so the runtime's prefix cache can
/// serve the shared head of the prompt instead of recomputing it.
const SYSTEM: &str = "You summarise software commit logs. You state only what \
                      the given subjects say. You never invent detail, motive, \
                      or quality judgements.";

#[derive(Debug, Clone)]
pub struct Narrator {
    pub runtime: String,
    pub endpoint: String,
    /// The capable model, used once per week for the synthesis.
    pub model: String,
    /// The cheap model, used once per day. Falls back to `model`.
    pub small: String,
}

/// One day's worth of narration.
#[derive(Debug, Clone)]
pub struct DayLine {
    pub label: String,
    pub text: String,
    /// True when this line came from cache -- i.e. no inference was run.
    pub reused: bool,
}

#[derive(Debug, Clone)]
pub struct Narration {
    pub days: Vec<DayLine>,
    pub week: String,
    pub computed: usize,
    pub reused: usize,
    pub model: String,
}

/// Probe loopback for a runtime.
///
/// Deliberately short timeouts: this runs at startup, and a tool that pauses
/// to discover an absent optional feature has already spent more than the
/// feature is worth. A refused connection returns immediately, so the common
/// case -- no model installed -- costs microseconds.
pub fn detect() -> Option<Narrator> {
    let configured_model = std::env::var("GIT_HISTORY_MODEL").ok();
    let small = std::env::var("GIT_HISTORY_MODEL_SMALL").ok();

    if let Ok(custom) = std::env::var("GIT_HISTORY_LLM") {
        let model = configured_model.unwrap_or_else(|| "llama3.2".into());
        return Some(Narrator {
            runtime: "configured".into(),
            endpoint: custom,
            small: small.unwrap_or_else(|| model.clone()),
            model,
        });
    }

    for (runtime, endpoint) in ENDPOINTS {
        let models_url = endpoint.replace("/chat/completions", "/models");
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(400)))
            .build()
            .into();
        let Ok(mut resp) = agent.get(&models_url).call() else {
            continue;
        };
        // A 200 with a model list is the only acceptable proof: 8080 is a
        // popular port, and something else answering there is not a model.
        let Ok(body) = resp.body_mut().read_json::<serde_json::Value>() else {
            continue;
        };
        let first = body
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|a| a.first())
            .and_then(|m| m.get("id"))
            .and_then(|i| i.as_str());
        if let Some(id) = first {
            let model = configured_model.clone().unwrap_or_else(|| id.to_string());
            return Some(Narrator {
                runtime: runtime.to_string(),
                endpoint: endpoint.to_string(),
                small: small.clone().unwrap_or_else(|| model.clone()),
                model,
            });
        }
    }
    None
}

fn cache_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .or_else(|_| std::env::var("XDG_CACHE_HOME"))
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.cache")))
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("git-history").join("narration")
}

/// Content-addressed: the key is what was summarised, never when.
fn cache_key(model: &str, kind: &str, parts: &[String]) -> String {
    let mut h = Sha256::new();
    h.update(PROMPT_VERSION.as_bytes());
    h.update(model.as_bytes());
    h.update(kind.as_bytes());
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn cache_read(key: &str) -> Option<String> {
    std::fs::read_to_string(cache_dir().join(format!("{key}.txt"))).ok()
}

fn cache_write(key: &str, text: &str) {
    let dir = cache_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join(format!("{key}.txt")), text);
    }
}

impl Narrator {
    fn ask(&self, model: &str, prompt: &str, budget_secs: u64) -> Result<String, String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(budget_secs)))
            .build()
            .into();

        let body = serde_json::json!({
            "model": model,
            "messages": [
                { "role": "system", "content": SYSTEM },
                { "role": "user", "content": prompt }
            ],
            "temperature": 0.2,
            "stream": false,
        });

        let mut resp = agent
            .post(&self.endpoint)
            .send_json(&body)
            .map_err(|e| format!("{} did not answer: {e}", self.runtime))?;

        let value: serde_json::Value = resp
            .body_mut()
            .read_json()
            .map_err(|e| format!("unreadable reply from {}: {e}", self.runtime))?;

        value
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("{} returned no text", self.runtime))
    }

    /// Map: one line per day that had commits.
    ///
    /// Each day is keyed by its own commit hashes, so a finished day is
    /// answered from disk forever after. At day end exactly one of these
    /// runs.
    fn narrate_day(&self, day: &str, commits: &[&Commit]) -> (String, bool) {
        let shas: Vec<String> = commits.iter().map(|c| c.sha.clone()).collect();
        let key = cache_key(&self.small, "day", &shas);
        if let Some(hit) = cache_read(&key) {
            return (hit, true);
        }

        let subjects: Vec<String> = commits
            .iter()
            .take(30)
            .map(|c| format!("- {}", c.subject))
            .collect();
        let prompt = format!(
            "Commit subjects from {day}:\n{}\n\nIn ONE short clause, say what \
             this day's work was about. No preamble, no full stop needed.",
            subjects.join("\n")
        );

        match self.ask(&self.small, &prompt, 45) {
            Ok(text) => {
                let text = text.lines().next().unwrap_or("").trim().to_string();
                cache_write(&key, &text);
                (text, false)
            }
            // A failed day degrades to the commit subjects themselves rather
            // than failing the whole narration.
            Err(_) => (
                commits
                    .first()
                    .map(|c| c.subject.clone())
                    .unwrap_or_default(),
                false,
            ),
        }
    }

    /// Reduce: the week, over the day lines rather than over every commit.
    ///
    /// The synthesis reads seven short clauses instead of a hundred subjects,
    /// which is what keeps the capable model's context -- and its cost --
    /// small regardless of how busy the week was.
    pub fn narrate(&self, d: &WeekDigest) -> Result<Narration, String> {
        if d.commits.is_empty() {
            return Ok(Narration {
                days: Vec::new(),
                week: "Nothing to narrate: no commits in this window.".into(),
                computed: 0,
                reused: 0,
                model: self.model.clone(),
            });
        }

        // Bucket by calendar day, oldest first.
        let mut buckets: BTreeMap<String, Vec<&Commit>> = BTreeMap::new();
        for c in &d.commits {
            let Some(date) = c.date else { continue };
            buckets
                .entry(date.format("%Y-%m-%d").to_string())
                .or_default()
                .push(c);
        }

        let mut days = Vec::new();
        let (mut computed, mut reused) = (0, 0);
        for (date, commits) in &buckets {
            let label = commits
                .first()
                .and_then(|c| c.date)
                .map(|dt| format!("{} {}", dt.weekday(), dt.format("%b %d")))
                .unwrap_or_else(|| date.clone());
            let (text, from_cache) = self.narrate_day(&label, commits);
            if from_cache { reused += 1 } else { computed += 1 }
            days.push(DayLine { label, text, reused: from_cache });
        }

        let day_lines: Vec<String> = days
            .iter()
            .map(|d| format!("- {}: {}", d.label, d.text))
            .collect();

        let week_key = cache_key(&self.model, "week", &day_lines);
        if let Some(hit) = cache_read(&week_key) {
            return Ok(Narration {
                days,
                week: hit,
                computed,
                reused: reused + 1,
                model: self.model.clone(),
            });
        }

        let prompt = format!(
            "Project: {repo}\nWeek beginning {start}. {n} commits over {active} \
             active days.\n\nWhat happened each day:\n{lines}\n\nIn at most two \
             sentences, say what this week of work was about. Describe only what \
             these lines state. No preamble.",
            repo = d.repo,
            start = d.start.format("%B %d"),
            n = d.commits.len(),
            active = d.days_active(),
            lines = day_lines.join("\n"),
        );

        let week = self.ask(&self.model, &prompt, 90)?;
        cache_write(&week_key, &week);
        Ok(Narration { days, week, computed: computed + 1, reused, model: self.model.clone() })
    }
}

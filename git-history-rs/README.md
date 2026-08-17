# git history (Rust)

A terminal browser for commit logs, across local clones and GitHub. Rust
rewrite of `git_history_native.py`; the reasoning behind the shared
algorithms is in [`../ALGORITHMS.md`](../ALGORITHMS.md).

```
git-history                 browse the working root (Z:\), or the current
                            directory if it is not mounted
git-history -d C:\code      a directory of clones
git-history ObviousThr33s   a GitHub account  (the only networked mode)
git-history --check         print everything found, no terminal needed
git-history -h              usage
```

| key | |
|---|---|
| `↑↓` `jk` | move |
| `⏎` | open |
| `w` | the week |
| `p` `n` | step the week back / forward |
| `m` | narrate with a local model, if one is running |
| `/` | filter |
| `esc` | back |
| `q` | back one frame; quits only at the repo list |

## Offline

Nothing here touches the network unless you name a GitHub account.

* No update check, no telemetry, no hostname resolved at startup.
* The default path reads the `git` binary and the filesystem. That is all.
* The model narrator addresses `127.0.0.1` only.
* `cargo build --offline` works from the local registry cache. For a machine
  that has never had network, run `tools\vendor.ps1` first.

## Local models

The digest is counted from the log and is exact. A model can only ever add
prose *beside* it, on request (`m`), labelled as generated.

Connection is plain HTTP to whatever is already running on loopback —
Ollama, `llama-server`, LM Studio, Jan — all of which speak
`/v1/chat/completions`. No bindings: `llama-cpp-2`/`candle`/`mistral.rs`
would drag a C++/CUDA toolchain and hundreds of megabytes into a tool that
is 3 MB and starts instantly.

```
GIT_HISTORY_LLM          endpoint override; otherwise loopback is probed
GIT_HISTORY_MODEL        model for the weekly synthesis
GIT_HISTORY_MODEL_SMALL  model for the per-day passes
```

### Partitioning the work, not the weights

A dense transformer cannot be partly executed. Every token passes through
every layer; there is no subset of a 7B model that answers "what happened
Tuesday". Four things are meant when people say otherwise:

1. **Mixture-of-experts weights** genuinely activate a fraction of their
   parameters per token — Qwen3-30B-A3B runs ~3B active of 30B. That is real,
   but it is a property of the model you pull, not something a client
   imposes. Use one; it composes with everything below.
2. **Layer offload** (`--n-gpu-layers`) decides *where* layers run, not
   whether. Speed, not work.
3. **Prefix/KV caching** skips recomputing a shared prompt head. Free here:
   the system prompt is byte-identical across every call, so the runtime
   reuses its cache.
4. **Partitioning the work** — the one that fits day-end logs, and what this
   does.

A week decomposes by day, and the point is that **a past day is immutable**.
Once Tuesday ends, Tuesday's commits never change, so Tuesday's summary never
needs recomputing:

```
Sun..Sat commits ─┬─ day pass (small model) ─┐
                  ├─ day pass (small model) ─┼──→ week pass (main model)
                  └─ day pass (small model) ─┘
```

At day end exactly one day is unseen; the rest are served from disk. Asking
"where is this project at" each evening costs one short inference over one
day of subjects — not one long inference over the whole week. The synthesis
then reads seven short clauses instead of a hundred subjects, so the capable
model's context stays small however busy the week was. The UI marks each day
line `▸` when it was inferred and `·` when it was reused.

**Why this cache is allowed to exist**, when the digest is deliberately never
cached: its key is the *content* — a hash of the exact commit hashes
summarised, plus model and prompt version. It cannot go stale, because
different facts produce a different key. Time never invalidates it.

## Layout

| file | |
|---|---|
| `model.rs` | projects, commits, the week digest. Pure; all the tests live here |
| `source.rs` | `Source` trait, local git subprocess, GitHub over `ureq` |
| `sigil.rs` | the per-project mark |
| `llm.rs` | optional narrator, day-partitioned |
| `ui.rs` | ratatui screens |

`cargo test` covers the parts that can be wrong quietly: word-boundary
classification, Sunday alignment, the empty and closed-week sentences, state
selection, and that the sigil still matches the Python implementation
byte for byte.

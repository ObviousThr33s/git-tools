# git-tools — a terminal browser for commit logs, written twice

One tool: a full-screen browser for commit history, over either a directory of local
clones or a GitHub account. It exists twice on purpose — a stdlib-only Python original
that freezes to a single file, and a Rust rewrite that shares its reasoning. The
algorithms are the same in both; only the tongue differs.

## Running it

```
.\run.bat                       browse the clones in the current directory
.\run.bat ObviousThr33s         browse a GitHub account (the only networked mode)
.\run.bat -h                    the full usage
```

The Rust build takes the same arguments, plus `--check` to print everything it finds
without needing a terminal at all.

## The rooms

| what | where |
|---|---|
| the law | `ALGORITHMS.md` — how it thinks, stated as reasoning rather than as API notes |
| the Python original | `git_history_native.py` — stdlib only, so the frozen build stays one file |
| the Rust rewrite | `git-history-rs/` — same behaviour, its own README |
| the build | `build.ps1` cuts `dist\git_history_native.exe`; `run.bat` is what launches it |

## What this is not

Not a git client — it reads and never writes, so nothing here can change a repository
it is pointed at. Not a replacement for `git log` either: it is the view you want when
the question is *"what has been happening across all of these,"* and `git log` is still
the right answer inside one of them.

`[2026-08-06]`

# How it thinks

Notes on the algorithms behind `git_history_native.py`, written as reasoning
rather than as API documentation. Each section states the problem, the
constraint that shapes the answer, and the method that follows from it.

---

## 1. Measuring text that has no fixed width

**Problem.** Every frame is padded to an exact column count. Pad by
`len(string)` and the borders drift.

Two things break the identity `len(s) == columns(s)`:

- **ANSI escapes** — `\033[38;5;39m` is eight characters and zero columns.
- **Cell classes** — terminals place characters on a grid with three widths:
  zero (combining marks, `Mn`/`Me`/`Cf`), one (Latin), two (CJK, most emoji).

**Method.** Strip escapes with a regex, then sum a per-character width:

```
width(c) = 0  if combining(c) or category(c) ∈ {Mn, Me, Cf}
           2  if east_asian_width(c) ∈ {W, F}
           1  otherwise
```

Everything downstream — `fit`, `wrap`, `cells`, `pad`, `row` — budgets against
this function and never against `len`. Truncation accumulates width until the
next character would exceed the budget, so a double-wide glyph is dropped
whole rather than split into half a cell.

**Invariant.** For every screen and every width *W*, each rendered line
satisfies `visible_len(line) == W`. This is machine-checkable, and the test
harness asserts it across five widths on every screen — it is the single
property that keeps the frame square.

---

## 2. Wrapping, then reflowing

`wrap` is greedy: accumulate words while `used + 1 + w ≤ width`, break when
they don't fit, hard-break any single token longer than the measure.

Greedy wrapping alone produces a ragged comb on commit bodies, because git
convention already hard-wraps them at 72 columns. Re-wrapping pre-wrapped
text at a different measure alternates long and short lines.

**Method.** Reflow before wrapping. `paragraphs()` rejoins consecutive
non-blank lines into one logical block, *except* lines whose break carries
meaning:

- indented lines (code, quoted output)
- lines opening with `- `, `* `, `> ` (bullets, quotes)

Those pass through intact. Everything else is joined and re-wrapped to the
live measure.

---

## 3. Project marks

**Problem.** Give every project a distinct visual identity. No authored
assets, no network, stable across runs, unique per project.

**Rejected:** fetching each repository's avatar (network per render, and PNG
decoding the standard library won't do cheaply); hand-drawn per-language
logos (represents the language, not the project — four Rust projects share
one mark).

**Method.** An identicon, seeded by the project's name.

```
digest = sha256(name)                       deterministic, name-sensitive
for each cell (y, x) in the left half:
    lit = digest[(y·half + x) mod 32] mod 100 < FILL
    grid[y][x] = grid[y][size-1-x] = lit    ← mirror
```

Three decisions carry the result:

1. **Mirroring.** The mark is symmetric about its centre column. Symmetry is
   what the eye reads as deliberate; without it the same bits read as noise.
   An odd size (7) guarantees a true centre column rather than a seam.
2. **Fill ratio.** 44%. Below ~35% the figure reads as scattered dots, above
   ~55% as a solid block. Both extremes destroy distinctiveness.
3. **Half-blocks.** `▀ ▄ █` encode two grid rows per terminal row, so a 7×7
   figure occupies 4 lines. Each cell is drawn two columns wide, correcting
   for the ~2:1 aspect of a terminal cell so the mark reads square.

Colour is looked up by language when known, else drawn from a fixed palette
by hash — so the tint carries information where information exists, and stays
stable where it doesn't.

**Property.** Same name → same mark, forever, on any machine, offline.
Measured: 13 repositories produced 13 distinct marks.

---

## 4. Finding repositories

**Observation.** Projects are not all checked out at the same depth. On the
working root, `game_projects` is a repository *and* contains four more;
`Archive` is a plain container; `Archive/2026-07/Obelisk v0` sits two levels
down. A one-level scan found 1 of 13.

**Method.** Bounded depth-first walk, depth 3.

- A directory is a repository if it holds `.git/` (working clone) or
  `objects/` (bare).
- **Finding one is not a reason to stop descending** — nesting is the normal
  case here, not an anomaly.
- Skip dotted directories: tooling, not projects, and it prevents walking
  into `.git` itself.
- Unreadable directories are skipped silently — on a network share,
  permission walls are ordinary, not exceptional.

Repositories are named by their path relative to the root, so
`Archive/ARCADIA` and `game_projects/Obelisk v1` stay distinguishable.
Ordering is by last commit, descending: recency is what you're usually
looking for.

---

## 5. The week digest

**Problem.** "Where is this project at?" — answered offline, instantly.

**What this is not.** There is no model here. Nothing is generated. The
digest counts, groups, ranks and renders what the log already states. That
buys exactness, zero latency and full offline operation; the cost is that it
can describe *activity* but never *intent*. It reports that four commits
reworked things in `src/render`; it cannot tell you whether the refactor was
a good idea.

### 5.1 The window

Weeks start Sunday. Local midnight, not UTC — you experience weeks in your
own timezone.

```
start = midnight_today − ((weekday + 1) mod 7) days − 7·back days
```

`(weekday + 1) mod 7` converts Python's Monday-zero to Sunday-zero. On a
Sunday it evaluates to 0: the week has already turned, and the window opens
that morning.

Which exposes the real problem: **on Sunday morning the current week
describes nothing**. So the window steps backwards (`p`/`n`). A closed past
week is flagged, which changes two things:

- an `until` bound is added, so the window is Sunday-to-Sunday rather than
  open-ended;
- working-tree state is suppressed. Uncommitted files describe *now*.
  Printing them beside a past week would attribute today's edits to it.

### 5.2 Classification

Each subject line is bucketed by keyword: `fixed`, `removed`, `reworked`,
`documented`, `tested`, `packaged`, `added`, defaulting to `changed`.

The subject is tokenised to a **set of lowercase words** and intersected with
each bucket's triggers. Word-set intersection rather than substring search —
`"prefix"` contains `"fix"`, and substring matching would file it as a bug
fix.

**Order is the tie-breaker, and it encodes a judgement.** The first match
wins, so buckets are ordered specific-to-generic: "fix the broken test" is a
fix, not a test. This is a heuristic over English commit subjects, and it is
wrong on subjects that don't describe their change. Its output is always
presented as a count of subjects, never as a claim about the code.

### 5.3 Aggregation

- **Days.** `(commit_date − start).days`, bucketed 0–6, rendered as a
  sparkline over `▁▂▃▄▅▆▇█` scaled to the week's own peak — relative shape,
  not absolute volume. Empty days are `·`, not `▁`: zero should look like
  nothing, not like a little.
- **Areas.** The first path segment of each touched file, ranked by file
  count. It's the closest thing to a subject area available without knowing
  anything about the project.
- **Churn.** Additions and deletions per file, summed. `-` in numstat means
  binary; counted as 0 rather than crashing on `int("-")`.

### 5.4 State

One word for the week's shape, from commit count *n* and active days *d*:

| condition | word |
|---|---|
| `n = 0` | quiet |
| `d ≥ 5` | sustained |
| `n ≥ 12` | concentrated |
| `d ≥ 2` | moving |
| otherwise | touched |

`d` is tested before `n` deliberately: five commits across five days is a
different week from twelve in one afternoon, and the distinction matters more
than the volume.

### 5.5 Where the two sources diverge

Local clones give file statistics for free, so the digest reports where work
landed. The GitHub API bills **one request per commit** for file
statistics — a thirty-round-trip digest is not a digest. Over the API the
tool computes counts and classification only, and *says so on screen* rather
than quietly presenting a thinner answer as the same answer.

### 5.6 Never cached

Recomputed on every visit, including every step between weeks. A digest is a
claim about the present; a stored one ages silently into a lie. The
computation is a single `git log` against a local clone — cheap enough that
caching would trade correctness for nothing worth having.

---

## 6. Rendering

**Frames.** Clearing the screen between frames causes flicker: there is a
window where the terminal holds nothing. Instead, repaint from home
(`\033[H`), erase each line as it is rewritten (`\033[K`), then erase the
tail (`\033[J`). The screen is never empty.

**Layout.** Measured live every frame, so a resized window is tracked rather
than assumed. Width is clamped to 34–96 columns — a reading measure, not the
terminal's full width.

The side panel is a luxury of width: below 78 columns it is dropped and the
list takes every column. Where it is drawn, the horizontal rules carry `┬`
and `┴` junctions at the divider, so the columns read as one table rather
than two boxes that happen to be adjacent.

**Paging.** Commits load 100 at a time and fetch the next page when the
cursor reaches the end of the list — the scroll position *is* the trigger,
so no explicit "load more" is needed.

---

## 7. Failure

Every failure path was designed against one rule: **never invent data**.

The original version scraped profile HTML and, on any failure, returned a
hardcoded list of repository names as though they were real — the worst
possible outcome, because it is indistinguishable from success.

So: sources raise `SourceError` with a cause fit to show a person ("rate
limited by GitHub — set GITHUB_TOKEN to raise it"), the UI renders it in an
error card, and the app continues. Specifically:

- **404** → no such account or repository
- **403/429** → rate limited, with the fix named
- **network** → unreachable, with the reason
- **empty repository** → said plainly, not rendered as an empty log
- **offline share** → falls back to the current directory, *announcing that
  it did*

The frozen build taught the same lesson at the encoding layer: PyInstaller's
stdout inherits cp1252 when redirected, and every glyph in the frame lies
outside it, so the first border drawn raised `UnicodeEncodeError`. Fixed by
setting UTF-8 at startup — and it only surfaced because the binary was built
and run, not because the source was read.

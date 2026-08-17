//! The domain: projects, commits, and what a week of work amounts to.
//!
//! Nothing here touches the terminal or the network. The digest in
//! particular is pure: given commits and file churn it produces sentences,
//! which makes it the one part of the program that is trivially testable.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDateTime, TimeZone};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Repo {
    pub name: String,
    pub subtitle: String,
    pub updated: Option<DateTime<Local>>,
    pub extra: String,
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub sha: String,
    pub date: Option<DateTime<Local>>,
    pub author: String,
    pub subject: String,
    pub body: String,
}

impl Commit {
    pub fn short(&self) -> &str {
        let n = self.sha.len().min(7);
        &self.sha[..n]
    }
}

#[derive(Debug, Clone, Default)]
pub struct Detail {
    pub additions: i64,
    pub deletions: i64,
    pub files: Vec<(String, i64, i64)>,
    /// From `--summary`, keyed by final path. Absent for a source that cannot
    /// say. This is the only thing entitled to call a file new -- numstat
    /// alone cannot tell a created file from an appended one.
    pub marks: HashMap<String, Mark>,
}

// --- time ---------------------------------------------------------------

pub fn parse_iso(text: &str) -> Option<DateTime<Local>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(text)
        .map(|d| d.with_timezone(&Local))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
                .ok()
                .and_then(|n| Local.from_local_datetime(&n).single())
        })
}

pub fn stamp(dt: Option<DateTime<Local>>) -> String {
    match dt {
        Some(d) => d.format("%Y-%m-%d %H:%M").to_string(),
        None => "unknown date".into(),
    }
}

/// Coarse relative age -- precision past a week is noise in a list.
pub fn ago(dt: Option<DateTime<Local>>) -> String {
    let Some(d) = dt else { return String::new() };
    let secs = (Local::now() - d).num_seconds();
    if secs < 0 {
        return "just now".into();
    }
    for (span, unit) in [
        (31_536_000, "y"),
        (2_592_000, "mo"),
        (604_800, "w"),
        (86_400, "d"),
        (3_600, "h"),
        (60, "m"),
    ] {
        if secs >= span {
            return format!("{}{} ago", secs / span, unit);
        }
    }
    "just now".into()
}

/// Midnight on the Sunday that opened the week, local time.
///
/// On a Sunday the week has already turned: the window opens that morning,
/// not seven days earlier -- which is why the view can be stepped backwards.
/// A week one hour old describes nothing.
pub fn week_start(back: i64) -> DateTime<Local> {
    let now = Local::now();
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|n| Local.from_local_datetime(&n).single())
        .unwrap_or(now);
    // Weekday::num_days_from_sunday() gives Sunday = 0 directly.
    let offset = now.weekday().num_days_from_sunday() as i64;
    midnight - Duration::days(offset + 7 * back)
}

pub fn week_label(back: i64) -> String {
    match back {
        0 => "this week".into(),
        1 => "last week".into(),
        n => format!("{n} weeks back"),
    }
}

// --- classification -----------------------------------------------------

/// Ordered specific-to-generic: the first match wins, so "fix the broken
/// test" is a fix rather than a test. This is a heuristic over English
/// commit subjects, and its output is always presented as a count of
/// subjects -- never as a claim about the code.
const KINDS: &[(&str, &[&str])] = &[
    ("fixed", &["fix", "bug", "patch", "repair", "correct", "resolve",
                "hotfix", "broken", "crash", "regression"]),
    ("removed", &["remove", "delete", "drop", "strip", "prune", "purge"]),
    ("reworked", &["refactor", "rework", "rewrite", "simplify", "clean",
                   "rename", "restructure", "replace", "move", "extract",
                   "split", "merge", "tidy"]),
    ("documented", &["doc", "docs", "readme", "comment", "changelog", "license"]),
    ("tested", &["test", "tests", "spec", "coverage", "fixture"]),
    ("packaged", &["build", "ci", "deps", "dependency", "bump", "release",
                   "version", "package", "publish", "deploy"]),
    ("added", &["add", "create", "introduce", "implement", "new", "support",
                "give", "teach", "wire", "begin", "start", "init"]),
];

/// Match whole words, never substrings: "prefix" contains "fix", and
/// substring matching would file it as a bug fix.
pub fn classify(subject: &str) -> &'static str {
    let words: Vec<String> = subject
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect();
    for (kind, triggers) in KINDS {
        if words.iter().any(|w| triggers.contains(&w.as_str())) {
            return kind;
        }
    }
    "changed"
}

// --- what a file is, and which way it moved -----------------------------
//
// Everything here is derived from the path and the two churn numbers git
// already returns. No model, no second git call. The register is the
// digest's: it says what the log states and stops there. It names what a
// file *is* by convention, and which way its lines *moved* -- never what the
// code does, which is the one thing a path cannot tell you.

/// numstat spells a rename `old => new`, or `src/{a.rs => b.rs}` where a
/// prefix and suffix are shared. Only the destination is a real path, and
/// only the destination is a valid pathspec.
pub fn final_path(raw: &str) -> String {
    if let Some(open) = raw.find('{') {
        if let Some(rel) = raw[open..].find('}') {
            let close = open + rel;
            let inner = &raw[open + 1..close];
            let dest = match inner.split_once(" => ") {
                Some((_, to)) => to,
                None => inner,
            };
            // A rename into or out of the root leaves an empty half, which
            // would otherwise splice into a doubled separator.
            return format!("{}{}{}", &raw[..open], dest, &raw[close + 1..]).replace("//", "/");
        }
    }
    match raw.split_once(" => ") {
        Some((_, to)) => to.to_string(),
        None => raw.to_string(),
    }
}

/// The first path segment is the closest thing to a subject area without
/// knowing anything about the project.
pub fn area(path: &str) -> String {
    match path.split_once('/') {
        Some((head, _)) => head.to_string(),
        None => "(root)".to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Test, Docs, Build, Config, Shader, Markup, Data, Asset, Source, Other,
}

impl Role {
    /// One word, because it shares a row with the churn counts.
    pub fn tag(self) -> &'static str {
        match self {
            Role::Test => "test",
            Role::Docs => "docs",
            Role::Build => "build",
            Role::Config => "config",
            Role::Shader => "shader",
            Role::Markup => "markup",
            Role::Data => "data",
            Role::Asset => "asset",
            Role::Source => "source",
            Role::Other => "file",
        }
    }
}

/// Ordered specific-to-generic, first match wins -- the same discipline as
/// `KINDS`, for the same reason. Directory segments are compared whole and
/// never as substrings: `contest/` ends in `test` and `src/latest.rs` starts
/// with it, and substring matching would file both as tests. That is the
/// `prefix`/`fix` trap one directory up.
///
/// Segments are consulted before the extension, which is what makes
/// `.github/workflows/ci.yml` read as build rather than config.
pub fn role(path: &str) -> Role {
    let path = final_path(path);
    let lower = path.to_lowercase();
    let segments: Vec<&str> = lower.split('/').collect();
    let file = *segments.last().unwrap_or(&"");
    let stem = file.split('.').next().unwrap_or("");

    // 1. The file's own name.
    if stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file == "conftest.py"
    {
        return Role::Test;
    }

    // 2. Whole directory segments, outermost first, so tests/fixtures/big.json
    //    is a test rather than data.
    for seg in segments.iter().take(segments.len().saturating_sub(1)) {
        match *seg {
            "tests" | "test" | "spec" | "specs" | "__tests__" => return Role::Test,
            "docs" | "doc" | "man" => return Role::Docs,
            ".github" | "ci" | "tools" | "scripts" => return Role::Build,
            "assets" | "res" | "static" => return Role::Asset,
            "shaders" => return Role::Shader,
            "migrations" | "fixtures" => return Role::Data,
            _ => {}
        }
    }

    // 3. Names that carry their role without an extension.
    match stem {
        "readme" | "changelog" | "license" | "licence" | "copying" | "contributing"
        | "authors" | "notice" => return Role::Docs,
        "makefile" | "dockerfile" | "justfile" | "cmakelists" | "vagrantfile" => {
            return Role::Build
        }
        _ => {}
    }
    match file {
        "cargo.toml" | "cargo.lock" | "package.json" | "package-lock.json" | "go.mod"
        | "go.sum" | "pyproject.toml" | "requirements.txt" => return Role::Config,
        _ => {}
    }

    // 4. A dotfile is configuration by convention; it has a name, not an
    //    extension, so this must come before the extension table.
    if file.starts_with('.') {
        return Role::Config;
    }

    // 5. Extension.
    let ext = file.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "c" | "h" | "cpp" | "hpp"
        | "cs" | "java" | "kt" | "rb" | "swift" | "lua" | "zig" | "hs" | "ml" | "php"
        | "dart" | "scala" | "clj" | "ex" | "erl" | "f90" | "cob" => Role::Source,
        "md" | "rst" | "adoc" | "txt" | "tex" => Role::Docs,
        "toml" | "yaml" | "yml" | "json" | "ini" | "cfg" | "conf" | "lock" | "properties" => {
            Role::Config
        }
        "ps1" | "sh" | "bat" | "cmd" | "mk" | "bazel" | "gradle" | "spec" => Role::Build,
        "wgsl" | "glsl" | "hlsl" | "frag" | "vert" | "metal" | "shader" => Role::Shader,
        "html" | "htm" | "css" | "scss" | "sass" | "xaml" | "svelte" | "vue" => Role::Markup,
        "csv" | "tsv" | "sql" | "ndjson" | "parquet" => Role::Data,
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "ico" | "ttf" | "otf" | "woff" | "woff2"
        | "wav" | "ogg" | "mp3" | "glb" | "obj" | "pdf" => Role::Asset,
        _ => Role::Other,
    }
}

/// What `--summary` reported, when a source can report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Create,
    Delete,
    Rename,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Created, Deleted, Renamed, OnlyAdded, OnlyRemoved, Grew, Trimmed, Reworked, Unlined,
}

/// A mark is testimony and wins outright; without one, all that is left is
/// the ratio of the two counts.
pub fn shape(adds: i64, dels: i64, mark: Option<Mark>) -> Shape {
    match mark {
        Some(Mark::Create) => return Shape::Created,
        Some(Mark::Delete) => return Shape::Deleted,
        Some(Mark::Rename) => return Shape::Renamed,
        None => {}
    }
    if adds == 0 && dels == 0 {
        Shape::Unlined
    } else if dels == 0 {
        Shape::OnlyAdded
    } else if adds == 0 {
        Shape::OnlyRemoved
    } else if adds >= dels * 3 {
        Shape::Grew
    } else if dels >= adds * 3 {
        Shape::Trimmed
    } else {
        Shape::Reworked
    }
}

impl Shape {
    /// Two rules hold this table together, and both are about refusing to
    /// say more than the numbers support.
    ///
    /// `OnlyAdded` never says "new". A `+35 −0` file is indistinguishable
    /// from an append to a file that already existed, so only `Created` --
    /// which can come from nowhere but `--summary` -- may use the word.
    ///
    /// `Unlined` never says "empty" or "untouched". numstat writes `-` for a
    /// binary, which the parser reads as zero, so a swapped 4 MB texture and
    /// a bare mode change arrive as the same pair of numbers.
    pub fn motion(self) -> &'static str {
        match self {
            Shape::Created => "new",
            Shape::Deleted => "deleted",
            Shape::Renamed => "renamed",
            Shape::OnlyAdded => "added to, nothing cut",
            Shape::OnlyRemoved => "cut back only",
            Shape::Grew => "mostly growth",
            Shape::Trimmed => "mostly cut",
            Shape::Reworked => "churned in place",
            Shape::Unlined => "no lines moved — binary, rename or mode",
        }
    }
}

/// Composed rather than tabulated: a role word and a motion word, plus a
/// handful of pairings where English does better than the two halves apart.
/// Note that every override stays inside what the numbers license -- a test
/// file with adds and no deletions really did gain cases.
pub fn motion_for(role: Role, shape: Shape) -> &'static str {
    match (role, shape) {
        (Role::Test, Shape::Created) | (Role::Test, Shape::OnlyAdded) => "new cases",
        (Role::Test, Shape::Deleted) | (Role::Test, Shape::OnlyRemoved) => "cases withdrawn",
        (Role::Docs, Shape::OnlyAdded) => "more said",
        (Role::Docs, Shape::Reworked) => "reworded",
        (Role::Config, Shape::Reworked) => "retuned",
        (Role::Asset, Shape::Unlined) | (Role::Data, Shape::Unlined) => "binary, swapped",
        _ => shape.motion(),
    }
}


#[derive(Debug, Clone)]
pub struct FileNote {
    pub path: String,
    pub role: Role,
    pub area: String,
    pub shape: Shape,
    pub adds: i64,
    pub dels: i64,
}

impl FileNote {
    /// The motion half alone, so a caller can tint the role tag separately.
    pub fn motion(&self) -> &'static str {
        motion_for(self.role, self.shape)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Portrait {
    pub sentences: Vec<String>,
    pub notes: Vec<FileNote>,
}

impl Detail {
    /// The commit's own digest, in the register of `WeekDigest::sentences`:
    /// counted, never generated. The only claim it makes about intent is
    /// explicitly a claim about the *subject line*.
    pub fn portrait(&self, commit: &Commit) -> Portrait {
        let notes: Vec<FileNote> = self
            .files
            .iter()
            .map(|(raw, adds, dels)| {
                let path = final_path(raw);
                let mark = self.marks.get(&path).or_else(|| self.marks.get(raw)).copied();
                FileNote {
                    role: role(&path),
                    area: area(&path),
                    shape: shape(*adds, *dels, mark),
                    adds: *adds,
                    dels: *dels,
                    path,
                }
            })
            .collect();

        let mut out: Vec<String> = Vec::new();
        let n = notes.len();

        // A merge commit shows no diff at all under plain `git show`. Say so
        // rather than rendering an empty portrait that reads like a bug.
        if n == 0 {
            out.push("No file diff — git shows none for this commit.".into());
            return Portrait { sentences: out, notes };
        }

        // What.
        let mut by_role: HashMap<&'static str, usize> = HashMap::new();
        for f in &notes {
            *by_role.entry(f.role.tag()).or_insert(0) += 1;
        }
        let mut roles: Vec<(&str, usize)> = by_role.into_iter().collect();
        roles.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        if n == 1 {
            let f = &notes[0];
            out.push(format!(
                "One file: {} in {}, {}.",
                f.role.tag(),
                f.area,
                f.shape.motion()
            ));
        } else if roles.len() == 1 {
            out.push(format!(
                "{}, all {}. +{} −{}.",
                plural(n, "file"),
                roles[0].0,
                self.additions,
                self.deletions
            ));
        } else {
            let listed: Vec<String> = roles
                .iter()
                .take(3)
                .map(|(tag, count)| format!("{count} {tag}"))
                .collect();
            out.push(format!(
                "{}: {}. +{} −{}.",
                plural(n, "file"),
                listed.join(", "),
                self.additions,
                self.deletions
            ));
        }

        // Where. Ranked by the same rule as WeekDigest's areas, so the two
        // screens never disagree about where the work landed.
        let mut by_area: HashMap<String, usize> = HashMap::new();
        for f in &notes {
            *by_area.entry(f.area.clone()).or_insert(0) += 1;
        }
        let mut areas: Vec<(String, usize)> = by_area.into_iter().collect();
        areas.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        if areas.len() == 1 {
            out.push(format!("All of it under {}.", areas[0].0));
        } else {
            out.push(format!(
                "Across {}, heaviest in {} ({}).",
                plural(areas.len(), "area"),
                areas[0].0,
                plural(areas[0].1, "file")
            ));
        }

        // Weight.
        let total = self.additions + self.deletions;
        if n > 1 && total > 0 {
            if let Some(top) = notes.iter().max_by_key(|f| f.adds + f.dels) {
                if (top.adds + top.dels) * 5 >= total * 3 {
                    out.push(format!(
                        "{} carries most of it: +{} −{}.",
                        top.path, top.adds, top.dels
                    ));
                }
            }
        }
        if self.deletions == 0 && self.additions > 0 && n > 1 {
            out.push("Nothing was removed anywhere.".into());
        } else if self.deletions > self.additions {
            out.push(format!(
                "More left than arrived: −{} against +{}.",
                self.deletions, self.additions
            ));
        }

        // Subject. A claim about the subject line, never about the code.
        let kind = classify(&commit.subject);
        if kind != "changed" {
            out.push(format!("The subject calls it {kind}."));
        }

        out.truncate(4);
        Portrait { sentences: out, notes }
    }
}

pub const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
pub const DAY_INITIALS: [&str; 7] = ["S", "M", "T", "W", "T", "F", "S"];

/// Scaled to the week's own peak: relative shape, not absolute volume.
/// Empty days are a dot, not the shortest bar -- zero should look like
/// nothing, not like a little.
pub fn spark(counts: &[usize; 7]) -> Vec<char> {
    let peak = *counts.iter().max().unwrap_or(&0);
    counts
        .iter()
        .map(|&c| {
            if c == 0 || peak == 0 {
                '·'
            } else {
                SPARK[((c * (SPARK.len() - 1)) / peak).min(SPARK.len() - 1)]
            }
        })
        .collect()
}

pub fn plural(n: usize, word: &str) -> String {
    if n == 1 { format!("{n} {word}") } else { format!("{n} {word}s") }
}

// --- the week -----------------------------------------------------------

/// What the log says about the week that began on Sunday.
///
/// There is no model here. Nothing is generated: this counts, groups and
/// ranks what the log already states. That buys exactness, zero latency and
/// full offline operation; the cost is that it can describe activity but
/// never intent.
pub struct WeekDigest {
    pub repo: String,
    pub start: DateTime<Local>,
    pub back: i64,
    pub commits: Vec<Commit>,
    pub files: HashMap<String, (i64, i64)>,
    /// Local sources only: count of uncommitted changes, now.
    pub worktree: Option<usize>,
    pub partial: bool,
    pub by_day: [usize; 7],
    pub kinds: Vec<(String, usize)>,
    pub authors: Vec<(String, usize)>,
    pub areas: Vec<(String, (i64, i64, usize))>,
}

impl WeekDigest {
    pub fn new(
        repo: String,
        start: DateTime<Local>,
        back: i64,
        commits: Vec<Commit>,
        files: HashMap<String, (i64, i64)>,
        worktree: Option<usize>,
        partial: bool,
    ) -> Self {
        let mut by_day = [0usize; 7];
        let mut kinds: HashMap<String, usize> = HashMap::new();
        let mut authors: HashMap<String, usize> = HashMap::new();

        for c in &commits {
            if let Some(d) = c.date {
                let day = (d - start).num_days();
                if (0..7).contains(&day) {
                    by_day[day as usize] += 1;
                }
            }
            *kinds.entry(classify(&c.subject).to_string()).or_insert(0) += 1;
            if !c.author.is_empty() {
                *authors.entry(c.author.clone()).or_insert(0) += 1;
            }
        }

        // The first path segment is the closest thing to a subject area
        // without knowing anything about the project. Through final_path so a
        // renamed file lands in `src` rather than in an area literally named
        // `src/{a.rs` -- one rule, shared with the per-commit portrait.
        let mut areas: HashMap<String, (i64, i64, usize)> = HashMap::new();
        for (path, (adds, dels)) in &files {
            let area = area(&final_path(path));
            let e = areas.entry(area).or_insert((0, 0, 0));
            e.0 += adds;
            e.1 += dels;
            e.2 += 1;
        }

        let mut kinds: Vec<(String, usize)> = kinds.into_iter().collect();
        kinds.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut authors: Vec<(String, usize)> = authors.into_iter().collect();
        authors.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut areas: Vec<(String, (i64, i64, usize))> = areas.into_iter().collect();
        areas.sort_by(|a, b| b.1.2.cmp(&a.1.2).then(a.0.cmp(&b.0)));

        Self { repo, start, back, commits, files, worktree, partial,
               by_day, kinds, authors, areas }
    }

    /// A past week cannot gain more commits, and its working tree is not
    /// today's.
    pub fn closed(&self) -> bool {
        self.back > 0
    }

    pub fn additions(&self) -> i64 {
        self.files.values().map(|v| v.0).sum()
    }

    pub fn deletions(&self) -> i64 {
        self.files.values().map(|v| v.1).sum()
    }

    pub fn days_active(&self) -> usize {
        self.by_day.iter().filter(|&&d| d > 0).count()
    }

    fn elapsed(&self) -> usize {
        (((Local::now() - self.start).num_days() + 1).max(1) as usize).min(7)
    }

    /// One word for the week's shape. Days are tested before volume
    /// deliberately: five commits across five days is a different week from
    /// twelve in one afternoon.
    pub fn state(&self) -> &'static str {
        let (n, d) = (self.commits.len(), self.days_active());
        if n == 0 { "quiet" }
        else if d >= 5 { "sustained" }
        else if n >= 12 { "concentrated" }
        else if d >= 2 { "moving" }
        else { "touched" }
    }

    /// The digest proper: what happened, where, and by whose hand.
    pub fn sentences(&self) -> Vec<String> {
        let mut out = Vec::new();
        let elapsed = self.elapsed();

        if self.commits.is_empty() {
            if self.closed() {
                out.push(format!("Nothing committed in the week of {}.",
                                 self.start.format("%B %d")));
            } else if elapsed <= 1 {
                out.push("Nothing committed yet -- the week opened today.".into());
            } else {
                out.push(format!("No commits in the {} since Sunday.",
                                 plural(elapsed, "day")));
            }
            // The working tree describes now, not a past window.
            if !self.closed() {
                match self.worktree {
                    Some(0) => out.push("The working tree is clean.".into()),
                    Some(n) => out.push(format!(
                        "{} changed in the working tree, uncommitted.",
                        plural(n, "file"))),
                    None => {}
                }
            }
            return out;
        }

        let n = self.commits.len();
        let window = if self.closed() { "that week" }
            else if elapsed < 7 { "the week so far" } else { "the week" };
        out.push(format!("{} across {} of {}{}.",
                         plural(n, "commit"),
                         plural(self.days_active(), "day"),
                         window,
                         if self.partial { "+" } else { "" }));

        let (lead, lead_n) = &self.kinds[0];
        if n == 1 {
            out.push(format!("It {lead} something."));
        } else if *lead_n == n {
            out.push(format!("Every one of them {lead} something."));
        } else if lead_n * 2 >= n {
            let tail = self.kinds.get(1)
                .map(|(k, c)| format!(", then {c} {k}."))
                .unwrap_or_else(|| ".".into());
            out.push(format!("Mostly {lead} -- {lead_n} of {n}{tail}"));
        } else {
            let spread: Vec<String> = self.kinds.iter().take(3)
                .map(|(k, c)| format!("{c} {k}")).collect();
            out.push(format!("A spread of work: {}.", spread.join(", ")));
        }

        if !self.files.is_empty() {
            let (top, (_, _, count)) = &self.areas[0];
            let where_ = if self.areas.len() == 1 {
                format!(", all under {top}.")
            } else {
                format!(", heaviest in {top} ({}).", plural(*count, "file"))
            };
            out.push(format!("{} touched, +{} −{}{}",
                             plural(self.files.len(), "file"),
                             self.additions(), self.deletions(), where_));
        }

        if self.authors.len() > 1 {
            let hands: Vec<String> = self.authors.iter().take(3)
                .map(|(who, c)| format!("{who} ({c})")).collect();
            out.push(format!("Hands: {}.", hands.join(", ")));
        }

        let head = &self.commits[0];
        out.push(format!("Latest: \"{}\" -- {}.", head.subject, ago(head.date)));
        if !self.closed() {
            if let Some(n) = self.worktree.filter(|&n| n > 0) {
                out.push(format!("{} changed since, uncommitted.",
                                 plural(n, "file")));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn commit(subject: &str, day_offset: i64) -> Commit {
        Commit {
            sha: format!("{subject:x<40}"),
            date: Some(week_start(0) + Duration::days(day_offset) + Duration::hours(9)),
            author: "someone".into(),
            subject: subject.into(),
            body: String::new(),
        }
    }

    fn digest(commits: Vec<Commit>, back: i64) -> WeekDigest {
        WeekDigest::new("proj".into(), week_start(back), back, commits,
                        HashMap::new(), Some(0), false)
    }

    fn detail(files: &[(&str, i64, i64)]) -> Detail {
        let mut d = Detail::default();
        for (p, a, del) in files {
            d.additions += a;
            d.deletions += del;
            d.files.push((p.to_string(), *a, *del));
        }
        d
    }

    #[test]
    fn role_matches_whole_segments_not_substrings() {
        // The prefix/fix trap, one directory up: "latest" ends in "test" and
        // "contest" contains it. Neither is a test.
        assert_eq!(role("src/latest.rs"), Role::Source);
        assert_eq!(role("contest/main.rs"), Role::Source);
        assert_eq!(role("tests/week.rs"), Role::Test);
        assert_eq!(role("src/ui_test.rs"), Role::Test);
    }

    #[test]
    fn role_reads_segments_before_extensions() {
        // .yml alone would say config; the .github segment says build.
        assert_eq!(role(".github/workflows/ci.yml"), Role::Build);
        assert_eq!(role("config/app.yml"), Role::Config);
        // A fixture under tests/ is a test, not data -- outermost wins.
        assert_eq!(role("tests/fixtures/big.json"), Role::Test);
    }

    #[test]
    fn role_knows_names_without_extensions() {
        assert_eq!(role("README.md"), Role::Docs);
        assert_eq!(role("LICENSE"), Role::Docs);
        assert_eq!(role("Makefile"), Role::Build);
        assert_eq!(role("Cargo.lock"), Role::Config);
        assert_eq!(role(".gitignore"), Role::Config);
    }

    #[test]
    fn only_added_is_never_called_new() {
        // +35 -0 is indistinguishable from an append to an existing file.
        // Only a --summary mark may say "new".
        let s = shape(35, 0, None);
        assert_eq!(s, Shape::OnlyAdded);
        assert!(!s.motion().contains("new"));
        assert_eq!(shape(35, 0, Some(Mark::Create)).motion(), "new");
    }

    #[test]
    fn zero_churn_is_never_called_empty() {
        // numstat writes "-" for a binary, which parses to zero. A swapped
        // texture and a bare mode change arrive identically.
        let s = shape(0, 0, None);
        assert_eq!(s, Shape::Unlined);
        let m = s.motion();
        assert!(!m.contains("empty") && !m.contains("untouched"), "{m}");
        assert!(m.contains("binary"));
    }

    #[test]
    fn final_path_takes_the_destination_of_a_rename() {
        assert_eq!(final_path("src/{a.rs => b.rs}"), "src/b.rs");
        assert_eq!(final_path("old/a.rs => new/b.rs"), "new/b.rs");
        assert_eq!(final_path("src/{ => sub}/a.rs"), "src/sub/a.rs");
        assert_eq!(final_path("plain.rs"), "plain.rs");
        // And the area follows from the destination, not the brace form.
        assert_eq!(area(&final_path("src/{a.rs => b.rs}")), "src");
    }

    #[test]
    fn portrait_says_plainly_when_there_is_no_diff() {
        // A merge commit shows no numstat at all under plain `git show`.
        let p = Detail::default().portrait(&commit("Merge branch 'main'", 0));
        assert_eq!(p.notes.len(), 0);
        assert!(p.sentences[0].contains("No file diff"), "{:?}", p.sentences);
    }

    #[test]
    fn portrait_counts_roles_and_areas() {
        let d = detail(&[
            ("src/ui.rs", 40, 10),
            ("src/model.rs", 12, 3),
            ("tests/week.rs", 20, 0),
            ("README.md", 2, 1),
        ]);
        let p = d.portrait(&commit("Add the detail portrait", 0));
        assert_eq!(p.notes.len(), 4);
        assert_eq!(p.notes[0].role, Role::Source);
        assert_eq!(p.notes[2].role, Role::Test);
        assert!(p.sentences[0].starts_with("4 files:"), "{:?}", p.sentences);
        // Three areas: src, tests, (root) -- heaviest is src with two files.
        assert!(p.sentences[1].contains("heaviest in src"), "{:?}", p.sentences);
        assert!(p.sentences.len() <= 4);
    }

    #[test]
    fn portrait_claims_only_what_the_subject_says() {
        let d = detail(&[("src/a.rs", 1, 1)]);
        let p = d.portrait(&commit("Fix the broken parser", 0));
        assert!(p.sentences.iter().any(|s| s == "The subject calls it fixed."));
        // A subject with no recognised kind makes no claim at all.
        let q = d.portrait(&commit("Wednesday", 0));
        assert!(!q.sentences.iter().any(|s| s.contains("subject calls it")));
    }

    #[test]
    fn classifies_by_whole_words_not_substrings() {
        assert_eq!(classify("Fix the broken parser"), "fixed");
        assert_eq!(classify("Add a new door"), "added");
        assert_eq!(classify("Refactor the render loop"), "reworked");
        assert_eq!(classify("Update README"), "documented");
        assert_eq!(classify("bump deps to 2.0"), "packaged");
        assert_eq!(classify("wobble"), "changed");
        // "prefix" contains "fix"; substring matching would call this a fix.
        assert_eq!(classify("Add a prefix to the id"), "added");
    }

    #[test]
    fn specific_kinds_win_over_generic_ones() {
        // Ordered fix-before-test: this is a fix, not a test.
        assert_eq!(classify("fix the broken test"), "fixed");
    }

    #[test]
    fn week_starts_on_a_sunday() {
        for back in 0..5 {
            assert_eq!(week_start(back).weekday(), chrono::Weekday::Sun);
        }
        // Stepping back moves in exact weeks.
        assert_eq!((week_start(0) - week_start(2)).num_days(), 14);
    }

    #[test]
    fn empty_weeks_say_so_without_inventing_activity() {
        let d = digest(vec![], 0);
        assert_eq!(d.state(), "quiet");
        let s = d.sentences().join(" ");
        assert!(s.contains("Nothing committed") || s.contains("No commits"), "{s}");
        assert!(s.contains("working tree is clean"));
    }

    #[test]
    fn a_closed_week_never_reports_todays_working_tree() {
        let past = WeekDigest::new("proj".into(), week_start(2), 2, vec![],
                                   HashMap::new(), Some(9), false);
        let s = past.sentences().join(" ");
        assert!(!s.contains("uncommitted"), "past week claimed today's edits: {s}");
    }

    #[test]
    fn days_active_outrank_volume_in_the_state_word() {
        // Five days of work reads differently from a single busy afternoon.
        let spread: Vec<Commit> = (0..5).map(|i| commit("add a thing", i)).collect();
        assert_eq!(digest(spread, 0).state(), "sustained");

        let burst: Vec<Commit> = (0..12).map(|_| commit("add a thing", 0)).collect();
        assert_eq!(digest(burst, 0).state(), "concentrated");
    }

    #[test]
    fn sparkline_shows_absence_as_nothing() {
        let bars = spark(&[0, 3, 0, 0, 1, 0, 0]);
        assert_eq!(bars[0], '·', "an empty day must not look like a small one");
        assert_eq!(bars[1], '█', "the peak scales to the week's own maximum");
    }

    #[test]
    fn one_commit_reads_as_singular() {
        let d = digest(vec![commit("fix the crash", 0)], 0);
        let s = d.sentences();
        assert!(s[0].starts_with("1 commit across 1 day"), "{}", s[0]);
        assert_eq!(s[1], "It fixed something.");
    }
}

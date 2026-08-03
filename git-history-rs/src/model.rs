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
        // without knowing anything about the project.
        let mut areas: HashMap<String, (i64, i64, usize)> = HashMap::new();
        for (path, (adds, dels)) in &files {
            let area = match path.split_once('/') {
                Some((head, _)) => head.to_string(),
                None => "(root)".to_string(),
            };
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

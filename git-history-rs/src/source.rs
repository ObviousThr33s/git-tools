//! Where history comes from: clones on disk, or an account over the API.
//!
//! One rule governs every path in this file -- never invent data. A failure
//! reports its cause in words fit to show a person; it does not fall back to
//! something plausible, because a plausible lie is indistinguishable from
//! success.

use crate::model::*;
use chrono::{DateTime, Duration, Local};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub struct SourceError(pub String);

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SourceError {}

pub type Result<T> = std::result::Result<T, SourceError>;

pub trait Source {
    fn label(&self) -> String;
    fn repos(&mut self) -> Result<Vec<Repo>>;
    fn commits(&self, repo: &str, page: usize) -> Result<(Vec<Commit>, bool)>;
    fn detail(&self, repo: &str, commit: &Commit) -> Result<Detail>;
    fn week(&self, repo: &str, start: DateTime<Local>, back: i64) -> Result<WeekDigest>;

    /// Diff text for one commit, restricted to the named files.
    ///
    /// Provided rather than required: a source that cannot produce one
    /// cheaply says nothing, and the caller falls back to describing from
    /// paths and churn. An empty string is a valid answer, never an error.
    fn patch(&self, _repo: &str, _commit: &Commit, _files: &[String]) -> Result<String> {
        Ok(String::new())
    }
}

/// Cut at the last newline before the limit -- never at a byte index, which
/// would panic off a UTF-8 boundary, and never mid-line, which would hand a
/// reader half a statement.
pub fn cap_text(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = text[..end].rfind('\n').unwrap_or(end);
    let mut out = text[..cut].to_string();
    out.push_str("\n[… truncated]\n");
    out
}

pub const PAGE: usize = 100;

/// Bytes of diff text a description may see. Enough for the shape of a
/// change, far short of a whole feature branch.
pub const PATCH_CAP: usize = 16 * 1024;

/// Files a description may open. The rest are described from their names.
pub const PATCH_FILES: usize = 12;

/// Above this, skip the diff entirely and describe from paths and churn.
/// Not hypothetical: one commit in this very repository is 4,700 lines, of
/// which 2,470 are a lockfile.
pub const PATCH_GATE_LINES: i64 = 4_000;
pub const PATCH_GATE_FILES: usize = 60;

/// Projects are not all checked out at the same level: a working root may
/// hold containers of repositories, and may be a repository itself. Three
/// levels covers that without walking an entire drive.
pub const SCAN_DEPTH: usize = 3;

// --- local --------------------------------------------------------------

pub struct LocalSource {
    root: PathBuf,
    paths: HashMap<String, PathBuf>,
}

impl LocalSource {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        // Deliberately not canonicalize(): on Windows it rewrites a mapped
        // network drive to its \\?\UNC\server\share form, which is correct
        // and unrecognisable. The user typed Z:\ and should be shown Z:\.
        let root = std::path::absolute(root.as_ref())
            .unwrap_or_else(|_| root.as_ref().to_path_buf());
        Ok(Self { root, paths: HashMap::new() })
    }

    fn is_repo(path: &Path) -> bool {
        path.join(".git").is_dir() || path.join("objects").is_dir()
    }

    fn run(path: &Path, args: &[&str]) -> Result<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .map_err(|e| SourceError(format!("could not run git: {e}")))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let first = err.lines().next().unwrap_or("git failed").trim();
            return Err(SourceError(first.to_string()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Bounded depth-first walk. Finding a repository is not a reason to
    /// stop descending -- nesting is the normal case here, not an anomaly.
    fn discover(path: &Path, depth: usize, found: &mut Vec<PathBuf>) {
        if Self::is_repo(path) {
            found.push(path.to_path_buf());
        }
        if depth >= SCAN_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            return; // unreadable share or permission wall: ordinary, not fatal
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .filter(|p| {
                // Dotted directories are tooling, not projects -- and it
                // stops .git itself being walked as though it were one.
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| !n.starts_with('.'))
                    .unwrap_or(false)
            })
            .collect();
        dirs.sort();
        for dir in dirs {
            Self::discover(&dir, depth + 1, found);
        }
    }

    fn path_for(&self, repo: &str) -> PathBuf {
        self.paths
            .get(repo)
            .cloned()
            .unwrap_or_else(|| self.root.join(repo))
    }

    fn numstat(raw: &str) -> Detail {
        let mut d = Detail::default();
        for line in raw.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() == 3 {
                // "-" marks a binary file; count it as zero rather than failing.
                let a: i64 = parts[0].parse().unwrap_or(0);
                let del: i64 = parts[1].parse().unwrap_or(0);
                d.additions += a;
                d.deletions += del;
                d.files.push((parts[2].to_string(), a, del));
                continue;
            }
            // --summary lines carry no tabs, so they fall past the arm above
            // and land here. They are the only testimony that a file was
            // created rather than merely appended to; numstat alone cannot
            // tell those apart, and the portrait refuses to guess.
            let t = line.trim_start();
            let (mark, rest) = if let Some(r) = t.strip_prefix("create mode ") {
                (Mark::Create, r)
            } else if let Some(r) = t.strip_prefix("delete mode ") {
                (Mark::Delete, r)
            } else if let Some(r) = t.strip_prefix("rename ") {
                (Mark::Rename, r)
            } else {
                continue;
            };
            let path = match mark {
                // "100644 some/path" -- a path may contain spaces, so split once.
                Mark::Create | Mark::Delete => match rest.split_once(' ') {
                    Some((_, p)) => p,
                    None => continue,
                },
                // "src/{a.rs => b.rs} (95%)" -- drop the similarity score.
                Mark::Rename => match rest.rfind(" (") {
                    Some(i) => &rest[..i],
                    None => rest,
                },
            };
            d.marks.insert(final_path(path.trim()), mark);
        }
        d
    }
}

impl Source for LocalSource {
    fn label(&self) -> String {
        self.root.display().to_string()
    }

    fn repos(&mut self) -> Result<Vec<Repo>> {
        let mut found = Vec::new();
        Self::discover(&self.root, 0, &mut found);
        if found.is_empty() {
            return Err(SourceError(format!(
                "no git repositories under {}",
                self.root.display()
            )));
        }

        let mut out = Vec::new();
        for path in found {
            // Nested projects are named by their path from the root, so
            // "Archive/foo" and "game_projects/foo" stay distinguishable.
            let rel = path
                .strip_prefix(&self.root)
                .ok()
                .map(|p| p.display().to_string().replace('\\', "/"))
                .unwrap_or_default();
            let rel = rel.trim_start_matches('/').to_string();
            // The root itself is named for its own directory -- a drive root
            // has no file_name, so it falls back to the path it was given.
            let name = if rel.is_empty() {
                self.root
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| self.root.display().to_string())
            } else {
                rel
            };

            let (updated, branch) =
                match Self::run(&path, &["log", "-1", "--pretty=format:%cI\x1f%D"]) {
                    Ok(head) => {
                        let (when, refs) = head.split_once('\x1f').unwrap_or((&head, ""));
                        let branch = refs
                            .split(',')
                            .next()
                            .unwrap_or("")
                            .replace("HEAD -> ", "")
                            .trim()
                            .to_string();
                        (parse_iso(when), branch)
                    }
                    // An empty repository has no commits: not an error.
                    Err(_) => (None, "empty".to_string()),
                };

            self.paths.insert(name.clone(), path.clone());
            out.push(Repo { name, subtitle: branch, updated, extra: String::new() });
        }
        // Recency is what you are usually looking for.
        out.sort_by(|a, b| b.updated.cmp(&a.updated));
        Ok(out)
    }

    fn commits(&self, repo: &str, page: usize) -> Result<(Vec<Commit>, bool)> {
        let path = self.path_for(repo);
        let skip = format!("--skip={}", (page - 1) * PAGE);
        let take = format!("-n{PAGE}");
        let raw = Self::run(
            &path,
            &["log", &skip, &take, "--pretty=format:%H\x1f%aI\x1f%an\x1f%s\x1f%b\x1e"],
        )?;
        let mut out = Vec::new();
        for record in raw.split('\x1e') {
            let record = record.trim_matches('\n');
            if record.trim().is_empty() {
                continue;
            }
            let f: Vec<&str> = record.splitn(5, '\x1f').collect();
            out.push(Commit {
                sha: f.first().unwrap_or(&"").to_string(),
                date: parse_iso(f.get(1).unwrap_or(&"")),
                author: f.get(2).unwrap_or(&"").to_string(),
                subject: f.get(3).unwrap_or(&"").to_string(),
                body: f.get(4).unwrap_or(&"").trim().to_string(),
            });
        }
        let more = out.len() == PAGE;
        Ok((out, more))
    }

    fn detail(&self, repo: &str, commit: &Commit) -> Result<Detail> {
        let path = self.path_for(repo);
        // --summary rides along on the same invocation: it costs no extra
        // process and is what lets a created file be called new truthfully.
        let raw = Self::run(
            &path,
            &["show", "--numstat", "--summary", "--format=", &commit.sha],
        )?;
        Ok(Self::numstat(&raw))
    }

    fn patch(&self, repo: &str, commit: &Commit, files: &[String]) -> Result<String> {
        if files.is_empty() {
            return Ok(String::new());
        }
        let path = self.path_for(repo);
        let mut args: Vec<&str> = vec![
            "--no-pager",
            "show",
            &commit.sha,
            "--format=",
            "--patch",
            // The @@ header already names the enclosing item, so context
            // lines are mostly bytes without signal.
            "--unified=0",
            "--no-color",
            // Not tidiness. A .gitattributes textconv filter or a configured
            // external difftool would RUN A PROGRAM out of a repository this
            // tool only ever meant to read -- and it reads whatever clones
            // happen to sit on the working root.
            "--no-ext-diff",
            "--no-textconv",
            // Literal paths remain valid pathspecs; a rename spelling does not.
            "--no-renames",
            // A deletion's body is the whole of the old file: pure noise.
            "--diff-filter=ACMR",
            "--",
        ];
        let specs: Vec<String> = files
            .iter()
            // A brace-form rename is not a pathspec, and :(literal) keeps a
            // path containing glob characters from being read as a pattern.
            .filter(|f| !f.contains(" => "))
            .map(|f| format!(":(literal){f}"))
            .collect();
        args.extend(specs.iter().map(|s| s.as_str()));
        Ok(cap_text(Self::run(&path, &args)?, PATCH_CAP))
    }

    /// On disk the file statistics are free, so unlike the API path this
    /// digest can say where the work landed, not merely that it happened.
    fn week(&self, repo: &str, start: DateTime<Local>, back: i64) -> Result<WeekDigest> {
        let path = self.path_for(repo);
        let since = format!("--since={}", start.to_rfc3339());
        let until = format!("--until={}", (start + Duration::days(7)).to_rfc3339());
        let mut args = vec!["log", &since];
        if back > 0 {
            args.push(&until);
        }
        args.push("--numstat");
        args.push("--pretty=format:\x1e%H\x1f%aI\x1f%an\x1f%s");
        let raw = Self::run(&path, &args)?;

        let mut commits = Vec::new();
        let mut files: HashMap<String, (i64, i64)> = HashMap::new();
        for record in raw.split('\x1e') {
            let lines: Vec<&str> = record.lines().filter(|l| !l.trim().is_empty()).collect();
            if lines.is_empty() {
                continue;
            }
            let f: Vec<&str> = lines[0].splitn(4, '\x1f').collect();
            commits.push(Commit {
                sha: f.first().unwrap_or(&"").to_string(),
                date: parse_iso(f.get(1).unwrap_or(&"")),
                author: f.get(2).unwrap_or(&"").to_string(),
                subject: f.get(3).unwrap_or(&"").to_string(),
                body: String::new(),
            });
            for (name, a, d) in Self::numstat(&lines[1..].join("\n")).files {
                let e = files.entry(name).or_insert((0, 0));
                e.0 += a;
                e.1 += d;
            }
        }

        // A bare repository has no working tree to read.
        let worktree = Self::run(&path, &["status", "--porcelain"])
            .ok()
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count());

        Ok(WeekDigest::new(repo.to_string(), start, back, commits, files,
                           worktree, false))
    }
}

// --- github -------------------------------------------------------------

pub struct GitHubSource {
    user: String,
    token: Option<String>,
}

impl GitHubSource {
    pub fn new(user: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            token: std::env::var("GITHUB_TOKEN")
                .or_else(|_| std::env::var("GH_TOKEN"))
                .ok(),
        }
    }

    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<serde_json::Value> {
        let mut url = format!("https://api.github.com{path}");
        if !query.is_empty() {
            let q: Vec<String> = query
                .iter()
                .map(|(k, v)| format!("{k}={}", urlencode(v)))
                .collect();
            url.push('?');
            url.push_str(&q.join("&"));
        }

        let mut req = ureq::get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "git-history-rs");
        if let Some(t) = &self.token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }

        match req.call() {
            Ok(mut resp) => resp
                .body_mut()
                .read_json::<serde_json::Value>()
                .map_err(|e| SourceError(format!("bad response from github: {e}"))),
            Err(ureq::Error::StatusCode(404)) => {
                Err(SourceError(format!("no such account or repository: {path}")))
            }
            Err(ureq::Error::StatusCode(403 | 429)) => {
                let hint = if self.token.is_some() {
                    ""
                } else {
                    "  set GITHUB_TOKEN to raise it"
                };
                Err(SourceError(format!("rate limited by GitHub.{hint}")))
            }
            Err(ureq::Error::StatusCode(code)) => {
                Err(SourceError(format!("github returned {code}")))
            }
            Err(e) => Err(SourceError(format!("network unreachable: {e}"))),
        }
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn s(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

impl Source for GitHubSource {
    fn label(&self) -> String {
        format!("github.com/{}", self.user)
    }

    fn repos(&mut self) -> Result<Vec<Repo>> {
        let mut out = Vec::new();
        let mut page = 1;
        loop {
            let batch = self.get(
                &format!("/users/{}/repos", self.user),
                &[
                    ("per_page", PAGE.to_string()),
                    ("sort", "pushed".into()),
                    ("page", page.to_string()),
                ],
            )?;
            let arr = batch.as_array().cloned().unwrap_or_default();
            if arr.is_empty() {
                break;
            }
            for r in &arr {
                let mut bits = Vec::new();
                let lang = s(r, "language");
                if !lang.is_empty() {
                    bits.push(lang);
                }
                if r.get("fork").and_then(|v| v.as_bool()).unwrap_or(false) {
                    bits.push("fork".into());
                }
                if r.get("private").and_then(|v| v.as_bool()).unwrap_or(false) {
                    bits.push("private".into());
                }
                let stars = r.get("stargazers_count").and_then(|v| v.as_i64()).unwrap_or(0);
                let pushed = if s(r, "pushed_at").is_empty() {
                    s(r, "updated_at")
                } else {
                    s(r, "pushed_at")
                };
                out.push(Repo {
                    name: s(r, "name"),
                    subtitle: bits.join("  "),
                    updated: parse_iso(&pushed),
                    extra: if stars > 0 { format!("★{stars}") } else { String::new() },
                });
            }
            if arr.len() < PAGE {
                break;
            }
            page += 1;
        }
        if out.is_empty() {
            return Err(SourceError(format!(
                "{} has no public repositories",
                self.user
            )));
        }
        Ok(out)
    }

    fn commits(&self, repo: &str, page: usize) -> Result<(Vec<Commit>, bool)> {
        let batch = self.get(
            &format!("/repos/{}/{}/commits", self.user, repo),
            &[("per_page", PAGE.to_string()), ("page", page.to_string())],
        )?;
        let arr = batch.as_array().cloned().unwrap_or_default();
        let more = arr.len() == PAGE;
        let mut out = Vec::new();
        for c in &arr {
            let info = c.get("commit").cloned().unwrap_or_default();
            let message = s(&info, "message");
            let (subject, body) = message.split_once('\n').unwrap_or((&message, ""));
            let login = c
                .get("author")
                .and_then(|a| a.get("login"))
                .and_then(|x| x.as_str())
                .map(|x| x.to_string());
            let author = login.unwrap_or_else(|| {
                info.get("author").map(|a| s(a, "name")).unwrap_or_default()
            });
            let date = info.get("author").map(|a| s(a, "date")).unwrap_or_default();
            out.push(Commit {
                sha: s(c, "sha"),
                date: parse_iso(&date),
                author,
                subject: subject.trim().to_string(),
                body: body.trim().to_string(),
            });
        }
        Ok((out, more))
    }

    fn detail(&self, repo: &str, commit: &Commit) -> Result<Detail> {
        let data = self.get(
            &format!("/repos/{}/{}/commits/{}", self.user, repo, commit.sha),
            &[],
        )?;
        let stats = data.get("stats").cloned().unwrap_or_default();
        let files = data
            .get("files")
            .and_then(|f| f.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|f| {
                        (
                            s(f, "filename"),
                            f.get("additions").and_then(|v| v.as_i64()).unwrap_or(0),
                            f.get("deletions").and_then(|v| v.as_i64()).unwrap_or(0),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The API states outright what --summary has to be parsed out of on
        // disk, so this path can mark files without a second request.
        let mut marks = std::collections::HashMap::new();
        if let Some(arr) = data.get("files").and_then(|f| f.as_array()) {
            for f in arr {
                let mark = match s(f, "status").as_str() {
                    "added" => Mark::Create,
                    "removed" => Mark::Delete,
                    "renamed" => Mark::Rename,
                    _ => continue,
                };
                marks.insert(final_path(&s(f, "filename")), mark);
            }
        }
        Ok(Detail {
            additions: stats.get("additions").and_then(|v| v.as_i64()).unwrap_or(0),
            deletions: stats.get("deletions").and_then(|v| v.as_i64()).unwrap_or(0),
            files,
            marks,
        })
    }

    fn patch(&self, repo: &str, commit: &Commit, files: &[String]) -> Result<String> {
        if files.is_empty() {
            return Ok(String::new());
        }
        // One request, on a key the reader pressed -- never on screen entry.
        let data = self.get(
            &format!("/repos/{}/{}/commits/{}", self.user, repo, commit.sha),
            &[],
        )?;
        let wanted: std::collections::HashSet<&str> = files.iter().map(|f| f.as_str()).collect();
        let mut out = String::new();
        if let Some(arr) = data.get("files").and_then(|f| f.as_array()) {
            for f in arr {
                let name = s(f, "filename");
                if !wanted.contains(name.as_str()) {
                    continue;
                }
                // The API omits `patch` for very large files and past 300
                // files in a commit. A missing hunk is silence, not an error
                // -- which is why the caller reports what actually arrived
                // rather than what it asked for.
                let Some(p) = f.get("patch").and_then(|v| v.as_str()) else {
                    continue;
                };
                out.push_str(&format!("--- {name}\n{p}\n"));
            }
        }
        Ok(cap_text(out, PATCH_CAP))
    }

    /// Per-file churn is deliberately not fetched: the API bills a request
    /// per commit for it, and a digest that costs thirty round trips is not
    /// a digest. Counts and classification only, over the wire.
    fn week(&self, repo: &str, start: DateTime<Local>, back: i64) -> Result<WeekDigest> {
        let mut query = vec![
            ("since", start.to_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            ("per_page", PAGE.to_string()),
        ];
        if back > 0 {
            query.push((
                "until",
                (start + Duration::days(7))
                    .to_utc()
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string(),
            ));
        }
        let batch = self.get(&format!("/repos/{}/{}/commits", self.user, repo), &query)?;
        let arr = batch.as_array().cloned().unwrap_or_default();
        let partial = arr.len() == PAGE;
        let mut commits = Vec::new();
        for c in &arr {
            let info = c.get("commit").cloned().unwrap_or_default();
            let message = s(&info, "message");
            let subject = message.split('\n').next().unwrap_or("").trim().to_string();
            let login = c
                .get("author")
                .and_then(|a| a.get("login"))
                .and_then(|x| x.as_str())
                .map(|x| x.to_string());
            let author = login.unwrap_or_else(|| {
                info.get("author").map(|a| s(a, "name")).unwrap_or_default()
            });
            let date = info.get("author").map(|a| s(a, "date")).unwrap_or_default();
            commits.push(Commit {
                sha: s(c, "sha"),
                date: parse_iso(&date),
                author,
                subject,
                body: String::new(),
            });
        }
        Ok(WeekDigest::new(repo.to_string(), start, back, commits,
                           HashMap::new(), None, partial))
    }
}

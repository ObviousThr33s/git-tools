"""git history — a terminal browser for commit logs.

Two sources, one interface:

  * a GitHub account, read over the public REST API
  * a directory of local clones, read with the git binary

Everything below the source layer is shared: the same list widget, the same
key map, the same frame. Stdlib only, so the frozen build stays a single file.
"""

import json
import os
import re
import shutil
import subprocess
import sys
import unicodedata
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone

# Windows consoles don't interpret ANSI escapes unless virtual terminal
# processing is switched on, and they default to a legacy codepage that has no
# box-drawing characters. Both are set here; harmless no-ops everywhere else.
if os.name == 'nt':
    try:
        import ctypes
        kernel32 = ctypes.windll.kernel32
        kernel32.SetConsoleMode(kernel32.GetStdHandle(-11), 7)
        kernel32.SetConsoleOutputCP(65001)      # UTF-8
    except Exception:
        pass

# Two stream properties matter here.
#
# Encoding: the frozen build inherits cp1252 when stdout is redirected, and
# every glyph in the frame is outside it -- unset, that is a crash on the first
# border, not a cosmetic fault. UTF-8 with replacement is the floor.
#
# Buffering: child git processes write straight to our stdout. When stdout is a
# pipe rather than a terminal it is block-buffered, so our own prints would
# surface after git's. Line buffering keeps the two streams in order.
try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace", line_buffering=True)
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
except (AttributeError, ValueError):    # Python < 3.7, or a stream that can't
    pass

# --- palette -----------------------------------------------------------
_COLOR = sys.stdout.isatty() and not os.environ.get("NO_COLOR")


def _c(code):
    return code if _COLOR else ""


RESET   = _c("\033[0m")
DIM     = _c("\033[90m")
BOLD    = _c("\033[1m")
ACCENT  = _c("\033[38;5;39m")   # cyan-blue chrome
ACTIVE  = _c("\033[38;5;120m")  # selected row
IDLE    = _c("\033[38;5;250m")  # unselected row
WARN    = _c("\033[38;5;214m")
ERR     = _c("\033[38;5;203m")
ADD     = _c("\033[38;5;114m")
DEL     = _c("\033[38;5;203m")

# The layout matrix resizes with the window: every frame is measured against
# the live terminal size and clamped to a comfortable reading measure.
MIN_BOX_W = 34
MAX_BOX_W = 96
GUTTER = 2          # blank columns outside the frame on each side

ANSI_RE = re.compile(r'\033\[[0-9;]*m')

PAGE = 100          # commits fetched per request, both sources


# --- measuring ---------------------------------------------------------

def term_size():
    return shutil.get_terminal_size(fallback=(100, 24))


def box_width():
    """Frame width for the current window, inside sane typographic bounds."""
    return max(MIN_BOX_W, min(MAX_BOX_W, term_size().columns - GUTTER * 2))


def char_width(ch):
    """Columns one character occupies in a terminal cell grid.

    Monospace terminals have no kerning pairs, but they do have three cell
    classes -- zero-width (combining marks), single, and double (CJK, most
    emoji). Treating them all as width 1 is what actually breaks alignment.
    """
    if unicodedata.combining(ch) or unicodedata.category(ch) in ('Mn', 'Me', 'Cf'):
        return 0
    return 2 if unicodedata.east_asian_width(ch) in ('W', 'F') else 1


def visible_len(text):
    """Display width of a string, ignoring ANSI escapes."""
    return sum(char_width(c) for c in ANSI_RE.sub('', text))


def fit(text, width, ellipsis="…"):
    """Truncate to a display width, never splitting a double-wide cell."""
    if width <= 0:
        return ""
    if visible_len(text) <= width:
        return text
    budget = width - visible_len(ellipsis)
    out, used = [], 0
    for ch in text:
        w = char_width(ch)
        if used + w > budget:
            break
        out.append(ch)
        used += w
    return "".join(out) + ellipsis


def wrap(text, width):
    """Width-aware word wrap; falls back to hard breaks for long tokens."""
    if width <= 0:
        return [""]
    lines, current, used = [], [], 0
    for word in text.split():
        w = visible_len(word)
        if w > width:                      # token longer than the measure
            if current:
                lines.append(" ".join(current))
                current, used = [], 0
            while visible_len(word) > width:
                head = fit(word, width, ellipsis="")
                lines.append(head)
                word = word[len(head):]
            w = visible_len(word)
        if current and used + 1 + w > width:
            lines.append(" ".join(current))
            current, used = [word], w
        else:
            current.append(word)
            used = used + 1 + w if used else w
    if current:
        lines.append(" ".join(current))
    return lines or [""]


# --- frame -------------------------------------------------------------

def paragraphs(text):
    """Reflow a commit body into blocks.

    Commit messages are hard-wrapped at 72 columns by convention. Wrapping
    those lines again at our own measure produces a ragged short/long comb,
    so prose blocks are rejoined first. Bullets and indented lines are left
    alone -- their line breaks carry meaning.
    """
    blocks, run = [], []

    def flush():
        if run:
            blocks.append(" ".join(run))
            run.clear()

    for line in text.splitlines():
        if not line.strip():
            flush()
            if blocks and blocks[-1] != "":
                blocks.append("")
        elif line[:1].isspace() or line.lstrip()[:2] in ("- ", "* ", "> "):
            flush()
            blocks.append(line.strip())
        else:
            run.append(line.strip())
    flush()
    return blocks


def rule(left, right, fill="─", width=None):
    width = width or box_width()
    return f"{DIM}{left}{fill * (width - 2)}{right}{RESET}"


def row(content, width=None):
    """Pad a possibly-colored string inside the box borders."""
    width = width or box_width()
    content = fit(content, width - 2)
    pad = width - 2 - visible_len(content)
    return f"{DIM}│{RESET}{content}{' ' * max(0, pad)}{DIM}│{RESET}"


def split_row(left, right, width=None):
    """Two cells on one line: left flush, right flush, elastic space between."""
    width = width or box_width()
    inner = width - 2
    space = inner - visible_len(left) - visible_len(right)
    if space < 1:                                  # squeeze the left cell first
        left = fit(left, max(0, inner - visible_len(right) - 1))
        space = inner - visible_len(left) - visible_len(right)
    return row(left + " " * max(1, space) + right, width)


def center(content, width=None):
    width = width or box_width()
    space = width - 2 - visible_len(content)
    return row(" " * max(0, space // 2) + content, width)


def hide_cursor():
    if _COLOR:
        sys.stdout.write("\033[?25l")


def show_cursor():
    if _COLOR:
        sys.stdout.write("\033[?25h")


def clear_screen():
    sys.stdout.write("\033[2J\033[H")


def draw(lines):
    """Repaint from the home position.

    Clearing the whole screen between frames is what produces flicker; erasing
    each line as it is rewritten, then erasing the tail, does not.
    """
    body = "".join(line + "\033[K\n" for line in lines)
    sys.stdout.write("\033[H" + body + "\033[J")
    sys.stdout.flush()


# --- keyboard ----------------------------------------------------------
# Returns a symbolic name for navigation keys, or the literal character for
# anything printable (the filter prompt needs the raw text).

if os.name == 'nt':
    import msvcrt

    _NT_SPECIAL = {'H': 'up', 'P': 'down', 'I': 'pgup', 'Q': 'pgdn',
                   'G': 'home', 'O': 'end', 'K': 'left', 'M': 'right'}

    def get_key():
        ch = msvcrt.getwch()
        if ch in ('\x00', '\xe0'):
            return _NT_SPECIAL.get(msvcrt.getwch())
        if ch == '\r':
            return 'enter'
        if ch == '\x1b':
            return 'esc'
        if ch in ('\x08',):
            return 'backspace'
        if ch == '\x03':
            return 'interrupt'
        return ch
else:
    import select
    import termios
    import tty

    _UNIX_SPECIAL = {'[A': 'up', '[B': 'down', '[C': 'right', '[D': 'left',
                     '[H': 'home', '[F': 'end', 'OH': 'home', 'OF': 'end'}
    _UNIX_TILDE = {'1': 'home', '4': 'end', '5': 'pgup', '6': 'pgdn'}

    def _ready(fd):
        return bool(select.select([fd], [], [], 0.02)[0])

    def get_key():
        fd = sys.stdin.fileno()
        old = termios.tcgetattr(fd)
        try:
            tty.setraw(fd)
            ch = sys.stdin.read(1)
            if ch == '\x1b':
                if not _ready(fd):
                    return 'esc'
                seq = sys.stdin.read(1)
                if not _ready(fd) and seq not in ('[', 'O'):
                    return 'esc'
                seq += sys.stdin.read(1)
                if seq in _UNIX_SPECIAL:
                    return _UNIX_SPECIAL[seq]
                if seq[0] == '[' and seq[1].isdigit():
                    while _ready(fd):
                        c = sys.stdin.read(1)
                        if c == '~':
                            break
                    return _UNIX_TILDE.get(seq[1])
                return 'esc'
            if ch in ('\n', '\r'):
                return 'enter'
            if ch in ('\x7f', '\x08'):
                return 'backspace'
            if ch == '\x03':
                return 'interrupt'
            return ch
        finally:
            termios.tcsetattr(fd, termios.TCSADRAIN, old)


# --- time --------------------------------------------------------------

def parse_iso(text):
    """Parse the ISO 8601 stamps both git and the GitHub API emit."""
    if not text:
        return None
    text = text.strip().replace("Z", "+00:00")
    try:
        return datetime.fromisoformat(text)
    except ValueError:
        return None


def stamp(dt):
    if dt is None:
        return "unknown date"
    return dt.astimezone().strftime("%Y-%m-%d %H:%M")


def ago(dt):
    """Coarse relative age -- precision past a week is noise in a list."""
    if dt is None:
        return ""
    delta = datetime.now(timezone.utc) - dt.astimezone(timezone.utc)
    secs = delta.total_seconds()
    if secs < 0:
        return "just now"
    for span, unit in ((31536000, "y"), (2592000, "mo"), (604800, "w"),
                       (86400, "d"), (3600, "h"), (60, "m")):
        if secs >= span:
            return f"{int(secs // span)}{unit} ago"
    return "just now"


# --- sources -----------------------------------------------------------

class SourceError(Exception):
    """A failure worth showing the user verbatim, not a stack trace."""


class Repo:
    def __init__(self, name, subtitle="", updated=None, extra=""):
        self.name = name
        self.subtitle = subtitle
        self.updated = updated
        self.extra = extra


class Commit:
    def __init__(self, sha, date, author, subject, body=""):
        self.sha = sha
        self.date = date
        self.author = author
        self.subject = subject
        self.body = body

    @property
    def short(self):
        return self.sha[:7]


class GitHubSource:
    """Reads a public account over the REST API.

    A token in GITHUB_TOKEN or GH_TOKEN is used when present; it only raises
    the rate limit, nothing here needs write scope.
    """

    def __init__(self, user):
        self.user = user
        self.label = f"github.com/{user}"
        self.token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")

    def _get(self, path, params=None):
        url = f"https://api.github.com{path}"
        if params:
            url += "?" + urllib.parse.urlencode(params)
        headers = {
            "Accept": "application/vnd.github+json",
            "User-Agent": "git-history-native",
        }
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        req = urllib.request.Request(url, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=15) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as e:
            if e.code == 404:
                raise SourceError(f"no such account or repository: {path}")
            if e.code in (403, 429):
                hint = "" if self.token else "  set GITHUB_TOKEN to raise it"
                raise SourceError(f"rate limited by GitHub.{hint}")
            raise SourceError(f"github returned {e.code}")
        except urllib.error.URLError as e:
            raise SourceError(f"network unreachable: {e.reason}")
        except (ValueError, TimeoutError) as e:
            raise SourceError(f"bad response from github: {e}")

    def repos(self):
        out, page = [], 1
        while True:
            batch = self._get(f"/users/{self.user}/repos",
                              {"per_page": PAGE, "sort": "pushed", "page": page})
            if not batch:
                break
            for r in batch:
                updated = parse_iso(r.get("pushed_at") or r.get("updated_at"))
                bits = []
                if r.get("language"):
                    bits.append(r["language"])
                if r.get("fork"):
                    bits.append("fork")
                if r.get("private"):
                    bits.append("private")
                out.append(Repo(r["name"], "  ".join(bits), updated,
                                f"★{r.get('stargazers_count', 0)}"
                                if r.get("stargazers_count") else ""))
            if len(batch) < PAGE:
                break
            page += 1
        if not out:
            raise SourceError(f"{self.user} has no public repositories")
        return out

    def commits(self, repo, page):
        batch = self._get(f"/repos/{self.user}/{repo}/commits",
                          {"per_page": PAGE, "page": page})
        out = []
        for c in batch:
            info = c.get("commit", {})
            message = info.get("message", "")
            subject, _, body = message.partition("\n")
            author = (c.get("author") or {}).get("login") \
                or (info.get("author") or {}).get("name", "")
            out.append(Commit(c.get("sha", ""),
                              parse_iso((info.get("author") or {}).get("date")),
                              author, subject.strip(), body.strip()))
        return out, len(batch) == PAGE

    def detail(self, repo, commit):
        data = self._get(f"/repos/{self.user}/{repo}/commits/{commit.sha}")
        stats = data.get("stats", {})
        files = [(f.get("filename", ""), f.get("additions", 0), f.get("deletions", 0))
                 for f in data.get("files", [])]
        return {
            "additions": stats.get("additions", 0),
            "deletions": stats.get("deletions", 0),
            "files": files,
        }


class LocalSource:
    """Reads clones on disk with the git binary."""

    def __init__(self, root):
        self.root = os.path.abspath(root)
        self.label = self.root
        self._paths = {}                # repo name -> directory on disk
        if not shutil.which("git"):
            raise SourceError("git is not on PATH")

    @staticmethod
    def _is_repo(path):
        return os.path.isdir(os.path.join(path, ".git")) or \
            os.path.isdir(os.path.join(path, "objects"))

    def _run(self, path, args):
        try:
            r = subprocess.run(["git", "-C", path] + args, capture_output=True,
                               text=True, encoding="utf-8", errors="replace")
        except OSError as e:
            raise SourceError(f"could not run git: {e}")
        if r.returncode != 0:
            raise SourceError((r.stderr or "git failed").strip().splitlines()[0])
        return r.stdout

    def repos(self):
        found = []
        if self._is_repo(self.root):
            found.append(self.root)
        try:
            for entry in sorted(os.listdir(self.root)):
                path = os.path.join(self.root, entry)
                if path not in found and os.path.isdir(path) and self._is_repo(path):
                    found.append(path)
        except OSError as e:
            raise SourceError(f"cannot read {self.root}: {e.strerror}")

        out = []
        for path in found:
            name = os.path.basename(path.rstrip(os.sep)) or path
            try:
                head = self._run(path, ["log", "-1", "--pretty=format:%cI\x1f%D"])
                when, _, refs = head.partition("\x1f")
                updated = parse_iso(when)
                branch = refs.split(",")[0].replace("HEAD -> ", "").strip()
            except SourceError:                     # empty repo, no commits yet
                updated, branch = None, "empty"
            out.append(Repo(name, branch, updated))
            self._paths[name] = path
        if not out:
            raise SourceError(f"no git repositories under {self.root}")
        out.sort(key=lambda r: r.updated or datetime.min.replace(tzinfo=timezone.utc),
                 reverse=True)
        return out

    def commits(self, repo, page):
        path = self._paths.get(repo, os.path.join(self.root, repo))
        raw = self._run(path, [
            "log", f"--skip={(page - 1) * PAGE}", f"-n{PAGE}",
            "--pretty=format:%H\x1f%aI\x1f%an\x1f%s\x1f%b\x1e",
        ])
        out = []
        for record in raw.split("\x1e"):
            record = record.strip("\n")
            if not record.strip():
                continue
            parts = (record.split("\x1f") + [""] * 5)[:5]
            out.append(Commit(parts[0], parse_iso(parts[1]), parts[2],
                              parts[3], parts[4].strip()))
        return out, len(out) == PAGE

    def detail(self, repo, commit):
        path = self._paths.get(repo, os.path.join(self.root, repo))
        raw = self._run(path, ["show", "--numstat", "--format=", commit.sha])
        files, adds, dels = [], 0, 0
        for line in raw.splitlines():
            parts = line.split("\t")
            if len(parts) != 3:
                continue
            a = 0 if parts[0] == "-" else int(parts[0])   # "-" means binary
            d = 0 if parts[1] == "-" else int(parts[1])
            adds += a
            dels += d
            files.append((parts[2], a, d))
        return {"additions": adds, "deletions": dels, "files": files}


# --- screens -----------------------------------------------------------

class Screen:
    """A scrollable list with an optional filter prompt.

    Subclasses supply the rows and decide what Enter does; scrolling, the
    filter, and the frame are handled once, here.
    """

    title = ""
    hint = "↑↓ move   ⏎ open   / filter   q quit"

    def __init__(self, app):
        self.app = app
        self.items = []
        self.cursor = 0
        self.filter = ""
        self.filtering = False
        self.status = ""

    # -- data
    def matches(self, item):
        return self.filter.lower() in self.label_of(item).lower()

    def visible_items(self):
        return [i for i in self.items if not self.filter or self.matches(i)]

    def label_of(self, item):
        return str(item)

    def render_row(self, item, selected, width):
        raise NotImplementedError

    def activate(self, item):
        pass

    def on_bottom(self):
        """Hook for sources that page lazily."""

    # -- frame
    def subtitle(self):
        return ""

    def build(self, width, height):
        items = self.visible_items()
        chrome = 8 if self.status or self.filtering else 7
        rows = max(1, height - chrome)
        self.cursor = max(0, min(self.cursor, max(0, len(items) - 1)))
        top = min(max(0, self.cursor - rows // 2), max(0, len(items) - rows))

        lines = ["", "  " + rule("╭", "╮", width=width)]
        lines.append("  " + split_row(f" {ACCENT}{BOLD}{self.title}{RESET}",
                                      f"{DIM}{self.subtitle()}{RESET} ", width=width))
        lines.append("  " + rule("├", "┤", width=width))

        if not items:
            empty = "no matches" if self.filter else "nothing here"
            lines.append("  " + row(f" {DIM}{empty}{RESET}", width=width))
            for _ in range(rows - 1):
                lines.append("  " + row("", width=width))
        else:
            for offset in range(rows):
                idx = top + offset
                if idx < len(items):
                    lines.append("  " + self.render_row(items[idx],
                                                        idx == self.cursor, width))
                else:
                    lines.append("  " + row("", width=width))
            if self.cursor >= len(items) - 1:
                self.on_bottom()

        lines.append("  " + rule("├", "┤", width=width))
        if self.filtering:
            lines.append("  " + row(f" {ACCENT}/{RESET}{self.filter}"
                                    f"{ACTIVE}▏{RESET}", width=width))
        elif self.status:
            lines.append("  " + row(f" {WARN}{self.status}{RESET}", width=width))
        counter = f"{self.cursor + 1}/{len(items)}" if items else "0/0"
        if self.filter and not self.filtering:
            counter = f"/{self.filter}  " + counter
        hint = self.hint if width >= 56 else "↑↓  ⏎  /  q"
        lines.append("  " + split_row(f" {DIM}{hint}{RESET}",
                                      f"{DIM}{counter}{RESET} ", width=width))
        lines.append("  " + rule("╰", "╯", width=width))
        return lines

    # -- input
    def handle(self, key):
        items = self.visible_items()
        count = max(1, len(items))
        page = max(1, term_size().lines - 10)

        if self.filtering:
            if key == 'enter':
                self.filtering = False
            elif key == 'esc':
                self.filtering, self.filter = False, ""
            elif key == 'backspace':
                self.filter = self.filter[:-1]
                self.cursor = 0
            elif key and len(key) == 1 and key.isprintable():
                self.filter += key
                self.cursor = 0
            return True

        if key in ('up', 'k'):
            self.cursor = (self.cursor - 1) % count
        elif key in ('down', 'j'):
            self.cursor = (self.cursor + 1) % count
        elif key == 'pgup':
            self.cursor = max(0, self.cursor - page)
        elif key == 'pgdn':
            self.cursor = min(count - 1, self.cursor + page)
        elif key in ('home', 'g'):
            self.cursor = 0
        elif key in ('end', 'G'):
            self.cursor = count - 1
        elif key == '/':
            self.filtering, self.filter = True, ""
        elif key == 'enter':
            if items:
                self.activate(items[self.cursor])
        elif key in ('esc', 'backspace', 'left', 'h'):
            return False                          # pop this screen
        elif key in ('q', 'Q', 'interrupt'):
            self.app.running = False
        return True


class RepoScreen(Screen):
    hint = "↑↓ move   ⏎ commits   / filter   q quit"

    def __init__(self, app, repos):
        super().__init__(app)
        self.title = "git history"
        self.items = repos

    def subtitle(self):
        return self.app.source.label

    def label_of(self, repo):
        return repo.name + " " + repo.subtitle

    def render_row(self, repo, selected, width):
        age = ago(repo.updated)
        right = f"{repo.extra}  {age}".strip()
        name_w = width - 8 - visible_len(right) - visible_len(repo.subtitle)
        name = fit(repo.name, max(8, name_w))
        marker, tone = (f"{ACTIVE}▍{RESET}", f"{ACTIVE}{BOLD}") if selected \
            else ("  ", IDLE)
        left = f" {marker} {tone}{name}{RESET}"
        if repo.subtitle and width >= 60:
            left += f"  {DIM}{repo.subtitle}{RESET}"
        return split_row(left, f"{DIM}{right}{RESET} ", width)

    def activate(self, repo):
        self.app.open_commits(repo.name)

    def handle(self, key):
        if key in ('esc', 'backspace', 'left', 'h') and not self.filtering:
            if self.filter:
                self.filter = ""
                return True
            self.app.running = False
            return True
        return super().handle(key)


class CommitScreen(Screen):
    hint = "↑↓ move   ⏎ detail   / filter   esc back"

    def __init__(self, app, repo, commits, more):
        super().__init__(app)
        self.title = repo
        self.repo = repo
        self.items = commits
        self.more = more
        self.page = 1
        self.loading = False

    def subtitle(self):
        n = len(self.items)
        return f"{n}+ commits" if self.more else f"{n} commits"

    def label_of(self, commit):
        return f"{commit.short} {commit.subject} {commit.author}"

    def render_row(self, commit, selected, width):
        when = stamp(commit.date)
        marker, tone = (f"{ACTIVE}●{RESET}", f"{ACTIVE}{BOLD}") if selected \
            else (f"{DIM}·{RESET}", IDLE)
        meta_w = visible_len(when) + 10
        subject = fit(commit.subject or "(no subject)", max(10, width - 8 - meta_w))
        return split_row(f" {marker} {tone}{subject}{RESET}",
                         f"{DIM}{commit.short}  {when}{RESET} ", width)

    def on_bottom(self):
        """Pull the next page once the cursor reaches the end of this one."""
        if not self.more or self.loading:
            return
        self.loading = True
        try:
            batch, self.more = self.app.source.commits(self.repo, self.page + 1)
            self.items.extend(batch)
            self.page += 1
            self.status = ""
        except SourceError as e:
            self.more = False
            self.status = str(e)
        finally:
            self.loading = False

    def activate(self, commit):
        self.app.open_detail(self.repo, commit)


class DetailScreen(Screen):
    hint = "↑↓ scroll   esc back   q quit"

    def __init__(self, app, repo, commit, detail):
        super().__init__(app)
        self.title = commit.short
        self.repo = repo
        self.commit = commit
        self.detail = detail
        self.items = [None]          # rebuilt per frame; width-dependent

    def subtitle(self):
        d = self.detail
        if not d:
            return "no diff"
        n = len(d["files"])
        return f"{n} file{'' if n == 1 else 's'}  +{d['additions']} −{d['deletions']}"

    def lines_for(self, width):
        inner = width - 4
        c, out = self.commit, []
        for line in wrap(c.subject or "(no subject)", inner - 1):
            out.append(f" {BOLD}{line}{RESET}")
        out.append(f" {DIM}{c.author or 'unknown'}  ·  {stamp(c.date)}"
                   f"  ·  {ago(c.date)}{RESET}")
        out.append(f" {DIM}{c.sha}{RESET}")
        if c.body:
            out.append("")
            for block in paragraphs(c.body):
                for line in wrap(block, inner - 2) if block else [""]:
                    out.append(f"  {IDLE}{line}{RESET}")
        if self.detail and self.detail["files"]:
            out.append("")
            out.append(f" {DIM}files{RESET}")
            for name, a, d in self.detail["files"]:
                churn = f"+{a} −{d}"
                name = fit(name, max(6, inner - visible_len(churn) - 4))
                gap = " " * max(1, inner - visible_len(name) - visible_len(churn) - 2)
                out.append(f"  {IDLE}{name}{RESET}{gap}"
                           f"{ADD}+{a}{RESET} {DEL}−{d}{RESET}")
        elif self.detail is not None:
            out.append("")
            out.append(f" {DIM}no file changes{RESET}")
        elif self.status:
            out.append("")
            out.append(f" {ERR}{self.status}{RESET}")
        return out

    def build(self, width, height):
        body = self.lines_for(width)
        rows = max(1, height - 7)
        self.cursor = max(0, min(self.cursor, max(0, len(body) - 1)))
        top = min(self.cursor, max(0, len(body) - rows))

        lines = ["", "  " + rule("╭", "╮", width=width)]
        lines.append("  " + split_row(f" {ACCENT}{BOLD}{self.repo}"
                                      f"{RESET} {DIM}@ {self.commit.short}{RESET}",
                                      f"{DIM}{self.subtitle()}{RESET} ", width=width))
        lines.append("  " + rule("├", "┤", width=width))
        for offset in range(rows):
            idx = top + offset
            lines.append("  " + row(body[idx] if idx < len(body) else "", width=width))
        lines.append("  " + rule("├", "┤", width=width))
        pos = f"{min(top + rows, len(body))}/{len(body)}"
        lines.append("  " + split_row(f" {DIM}{self.hint}{RESET}",
                                      f"{DIM}{pos}{RESET} ", width=width))
        lines.append("  " + rule("╰", "╯", width=width))
        return lines

    def handle(self, key):
        page = max(1, term_size().lines - 10)
        if key in ('up', 'k'):
            self.cursor = max(0, self.cursor - 1)
        elif key in ('down', 'j'):
            self.cursor += 1
        elif key == 'pgup':
            self.cursor = max(0, self.cursor - page)
        elif key == 'pgdn':
            self.cursor += page
        elif key in ('home', 'g'):
            self.cursor = 0
        elif key in ('esc', 'backspace', 'left', 'h', 'enter'):
            return False
        elif key in ('q', 'Q', 'interrupt'):
            self.app.running = False
        return True


# --- app ---------------------------------------------------------------

class App:
    def __init__(self, source):
        self.source = source
        self.stack = []
        self.running = True
        self.commit_cache = {}

    def splash(self, message):
        width = box_width()
        draw(["", "  " + rule("╭", "╮", width=width),
              "  " + row(f" {WARN}◐{RESET} {DIM}{message}{RESET}", width=width),
              "  " + rule("╰", "╯", width=width), ""])

    def error(self, message):
        """Blocking error card -- the user should see why, then continue."""
        width = box_width()
        lines = ["", "  " + rule("╭", "╮", width=width),
                 "  " + row(f" {ERR}✕{RESET} {BOLD}something went wrong{RESET}",
                            width=width)]
        for line in wrap(message, width - 8):
            lines.append("  " + row(f"   {IDLE}{line}{RESET}", width=width))
        lines.append("  " + rule("╰", "╯", width=width))
        lines.append(f"  {DIM}press any key{RESET}")
        draw(lines)
        get_key()

    def open_commits(self, repo):
        if repo not in self.commit_cache:
            self.splash(f"reading {repo}…")
            try:
                self.commit_cache[repo] = self.source.commits(repo, 1)
            except SourceError as e:
                self.error(str(e))
                return
        commits, more = self.commit_cache[repo]
        if not commits:
            self.error(f"{repo} has no commits yet")
            return
        self.stack.append(CommitScreen(self, repo, list(commits), more))

    def open_detail(self, repo, commit):
        self.splash(f"reading {commit.short}…")
        try:
            detail = self.source.detail(repo, commit)
        except SourceError as e:
            detail = None
            self.stack.append(DetailScreen(self, repo, commit, detail))
            self.stack[-1].status = str(e)
            return
        self.stack.append(DetailScreen(self, repo, commit, detail))

    def run(self, repos):
        self.stack.append(RepoScreen(self, repos))
        hide_cursor()
        clear_screen()
        try:
            while self.running and self.stack:
                size = term_size()
                draw(self.stack[-1].build(box_width(), size.lines))
                key = get_key()
                if key is None:
                    continue
                if not self.stack[-1].handle(key):
                    self.stack.pop()
                    clear_screen()          # the new top may be shorter
        finally:
            show_cursor()


# --- entry -------------------------------------------------------------

USAGE = """git history — browse commit logs in the terminal

  git_history_native [TARGET]

  TARGET   a directory of clones, or a GitHub username.
           Defaults to the current directory.

  -u USER  force GitHub lookup for USER
  -d DIR   force local lookup under DIR
  -h       this message

  GITHUB_TOKEN or GH_TOKEN raises the GitHub rate limit when set.
"""


def build_source(argv):
    args = list(argv)
    if "-h" in args or "--help" in args:
        print(USAGE)
        return None
    if "-u" in args:
        return GitHubSource(args[args.index("-u") + 1])
    if "-d" in args:
        return LocalSource(args[args.index("-d") + 1])

    positional = [a for a in args if not a.startswith("-")]
    if not positional:
        return LocalSource(os.getcwd())
    target = positional[0]
    # A path that exists is a path; anything else is an account name.
    if os.path.isdir(target):
        return LocalSource(target)
    return GitHubSource(target)


def main(argv):
    try:
        source = build_source(argv)
    except IndexError:
        print(USAGE)
        return 2
    if source is None:
        return 0

    print(f"\n  {DIM}reading {source.label}…{RESET}")
    try:
        repos = source.repos()
    except SourceError as e:
        print(f"\n  {ERR}✕{RESET} {e}\n")
        return 1

    App(source).run(repos)
    clear_screen()
    print(f"\n  {DIM}bye.{RESET}\n")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except KeyboardInterrupt:
        show_cursor()
        print(f"\n  {DIM}interrupted.{RESET}\n")
        sys.exit(130)
    finally:
        show_cursor()

"""git history — a terminal browser for commit logs.

Two sources, one interface:

  * a GitHub account, read over the public REST API
  * a directory of local clones, read with the git binary

Everything below the source layer is shared: the same list widget, the same
key map, the same frame. Stdlib only, so the frozen build stays a single file.
"""

import hashlib
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
from datetime import datetime, timedelta, timezone

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

# Projects are not all checked out at the same level: a working root may hold
# containers of repositories, and may be a repository itself. Three levels
# covers that without walking an entire drive.
SCAN_DEPTH = 3

# The working root. A network share is the normal case here, so its absence is
# a routine condition -- disconnected, not misconfigured -- and is reported as
# such rather than crashing.
DEFAULT_ROOT = os.environ.get("GIT_HISTORY_ROOT", "Z:\\" if os.name == 'nt' else "")


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


def cells(left, right, inner):
    """Left flush, right flush, elastic space between -- no borders."""
    space = inner - visible_len(left) - visible_len(right)
    if space < 1:                                  # squeeze the left cell first
        left = fit(left, max(0, inner - visible_len(right) - 1))
        space = inner - visible_len(left) - visible_len(right)
    return left + " " * max(1, space) + right


def split_row(left, right, width=None):
    """cells(), wrapped in the frame."""
    width = width or box_width()
    return row(cells(left, right, width - 2), width)


def pad(content, width):
    """Pad to an exact display width, ANSI-aware."""
    content = fit(content, width)
    return content + " " * max(0, width - visible_len(content))


def rule_split(left, right, pos, junction, width):
    """A horizontal rule carrying a junction where a column divider meets it."""
    inner = width - 2
    fill = ["─"] * inner
    if 0 <= pos < inner:
        fill[pos] = junction
    return f"{DIM}{left}{''.join(fill)}{right}{RESET}"


def center(content, width=None):
    width = width or box_width()
    space = width - 2 - visible_len(content)
    return row(" " * max(0, space // 2) + content, width)


# --- sigils ------------------------------------------------------------
# Every project gets a mark derived from its name. Nothing is authored and
# nothing is fetched: the same name always yields the same figure, so a
# project becomes recognisable by shape before you have read its label.
#
# The figure is mirrored down its centre column. That single constraint is
# what separates a mark from noise -- symmetry is the thing the eye reads as
# deliberate, and it is why identicons work at all.

SIGIL_SIZE = 7          # cells square; odd, so there is a true centre column
SIGIL_FILL = 44         # percent of cells lit; below ~35 reads sparse, above
                        # ~55 reads like a solid block

# Half-height blocks pack two grid rows into one terminal row, so a 7x7 figure
# occupies 4 lines rather than 7 and keeps its square aspect.
HALF_BLOCK = {(0, 0): "  ", (1, 0): "▀▀", (0, 1): "▄▄", (1, 1): "██"}

LANGUAGE_TINT = {
    "rust": 209, "go": 80, "python": 220, "javascript": 227, "typescript": 75,
    "c": 111, "c++": 176, "c#": 141, "java": 173, "kotlin": 141, "swift": 209,
    "ruby": 203, "php": 104, "shell": 149, "powershell": 69, "lua": 63,
    "haskell": 140, "scala": 203, "elixir": 141, "zig": 215, "dart": 45,
    "html": 209, "css": 105, "vue": 79, "nix": 75, "ocaml": 214, "perl": 111,
}

# Fallback tints, for projects whose language we don't know or GitHub doesn't
# report. Chosen to stay legible on both dark and light terminals.
NEUTRAL_TINTS = (39, 79, 141, 209, 214, 105, 45, 168)


def sigil_tint(name, language=None):
    if language:
        hit = LANGUAGE_TINT.get(language.strip().lower())
        if hit:
            return hit
    seed = hashlib.sha256(("tint:" + name).encode("utf-8")).digest()
    return NEUTRAL_TINTS[seed[0] % len(NEUTRAL_TINTS)]


def sigil_grid(name, size=SIGIL_SIZE):
    """A mirrored bit grid, seeded by the project name."""
    digest = hashlib.sha256(name.encode("utf-8")).digest()
    half = (size + 1) // 2
    grid = [[0] * size for _ in range(size)]
    for y in range(size):
        for x in range(half):
            lit = int(digest[(y * half + x) % len(digest)] % 100 < SIGIL_FILL)
            grid[y][x] = grid[y][size - 1 - x] = lit
    if not any(any(r) for r in grid):        # a blank mark represents nothing
        grid[size // 2][size // 2] = 1
    return grid


def sigil_lines(name, language=None, size=SIGIL_SIZE):
    """Render the mark as colored terminal rows, two grid rows per line."""
    grid = sigil_grid(name, size)
    tint = _c(f"\033[38;5;{sigil_tint(name, language)}m")
    out = []
    for y in range(0, size, 2):
        top = grid[y]
        bottom = grid[y + 1] if y + 1 < size else [0] * size
        body = "".join(HALF_BLOCK[(top[x], bottom[x])] for x in range(size))
        out.append(f"{tint}{body}{RESET}")
    return out


SIGIL_W = SIGIL_SIZE * 2                     # display columns a mark occupies
PANEL_W = SIGIL_W + 8                        # mark, plus room for facts beside
PANEL_MIN_TERM = 78                          # below this the panel is dropped


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

# --- the week ----------------------------------------------------------
# A standing question -- "where is this project at?" -- answered from the
# record itself rather than by a model. Nothing here is generative: it counts,
# groups and ranks what the log already states, then reports it in a sentence.
# That makes it exact, instant and available with the network down, and it
# means it can only describe activity, never intent.
#
# Recomputed on every visit. A digest is a claim about the present, and a
# cached one silently ages into a lie.

SUBJECT_KINDS = (
    # Ordered: the first match wins, so the more specific verbs come first.
    ("fixed",      ("fix", "bug", "patch", "repair", "correct", "resolve",
                    "hotfix", "broken", "crash", "regression")),
    ("removed",    ("remove", "delete", "drop", "strip", "prune", "purge")),
    ("reworked",   ("refactor", "rework", "rewrite", "simplify", "clean",
                    "rename", "restructure", "replace", "move", "extract",
                    "split", "merge", "tidy")),
    ("documented", ("doc", "docs", "readme", "comment", "changelog", "license")),
    ("tested",     ("test", "tests", "spec", "coverage", "fixture")),
    ("packaged",   ("build", "ci", "deps", "dependency", "bump", "release",
                    "version", "package", "publish", "deploy")),
    ("added",      ("add", "create", "introduce", "implement", "new", "support",
                    "give", "teach", "wire", "begin", "start", "init")),
)

SPARK = "▁▂▃▄▅▆▇█"
DAY_INITIALS = ("S", "M", "T", "W", "T", "F", "S")


def week_start(now=None, back=0):
    """Midnight on the Sunday that opened the week, local time.

    On a Sunday the week has already turned: the window opens that morning,
    not seven days earlier -- which is exactly why the view can be stepped
    backwards. A week one hour old describes nothing, and the honest answer
    to "where is this at" is then found in the week behind it.
    """
    now = now or datetime.now().astimezone()
    midnight = now.replace(hour=0, minute=0, second=0, microsecond=0)
    return midnight - timedelta(days=(now.weekday() + 1) % 7 + 7 * back)


def week_label(back):
    if back == 0:
        return "this week"
    if back == 1:
        return "last week"
    return f"{back} weeks back"


def classify(subject):
    words = set(re.findall(r"[a-z]+", subject.lower()))
    for kind, triggers in SUBJECT_KINDS:
        if words & set(triggers):
            return kind
    return "changed"


def sparkline(counts):
    peak = max(counts) or 1
    return "".join(SPARK[min(len(SPARK) - 1, (c * (len(SPARK) - 1)) // peak)]
                   if c else "·" for c in counts)


def plural(n, word, suffix="s"):
    return f"{n} {word}{'' if n == 1 else suffix}"


class WeekDigest:
    """What the log says about the week that began on Sunday."""

    def __init__(self, repo, start, commits, files, worktree=None, partial=False,
                 back=0):
        self.repo = repo
        self.start = start
        self.back = back
        self.closed = back > 0          # a past week cannot gain more commits
        self.commits = commits
        self.files = files              # {path: (additions, deletions)}
        self.worktree = worktree        # local only: uncommitted change count
        self.partial = partial          # more commits than we were served
        self.by_day = [0] * 7
        self.kinds = {}
        self.authors = {}
        self.areas = {}

        for c in commits:
            if c.date:
                day = (c.date.astimezone() - start).days
                if 0 <= day < 7:
                    self.by_day[day] += 1
            kind = classify(c.subject)
            self.kinds[kind] = self.kinds.get(kind, 0) + 1
            if c.author:
                self.authors[c.author] = self.authors.get(c.author, 0) + 1

        for path, (adds, dels) in files.items():
            # The first path segment is the closest thing a repository has to
            # a subject area without knowing anything about the project.
            area = path.split("/")[0] if "/" in path else "(root)"
            a, d, n = self.areas.get(area, (0, 0, 0))
            self.areas[area] = (a + adds, d + dels, n + 1)

    @property
    def additions(self):
        return sum(a for a, _ in self.files.values())

    @property
    def deletions(self):
        return sum(d for _, d in self.files.values())

    @property
    def days_active(self):
        return sum(1 for d in self.by_day if d)

    def ranked(self, mapping, key=None):
        return sorted(mapping.items(), key=key or (lambda kv: -kv[1]))

    def state(self):
        """A one-word reading of the week's shape."""
        n, days = len(self.commits), self.days_active
        if not n:
            return "quiet", DIM
        if days >= 5:
            return "sustained", ACTIVE
        if n >= 12:
            return "concentrated", ACTIVE
        if days >= 2:
            return "moving", ACCENT
        return "touched", WARN

    def sentences(self):
        """The digest proper: what happened, where, and by whose hand."""
        out = []
        elapsed = min(7, (datetime.now().astimezone() - self.start).days + 1)
        if not self.commits:
            if self.closed:
                out.append(f"Nothing committed in the week of "
                           f"{self.start.strftime('%B %d')}.")
            elif elapsed <= 1:
                out.append("Nothing committed yet -- the week opened today.")
            else:
                out.append(f"No commits in the {plural(elapsed, 'day')} "
                           f"since Sunday.")
            if self.closed:
                pass                    # the working tree is not that week's
            elif self.worktree:
                out.append(f"{plural(self.worktree, 'file')} changed in the "
                           f"working tree, uncommitted.")
            elif self.worktree == 0:
                out.append("The working tree is clean.")
            return out

        n = len(self.commits)
        span = f"{plural(n, 'commit')} across {plural(self.days_active, 'day')}"
        window = "that week" if self.closed else \
            ("the week so far" if elapsed < 7 else "the week")
        out.append(f"{span} of {window}{'+' if self.partial else ''}.")

        kinds = self.ranked(self.kinds)
        lead, lead_n = kinds[0]
        if n == 1:
            out.append(f"It {lead} something.")
        elif lead_n == n:
            out.append(f"Every one of them {lead} something.")
        elif lead_n * 2 >= n:
            out.append(f"Mostly {lead} -- {lead_n} of {n}"
                       + (f", then {kinds[1][1]} {kinds[1][0]}." if len(kinds) > 1
                          else "."))
        else:
            out.append("A spread of work: "
                       + ", ".join(f"{c} {k}" for k, c in kinds[:3]) + ".")

        if self.files:
            areas = self.ranked(self.areas, key=lambda kv: -kv[1][2])
            top, (a, d, count) = areas[0]
            where = f"{plural(len(self.files), 'file')} touched, "
            where += f"+{self.additions} −{self.deletions}"
            if len(areas) == 1:
                where += f", all under {top}."
            else:
                where += f", heaviest in {top} ({plural(count, 'file')})."
            out.append(where)

        if len(self.authors) > 1:
            hands = self.ranked(self.authors)
            out.append("Hands: " + ", ".join(f"{who} ({c})" for who, c in hands[:3])
                       + ".")

        head = self.commits[0]
        out.append(f"Latest: \"{head.subject}\" -- {ago(head.date)}.")
        # The working tree describes now, not the window. Reporting it beside
        # a past week would attribute today's edits to that week.
        if self.worktree and not self.closed:
            out.append(f"{plural(self.worktree, 'file')} changed since, "
                       f"uncommitted.")
        return out


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

    def week(self, repo, start, back=0):
        """Commits in the Sunday-to-Sunday window.

        Per-file churn is deliberately not fetched: the API bills a request
        per commit for it, and a digest that costs thirty round trips is not
        a digest. Counts and classification only, over the wire.
        """
        def utc(dt):
            return dt.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

        params = {"since": utc(start), "per_page": PAGE}
        if back:
            params["until"] = utc(start + timedelta(days=7))
        batch = self._get(f"/repos/{self.user}/{repo}/commits", params)
        commits = []
        for c in batch:
            info = c.get("commit", {})
            subject = info.get("message", "").partition("\n")[0].strip()
            author = (c.get("author") or {}).get("login") \
                or (info.get("author") or {}).get("name", "")
            commits.append(Commit(c.get("sha", ""),
                                  parse_iso((info.get("author") or {}).get("date")),
                                  author, subject))
        return WeekDigest(repo, start, commits, {}, partial=len(batch) == PAGE,
                          back=back)


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

    def _discover(self, path, depth, found):
        """Collect repositories down to SCAN_DEPTH levels.

        A repository can also contain repositories -- a working root that is
        itself under version control, with projects checked out inside it --
        so finding one is not a reason to stop descending.
        """
        if self._is_repo(path):
            found.append(path)
        if depth >= SCAN_DEPTH:
            return
        try:
            entries = sorted(os.scandir(path), key=lambda e: e.name)
        except OSError:
            return                     # unreadable share or permission wall
        for entry in entries:
            # Dotted directories are tooling, not projects, and .git itself
            # would otherwise be walked as though it were one.
            if entry.name.startswith(".") or not entry.is_dir(follow_symlinks=False):
                continue
            self._discover(entry.path, depth + 1, found)

    def repos(self):
        found = []
        self._discover(self.root, 0, found)
        if not found:
            raise SourceError(f"no git repositories under {self.root}")

        out = []
        for path in found:
            # Nested projects are named by their path from the root, so
            # "Archive/foo" and "game_projects/foo" stay distinguishable.
            rel = os.path.relpath(path, self.root).replace(os.sep, "/")
            name = os.path.basename(self.root.rstrip("\\/")) or self.root \
                if rel == "." else rel
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
        return self._numstat(raw)

    def week(self, repo, start, back=0):
        """Commits in the Sunday-to-Sunday window, with the churn behind them.

        On disk the file statistics are free, so unlike the API path this
        digest can say where the work landed, not merely that it happened.
        """
        path = self._paths.get(repo, os.path.join(self.root, repo))
        args = ["log", f"--since={start.isoformat()}", "--numstat",
                "--pretty=format:\x1e%H\x1f%aI\x1f%an\x1f%s"]
        if back:
            args.insert(2, f"--until={(start + timedelta(days=7)).isoformat()}")
        raw = self._run(path, args)
        commits, files = [], {}
        for record in raw.split("\x1e"):
            lines = [l for l in record.splitlines() if l.strip()]
            if not lines:
                continue
            parts = (lines[0].split("\x1f") + [""] * 4)[:4]
            commits.append(Commit(parts[0], parse_iso(parts[1]), parts[2], parts[3]))
            for name, a, d in self._numstat("\n".join(lines[1:]))["files"]:
                prev_a, prev_d = files.get(name, (0, 0))
                files[name] = (prev_a + a, prev_d + d)

        worktree = None
        try:
            dirty = self._run(path, ["status", "--porcelain"])
            worktree = len([l for l in dirty.splitlines() if l.strip()])
        except SourceError:
            pass                        # bare repository: no working tree to read
        return WeekDigest(repo, start, commits, files, worktree, back=back)

    @staticmethod
    def _numstat(raw):
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

    def render_cell(self, item, selected, inner):
        """One list line, without borders, at most `inner` columns wide."""
        raise NotImplementedError

    def side_panel(self, item, rows, width):
        """Optional right-hand column describing the selected item."""
        return None

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
        inner = width - 2

        # The panel is a luxury of width: on a narrow window the list keeps
        # every column and the marks simply aren't drawn.
        panel = None
        if items and width >= PANEL_MIN_TERM:
            panel = self.side_panel(items[self.cursor], rows, PANEL_W)
        list_w = inner - PANEL_W - 3 if panel else inner
        divider = f" {DIM}│{RESET} "

        def body_line(left, panel_idx):
            if not panel:
                return row(left, width)
            side = panel[panel_idx] if panel_idx < len(panel) else ""
            return row(pad(left, list_w) + divider + pad(side, PANEL_W), width)

        lines = ["", "  " + rule("╭", "╮", width=width)]
        lines.append("  " + split_row(f" {ACCENT}{BOLD}{self.title}{RESET}",
                                      f"{DIM}{self.subtitle()}{RESET} ", width=width))
        lines.append("  " + (rule_split("├", "┤", list_w + 1, "┬", width)
                             if panel else rule("├", "┤", width=width)))

        if not items:
            empty = "no matches" if self.filter else "nothing here"
            lines.append("  " + row(f" {DIM}{empty}{RESET}", width=width))
            for _ in range(rows - 1):
                lines.append("  " + row("", width=width))
        else:
            for offset in range(rows):
                idx = top + offset
                left = self.render_cell(items[idx], idx == self.cursor, list_w) \
                    if idx < len(items) else ""
                lines.append("  " + body_line(left, offset))
            if self.cursor >= len(items) - 1:
                self.on_bottom()

        lines.append("  " + (rule_split("├", "┤", list_w + 1, "┴", width)
                             if panel else rule("├", "┤", width=width)))
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
    hint = "↑↓ move   ⏎ commits   w week   / filter   q quit"

    def __init__(self, app, repos):
        super().__init__(app)
        self.title = "git history"
        self.items = repos

    def subtitle(self):
        return self.app.source.label

    def label_of(self, repo):
        return repo.name + " " + repo.subtitle

    def render_cell(self, repo, selected, inner):
        age = ago(repo.updated)
        right = f"{repo.extra}  {age}".strip()
        name_w = inner - 6 - visible_len(right) - visible_len(repo.subtitle)
        name = fit(repo.name, max(8, name_w))
        marker, tone = (f"{ACTIVE}▍{RESET}", f"{ACTIVE}{BOLD}") if selected \
            else ("  ", IDLE)
        left = f" {marker} {tone}{name}{RESET}"
        if repo.subtitle and inner >= 46:
            left += f"  {DIM}{repo.subtitle}{RESET}"
        return cells(left, f"{DIM}{right}{RESET} ", inner)

    def side_panel(self, repo, rows, width):
        """The project's mark, over the facts that identify it."""
        gap = " " * max(0, (width - SIGIL_W) // 2)
        lines = [""] + [gap + art for art in sigil_lines(repo.name, repo.subtitle)]
        lines.append("")
        for line in wrap(repo.name, width - 1):
            lines.append(f"{BOLD}{line}{RESET}")
        if repo.subtitle:
            lines.append(f"{DIM}{repo.subtitle}{RESET}")
        if repo.extra:
            lines.append(f"{DIM}{repo.extra}{RESET}")
        if repo.updated:
            lines.append("")
            lines.append(f"{DIM}{stamp(repo.updated)}{RESET}")
        return lines[:rows]

    def activate(self, repo):
        self.app.open_commits(repo.name)

    def handle(self, key):
        if key in ('esc', 'backspace', 'left', 'h') and not self.filtering:
            if self.filter:
                self.filter = ""
                return True
            self.app.running = False
            return True
        if key in ('w', 'W') and not self.filtering:
            items = self.visible_items()
            if items:
                self.app.open_week(items[self.cursor].name)
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

    def render_cell(self, commit, selected, inner):
        when = stamp(commit.date)
        marker, tone = (f"{ACTIVE}●{RESET}", f"{ACTIVE}{BOLD}") if selected \
            else (f"{DIM}·{RESET}", IDLE)
        meta_w = visible_len(when) + 10
        subject = fit(commit.subject or "(no subject)", max(10, inner - 6 - meta_w))
        return cells(f" {marker} {tone}{subject}{RESET}",
                     f"{DIM}{commit.short}  {when}{RESET} ", inner)

    def side_panel(self, commit, rows, width):
        gap = " " * max(0, (width - SIGIL_W) // 2)
        lines = [""] + [gap + art for art in sigil_lines(self.repo)]
        lines.append("")
        lines.append(f"{BOLD}{fit(commit.short, width)}{RESET}")
        if commit.author:
            lines.append(f"{DIM}{fit(commit.author, width)}{RESET}")
        lines.append(f"{DIM}{ago(commit.date)}{RESET}")
        return lines[:rows]

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


class TextScreen(Screen):
    """A scrolling body of prose rather than a list of rows."""

    hint = "↑↓ scroll   esc back   q quit"

    def heading(self, width):
        return f" {ACCENT}{BOLD}{self.title}{RESET}", f"{DIM}{self.subtitle()}{RESET} "

    def lines_for(self, width):
        raise NotImplementedError

    def build(self, width, height):
        body = self.lines_for(width)
        rows = max(1, height - 7)
        self.cursor = max(0, min(self.cursor, max(0, len(body) - 1)))
        top = min(self.cursor, max(0, len(body) - rows))

        left, right = self.heading(width)
        lines = ["", "  " + rule("╭", "╮", width=width)]
        lines.append("  " + split_row(left, right, width=width))
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


class DetailScreen(TextScreen):
    def __init__(self, app, repo, commit, detail):
        super().__init__(app)
        self.title = commit.short
        self.repo = repo
        self.commit = commit
        self.detail = detail

    def heading(self, width):
        return (f" {ACCENT}{BOLD}{self.repo}{RESET} "
                f"{DIM}@ {self.commit.short}{RESET}",
                f"{DIM}{self.subtitle()}{RESET} ")

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


class WeekScreen(TextScreen):
    """Where a project stands, for the week that began on Sunday."""

    hint = "↑↓ scroll   p/n week   esc back   q quit"

    def __init__(self, app, digest):
        super().__init__(app)
        self.digest = digest
        self.title = digest.repo

    def heading(self, width):
        word, tone = self.digest.state()
        return (f" {ACCENT}{BOLD}{fit(self.digest.repo, width // 2)}{RESET}"
                f" {DIM}·{RESET} {tone}{word}{RESET}",
                f"{DIM}{week_label(self.digest.back)}"
                f"  ·  {self.digest.start.strftime('%b %d')}{RESET} ")

    def step(self, delta):
        """Move the window a week and recompute. Never cached, by design."""
        back = self.digest.back + delta
        if back < 0:
            return
        self.app.splash(f"reading {week_label(back)} in {self.digest.repo}…")
        try:
            self.digest = self.app.source.week(self.digest.repo,
                                               week_start(back=back), back)
            self.cursor = 0
        except SourceError as e:
            self.status = str(e)

    def handle(self, key):
        if key in ('p', 'P'):
            self.step(1)
            return True
        if key in ('n', 'N'):
            self.step(-1)
            return True
        return super().handle(key)

    def lines_for(self, width):
        d = self.digest
        inner = width - 4
        out = []

        # The week as a shape before the week as a sentence: seven days, one
        # column each, Sunday on the left.
        today = (datetime.now().astimezone() - d.start).days
        bars = sparkline(d.by_day)
        labels, marks = [], []
        for i in range(7):
            labels.append(DAY_INITIALS[i])
            marks.append(bars[i])
        out.append(" " + " ".join(f"{ACTIVE if i == today else IDLE}{m}{RESET}"
                                  for i, m in enumerate(marks))
                   + f"   {DIM}{plural(sum(d.by_day), 'commit')}"
                     f" {week_label(d.back)}{RESET}")
        out.append(" " + " ".join(f"{DIM if i != today else ACCENT}{l}{RESET}"
                                  for i, l in enumerate(labels)))
        out.append("")

        for sentence in d.sentences():
            for line in wrap(sentence, inner - 1):
                out.append(f" {IDLE}{line}{RESET}")
            out.append("")

        if d.kinds:
            out.append(f" {DIM}kind of work{RESET}")
            widest = max(len(k) for k in d.kinds)
            for kind, count in d.ranked(d.kinds):
                bar = "█" * min(count, max(4, inner - widest - 12))
                out.append(f"  {IDLE}{kind.ljust(widest)}{RESET} "
                           f"{ACCENT}{bar}{RESET} {DIM}{count}{RESET}")
            out.append("")

        if d.areas:
            out.append(f" {DIM}where it landed{RESET}")
            for area, (a, dl, count) in d.ranked(d.areas,
                                                 key=lambda kv: -kv[1][2])[:8]:
                churn = f"+{a} −{dl}"
                name = fit(area, max(6, inner - visible_len(churn) - 14))
                gap = " " * max(1, inner - visible_len(name)
                                - visible_len(churn) - 12)
                out.append(f"  {IDLE}{name}{RESET}{gap}"
                           f"{DIM}{plural(count, 'file')}{RESET}  "
                           f"{ADD}+{a}{RESET} {DEL}−{dl}{RESET}")
            out.append("")

        if not d.files and d.commits:
            out.append(f" {DIM}file counts need a local clone; over the API"
                       f" they would cost a request per commit{RESET}")
        return out


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

    def open_week(self, repo):
        """Always recomputed. A digest describes now, and a stored one ages."""
        self.splash(f"reading the week in {repo}…")
        try:
            digest = self.source.week(repo, week_start())
        except SourceError as e:
            self.error(str(e))
            return
        self.stack.append(WeekScreen(self, digest))

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

  TARGET   a directory of clones, or a GitHub username. Defaults to the
           working root, and to the current directory if it is offline.

  -u USER  force GitHub lookup for USER
  -d DIR   force local lookup under DIR
  -h       this message

  Local repositories are found up to three directories deep.

  GIT_HISTORY_ROOT overrides the working root (currently {root}).
  GITHUB_TOKEN or GH_TOKEN raises the GitHub rate limit when set.
""".replace("{root}", DEFAULT_ROOT or "the current directory")


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
        # The share is the default working root; the current directory is the
        # fallback when it is not mounted, so an unplugged network drive
        # degrades to something useful instead of an error.
        if DEFAULT_ROOT and os.path.isdir(DEFAULT_ROOT):
            return LocalSource(DEFAULT_ROOT)
        if DEFAULT_ROOT:
            print(f"  {WARN}!{RESET} {DIM}{DEFAULT_ROOT} is not available"
                  f" -- reading the current directory{RESET}")
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

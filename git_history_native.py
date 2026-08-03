import os
import re
import sys
import subprocess
import urllib.request
import tempfile
import shutil
import unicodedata

# Windows consoles don't interpret ANSI escapes unless virtual terminal
# processing is switched on. Harmless no-op everywhere else.
if os.name == 'nt':
    try:
        import ctypes
        kernel32 = ctypes.windll.kernel32
        kernel32.SetConsoleMode(kernel32.GetStdHandle(-11), 7)
    except Exception:
        pass

# --- palette -----------------------------------------------------------
RESET   = "\033[0m"
DIM     = "\033[90m"
BOLD    = "\033[1m"
ACCENT  = "\033[38;5;39m"   # cyan-blue chrome
ACTIVE  = "\033[38;5;120m"  # selected row
IDLE    = "\033[38;5;250m"  # unselected row
WARN    = "\033[38;5;214m"
ERR     = "\033[38;5;203m"

# The layout matrix resizes with the window: every frame is measured against
# the live terminal size and clamped to a comfortable reading measure.
MIN_BOX_W = 34
MAX_BOX_W = 74
GUTTER = 2          # blank columns outside the frame on each side

ANSI_RE = re.compile(r'\033\[[0-9;]*m')


def term_size():
    return shutil.get_terminal_size(fallback=(100, 24))


def term_width():
    return term_size().columns


def box_width():
    """Frame width for the current window, inside sane typographic bounds."""
    return max(MIN_BOX_W, min(MAX_BOX_W, term_width() - GUTTER * 2))


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


def rule(left, right, fill="─", width=None):
    width = width or box_width()
    return f"{DIM}{left}{fill * (width - 2)}{right}{RESET}"


def row(content, width=None):
    """Pad a possibly-colored string inside the box borders."""
    width = width or box_width()
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
    left = max(0, space // 2)
    return row(" " * left + content, width)


def hide_cursor():
    sys.stdout.write("\033[?25l")


def show_cursor():
    sys.stdout.write("\033[?25h")

# Child git processes write straight to our stdout. When stdout is a pipe rather
# than a terminal it is block-buffered, so our own prints would surface after
# git's. Line buffering keeps the two streams interleaved in the right order.
try:
    sys.stdout.reconfigure(line_buffering=True)
except AttributeError:  # Python < 3.7
    pass

# Cross-platform keyboard reader for arrow-key navigation
try:
    import msvcrt
    def get_key():
        ch = msvcrt.getch()
        if ch in (b'\x00', b'\xe0'):  # Arrow key prefix on Windows
            ch = msvcrt.getch()
            if ch == b'H': return 'up'
            if ch == b'P': return 'down'
        if ch == b'\r': return 'enter'
        if ch in (b'\x03', b'q', b'Q'): return 'quit'
        return None
except ImportError:
    import tty, termios
    def get_key():
        fd = sys.stdin.fileno()
        old_settings = termios.tcgetattr(fd)
        try:
            tty.setraw(sys.stdin.fileno())
            ch = sys.stdin.read(1)
            if ch == '\x1b':  # Arrow key escape sequence on Unix/Linux/Mac
                ch2 = sys.stdin.read(2)
                if ch2 == '[A': return 'up'
                if ch2 == '[B': return 'down'
            if ch in ('\n', '\r'): return 'enter'
            if ch in ('\x03', 'q', 'Q'): return 'quit'
        finally:
            termios.tcsetattr(fd, termios.TCSADRAIN, old_settings)
        return None

# Profile-relative links that look like repo links but aren't
NON_REPO_PATHS = {"followers", "following", "stars", "repositories", "projects", "packages", "sponsors"}

def fetch_repositories(username):
    """Bypasses API blocks by parsing the public profile HTML layout directly."""
    url = f"https://github.com/{username}?tab=repositories"
    headers = {
        "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9"
    }
    try:
        req = urllib.request.Request(url, headers=headers)
        with urllib.request.urlopen(req, timeout=8) as response:
            html = response.read().decode('utf-8')
            
        # Extract repository names from the profile's repo links, in page order
        repo_matches = re.findall(r'href="/' + re.escape(username) + r'/([A-Za-z0-9._-]+)"', html)

        unique_repos = []
        for r in repo_matches:
            clean_name = r.strip()
            if clean_name and clean_name not in NON_REPO_PATHS and clean_name not in unique_repos:
                unique_repos.append(clean_name)
                
        if unique_repos:
            return unique_repos
    except Exception:
        pass
    
    # Robust fallback tracking your profile if offline/blocked
    return ["NEXUS", "Octioid", "adventure_generator", "obeliskv1", "obeliskv2"]
def render_commit_history(username, repo):
    """Uses system git binaries to pull log details instantly into memory."""
    os.system('cls' if os.name == 'nt' else 'clear')
    print()
    print(f"  {WARN}◐{RESET} {DIM}fetching{RESET} {BOLD}{username}/{repo}{RESET}{DIM}…{RESET}")
    tmp_dir = tempfile.mkdtemp()

    # Explicit URL construction with guaranteed slashes
    repo_url = f"https://github.com/{username}/{repo}.git"

    try:
        # Bare clone grabs references instantly without writing full files to disk
        subprocess.run(["git", "clone", "--bare", "--quiet", repo_url, tmp_dir], check=True)

        os.system('cls' if os.name == 'nt' else 'clear')
        width = box_width()
        print()
        print("  " + rule("╭", "╮", width=width))
        print("  " + split_row(f" {ACCENT}{BOLD}{fit(repo, width - 22)}{RESET}",
                               f"{DIM}last 10 commits{RESET} ", width=width))
        print("  " + rule("├", "┤", width=width))

        # Subjects are wrapped in Python rather than truncated by git's %<(),
        # so nothing is lost on long commit titles. Plain (uncolored) fields
        # are captured and colored after wrapping -- a wrapper would otherwise
        # count escape bytes as visible width and break the alignment.
        result = subprocess.run([
            "git", "-C", tmp_dir, "log",
            "--pretty=format:%h\x1f%cd\x1f%s",
            "--date=format:%Y-%m-%d %H:%M",
            "-n", "10"
        ], check=True, capture_output=True, text=True, encoding="utf-8", errors="replace")

        # The ● sits in the meta line beside the date, forming a timeline rail
        # down the left edge; subject lines hang off that same column.
        indent = "   "                       # under the ● glyph + its space
        subject_width = max(16, width - 2 - len(indent) - 1)

        commits = [l for l in result.stdout.splitlines() if l.strip()]
        for i, line in enumerate(commits):
            short_hash, when, subject = (line.split("\x1f", 2) + ["", ""])[:3]

            print("  " + row(f" {ACTIVE}●{RESET} {DIM}{when}{RESET}  {DIM}{short_hash}{RESET}",
                             width=width))
            for cont in wrap(subject, subject_width):
                print("  " + row(f"{indent}{BOLD}{cont}{RESET}", width=width))
            if i != len(commits) - 1:
                print("  " + row(f" {DIM}│{RESET}", width=width))

        print("  " + rule("╰", "╯", width=width))
        print()

    except subprocess.CalledProcessError:
        w = box_width()
        print("  " + rule("╭", "╮", width=w))
        print("  " + row(f" {ERR}✕{RESET} {BOLD}could not read logs{RESET}", width=w))
        print("  " + row(f"   {DIM}{fit(repo_url, w - 6)}{RESET}", width=w))
        print("  " + rule("╰", "╯", width=w))
        print()
    finally:
        # Safely scrub the temporary working directory tracking files.
        # Git marks object files read-only, which blocks plain deletes on Windows.
        def _force_remove(func, path, _exc):
            os.chmod(path, 0o700)
            func(path)
        shutil.rmtree(tmp_dir, onerror=_force_remove)
        
    print(f"  {DIM}press any key to return{RESET}")
    get_key()


def run_dashboard():
    username = "ObviousThr33s"
    os.system('cls' if os.name == 'nt' else 'clear')
    print(f"\n  {DIM}reading profile…{RESET}")
    repos = fetch_repositories(username)

    current_idx = 0
    hide_cursor()
    while True:
        # Clear the terminal screen window cleanly across OS environments
        os.system('cls' if os.name == 'nt' else 'clear')

        # Re-measure every frame so the layout tracks a resized window
        size = term_size()
        width = box_width()

        # Rows the list can use, leaving room for frame, header and footer
        visible = max(3, size.lines - 9)
        top = min(max(0, current_idx - visible // 2), max(0, len(repos) - visible))
        window = repos[top:top + visible]

        # Numbering column widens with the repo count instead of a magic ">2"
        num_w = len(str(len(repos)))
        name_w = width - 2 - (4 + num_w + 1)   # borders, marker, number, space

        print()
        print("  " + rule("╭", "╮", width=width))
        print("  " + center(f"{ACCENT}{BOLD}git history{RESET}  {DIM}·{RESET}  "
                            f"{IDLE}{username}{RESET}", width=width))
        print("  " + rule("├", "┤", width=width))

        for offset, repo in enumerate(window):
            idx = top + offset
            num = f"{idx + 1:>{num_w}}"
            name = fit(repo, name_w)
            if idx == current_idx:
                print("  " + row(f" {ACTIVE}▍{RESET} {DIM}{num}{RESET} "
                                 f"{ACTIVE}{BOLD}{name}{RESET}", width=width))
            else:
                print("  " + row(f"   {DIM}{num}{RESET} {IDLE}{name}{RESET}", width=width))

        print("  " + rule("├", "┤", width=width))
        counter = f"{current_idx + 1}/{len(repos)}"
        keys = "↑↓ move   ⏎ open   q quit" if width >= 46 else "↑↓  ⏎  q"
        print("  " + split_row(f" {DIM}{keys}{RESET}", f"{DIM}{counter}{RESET} ", width=width))
        print("  " + rule("╰", "╯", width=width))

        key = get_key()
        if key == 'up':
            current_idx = (current_idx - 1) % len(repos)
        elif key == 'down':
            current_idx = (current_idx + 1) % len(repos)
        elif key == 'enter':
            render_commit_history(username, repos[current_idx])
        elif key == 'quit':
            show_cursor()
            os.system('cls' if os.name == 'nt' else 'clear')
            print(f"\n  {DIM}bye.{RESET}\n")
            break

if __name__ == "__main__":
    try:
        run_dashboard()
    except KeyboardInterrupt:
        print(f"\n  {DIM}interrupted.{RESET}\n")
    finally:
        show_cursor()

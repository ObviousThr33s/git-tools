import os
import re
import sys
import subprocess
import urllib.request
import tempfile
import shutil

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
    print(f"\n\033[1;33m🔄 Pulling live log stream for {repo}... Please wait...\033[0m")
    tmp_dir = tempfile.mkdtemp()
    
    # Explicit URL construction with guaranteed slashes
    repo_url = f"https://github.com/{username}/{repo}.git"
    
    try:
        # Bare clone grabs references instantly without writing full files to disk
        subprocess.run(["git", "clone", "--bare", "--quiet", repo_url, tmp_dir], check=True)
        
        print(f"\n\033[1;36m=== Live Log History: {username}/{repo} ===\033[0m\n")
        
        # Keep every commit on exactly one line so the ● markers stay flush at
        # column 0 -- a wrapped subject would push an unmarked continuation line
        # into the column. Fixed overhead is "● " + "[date] " + " (hash)".
        subject_width = max(20, shutil.get_terminal_size(fallback=(100, 24)).columns - 31)

        # Colors come from git's %C placeholders, not raw escapes: git's %<()
        # padding counts literal escape bytes as visible width and would skew.
        log_format = (
            "%C(bold green)●%Creset "
            "%C(yellow)[%cd]%Creset "
            f"%C(bold blue)%<({subject_width},trunc)%s%Creset "
            "%C(brightblack)(%h)%Creset"
        )
        subprocess.run([
            "git", "-C", tmp_dir, "log", 
            f"--pretty=format:{log_format}", 
            "--date=format:%Y-%m-%d %H:%M", 
            "-n", "10"
        ], check=True)
        print("\n")
        
    except subprocess.CalledProcessError:
        print("\033[1;31m[-] System Git Engine Error: Failed to pipe remote repository logs.\033[0m")
        print(f"\033[90m💡 Debug Info: Tried to reach -> {repo_url}\033[0m\n")
    finally:
        # Safely scrub the temporary working directory tracking files.
        # Git marks object files read-only, which blocks plain deletes on Windows.
        def _force_remove(func, path, _exc):
            os.chmod(path, 0o700)
            func(path)
        shutil.rmtree(tmp_dir, onerror=_force_remove)
        
    print("\033[90mPress any key to go back to the menu...\033[0m")
    get_key()


def run_dashboard():
    username = "ObviousThr33s"
    print("\033[1;32m🔍 Connecting and rendering profile layout map...\033[0m")
    repos = fetch_repositories(username)
    
    current_idx = 0
    while True:
        # Clear the terminal screen window cleanly across OS environments
        os.system('cls' if os.name == 'nt' else 'clear')
        
        print(f"\033[1;36m=== GitHub Account Terminal Visualizer ({username}) ===\033[0m")
        print("\033[90mControls: [↑/↓] Navigate | [Enter] View Logs | [Q] Exit Code\033[0m\n")
        
        # Display the cursor options list
        for idx, repo in enumerate(repos):
            if idx == current_idx:
                print(f" \033[1;32m➔  [{idx + 1}] {repo:<25} (Selected)\033[0m")
            else:
                print(f"    [{idx + 1}] \033[1;34m{repo:<25}\033[0m")
                
        print("\n" + "─" * 60)
        
        key = get_key()
        if key == 'up':
            current_idx = (current_idx - 1) % len(repos)
        elif key == 'down':
            current_idx = (current_idx + 1) % len(repos)
        elif key == 'enter':
            render_commit_history(username, repos[current_idx])
        elif key == 'quit':
            print("\n\033[1;33mVisualizer session closed. Goodbye!\033[0m\n")
            break

if __name__ == "__main__":
    try:
        run_dashboard()
    except KeyboardInterrupt:
        print("\n\033[1;33mSession interrupted.\033[0m\n")

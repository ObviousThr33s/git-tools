//! git history -- browse commit logs in the terminal.
//!
//! # Offline by construction
//!
//! The tool makes no network call unless you name a GitHub account on the
//! command line. Nothing phones home, nothing checks for updates, nothing
//! resolves a hostname at startup. The default path -- a working root of
//! local clones -- reads the git binary and the filesystem, and that is all.
//!
//! The optional model narrator only ever addresses loopback: 127.0.0.1. A
//! machine with no network at all can still run it, because the model runs
//! on that machine.
//!
//! Dependencies are vendored, so `cargo build --offline` works from a cold
//! machine with the network unplugged.

#[cfg(feature = "window")]
mod gpu;
mod llm;
mod model;
mod sigil;
mod source;
mod ui;

use source::{GitHubSource, LocalSource, Source, SourceError};

/// The working root. A network share is the normal case here, so its absence
/// is a routine condition -- disconnected, not misconfigured.
fn default_root() -> String {
    std::env::var("GIT_HISTORY_ROOT").unwrap_or_else(|_| {
        if cfg!(windows) { "Z:\\".to_string() } else { String::new() }
    })
}

fn usage() -> String {
    let root = default_root();
    let root = if root.is_empty() { "the current directory".to_string() } else { root };
    format!(
        "git history — browse commit logs in the terminal

  git-history-rs [TARGET]

  TARGET        a directory of clones, or a GitHub username. Defaults to the
                working root, and to the current directory if it is offline.

  -d DIR        force local lookup under DIR
  -u USER       force GitHub lookup for USER  (the only networked mode)
  --check       print what was found and this week's digest, then exit
  --no-model    skip probing loopback for a local model
  -w, --window  open in a window instead of the terminal; the text grid is
                recut as you drag the frame, so the view reflows to the borders
  --vulkan      with --window, force the Vulkan adapter rather than the
                machine's default (DX12 on most Windows boxes)
  -h            this message

  Local repositories are found up to three directories deep.
  No network is touched unless -u or a GitHub username is given.

  GIT_HISTORY_ROOT   working root (currently {root})
  GIT_HISTORY_LLM    model endpoint, else loopback is probed
  GIT_HISTORY_MODEL  model for the weekly synthesis
  GIT_HISTORY_MODEL_SMALL  model for per-day passes, if different
  GITHUB_TOKEN / GH_TOKEN  raises the GitHub rate limit
"
    )
}

fn build_source(args: &[String]) -> Result<Option<Box<dyn Source>>, SourceError> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", usage());
        return Ok(None);
    }

    if let Some(i) = args.iter().position(|a| a == "-u") {
        let user = args
            .get(i + 1)
            .ok_or_else(|| SourceError("-u needs a username".into()))?;
        return Ok(Some(Box::new(GitHubSource::new(user.clone()))));
    }
    if let Some(i) = args.iter().position(|a| a == "-d") {
        let dir = args
            .get(i + 1)
            .ok_or_else(|| SourceError("-d needs a directory".into()))?;
        return Ok(Some(Box::new(LocalSource::new(dir)?)));
    }

    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if positional.is_empty() {
        let root = default_root();
        // The share is the default working root; the current directory is
        // the fallback, so an unplugged drive degrades to something useful.
        if !root.is_empty() && std::path::Path::new(&root).is_dir() {
            return Ok(Some(Box::new(LocalSource::new(&root)?)));
        }
        if !root.is_empty() {
            eprintln!("  ! {root} is not available -- reading the current directory");
        }
        let cwd = std::env::current_dir()
            .map_err(|e| SourceError(format!("no current directory: {e}")))?;
        return Ok(Some(Box::new(LocalSource::new(cwd)?)));
    }

    // A path that exists is a path; anything else is an account name -- and
    // only that second case ever reaches the network.
    let target = positional[0];
    if std::path::Path::new(target).is_dir() {
        Ok(Some(Box::new(LocalSource::new(target)?)))
    } else {
        Ok(Some(Box::new(GitHubSource::new(target.clone()))))
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut source = match build_source(&args) {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            ui::report(&e);
            std::process::exit(1);
        }
    };

    let repos = match source.repos() {
        Ok(r) => r,
        Err(e) => {
            ui::report(&e);
            std::process::exit(1);
        }
    };

    // A terminal-free path: everything the tool knows, printed. Useful for a
    // scripted check, and the only way to exercise the sources without a
    // console attached.
    if args.iter().any(|a| a == "--check") {
        println!("source: {}", source.label());
        println!("{} repositories", repos.len());
        for r in &repos {
            println!(
                "  {:<34} {:<12} {}",
                r.name,
                r.subtitle,
                model::ago(r.updated)
            );
        }
        if let Some(first) = repos.first() {
            match source.week(&first.name, model::week_start(0), 0) {
                Ok(d) => {
                    println!("\nweek of {} -- {}", d.start.format("%b %d"), d.state());
                    for s in d.sentences() {
                        println!("  {s}");
                    }
                }
                Err(e) => println!("\nweek: {e}"),
            }
        }
        match llm::detect() {
            Some(n) => println!("\nmodel: {} via {} ({})", n.model, n.runtime, n.endpoint),
            None => println!("\nmodel: none on loopback -- digest is unaffected"),
        }
        return;
    }

    // Loopback only, and skippable. A refused connection returns at once, so
    // a machine with no model pays almost nothing for the question.
    let narrator = if args.iter().any(|a| a == "--no-model") {
        None
    } else {
        llm::detect()
    };

    // The window path never touches the console, so it must not init ratatui's
    // terminal at all -- doing so would leave a raw-mode console behind it.
    if args.iter().any(|a| a == "-w" || a == "--window") {
        #[cfg(feature = "window")]
        {
            let prefs = gpu::Prefs { vulkan: args.iter().any(|a| a == "--vulkan") };
            if let Err(e) = gpu::run(source, repos, narrator, prefs) {
                eprintln!("\n  ✕ {e}\n");
                std::process::exit(1);
            }
            return;
        }
        #[cfg(not(feature = "window"))]
        {
            eprintln!(
                "\n  ✕ this build has no window.\n    rebuild it with:  cargo build --release --features window\n"
            );
            std::process::exit(1);
        }
    }

    let mut terminal = ratatui::init();
    let result = ui::App::new(source, repos, narrator).run(&mut terminal);
    ratatui::restore();

    if let Err(e) = result {
        eprintln!("\n  ✕ {e}\n");
        std::process::exit(1);
    }
    println!("\n  bye.\n");
}

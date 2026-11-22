//! `tango` entry point: the TUI binary is what `tango` on PATH runs. Interactive
//! launches open the TUI right here; every other use (one-shot prompts, task,
//! serve, repl, --help, pipes) is handed to the Node CLI by exec, so no idle
//! launcher process stays behind in either case.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// What an interactive launch asked for (mirrors src/cli.ts parseArgs).
#[derive(Debug, Default, PartialEq)]
pub struct Opts {
    pub yes: bool,
    pub readonly: bool,
    pub cont: bool,
    pub resume: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
}

/// Some(opts) when these args open the TUI; None hands them to the Node CLI,
/// which then produces exactly the behavior (and errors) it always has.
pub fn parse(args: &[String], tty: bool) -> Option<Opts> {
    let mut o = Opts::default();
    let mut rest: Vec<&str> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-y" | "--yes" => o.yes = true,
            "--readonly" => o.readonly = true,
            "-c" | "--continue" => o.cont = true,
            "-r" | "--resume" => o.resume = Some(it.next()?.clone()),
            "-m" | "--model" => o.model = Some(it.next()?.clone()),
            "--cwd" => o.cwd = Some(it.next()?.clone()),
            "-h" | "--help" | "-V" | "--version" | "--models" | "--sessions" => return None,
            other => rest.push(other),
        }
    }
    if o.yes && o.readonly {
        return None; // Node prints the "cannot be combined" error
    }
    match rest.first() {
        Some(&"tui") => Some(o),
        None if tty => Some(o),
        _ => None,
    }
}

/// The repo root: this binary lives at <root>/tui/target/{release,debug}/,
/// reached through the npm-link symlink, so resolve that first.
pub fn repo_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let root = exe.parent()?.parent()?.parent()?.parent()?.to_path_buf();
    root.join("bin").join("tango.js").exists().then_some(root)
}

/// `node <root>/bin/tango.js` — the Node CLI, run with the node on PATH.
pub fn node_cli(root: &Path) -> (String, PathBuf) {
    ("node".to_string(), root.join("bin").join("tango.js"))
}

fn tango_dir() -> PathBuf {
    match std::env::var_os("TANGO_HOME") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".tango"),
    }
}

/// `tango -c`: the newest session by its meta createdAt (SessionStore.list order).
/// The meta entry is always a session file's first line, and renames keep createdAt.
pub fn latest_session(dir: &Path) -> Option<String> {
    use std::io::BufRead;
    let mut best: Option<(String, String)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut first = String::new();
        if std::io::BufReader::new(file).read_line(&mut first).is_err() {
            continue;
        }
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(&first) else {
            continue;
        };
        if meta["kind"] != "meta" {
            continue;
        }
        let Some(created) = meta["createdAt"].as_str() else {
            continue;
        };
        let id = match meta["id"].as_str() {
            Some(id) => id.to_string(),
            None => path.file_stem()?.to_string_lossy().into_owned(),
        };
        if best.as_ref().is_none_or(|(c, _)| created > c.as_str()) {
            best = Some((created.to_string(), id));
        }
    }
    best.map(|(_, id)| id)
}

/// Apply an interactive launch: working directory and the TANGO_* environment
/// the serve child reads (same mapping src/cli.ts launchTui used).
pub fn apply(o: &Opts, root: Option<&Path>) -> Result<(), String> {
    // SAFETY (set_var): runs at startup, before any thread is spawned
    if let Some(dir) = &o.cwd {
        std::env::set_current_dir(dir).map_err(|e| format!("--cwd {dir}: {e}"))?;
    }
    if let Some(root) = root
        && std::env::var_os("TANGO_BIN").is_none()
    {
        let (node, cli) = node_cli(root);
        unsafe { std::env::set_var("TANGO_BIN", format!("{node} {}", cli.display())) };
    }
    let resume = match (&o.resume, o.cont) {
        (Some(id), _) => Some(id.clone()),
        (None, true) => Some(
            latest_session(&tango_dir().join("sessions")).ok_or("no sessions to continue yet")?,
        ),
        _ => None,
    };
    unsafe {
        if let Some(id) = resume {
            std::env::set_var("TANGO_RESUME", id);
        }
        if let Some(m) = &o.model {
            std::env::set_var("TANGO_MODEL", m);
        }
        if o.yes {
            std::env::set_var("TANGO_MODE", "auto");
        }
        if o.readonly {
            std::env::set_var("TANGO_MODE", "readonly");
        }
    }
    Ok(())
}

/// Replace this process with the Node CLI, passing the original args through.
pub fn exec_node(root: &Path, args: &[OsString]) -> ! {
    let (node, cli) = node_cli(root);
    let mut cmd = std::process::Command::new(&node);
    cmd.arg(&cli).args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = cmd.exec(); // only returns on failure
        eprintln!("tango: cannot run {node} {}: {e}", cli.display());
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    match cmd.status() {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("tango: cannot run {node} {}: {e}", cli.display());
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str], tty: bool) -> Option<Opts> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>(), tty)
    }

    #[test]
    fn interactive_launches_open_the_tui() {
        assert_eq!(p(&[], true), Some(Opts::default()));
        let o = p(&["-y", "-m", "x/y", "--cwd", "/tmp"], true).unwrap();
        assert!(o.yes);
        assert_eq!(o.model.as_deref(), Some("x/y"));
        assert_eq!(o.cwd.as_deref(), Some("/tmp"));
        assert!(p(&["-c"], true).unwrap().cont);
        assert_eq!(
            p(&["-r", "ab12"], true).unwrap().resume.as_deref(),
            Some("ab12")
        );
        assert!(p(&["--readonly"], true).unwrap().readonly);
        // explicit `tui` opens it even without a terminal check
        assert!(p(&["tui"], false).is_some());
    }

    #[test]
    fn everything_else_goes_to_the_node_cli() {
        assert_eq!(p(&[], false), None, "pipes / launchd");
        assert_eq!(p(&["list", "files"], true), None, "one-shot prompt");
        for sub in ["task", "serve", "repl"] {
            assert_eq!(p(&[sub], true), None, "{sub}");
        }
        for flag in ["-h", "--help", "-V", "--version", "--models", "--sessions"] {
            assert_eq!(p(&[flag], true), None, "{flag}");
        }
        assert_eq!(
            p(&["-y", "--readonly"], true),
            None,
            "Node reports the conflict"
        );
        assert_eq!(p(&["-m"], true), None, "missing value");
        assert_eq!(
            p(&["--weird"], true),
            None,
            "unknown flags are prompt text in Node"
        );
    }

    #[test]
    fn continue_picks_the_newest_created_session() {
        let dir = std::env::temp_dir().join(format!("tango-launch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = |name: &str, body: &str| std::fs::write(dir.join(name), body).unwrap();
        w(
            "old.jsonl",
            "{\"kind\":\"meta\",\"id\":\"old\",\"createdAt\":\"2026-01-01T00:00:00Z\"}\n",
        );
        w(
            "new.jsonl",
            "{\"kind\":\"meta\",\"id\":\"new\",\"createdAt\":\"2026-02-01T00:00:00Z\"}\n{}\n",
        );
        w("junk.jsonl", "not json\n");
        w(
            "note.txt",
            "{\"kind\":\"meta\",\"id\":\"txt\",\"createdAt\":\"2027-01-01T00:00:00Z\"}\n",
        );
        assert_eq!(latest_session(&dir).as_deref(), Some("new"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(latest_session(&dir), None);
    }
}

//! Ported wholesale from gray-autocommit. Entry points renamed: manifest/handle/main -> *_autocommit.
//! gray-autocommit — commit uncommitted changes when the turn ends.
//!
//! Port of pi's `auto-commit-on-exit` extension. Pi committed at session
//! shutdown with a message derived from the last assistant reply; the
//! sidecar sees turns, not exits, so it commits on `turn_end` instead with
//! a fixed `gray: auto-commit (session <id>)` message.
//!
//! Opt-in like the pi original: default OFF. `/autocommit on|off|status`,
//! persisted at ~/.gray/autocommit/state. Never commits outside a git work
//! tree, never when the tree is clean, and never on a turn where no tool
//! ran — a dirty tree the agent didn't touch is left alone.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

fn manifest_autocommit() -> Value {
    json!({
        "name": "autocommit",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [],
        "commands": ["/autocommit"],
    })
}

fn state_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("autocommit")
}

fn enabled_in(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("state"))
        .map(|s| s.trim() == "on")
        .unwrap_or(false)
}

fn set_enabled(dir: &Path, on: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("state"), if on { "on\n" } else { "off\n" })
}

/// Run `git -C <cwd> <args>`; stdout on success, None on any failure
/// (missing binary, non-zero exit, non-UTF8).
fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn in_work_tree(cwd: &Path) -> bool {
    git(cwd, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

fn changed_files(cwd: &Path) -> Option<Vec<String>> {
    let status = git(cwd, &["status", "--porcelain"])?;
    Some(status.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect())
}

/// The core rule: commit everything if `cwd` is a work tree with a dirty
/// status. `Ok(Some(msg))` committed, `Ok(None)` skipped (not a repo or
/// clean), `Err` a git failure — callers in the event loop swallow it.
fn autocommit_if_dirty(cwd: &Path, session_id: &str) -> Result<Option<String>, String> {
    if !in_work_tree(cwd) {
        return Ok(None);
    }
    let files = changed_files(cwd).ok_or_else(|| "git status failed".to_string())?;
    if files.is_empty() {
        return Ok(None);
    }
    let msg = format!("gray: auto-commit (session {session_id})");
    git(cwd, &["add", "-A"]).ok_or_else(|| "git add failed".to_string())?;
    git(cwd, &["commit", "-m", &msg]).ok_or_else(|| "git commit failed".to_string())?;
    Ok(Some(format!("{msg} — {} file(s)", files.len())))
}

/// `/autocommit …` — `argv` excludes the command name.
fn run_command(argv: &[&str], dir: &Path, cwd: &Path) -> String {
    match argv.first().copied() {
        Some("on") => match set_enabled(dir, true) {
            Ok(()) => "autocommit on — dirty work trees get a `gray: auto-commit` commit at turn end".into(),
            Err(e) => format!("couldn't enable: {e}"),
        },
        Some("off") => match set_enabled(dir, false) {
            Ok(()) => "autocommit off".into(),
            Err(e) => format!("couldn't disable: {e}"),
        },
        _ => {
            let state = if enabled_in(dir) { "on" } else { "off" };
            let repo = match (in_work_tree(cwd), changed_files(cwd)) {
                (false, _) => "not a git work tree".to_string(),
                (true, Some(f)) => format!("{} uncommitted change(s)", f.len()),
                (true, None) => "git status failed".to_string(),
            };
            format!(
                "gray-autocommit {} — {state} — cwd: {repo}. /autocommit on|off|status",
                env!("CARGO_PKG_VERSION")
            )
        }
    }
}

/// Small rolling state for the current turn: did any tool actually run?
/// Pure-chat turns never sweep up a pre-existing dirty tree.
#[derive(Default)]
pub struct Turn {
    saw_tool: bool,
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_autocommit(req: &Value, turn: &mut Turn) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        if method == "event/notify" {
            match params.get("type").and_then(Value::as_str) {
                Some("pre_tool") | Some("post_tool") => turn.saw_tool = true,
                Some("turn_end") => {
                    if turn.saw_tool && enabled_in(&state_dir()) {
                        let session = params.get("session").cloned().unwrap_or(Value::Null);
                        let sid = session
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        let cwd = session
                            .get("cwd")
                            .and_then(Value::as_str)
                            .map(PathBuf::from)
                            .or_else(|| std::env::current_dir().ok())
                            .unwrap_or_else(|| PathBuf::from("."));
                        let _ = autocommit_if_dirty(&cwd, sid);
                    }
                    *turn = Turn::default();
                }
                _ => {}
            }
        }
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_autocommit(),
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let cwd = params
                .get("session")
                .and_then(|s| s.get("cwd"))
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            json!({ "text": run_command(&argv, &state_dir(), &cwd) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main_autocommit() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_autocommit());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut turn = Turn::default();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let (reply, exit) = handle_autocommit(&req, &mut turn);
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
        if exit {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(turn: &mut Turn, method: &str, params: Value) -> Value {
        handle_autocommit(&json!({ "id": 1, "method": method, "params": params }), turn)
            .0
            .unwrap()
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gray-autocommit-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A temp git repo with an identity so `git commit` works.
    fn tmprepo(tag: &str) -> PathBuf {
        let d = tmpdir(tag);
        let run = |args: &[&str]| {
            assert!(git(&d, args).is_some(), "git {:?} failed", args);
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        d
    }

    #[test]
    fn manifest_has_no_tools_and_one_command() {
        let mut t = Turn::default();
        let m = call(&mut t, "plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "autocommit");
        assert_eq!(m["protocol"], "1.1");
        assert_eq!(m["commands"], json!(["/autocommit"]));
        assert_eq!(m["tools"], json!([]));
    }

    #[test]
    fn default_state_is_off() {
        let d = tmpdir("off");
        assert!(!enabled_in(&d));
        set_enabled(&d, true).unwrap();
        assert!(enabled_in(&d));
        set_enabled(&d, false).unwrap();
        assert!(!enabled_in(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn commits_dirty_work_tree() {
        let repo = tmprepo("dirty");
        std::fs::write(repo.join("a.txt"), "hi").unwrap();
        let r = autocommit_if_dirty(&repo, "s1").unwrap();
        assert!(r.unwrap().contains("gray: auto-commit (session s1)"));
        assert!(changed_files(&repo).unwrap().is_empty());
        assert_eq!(
            git(&repo, &["log", "-1", "--format=%s"]).unwrap().trim(),
            "gray: auto-commit (session s1)"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn skips_clean_repo_and_non_repo() {
        let repo = tmprepo("clean");
        std::fs::write(repo.join("a.txt"), "hi").unwrap();
        autocommit_if_dirty(&repo, "s1").unwrap();
        // Clean now — second call must be a no-op.
        assert_eq!(autocommit_if_dirty(&repo, "s1").unwrap(), None);
        let outside = tmpdir("norepo");
        assert_eq!(autocommit_if_dirty(&outside, "s1").unwrap(), None);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn notifications_are_silent_and_dont_exit() {
        let mut t = Turn::default();
        for ty in ["pre_tool", "post_tool", "turn_end"] {
            let (reply, exit) = handle_autocommit(
                &json!({"method":"event/notify","params":{"type":ty,"session":{}}}),
                &mut t,
            );
            assert!(reply.is_none() && !exit);
        }
    }

    #[test]
    fn tool_events_mark_turn_then_reset() {
        let mut t = Turn::default();
        handle_autocommit(
            &json!({"method":"event/notify","params":{"type":"pre_tool","name":"bash"}}),
            &mut t,
        );
        assert!(t.saw_tool);
        handle_autocommit(
            &json!({"method":"event/notify","params":{"type":"turn_end","session":{}}}),
            &mut t,
        );
        assert!(!t.saw_tool);
    }

    #[test]
    fn status_reports_state_and_repo() {
        let mut t = Turn::default();
        let repo = tmprepo("status");
        std::fs::write(repo.join("x"), "1").unwrap();
        let r = call(
            &mut t,
            "command/run",
            json!({"name":"/autocommit","argv":["status"],"session":{"cwd":repo}}),
        );
        let text = r["result"]["text"].as_str().unwrap();
        assert!(text.contains("off") && text.contains("1 uncommitted"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let mut t = Turn::default();
        let (reply, exit) = handle_autocommit(&json!({ "id": 2, "method": "plugin/shutdown" }), &mut t);
        assert!(reply.is_some() && exit);
    }
}

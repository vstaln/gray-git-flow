//! Ported wholesale from gray-checkpoint. Entry points renamed: manifest/handle/main -> *_checkpoint.
//! gray-checkpoint — git stash checkpoints per session.
//!
//! Port of pi's `git-checkpoint` extension. pi snapped a `git stash create`
//! commit on every turn_start so /fork could roll code back; gray's wire has
//! no fork event, so this sidecar snapshots on `agent/before_start` and after
//! every `turn_end`, appending `{turn, ref}` lines to
//! `~/.gray/checkpoint/<session-id>.jsonl`. Restore is manual:
//! `/checkpoint list` shows them, `/checkpoint restore <n>` runs
//! `git stash apply <ref>` in the session's repo, and bare
//! `/checkpoint restore` asks interactively via `host/ask`
//! (capability `host.ask`; non-interactive → usage text).
//!
//! Wire methods: plugin/manifest, agent/before_start, command/run,
//! plugin/shutdown; notification event/notify (turn_end).

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

/// Internal cap on `host/ask` waits — under the host's 330s outer TTL.
const ASK_TTL: Duration = Duration::from_secs(300);
const RESTORE_QID: &str = "checkpoint-restore";
const LABEL_CANCEL: &str = "cancel";

fn manifest_checkpoint() -> Value {
    json!({
        "name": "checkpoint",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [],
        "commands": ["/checkpoint"],
        "hooks": ["agent/before_start"],
        "capabilities": ["host.ask"],
    })
}

/// `~/.gray/checkpoint`, honoring `$GRAY_HOME` first.
fn state_dir() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("checkpoint")
}

/// Session ids are arbitrary strings; keep them filename-safe.
fn sanitize(sid: &str) -> String {
    sid.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect()
}

fn session_file(dir: &Path, sid: &str) -> PathBuf {
    dir.join(format!("{}.jsonl", sanitize(sid)))
}

fn git(cwd: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
}

fn in_repo(cwd: &str) -> bool {
    !cwd.is_empty()
        && git(cwd, &["rev-parse", "--git-dir"])
            .map(|o| o.status.success())
            .unwrap_or(false)
}

/// Extract (session id, cwd) from a request's `params.session`.
fn session(params: &Value) -> (String, String) {
    let s = params.get("session").cloned().unwrap_or(Value::Null);
    (
        s.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
        s.get("cwd").and_then(Value::as_str).unwrap_or("").to_string(),
    )
}

fn read_entries(dir: &Path, sid: &str) -> Vec<(u64, String)> {
    std::fs::read_to_string(session_file(dir, sid))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            Some((
                v.get("turn").and_then(Value::as_u64)?,
                v.get("ref").and_then(Value::as_str)?.to_string(),
            ))
        })
        .collect()
}

/// `git stash create` in the session's repo; append `{turn, ref}` when it
/// yields a commit. Silently skips non-repos, missing ids, and clean trees
/// (stash create prints nothing when there's no diff).
fn record(dir: &Path, sid: &str, cwd: &str) {
    if sid.is_empty() || !in_repo(cwd) {
        return;
    }
    let Ok(out) = git(cwd, &["stash", "create"]) else { return };
    if !out.status.success() {
        return;
    }
    let r = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if r.is_empty() {
        return;
    }
    let turn = read_entries(dir, sid).len() as u64 + 1;
    let line = json!({ "turn": turn, "ref": r }).to_string();
    if std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(session_file(dir, sid))
            .and_then(|mut f| writeln!(f, "{line}"));
    }
}

fn short(r: &str) -> &str {
    &r[..7.min(r.len())]
}

/// The restore path shared by `/checkpoint restore <n>` and the picker:
/// `git stash apply <ref>` in the session's repo.
fn apply_ref(cwd: &str, n: u64, r: &str) -> String {
    if !in_repo(cwd) {
        return "not in a git repo — cannot restore".into();
    }
    match git(cwd, &["stash", "apply", r]) {
        Ok(o) if o.status.success() => {
            let detail = String::from_utf8_lossy(&o.stdout).trim().to_string();
            format!(
                "restored checkpoint {n} ({}){}",
                short(r),
                if detail.is_empty() { String::new() } else { format!(" — {detail}") }
            )
        }
        Ok(o) => format!(
            "restore of {n} failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => format!("restore of {n} failed: {e}"),
    }
}

/// `/checkpoint restore <n>` — look up checkpoint `n` and apply it.
fn restore_n(dir: &Path, sid: &str, cwd: &str, n: u64) -> String {
    let Some(r) = read_entries(dir, sid)
        .into_iter()
        .find(|(t, _)| *t == n)
        .map(|(_, r)| r)
    else {
        return format!("no checkpoint {n} — /checkpoint list to see them");
    };
    apply_ref(cwd, n, &r)
}

/// First picked option label for question `qid` in a host/ask result.
fn picked_label<'a>(result: &'a Value, qid: &str) -> Option<&'a str> {
    result
        .get("answers")?
        .get(qid)?
        .get("answers")?
        .as_array()?
        .first()?
        .as_str()
}

/// Bare `/checkpoint restore` — ask which checkpoint via `host/ask`.
/// `ask` sends a questions array and returns the host's result (None when
/// the capability is ungranted, the call errors, or it times out).
fn restore_interactive(
    dir: &Path,
    sid: &str,
    cwd: &str,
    ask: &mut dyn FnMut(&[Value]) -> Option<Value>,
) -> String {
    const USAGE: &str = "usage: /checkpoint restore <n>";
    let entries = read_entries(dir, sid);
    if entries.is_empty() {
        return "no checkpoints for this session yet — nothing to restore".into();
    }
    let options: Vec<Value> = entries
        .iter()
        .map(|(t, r)| json!({
            "label": format!("#{t} {}", short(r)),
            "description": r,
        }))
        .chain(std::iter::once(
            json!({"label": LABEL_CANCEL, "description": "don't restore anything"}),
        ))
        .collect();
    let questions = [json!({
        "id": RESTORE_QID,
        "header": "Restore checkpoint",
        "question": "Restore which checkpoint?",
        "options": options,
    })];
    let Some(result) = ask(&questions) else {
        return USAGE.into();
    };
    let Some(picked) = picked_label(&result, RESTORE_QID) else {
        return USAGE.into();
    };
    if picked == LABEL_CANCEL {
        return "restore cancelled".into();
    }
    let Some((n, r)) = entries
        .iter()
        .find(|(t, r)| format!("#{t} {}", short(r)) == picked)
        .map(|(t, r)| (*t, r.clone()))
    else {
        return USAGE.into();
    };
    apply_ref(cwd, n, &r)
}

/// `/checkpoint …` — `argv` excludes the command name.
fn run_command(
    dir: &Path,
    argv: &[&str],
    sid: &str,
    cwd: &str,
    ask: &mut dyn FnMut(&[Value]) -> Option<Value>,
) -> String {
    match argv.first().copied() {
        Some("restore") => {
            if let Some(n) = argv.get(1).and_then(|s| s.parse::<u64>().ok()) {
                restore_n(dir, sid, cwd, n)
            } else if argv.get(1).is_some() {
                "usage: /checkpoint restore <n>".into()
            } else {
                restore_interactive(dir, sid, cwd, ask)
            }
        }
        Some("list") | None => {
            let entries = read_entries(dir, sid);
            if entries.is_empty() {
                return "no checkpoints for this session yet".into();
            }
            let mut out = String::from("checkpoints (turn → stash ref):\n");
            for (t, r) in entries {
                out.push_str(&format!("  {t}  {r}\n"));
            }
            out.push_str("/checkpoint restore <n> applies one with `git stash apply`");
            out
        }
        Some(other) => format!("unknown subcommand {other:?} — /checkpoint [list|restore <n>]"),
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply. `ask` is the host/ask channel.
pub fn handle_checkpoint(req: &Value, ask: &mut dyn FnMut(&[Value]) -> Option<Value>) -> (Option<Value>, bool) {
    let dir = state_dir();
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        if method == "event/notify"
            && params.get("type").and_then(Value::as_str) == Some("turn_end")
        {
            let (sid, cwd) = session(&params);
            record(&dir, &sid, &cwd);
        }
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_checkpoint(),
        "agent/before_start" => {
            let (sid, cwd) = session(&params);
            record(&dir, &sid, &cwd);
            json!({})
        }
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let (sid, cwd) = session(&params);
            json!({ "text": run_command(&dir, &argv, &sid, &cwd, ask) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

pub type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;

fn next_id(counter: &Mutex<u64>) -> String {
    let mut n = counter.lock().expect("id counter");
    *n += 1;
    format!("q{n}")
}

/// `host/ask` round-trip: write the request, wait on the pending channel.
/// None on timeout or a dead channel — callers degrade to text.
pub fn ask_host(
    out: &Mutex<std::io::Stdout>,
    pending: &Pending,
    counter: &Mutex<u64>,
    questions: &[Value],
) -> Option<Value> {
    let id = next_id(counter);
    let (tx, rx) = mpsc::channel();
    pending.lock().expect("pending").insert(id.clone(), tx);
    let req = json!({
        "id": id,
        "method": "host/ask",
        "params": { "questions": questions, "blocking": true },
    });
    {
        let mut o = out.lock().expect("stdout");
        let _ = writeln!(o, "{req}");
        let _ = o.flush();
    }
    let reply = rx.recv_timeout(ASK_TTL).ok();
    pending.lock().expect("pending").remove(&id);
    reply
}

fn main_checkpoint() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_checkpoint());
        return Ok(());
    }
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let counter = Mutex::new(0u64);

    // Reader thread: host answers (string ids, no method) go to `pending`;
    // everything else — requests and notifications — goes to the work loop.
    let (work_tx, work_rx) = mpsc::channel::<Value>();
    let reader_pending = pending.clone();
    let _reader = std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if let Some(id) = v.get("id").and_then(Value::as_str)
                && v.get("method").is_none()
                && let Some(tx) = reader_pending.lock().expect("pending").remove(id)
            {
                let _ = tx.send(v.get("result").cloned().unwrap_or(Value::Null));
                continue;
            }
            if work_tx.send(v).is_err() {
                break;
            }
        }
    });

    for req in work_rx {
        let mut asker = |questions: &[Value]| ask_host(&stdout, &pending, &counter, questions);
        let (reply, exit) = handle_checkpoint(&req, &mut asker);
        if let Some(reply) = reply {
            let mut o = stdout.lock().expect("stdout");
            writeln!(o, "{reply}")?;
            o.flush()?;
        }
        if exit {
            break;
        }
    }
    // The reader thread is a detached stdin pump; returning from main
    // ends the process with it, so there is nothing to join.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_ask() -> impl FnMut(&[Value]) -> Option<Value> {
        |_: &[Value]| None
    }

    fn call(method: &str, params: Value) -> Value {
        handle_checkpoint(&json!({ "id": 1, "method": method, "params": params }), &mut no_ask())
            .0
            .unwrap()
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gray-checkpoint-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// git init + one committed file; returns the repo path.
    fn git_repo(tag: &str) -> PathBuf {
        let d = tmpdir(tag);
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&d)
                .args(args)
                .output()
                .unwrap()
        };
        assert!(run(&["init", "-q"]).status.success());
        assert!(run(&["config", "user.email", "t@t"]).status.success());
        assert!(run(&["config", "user.name", "t"]).status.success());
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        assert!(run(&["add", "a.txt"]).status.success());
        assert!(run(&["commit", "-qm", "init"]).status.success());
        d
    }

    #[test]
    fn manifest_declares_hooks_command_and_capability() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "checkpoint");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["agent/before_start"]));
        assert_eq!(m["commands"], json!(["/checkpoint"]));
        assert_eq!(m["capabilities"], json!(["host.ask"]));
        assert_eq!(m["tools"], json!([]));
    }

    #[test]
    fn before_start_outside_a_repo_is_a_noop() {
        let dir = tmpdir("state-norepo");
        let cwd = tmpdir("not-a-repo");
        record(&dir, "s1", cwd.to_str().unwrap());
        assert!(read_entries(&dir, "s1").is_empty());
    }

    #[test]
    fn checkpoints_number_up_from_one() {
        let state = tmpdir("state-repo");
        let repo = git_repo("repo");
        let cwd = repo.to_str().unwrap();
        // Clean tree → stash create prints nothing → no entry.
        record(&state, "s1", cwd);
        assert!(read_entries(&state, "s1").is_empty());
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        record(&state, "s1", cwd);
        std::fs::write(repo.join("a.txt"), "three\n").unwrap();
        record(&state, "s1", cwd);
        let e = read_entries(&state, "s1");
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].0, 1);
        assert_eq!(e[1].0, 2);
        assert_ne!(e[0].1, e[1].1);
        let _ = std::fs::remove_dir_all(&state);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn session_ids_are_filename_safe() {
        assert_eq!(sanitize("a/b\\c:d e"), "a_b_c_d_e");
    }

    #[test]
    fn restore_missing_checkpoint_denies() {
        let dir = tmpdir("state-empty");
        let msg = run_command(&dir, &["restore", "9"], "nope", "/tmp", &mut no_ask());
        assert!(msg.contains("no checkpoint 9"), "got: {msg}");
    }

    #[test]
    fn bare_restore_without_checkpoints_says_so() {
        let dir = tmpdir("state-none");
        let msg = run_command(&dir, &["restore"], "fresh", "/tmp", &mut no_ask());
        assert!(msg.contains("nothing to restore"), "got: {msg}");
    }

    #[test]
    fn bare_restore_without_ask_channel_shows_usage() {
        let dir = tmpdir("state-noask");
        std::fs::write(
            session_file(&dir, "s1"),
            "{\"turn\":1,\"ref\":\"abc1234deadbeef\"}\n",
        )
        .unwrap();
        let msg = run_command(&dir, &["restore"], "s1", "/tmp", &mut no_ask());
        assert_eq!(msg, "usage: /checkpoint restore <n>");
    }

    #[test]
    fn bare_restore_cancel_is_polite() {
        let dir = tmpdir("state-cancel");
        std::fs::write(
            session_file(&dir, "s1"),
            "{\"turn\":1,\"ref\":\"abc1234deadbeef\"}\n",
        )
        .unwrap();
        let mut ask = |_: &[Value]| -> Option<Value> {
            Some(json!({"answers": {RESTORE_QID: {"answers": ["cancel"]}}}))
        };
        let msg = run_command(&dir, &["restore"], "s1", "/tmp", &mut ask);
        assert_eq!(msg, "restore cancelled");
    }

    #[test]
    fn bare_restore_picked_checkpoint_applies() {
        let state = tmpdir("state-pick");
        let repo = git_repo("repo-pick");
        let cwd = repo.to_str().unwrap();
        // Dirty the tree, snapshot, revert → a restorable stash ref.
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        record(&state, "s1", cwd);
        let e = read_entries(&state, "s1");
        assert_eq!(e.len(), 1);
        std::process::Command::new("git")
            .arg("-C").arg(&repo)
            .args(["checkout", "--", "a.txt"])
            .output().unwrap();
        let label = format!("#1 {}", short(&e[0].1));
        let mut ask = move |_: &[Value]| -> Option<Value> {
            Some(json!({"answers": {RESTORE_QID: {"answers": [label]}}}))
        };
        let msg = run_command(&state, &["restore"], "s1", cwd, &mut ask);
        assert!(msg.contains("restored checkpoint 1"), "got: {msg}");
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "two\n");
        let _ = std::fs::remove_dir_all(&state);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn list_empty_reports_none() {
        let dir = tmpdir("state-list");
        let msg = run_command(&dir, &[], "fresh", "/tmp", &mut no_ask());
        assert!(msg.contains("no checkpoints"), "got: {msg}");
    }

    #[test]
    fn turn_end_notification_records_and_stays_silent() {
        let (reply, exit) = handle_checkpoint(&json!({
            "method": "event/notify",
            "params": {"type": "turn_end", "session": {"id": "x", "cwd": "/nonexistent"}}
        }), &mut no_ask());
        assert!(reply.is_none() && !exit);
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) =
            handle_checkpoint(&json!({ "id": 2, "method": "plugin/shutdown" }), &mut no_ask());
        assert!(reply.is_some() && exit);
    }
}

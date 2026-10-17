//! Ported wholesale from gray-git-merge. Entry points renamed: manifest/handle/main -> *_gitmerge.
//! gray-git-merge — `/merge <branch>` plus a `git_conflicts` resolve loop.
//!
//! Gray's wire has no per-turn merge hook that fits, so merging is
//! explicit: `/merge <branch>` runs `git merge` in the session repo. A clean merge reports as `{text}`; a conflicted one returns
//! `{prompt}` — the host feeds it back as a user message telling the model
//! to fix the markers — and the `git_conflicts` tool enumerates remaining
//! unmerged paths and stages resolved ones.
//!
//! Wire methods: plugin/manifest, tool/call, command/run, plugin/shutdown.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

fn manifest_gitmerge() -> Value {
    json!({
        "name": "git-merge",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [{
            "name": "git_conflicts",
            "description": "List files still conflicted after a merge, or stage a file whose conflict markers you fixed. action \"list\" shows every `git diff --diff-filter=U` path and whether conflict markers remain; action \"mark-done\" with `file` verifies the markers are gone and runs `git add` on it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "mark-done"], "description": "\"list\" conflicts or \"mark-done\" a resolved file." },
                    "file": { "type": "string", "description": "Repo-relative path to stage. Required for mark-done." },
                    "cwd": { "type": "string", "description": "Repo root. Defaults to the cwd recorded by /merge." }
                },
                "required": ["action"]
            }
        }],
        "commands": ["/merge"],
    })
}

/// `~/.gray/git-merge`, honoring `$GRAY_HOME` first.
fn state_dir() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("git-merge")
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

/// Paths git still reports as unmerged.
fn unmerged(cwd: &str) -> Vec<String> {
    git(cwd, &["diff", "--name-only", "--diff-filter=U"])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn has_markers(cwd: &str, file: &str) -> bool {
    std::fs::read_to_string(Path::new(cwd).join(file))
        .map(|s| s.lines().any(|l| l.starts_with("<<<<<<<") || l.starts_with(">>>>>>>")))
        .unwrap_or(false)
}

/// `/merge` records where it ran so `git_conflicts` (which gets no session
/// block on the wire) can find the repo.
fn save_state(dir: &Path, sid: &str, cwd: &str, branch: &str, files: &[String]) {
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(
        dir.join("last.json"),
        json!({ "session": sid, "cwd": cwd, "branch": branch, "files": files }).to_string(),
    );
}

/// cwd precedence: explicit tool arg → cwd recorded by /merge → sidecar cwd.
fn resolve_cwd(dir: &Path, args: &Value) -> String {
    if let Some(c) = args.get("cwd").and_then(Value::as_str).filter(|c| !c.is_empty()) {
        return c.to_string();
    }
    if let Ok(s) = std::fs::read_to_string(dir.join("last.json"))
        && let Ok(v) = serde_json::from_str::<Value>(&s)
        && let Some(c) = v.get("cwd").and_then(Value::as_str).filter(|c| !c.is_empty())
    {
        return c.to_string();
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into())
}

fn conflict_report(cwd: &str) -> String {
    let files = unmerged(cwd);
    if files.is_empty() {
        return "no unresolved conflicts".into();
    }
    let mut out = format!("{} unresolved:\n", files.len());
    for f in &files {
        out.push_str(&format!(
            "  {f}{}\n",
            if has_markers(cwd, f) { " (markers still present)" } else { "" }
        ));
    }
    out.push_str("fix the <<<<<<< blocks, then git_conflicts mark-done each file");
    out
}

/// `git_conflicts` tool. `Ok` → model-facing text, `Err` → is_error.
fn git_conflicts(dir: &Path, args: &Value) -> Result<String, String> {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let cwd = resolve_cwd(dir, args);
    if !in_repo(&cwd) {
        return Err(format!("git_conflicts: not a git repo: {cwd}"));
    }
    match action {
        "list" => Ok(conflict_report(&cwd)),
        "mark-done" => {
            let file = args
                .get("file")
                .and_then(Value::as_str)
                .filter(|f| !f.is_empty())
                .ok_or("mark-done needs a `file` argument")?;
            if !Path::new(&cwd).join(file).exists() {
                return Err(format!("{file}: no such file in {cwd}"));
            }
            if has_markers(&cwd, file) {
                return Err(format!("{file} still contains conflict markers"));
            }
            match git(&cwd, &["add", "--", file]) {
                Ok(o) if o.status.success() => {}
                Ok(o) => {
                    return Err(format!(
                        "git add {file} failed: {}",
                        String::from_utf8_lossy(&o.stderr).trim()
                    ));
                }
                Err(e) => return Err(format!("git add {file} failed: {e}")),
            }
            let left = unmerged(&cwd);
            if left.is_empty() {
                Ok(format!("{file} staged — all conflicts resolved; commit to finish the merge"))
            } else {
                Ok(format!("{file} staged — still unresolved: {}", left.join(", ")))
            }
        }
        other => Err(format!("unknown action {other:?} — use \"list\" or \"mark-done\"")),
    }
}

/// `/merge <branch>`. Clean → `{text}`; conflicts → `{prompt}`.
fn cmd_merge(dir: &Path, argv: &[&str], sid: &str, cwd: &str) -> Value {
    let Some(branch) = argv.first().filter(|b| !b.is_empty()) else {
        return json!({ "text": "usage: /merge <branch>" });
    };
    if !in_repo(cwd) {
        return json!({ "text": format!("not a git repo: {cwd}") });
    }
    let out = match git(cwd, &["merge", branch]) {
        Ok(o) => o,
        Err(e) => return json!({ "text": format!("git merge failed to run: {e}") }),
    };
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if out.status.success() {
        let detail = if stdout.is_empty() { stderr } else { stdout };
        return json!({ "text": format!("merge of {branch} clean — {detail}") });
    }
    let files = unmerged(cwd);
    if files.is_empty() {
        return json!({ "text": format!("merge of {branch} failed: {}", if stderr.is_empty() { stdout } else { stderr }) });
    }
    save_state(dir, sid, cwd, branch, &files);
    json!({ "prompt": format!(
        "The merge produced conflicts in these files: {}. Resolve them: read each file, fix the conflict markers, run git add. The git_conflicts tool (action \"list\") shows what's still unmerged; (action \"mark-done\", file) stages a file once its markers are fixed.",
        files.join(", ")
    ) })
}

fn session(params: &Value) -> (String, String) {
    let s = params.get("session").cloned().unwrap_or(Value::Null);
    (
        s.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
        s.get("cwd").and_then(Value::as_str).unwrap_or("").to_string(),
    )
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_gitmerge(req: &Value) -> (Option<Value>, bool) {
    let dir = state_dir();
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_gitmerge(),
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or(Value::Null);
            match if name == "git_conflicts" {
                git_conflicts(&dir, &args)
            } else {
                Err(format!("unknown tool: {name}"))
            } {
                Ok(text) => json!({ "content": text }),
                Err(text) => json!({ "content": text, "is_error": true }),
            }
        }
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let (sid, cwd) = session(&params);
            cmd_merge(&dir, &argv, &sid, &cwd)
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main_gitmerge() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_gitmerge());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let (reply, exit) = handle_gitmerge(&req);
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

    fn call(method: &str, params: Value) -> Value {
        handle_gitmerge(&json!({ "id": 1, "method": method, "params": params }))
            .0
            .unwrap()
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gray-git-merge-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn run(repo: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap()
    }

    /// Repo whose `side` branch conflicts with `main` on a.txt.
    fn conflicted_repo(tag: &str) -> PathBuf {
        let d = tmpdir(tag);
        assert!(run(&d, &["init", "-qb", "main"]).status.success());
        assert!(run(&d, &["config", "user.email", "t@t"]).status.success());
        assert!(run(&d, &["config", "user.name", "t"]).status.success());
        std::fs::write(d.join("a.txt"), "base\n").unwrap();
        assert!(run(&d, &["add", "a.txt"]).status.success());
        assert!(run(&d, &["commit", "-qm", "init"]).status.success());
        assert!(run(&d, &["checkout", "-qb", "side"]).status.success());
        std::fs::write(d.join("a.txt"), "theirs\n").unwrap();
        assert!(run(&d, &["commit", "-qam", "theirs"]).status.success());
        assert!(run(&d, &["checkout", "-q", "main"]).status.success());
        std::fs::write(d.join("a.txt"), "ours\n").unwrap();
        assert!(run(&d, &["commit", "-qam", "ours"]).status.success());
        d
    }

    #[test]
    fn manifest_declares_tool_and_command() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "git-merge");
        assert_eq!(m["tools"][0]["name"], "git_conflicts");
        assert_eq!(m["commands"], json!(["/merge"]));
    }

    #[test]
    fn merge_without_branch_shows_usage() {
        let r = cmd_merge(&tmpdir("s1"), &[], "s", "/tmp");
        assert!(r["text"].as_str().unwrap().contains("usage"));
    }

    #[test]
    fn merge_outside_repo_reports() {
        let cwd = tmpdir("norepo");
        let r = cmd_merge(&tmpdir("s2"), &["main"], "s", cwd.to_str().unwrap());
        assert!(r["text"].as_str().unwrap().contains("not a git repo"));
    }

    #[test]
    fn conflicts_produce_prompt_then_mark_done_resolves() {
        let state = tmpdir("state");
        let repo = conflicted_repo("conflict");
        let cwd = repo.to_str().unwrap();
        let r = cmd_merge(&state, &["side"], "s", cwd);
        let prompt = r["prompt"].as_str().expect("conflicted merge → prompt");
        assert!(prompt.contains("a.txt"), "got: {prompt}");

        let listed = git_conflicts(&state, &json!({"action": "list"})).unwrap();
        assert!(listed.contains("a.txt"), "got: {listed}");

        // Marking done while markers remain is refused.
        let early = git_conflicts(&state, &json!({"action": "mark-done", "file": "a.txt"}));
        assert!(early.is_err_and(|e| e.contains("markers")));

        // Resolve by hand, then mark-done stages it.
        std::fs::write(repo.join("a.txt"), "merged\n").unwrap();
        let done = git_conflicts(&state, &json!({"action": "mark-done", "file": "a.txt"})).unwrap();
        assert!(done.contains("staged"), "got: {done}");
        assert!(git_conflicts(&state, &json!({"action": "list"}))
            .unwrap()
            .contains("no unresolved"));
        let _ = std::fs::remove_dir_all(&state);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn clean_merge_reports_text() {
        let state = tmpdir("state-clean");
        let repo = conflicted_repo("clean");
        let cwd = repo.to_str().unwrap();
        // Undo side's change so the merge is trivial.
        assert!(run(&repo, &["checkout", "-q", "side"]).status.success());
        assert!(run(&repo, &["reset", "-q", "--hard", "HEAD~1"]).status.success());
        std::fs::write(repo.join("b.txt"), "new\n").unwrap();
        assert!(run(&repo, &["add", "b.txt"]).status.success());
        assert!(run(&repo, &["commit", "-qm", "b"]).status.success());
        assert!(run(&repo, &["checkout", "-q", "main"]).status.success());
        let r = cmd_merge(&state, &["side"], "s", cwd);
        assert!(r["text"].as_str().unwrap().contains("clean"), "got: {r}");
        let _ = std::fs::remove_dir_all(&state);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) = handle_gitmerge(&json!({ "id": 2, "method": "plugin/shutdown" }));
        assert!(reply.is_some() && exit);
    }
}

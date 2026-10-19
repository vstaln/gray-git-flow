//! Ported wholesale from gray-review. Entry points renamed: manifest/handle/main -> *_review.
//! gray-review — `/review` and `/simplify` over the working-tree git diff.
//!
//! Both commands return a `{prompt}` result: the host runs it as a user
//! prompt, so the agent does the actual review/simplification. `/review`
//! embeds the raw `git diff` output (head+tail capped at ~30k chars);
//! `/simplify` lists changed files with changed line ranges, keeping edits
//! surgical.
//!
//! Outside a git repo (or when git fails) the commands return `{text}` with
//! an error line instead of a prompt.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

const DIFF_CAP: usize = 30_000;

fn manifest_review() -> Value {
    json!({
        "name": "review",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [],
        "commands": ["/review", "/simplify"],
    })
}

// ── git helpers ───────────────────────────────────────────────

fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git not runnable: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            format!("git {} exited {}", args.join(" "), out.status)
        } else {
            err
        })
    }
}

fn is_git_repo(cwd: &Path) -> bool {
    git(cwd, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

/// Syntactically obvious revisions: HEAD~/^ chains and hex shas.
fn looks_like_rev_syntax(tok: &str) -> bool {
    if tok.starts_with("HEAD") || tok.contains('~') || tok.contains('^') {
        return true;
    }
    tok.len() >= 6 && tok.len() <= 40 && tok.chars().all(|c| c.is_ascii_hexdigit())
}

/// A token is a revision when it looks like one syntactically, or it doesn't
/// name a file on disk and git resolves it (a branch name, a short sha…).
fn looks_like_rev(cwd: &Path, tok: &str) -> bool {
    looks_like_rev_syntax(tok)
        || (!cwd.join(tok).exists() && git(cwd, &["rev-parse", "--verify", "--quiet", tok]).is_ok())
}

/// Parsed `/review` or `/simplify` arguments.
struct Targets {
    staged: bool,
    rev: Option<String>,
    files: Vec<String>,
}

fn parse_targets(cwd: &Path, argv: &[&str]) -> Targets {
    let mut t = Targets {
        staged: false,
        rev: None,
        files: Vec::new(),
    };
    for tok in argv {
        if *tok == "--staged" || *tok == "--cached" {
            t.staged = true;
        } else if t.rev.is_none() && looks_like_rev(cwd, tok) {
            t.rev = Some(tok.to_string());
        } else {
            t.files.push(tok.to_string());
        }
    }
    t
}

/// Assemble the `git diff` argument list for the parsed targets.
fn diff_args(t: &Targets, extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = vec!["diff".into()];
    args.extend(extra.iter().map(|s| s.to_string()));
    if t.staged {
        args.push("--cached".into());
    } else if let Some(rev) = &t.rev {
        args.push(rev.clone());
    } else {
        args.push("HEAD".into());
    }
    if !t.files.is_empty() {
        args.push("--".into());
        args.extend(t.files.iter().cloned());
    }
    args
}

fn run_diff(cwd: &Path, t: &Targets, extra: &[&str]) -> Result<String, String> {
    let args = diff_args(t, extra);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match git(cwd, &refs) {
        Ok(s) => Ok(s),
        // Fresh repo with no HEAD yet: fall back to unstaged + staged.
        Err(e) if t.rev.is_none() && !t.staged => {
            let a = git(cwd, &["diff"]).unwrap_or_default();
            let b = git(cwd, &["diff", "--cached"]).unwrap_or_default();
            if a.is_empty() && b.is_empty() {
                Err(e)
            } else {
                Ok(format!("{a}{b}"))
            }
        }
        Err(e) => Err(e),
    }
}

/// Keep the first and last chunks of a long diff with an explicit marker.
fn cap_diff(diff: &str) -> String {
    if diff.len() <= DIFF_CAP {
        return diff.trim_end().to_string();
    }
    let half = DIFF_CAP / 2;
    let head = &diff[..half.min(diff.len())];
    let tail_start = diff.len().saturating_sub(half);
    let tail = &diff[tail_start..];
    format!(
        "{}\n\n[… diff truncated: {} chars total, middle {} chars omitted …]\n\n{}",
        head.trim_end(),
        diff.len(),
        diff.len() - head.len() - tail.len(),
        tail.trim_start()
    )
}

// ── /review ───────────────────────────────────────────────────

fn review_prompt(cwd: &Path, t: &Targets) -> Result<String, String> {
    let diff = run_diff(cwd, t, &[])?;
    if diff.trim().is_empty() {
        return Err("no changes to review (empty diff)".into());
    }
    let scope = if t.staged {
        "staged changes".to_string()
    } else if let Some(r) = &t.rev {
        format!("changes vs {r}")
    } else {
        "uncommitted changes vs HEAD".to_string()
    };
    let files = if t.files.is_empty() {
        String::new()
    } else {
        format!(" (paths: {})", t.files.join(", "))
    };
    Ok(format!(
        "Review the following {scope}{files} for clarity, consistency, and maintainability.\n\
         \n\
         ## What to look for\n\
         - Bugs, logic errors, and edge cases introduced by the change\n\
         - Inconsistencies with surrounding code and project conventions (AGENTS.md / CLAUDE.md)\n\
         - Unclear naming, dead code, and missing error handling where it matters\n\
         - Maintainability: unnecessary complexity, duplication, leaky abstractions\n\
         \n\
         ## Output\n\
         Report findings ordered by severity, each with a file:line reference and a concrete\n\
         suggestion. Skip pure style nitpicks. If the diff is clean, say so briefly.\n\
         \n\
         ## Diff\n\
         ```diff\n{}\n```",
        cap_diff(&diff)
    ))
}

// ── /simplify ─────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
struct ChangedFile {
    path: String,
    status: char,           // M A R C …
    added: Vec<(u32, u32)>, // new-file line ranges; empty = unknown/deletions
    known: bool,
}

fn parse_name_status(out: &str) -> Vec<ChangedFile> {
    let mut files = Vec::new();
    for line in out.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        let Some(code) = parts.first().and_then(|s| s.chars().next()) else {
            continue;
        };
        if !matches!(code, 'M' | 'A' | 'R' | 'C' | 'T') {
            continue;
        }
        let path = if matches!(code, 'R' | 'C') {
            parts.get(2)
        } else {
            parts.get(1)
        };
        if let Some(p) = path {
            files.push(ChangedFile {
                path: p.to_string(),
                status: code,
                added: Vec::new(),
                known: false,
            });
        }
    }
    files
}

/// Parse `git diff --unified=0` hunk headers into merged new-side ranges.
fn parse_changed_lines(diff: &str) -> Vec<(u32, u32)> {
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for line in diff.lines() {
        let Some(rest) = line.strip_prefix("@@ ") else {
            continue;
        };
        let Some(plus) = rest.split_whitespace().nth(1) else {
            continue;
        };
        let Some(plus) = plus.strip_prefix('+') else {
            continue;
        };
        let mut it = plus.split(',');
        let Ok(start) = it.next().unwrap_or("0").parse::<u32>() else {
            continue;
        };
        let count = it.next().and_then(|c| c.parse::<u32>().ok()).unwrap_or(1);
        if count == 0 {
            continue;
        }
        let end = start + count - 1;
        if let Some(last) = ranges.last_mut() {
            if start <= last.1 + 1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        ranges.push((start, end));
    }
    ranges
}

fn changed_files(cwd: &Path, t: &Targets) -> Result<Vec<ChangedFile>, String> {
    if !t.files.is_empty() {
        // Explicit paths: mark as modified, still compute line ranges.
        let mut files: Vec<ChangedFile> = t
            .files
            .iter()
            .map(|p| ChangedFile {
                path: p.clone(),
                status: 'M',
                added: Vec::new(),
                known: false,
            })
            .collect();
        fill_ranges(cwd, t, &mut files);
        return Ok(files);
    }
    let mut args: Vec<String> = vec!["diff".into(), "--name-status".into()];
    if t.staged {
        args.push("--cached".into());
    } else {
        args.push(t.rev.clone().unwrap_or_else(|| "HEAD".into()));
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = git(cwd, &refs)?;
    let mut files = parse_name_status(&out);
    fill_ranges(cwd, t, &mut files);
    Ok(files)
}

fn fill_ranges(cwd: &Path, t: &Targets, files: &mut [ChangedFile]) {
    for f in files.iter_mut() {
        if f.status == 'A' {
            f.known = true; // whole file in scope
            continue;
        }
        let mut single = Targets {
            staged: t.staged,
            rev: t.rev.clone(),
            files: vec![f.path.clone()],
        };
        // A rev token that is also needed per-file.
        single.rev = t.rev.clone();
        match run_diff(cwd, &single, &["--unified=0", "--no-ext-diff"]) {
            Ok(d) => {
                f.added = parse_changed_lines(&d);
                f.known = true;
            }
            Err(_) => f.known = false,
        }
    }
}

fn status_name(c: char) -> &'static str {
    match c {
        'A' => "added",
        'R' => "renamed",
        'C' => "copied",
        'T' => "type-changed",
        _ => "modified",
    }
}

fn simplify_prompt(cwd: &Path, t: &Targets) -> Result<String, String> {
    let files = changed_files(cwd, t)?;
    if files.is_empty() {
        return Err(
            "no changed files found — specify file paths or make some changes first".into(),
        );
    }
    let mut list = String::new();
    for f in &files {
        if f.status == 'A' {
            list.push_str(&format!("- {} (added; entire file is in scope)\n", f.path));
        } else if !f.known {
            list.push_str(&format!(
                "- {} ({}; changed lines unavailable — inspect git diff before editing)\n",
                f.path,
                status_name(f.status)
            ));
        } else if f.added.is_empty() {
            list.push_str(&format!(
                "- {} ({}; deletions only — no current lines to simplify)\n",
                f.path,
                status_name(f.status)
            ));
        } else {
            let ranges: Vec<String> = f
                .added
                .iter()
                .map(|(a, b)| {
                    if a == b {
                        a.to_string()
                    } else {
                        format!("{a}-{b}")
                    }
                })
                .collect();
            list.push_str(&format!(
                "- {} ({}; changed lines: {})\n",
                f.path,
                status_name(f.status),
                ranges.join(", ")
            ));
        }
    }
    Ok(format!(
        "Review the following recently changed files and apply simplification improvements.\n\
         \n\
         ## Principles\n\
         \n\
         - **Preserve functionality**: Never change what the code does. All existing tests must continue to pass.\n\
         - **Apply project standards**: Follow any conventions from AGENTS.md or CLAUDE.md in this project.\n\
         - **Enhance clarity**: Reduce unnecessary complexity and nesting, eliminate redundant code and\n\
           abstractions, improve variable and function names, and consolidate related logic. Keep valuable\n\
           comments that explain design rationale or intent; remove only truly redundant noise. Avoid nested\n\
           ternaries — prefer if/else chains.\n\
         - **Maintain balance**: Do not over-simplify. No clever-but-opaque rewrites, no merged concerns, no\n\
           removed helpful abstractions. Readability over line count.\n\
         \n\
         ## Scope\n\
         \n\
         Only review and modify the changed lines listed below. Line numbers refer to the current file\n\
         contents. You may read surrounding code for context, but must not edit it. For added files the\n\
         entire file is in scope.\n\
         {list}\n\
         ## Process\n\
         \n\
         1. Read each file listed above and inspect its changed lines\n\
         2. Identify concrete improvements within those lines (dead code, unclear names, redundant logic)\n\
         3. Apply changes one file at a time, keeping every edit within the listed line ranges\n\
         4. Run existing tests afterward to verify nothing broke\n\
         5. Summarize what you changed and why\n\
         \n\
         Do NOT add features, change public APIs, or refactor outside the listed ranges. If a worthwhile\n\
         simplification would require editing unchanged code, mention it in the summary instead."
    ))
}

// ── wire ──────────────────────────────────────────────────────

fn cwd_of(params: &Value) -> PathBuf {
    params
        .get("session")
        .and_then(|s| s.get("cwd"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn run_command(name: &str, argv: &[&str], params: &Value) -> Value {
    let cwd = cwd_of(params);
    match name.trim_start_matches('/') {
        "review" | "simplify" => {
            if !is_git_repo(&cwd) {
                return json!({ "text": format!("{name}: not a git repository ({})", cwd.display()) });
            }
            let t = parse_targets(&cwd, argv);
            let result = if name.ends_with("review") {
                review_prompt(&cwd, &t)
            } else {
                simplify_prompt(&cwd, &t)
            };
            match result {
                Ok(prompt) => json!({ "prompt": prompt }),
                Err(e) => json!({ "text": format!("{name}: {e}") }),
            }
        }
        other => json!({ "text": format!("unknown command: {other}") }),
    }
}

pub fn handle_review(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_review(),
        "command/run" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            run_command(name, &argv, &params)
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main_review() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_review());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (reply, exit) = handle_review(&req);
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
        handle_review(&json!({ "id": 1, "method": method, "params": params }))
            .0
            .unwrap()
    }

    #[test]
    fn manifest_has_two_commands_and_no_tools() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "review");
        assert_eq!(m["commands"], json!(["/review", "/simplify"]));
        assert_eq!(m["tools"], json!([]));
    }

    #[test]
    fn hunk_ranges_parse_and_merge() {
        let d = "@@ -1,2 +1,3 @@\nx\n@@ -10 +12,2 @@\ny\n@@ -20 +14 @@\nz\n";
        assert_eq!(parse_changed_lines(d), vec![(1, 3), (12, 14)]);
    }

    #[test]
    fn zero_count_hunks_are_pure_deletions() {
        assert_eq!(
            parse_changed_lines("@@ -5,3 +5,0 @@\n"),
            Vec::<(u32, u32)>::new()
        );
    }

    #[test]
    fn name_status_parses_renames() {
        let out = "M\tsrc/a.rs\nR100\told.rs\tnew.rs\nA\tnew.txt\n";
        let f = parse_name_status(out);
        assert_eq!(f.len(), 3);
        assert_eq!(f[1].path, "new.rs");
        assert_eq!(f[2].status, 'A');
    }

    #[test]
    fn arg_parsing_splits_flags_revs_and_files() {
        let cwd = Path::new(".");
        let t = parse_targets(cwd, &["--staged", "src/x.rs"]);
        assert!(t.staged && t.rev.is_none() && t.files == ["src/x.rs"]);
        let t = parse_targets(cwd, &["HEAD~3", "a.rs", "b.rs"]);
        assert_eq!(t.rev.as_deref(), Some("HEAD~3"));
        assert_eq!(t.files.len(), 2);
    }

    #[test]
    fn diff_cap_keeps_head_and_tail() {
        let big = "x".repeat(DIFF_CAP + 5000);
        let capped = cap_diff(&big);
        assert!(capped.contains("diff truncated"));
        assert!(capped.len() < big.len());
    }

    #[test]
    fn non_repo_returns_text_not_prompt() {
        let dir = std::env::temp_dir().join(format!("grayreview-norepo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = call(
            "command/run",
            json!({ "name": "/review", "argv": [], "session": { "cwd": dir } }),
        );
        assert!(
            r["result"]["text"]
                .as_str()
                .unwrap()
                .contains("not a git repository")
        );
        assert!(r["result"].get("prompt").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) = handle_review(&json!({ "id": 2, "method": "plugin/shutdown" }));
        assert!(reply.is_some() && exit);
    }
}

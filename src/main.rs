//! gray-git-flow — one sidecar for the git lifecycle.
//!
//! Merges gray-checkpoint, gray-autocommit, gray-git-merge and gray-review.
//! Each source lives verbatim in `mods/` with its own `~/.gray/<orig>/`
//! state and slash commands — only the wire plumbing is shared.
//!
//! Routing:
//! - `agent/before_start` → checkpoint (stash snapshot at agent start).
//! - `event/notify` (notification) → checkpoint (turn_end snapshot), then
//!   autocommit (pre_tool/post_tool tracking, turn_end commit). Checkpoint
//!   runs first so the snapshot captures the tree before it is committed.
//! - `tool/call` → `git_conflicts` → git-merge.
//! - `command/run` → routed by `params.name` to the owning module.
//! - `host/ask` round-trips (checkpoint's interactive restore) go through a
//!   reader thread that parks host answers on a pending map.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex, mpsc};

use serde_json::{Value, json};

#[path = "mods/autocommit.rs"]
mod autocommit;
#[path = "mods/checkpoint.rs"]
mod checkpoint;
#[path = "mods/gitmerge.rs"]
mod gitmerge;
#[path = "mods/review.rs"]
mod review;

fn manifest() -> Value {
    json!({
        "name": "git-flow",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
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
        "commands": [
            "/git-flow", "/checkpoint", "/autocommit", "/merge", "/review", "/simplify",
        ],
        "hooks": ["agent/before_start"],
        "capabilities": ["host.ask"],
    })
}

/// `/git-flow` — merged-plugin overview.
fn git_flow_command() -> String {
    format!(
        "gray-git-flow {} — one sidecar, four git-lifecycle modules:\n\
         \x20 /checkpoint [list|restore <n>]  git stash snapshots per session (~/.gray/checkpoint/)\n\
         \x20 /autocommit on|off|status       commit at turn end when tools ran (~/.gray/autocommit/)\n\
         \x20 /merge <branch>                 git merge; conflicts → resolve prompt + git_conflicts tool\n\
         \x20 /review, /simplify              review or simplify the working-tree diff",
        env!("CARGO_PKG_VERSION")
    )
}

fn unwrap_result(reply: Option<Value>) -> Value {
    match reply {
        Some(r) => r.get("result").cloned().unwrap_or(json!({})),
        None => json!({}),
    }
}

fn handle(
    req: &Value,
    turn: &mut autocommit::Turn,
    ask: &mut dyn FnMut(&[Value]) -> Option<Value>,
) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        // Notifications: fan out to every module that consumes them.
        if method == "event/notify" {
            let _ = checkpoint::handle_checkpoint(req, ask);
            let _ = autocommit::handle_autocommit(req, turn);
        }
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest(),
        "agent/before_start" => {
            let (reply, _) = checkpoint::handle_checkpoint(req, ask);
            return (reply, false);
        }
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if name == "git_conflicts" {
                let (reply, _) = gitmerge::handle_gitmerge(req);
                return (reply, false);
            }
            json!({ "content": format!("unknown tool: {name}"), "is_error": true })
        }
        "command/run" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            match name {
                "/git-flow" => json!({ "text": git_flow_command() }),
                "/checkpoint" => unwrap_result(checkpoint::handle_checkpoint(req, ask).0),
                "/autocommit" => unwrap_result(autocommit::handle_autocommit(req, turn).0),
                "/merge" => unwrap_result(gitmerge::handle_gitmerge(req).0),
                "/review" | "/simplify" => unwrap_result(review::handle_review(req).0),
                _ => json!({ "text": format!("unknown command {name} — try /git-flow") }),
            }
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest());
        return Ok(());
    }
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let pending: checkpoint::Pending = Arc::new(Mutex::new(HashMap::new()));
    let counter = Mutex::new(0u64);
    let mut turn = autocommit::Turn::default();

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
        let mut asker =
            |questions: &[Value]| checkpoint::ask_host(&stdout, &pending, &counter, questions);
        let (reply, exit) = handle(&req, &mut turn, &mut asker);
        if let Some(reply) = reply {
            let mut o = stdout.lock().expect("stdout");
            writeln!(o, "{reply}")?;
            o.flush()?;
        }
        if exit {
            break;
        }
    }
    Ok(())
}

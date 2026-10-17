<div align="center">
  <img alt="gray-git-flow" src="assets/icon.svg" width="120" height="120" />
  <h1>gray-git-flow</h1>
  <p><strong>One sidecar for the git lifecycle: checkpoints, commit-on-exit, merge + conflict prompt, diff review.</strong></p>
  <p>
    <a href="https://gray.alignment.id">Website</a> ·
    <a href="https://github.com/vstaln/gray-git-flow">Source</a> ·
    <a href="https://github.com/vstaln/gray">gray</a>
  </p>
  <p>
    <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-1c1c20?style=flat-square&labelColor=0a0a0b" /></a>
    <a href="https://www.rust-lang.org"><img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-rust-1c1c20?style=flat-square&labelColor=0a0a0b&logo=rust&logoColor=d4a373" /></a>
    <img alt="gray plugin" src="https://img.shields.io/badge/gray-plugin-1c1c20?style=flat-square&labelColor=0a0a0b&color=7aa2f7" />
  </p>
</div>

<br/>

```bash
gray plugin install git-flow
```

`gray-git-flow` merges four git plugins into a single sidecar process. Each
module keeps its own `~/.gray/<orig>/` state (`~/.gray/checkpoint/`,
`~/.gray/autocommit/`, `~/.gray/git-merge/`) and its own slash commands, so
existing setups keep working after you uninstall the standalone versions.

## Replaces

| Module | Former plugin | Hooks / events | Tools | Commands |
|---|---|---|---|---|
| checkpoint | `gray-checkpoint` | `agent/before_start`, `event/notify` (turn_end) | — | `/checkpoint` |
| autocommit | `gray-autocommit` | `event/notify` (pre_tool, post_tool, turn_end) | — | `/autocommit` |
| gitmerge | `gray-git-merge` | — | `git_conflicts` | `/merge` |
| review | `gray-review` | — | — | `/review` `/simplify` |

- **checkpoint**: snapshots the work tree with `git stash create` at agent
  start and after every turn. `/checkpoint list` shows the snapshots and
  `/checkpoint restore <n>` applies one. A bare `/checkpoint restore` asks you
  which one through `host/ask`, so the plugin declares the `host.ask` capability.
- **autocommit**: off by default. After `/autocommit on`, a turn that ran
  any tool and left the tree dirty gets committed when it ends.
- **gitmerge**: `/merge <branch>` runs `git merge`. If it conflicts, you get a
  resolve prompt, and the `git_conflicts` tool lists unmerged files and
  stages the ones you fix.
- **review**: `/review` turns the working-tree diff into a review prompt.
  `/simplify` gives a prompt to simplify only the changed line ranges.

`event/notify` goes to checkpoint first and then to autocommit, so the
turn-end snapshot is taken before the auto-commit cleans the tree.
`/git-flow` prints an overview of all four modules.

## Build from source

```sh
cargo build --release
gray plugin check ./target/release/git-flow
```

## License

MIT. See [NOTICE.md](NOTICE.md) for third-party attributions.

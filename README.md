# sprout

Minimal git worktree manager.

## What it does

- `sprout create <worktree>` creates a new worktree under `~/.sprout/worktrees` and a new branch.
- `sprout cd <worktree>` prints the worktree path so your current shell can `cd` there.
- `sprout base` prints the source repo path when run from a worktree.
- `sprout list` / `sprout ls` lists all tracked worktrees in a table sorted by last commit time.
- `sprout delete <worktree>` removes the worktree and its metadata entry.
- `sprout config get|set branch_prefix` reads or updates `~/.sprout/config.toml`.

## Install (local)

```bash
cargo install --path .
```

## Usage

```bash
sprout create my-feature
cd "$(sprout cd my-feature)"
cd "$(sprout base)"
sprout list
sprout delete my-feature
sprout config get branch_prefix
sprout config set branch_prefix sprout/
```

## Config

`~/.sprout/config.toml`

```toml
branch_prefix = "sprout/"
```

## Metadata

`~/.sprout/metadata.json` tracks all worktrees and their source repos.

## Notes

- `sprout cd` and `sprout base` print paths for shell integration.
- Worktree names are global across repos.

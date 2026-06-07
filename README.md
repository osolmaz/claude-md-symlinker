# CLAUDE.mDEEZ

CLAUDE.mDEEZ keeps `AGENTS.md` as the canonical agent-instructions file and
creates local compatibility files for tools that still expect their own
instruction filename.

The built-in adapter is:

```text
AGENTS.md -> CLAUDE.md
```

In repositories you opt into, `CLAUDE.md` is generated locally and ignored by
Git only when CLAUDE.mDEEZ created or already owns it. If a repository already
has a user-owned `CLAUDE.md`, CLAUDE.mDEEZ leaves it untouched and keeps it
visible to Git.

## Install

Requirements:

- Rust stable
- Git

Install from GitHub:

```sh
cargo install --git https://github.com/dutifuldev/claudemdeez
```

Or install from a local checkout:

```sh
git clone https://github.com/dutifuldev/claudemdeez
cd claudemdeez
cargo install --path .
```

For a one-off local run:

```sh
cargo run -- apply ~/repos --dry-run
```

## Quick Start

Choose the directories CLAUDE.mDEEZ is allowed to scan:

```sh
claudemdeez init ~/repos ~/work
```

Preview what would happen:

```sh
claudemdeez apply --dry-run
```

Apply the changes:

```sh
claudemdeez apply
```

For each managed repository with `AGENTS.md`, the result is:

```text
AGENTS.md   # canonical file, usually committed
CLAUDE.md   # local compatibility shim, ignored by Git
```

Where file symlinks are available, the default shim is a relative symlink:

```text
CLAUDE.md -> AGENTS.md
```

Check a managed repo:

```sh
git status --short -- CLAUDE.md
```

No output means Git is not seeing the generated shim. If `CLAUDE.md` was an
existing user file, it remains visible, usually as:

```text
?? CLAUDE.md
```

## Commands

```sh
claudemdeez init <roots...>
```

Creates or updates the configured scan roots. Roots must already exist.

```sh
claudemdeez apply [roots...]
```

Runs reconciliation. If roots are supplied after config exists, they must stay
inside the configured scan roots.

```sh
claudemdeez watch [roots...]
```

Runs the same reconciliation on startup, on relevant file events, and
periodically. Watching is only a trigger; `apply` is the source of truth.

```sh
claudemdeez doctor
```

Checks local setup, including Git availability, config, state storage, enabled
adapters, and symlink support.

```sh
claudemdeez clean [roots...] --remove-if-source-missing
```

Removes stale managed shims after the source file is gone. Unknown files are
not removed.

Global options:

```sh
--config <path>   Use an explicit config file
--dry-run         Validate and report without mutating repos or state
--json            Print machine-readable output
```

## Configuration

CLAUDE.mDEEZ uses the platform config directory by default. You can override the
config path with `--config <path>` or `CLAUDEMDEEZ_CONFIG`.

The local SQLite state directory can be overridden with `CLAUDEMDEEZ_DATA_DIR`.

Example config:

```toml
[scan]
roots = ["~/repos", "~/work"]
include_paths = []
exclude_paths = ["~/repos/archive"]
exclude_dir_names = ["node_modules", ".cache", ".venv", "target", "dist", "build"]

[git]
exclude_mode = "per_repo"

[watch]
enabled = true
reconcile_interval_minutes = 30
full_rescan_interval_hours = 12

[materialization]
strategy = "auto"
allow_hardlink = false

[adapters.claude]
enabled = true
source = "AGENTS.md"
target = "CLAUDE.md"
on_source_missing = "leave"
```

Important fields:

- `scan.roots` is the hard allowlist for discovery and writes.
- `scan.include_paths` narrows the allowlist when non-empty.
- `scan.exclude_paths` always wins over roots and include paths.
- `scan.exclude_dir_names` prunes noisy directories while walking.
- `git.exclude_mode` should stay `per_repo`; global exclude mode is currently
  rejected because it cannot be scoped to your configured roots.
- `materialization.strategy` can be `auto`, `symlink`, `copy`, or `hardlink`.
- `materialization.allow_hardlink` must be `true` before auto mode will try
  hardlinks.
- `adapters.claude.on_source_missing` can be `leave` or `remove_if_managed`.

## Safety Model

CLAUDE.mDEEZ is intentionally conservative:

- It only scans directories you opt into.
- It never scans the whole machine by default.
- It never creates `AGENTS.md`.
- It never overwrites an unknown `CLAUDE.md`.
- It never changes a tracked `CLAUDE.md`.
- It does not add `CLAUDE.md` to Git excludes when an existing user-owned file
  is present.
- It refuses source or target paths that escape the repository root.
- It reports conflicts instead of guessing ownership.
- `--dry-run` avoids filesystem and state mutations.

## Git Behavior

Managed shims are excluded with the repository-local Git exclude file:

```text
.git/info/exclude
```

CLAUDE.mDEEZ writes a managed block like this:

```text
# claudemdeez managed begin
/CLAUDE.md
# claudemdeez managed end
```

This file is private to your checkout and is not committed.

If `CLAUDE.md` already exists and is not managed by CLAUDE.mDEEZ:

- the file is left untouched
- no ignore entry is added for it
- Git continues to report it as untracked or tracked normally
- `apply` reports a conflict and exits with code `2`

To let CLAUDE.mDEEZ manage that repository, move the useful content into
`AGENTS.md`, then remove or rename the old `CLAUDE.md` and run:

```sh
claudemdeez apply
```

## Materialization

In `auto` mode, CLAUDE.mDEEZ tries:

1. Relative symlink
2. Hardlink, only when `allow_hardlink = true`
3. Managed copy with a header

Managed copies start with:

```html
<!-- claudemdeez managed: source=AGENTS.md; adapter=claude; do not edit this file directly. -->
```

Edit `AGENTS.md`, not generated shims.

## Exit Codes

```text
0  success
1  operational or configuration error
2  conflicts were found
```

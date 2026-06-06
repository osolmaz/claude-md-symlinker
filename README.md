# Claudectomy

Claudectomy keeps `AGENTS.md` as the canonical agent-instructions file and
creates local compatibility shims for tools that still expect their own file.

The first built-in adapter is:

```text
AGENTS.md -> CLAUDE.md
```

`CLAUDE.md` is generated locally and excluded from Git only when Claudectomy
created or already owns that shim.

## Safety Model

Claudectomy is intentionally conservative:

- It only scans directories you opt into.
- It never scans the whole machine by default.
- It never creates `AGENTS.md`.
- It never overwrites an unknown `CLAUDE.md`.
- It never changes a tracked `CLAUDE.md`.
- It does not add `CLAUDE.md` to Git excludes when an existing user-owned file
  is present.
- `--dry-run` reports planned changes without mutating repos or state.

## Usage

Initialize scan roots:

```sh
claudectomy init ~/repos ~/work
```

Reconcile configured roots:

```sh
claudectomy apply
```

Preview a narrower run:

```sh
claudectomy apply ~/repos/some-project --dry-run
```

Check local setup:

```sh
claudectomy doctor
```

Remove stale managed shims after `AGENTS.md` is deleted:

```sh
claudectomy clean --remove-if-source-missing
```

Watch configured roots and trigger reconciliation:

```sh
claudectomy watch
```

## Configuration

Claudectomy uses the platform config directory by default. You can pass
`--config <path>` or set `CLAUDECTOMY_CONFIG` for explicit config selection.

Example:

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

## Git Behavior

For managed shims, Claudectomy updates the repository-local exclude file:

```text
.git/info/exclude
```

with:

```text
# claudectomy managed begin
/CLAUDE.md
# claudectomy managed end
```

This is private to your checkout and is not committed.

If `CLAUDE.md` already exists and is not managed by Claudectomy, the file is
left untouched and remains visible to Git.

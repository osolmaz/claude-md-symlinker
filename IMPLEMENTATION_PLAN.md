# Claudectomy Implementation Plan

Status: planning only. No implementation has started.

## Goal

Build Claudectomy as a cross-platform local compatibility manager for agent
instruction files.

The core promise:

> In opted-in Git repositories, if `AGENTS.md` exists, make local tool-specific
> shims such as `CLAUDE.md` exist, keep those shims out of Git, and never
> overwrite unknown user files.

The long-term architecture should be present from the first implementation. The
first adapter is Claude Code, but the product is not a Claude-only generator.
It is an `AGENTS.md` compatibility layer.

## Product Contract

Claudectomy must preserve these invariants:

1. `AGENTS.md` is the canonical source file.
2. `CLAUDE.md` is a generated local compatibility shim.
3. Generated shims are excluded from Git by default.
4. Unknown user files are never overwritten.
5. Tracked target files are never modified.
6. The reconciler is the source of truth.
7. Watchers, timers, and startup hooks only trigger reconciliation.
8. Re-running the same command is safe and idempotent.
9. Behavior is opt-in by configured root, not whole-machine scanning.
10. Configured scan scope is a hard safety boundary.

## Target User Experience

Initial commands:

```sh
claudectomy init ~/repos ~/work
claudectomy apply
claudectomy apply ~/repos --dry-run
claudectomy watch
claudectomy doctor
claudectomy clean --dry-run
```

Expected repository result:

```text
AGENTS.md   # canonical, usually committed
CLAUDE.md   # generated local shim, ignored by Git
```

Preferred materialization:

```text
CLAUDE.md -> AGENTS.md
```

Fallback materialization:

```text
CLAUDE.md   # generated copy with a managed header
```

## Architecture

The implementation should be a Rust CLI with a small set of focused modules.

```text
claudectomy
├── cli
├── config
├── core
│   ├── adapters
│   ├── discovery
│   ├── reconciler
│   ├── materializer
│   ├── git_exclude
│   ├── state
│   ├── doctor
│   ├── cleaner
│   └── reporting
└── watch
```

Recommended crates:

```text
clap          CLI parsing
serde         config and JSON output
toml          config file parsing
rusqlite      local state database
ignore        fast walking with ignore-style filtering
notify        filesystem watcher
directories   platform config/data directories
tracing       structured logs
tempfile      tests
```

Git operations should call the local `git` binary rather than guessing
repository internals. This keeps worktrees and platform differences boring.

## Data Flow

```text
configured scan scope
  -> repo discovery
  -> adapter expansion
  -> current-state inspection
  -> conflict policy
  -> materialization plan
  -> Git exclusion plan
  -> state update
  -> human or JSON report
```

No command should directly create or delete shims without going through the
reconciler.

## CLI

### `claudectomy init <roots...>`

Creates or updates the user config with opted-in roots.

Behavior:

- Expands `~`.
- Stores canonical absolute roots.
- Does not scan or modify repositories unless `--apply` is passed.
- Refuses empty root lists unless config already exists.

### `claudectomy apply [roots...]`

Runs a full reconciliation.

Behavior:

- Uses explicit roots if provided and they are within configured scan scope.
- Otherwise uses configured scan roots.
- Discovers Git worktrees below those roots.
- Applies enabled adapters.
- Writes plain text summary by default.
- Supports `--dry-run`.
- Supports `--json`.

### `claudectomy watch [roots...]`

Runs a long-lived reconcile trigger.

Behavior:

- Runs `apply` on startup.
- Watches scoped roots for relevant file events.
- Debounces bursts.
- Periodically runs full reconciliation.
- Treats the watcher as an optimization, not correctness.

### `claudectomy doctor`

Checks local setup.

Checks:

- `git` is installed and usable.
- Config file is readable.
- Data directory is writable.
- SQLite state can be opened.
- Configured scan roots exist.
- Symlink support is available in a temporary directory.
- Global exclude mode, if enabled, is configured correctly.
- Enabled adapters do not target the same file in incompatible ways.

### `claudectomy clean`

Removes or reports stale managed shims.

Behavior:

- Only acts on files proven to be managed by Claudectomy.
- Never removes unknown files.
- Defaults to conservative behavior.
- Supports `--dry-run`.
- Supports `--remove-if-source-missing`.

## Configuration

Config location should use platform conventions through `directories`.

Example TOML:

```toml
[scan]
roots = ["~/repos", "~/work"]
# Hard allowlist. Claudectomy will not reconcile repositories outside these
# directories unless the config is changed.

include_paths = []
# Optional narrower allowlist inside roots. When non-empty, repository roots
# must be under one of these paths.

exclude_paths = ["~/repos/archive", "~/work/vendor"]
# Explicit directories to skip even if they are inside roots.

exclude_dir_names = [
  "node_modules",
  ".cache",
  ".venv",
  "target",
  "dist",
  "build"
]

[git]
exclude_mode = "per_repo"
# per_repo | global

[watch]
enabled = true
reconcile_interval_minutes = 30
full_rescan_interval_hours = 12

[materialization]
strategy = "auto"
# auto | symlink | copy | hardlink
allow_hardlink = false

[adapters.claude]
enabled = true
source = "AGENTS.md"
target = "CLAUDE.md"
on_source_missing = "leave"
# leave | remove_if_managed
```

Defaults:

- `scan.roots = []`
- `scan.include_paths = []`
- `scan.exclude_paths = []`
- common noisy names in `scan.exclude_dir_names`
- `git.exclude_mode = "per_repo"`
- `watch.enabled = true`
- `materialization.strategy = "auto"`
- `allow_hardlink = false`
- Claude adapter enabled
- source `AGENTS.md`
- target `CLAUDE.md`
- source-missing behavior `leave`

## Adapter Model

An adapter describes how a canonical file becomes a tool-specific shim.

Fields:

```text
name
enabled
source_rel_path
target_rel_path
materialization_policy
exclude_policy
on_source_missing
copy_header
```

Initial adapter:

```text
name: claude
source: AGENTS.md
target: CLAUDE.md
exclude: true
materialization: auto
```

Future adapters can be added without changing discovery, state, reporting, or
watch behavior.

## Repo Discovery

Discovery must be opt-in by root and must avoid scanning the whole machine.

Scan scope rules:

1. `scan.roots` is the hard allowlist for all discovery and writes.
2. Explicit CLI roots narrow the current run but must still be contained by
   `scan.roots` when config exists.
3. `scan.include_paths`, when non-empty, further limits eligible repositories.
4. `scan.exclude_paths` always wins over roots and include paths.
5. Directory names in `scan.exclude_dir_names` are pruned during walking.
6. All configured paths are expanded, normalized, and compared as canonical
   absolute paths.

Rules:

1. Walk scoped roots with `ignore`.
2. Skip configured noisy directories and excluded paths.
3. Detect Git worktrees with `git -C <path> rev-parse --show-toplevel`.
4. Skip bare repositories using `git -C <repo> rev-parse --is-bare-repository`.
5. Deduplicate repositories by canonical top-level path.
6. Reject discovered repositories outside the final allowed scope.
7. Do not treat a `.git` directory or file as sufficient proof by itself.

Useful Git commands:

```sh
git -C <path> rev-parse --show-toplevel
git -C <repo> rev-parse --is-bare-repository
git -C <repo> rev-parse --git-dir
git -C <repo> rev-parse --git-path info/exclude
git -C <repo> ls-files --error-unmatch -- CLAUDE.md
```

Use `--git-path info/exclude` for the exclude file so linked worktrees and
nonstandard Git directories are handled correctly.

## Git Exclusion

Default mode: per-repo exclude.

For each managed target, ensure the repository-local exclude file contains:

```text
# claudectomy managed begin
/CLAUDE.md
# claudectomy managed end
```

Rules:

1. Resolve the exclude file with `git rev-parse --git-path info/exclude`.
2. Create parent directories if needed.
3. Preserve existing user content.
4. Update only the managed block.
5. Do not create duplicate entries.
6. In dry-run mode, report the planned change only.

Optional global mode:

```sh
git config --global core.excludesFile <claudectomy-global-excludes>
```

Global mode should be explicit because it hides matching files in every Git
repository on the machine.

## Materialization

Materialization priority in `auto` mode:

1. Relative symlink.
2. Hardlink only when explicitly allowed.
3. Generated copy with managed header.

Symlink target:

```text
CLAUDE.md -> AGENTS.md
```

Managed copy header:

```html
<!-- claudectomy managed: source=AGENTS.md; adapter=claude; do not edit this file directly. -->
```

Materializer responsibilities:

- Create relative symlinks where supported.
- Detect symlink capability.
- Refresh managed copies when `AGENTS.md` changes.
- Repair broken managed symlinks.
- Skip unknown files.
- Preserve file permissions reasonably for generated copies.

## Conflict Policy

| Situation | Behavior |
| --- | --- |
| `AGENTS.md` exists, `CLAUDE.md` missing | Create shim |
| `CLAUDE.md` is symlink to `AGENTS.md` | Keep |
| `CLAUDE.md` is managed copy | Refresh |
| `CLAUDE.md` is broken managed symlink | Repair |
| `CLAUDE.md` is unknown regular file | Skip and report conflict |
| `CLAUDE.md` is unknown symlink | Skip and report conflict |
| `CLAUDE.md` is tracked by Git | Skip and report tracked conflict |
| `AGENTS.md` missing | Do nothing by default |
| `AGENTS.md` removed, managed shim remains | Leave by default, clean only if configured |
| Exclude file is unwritable | Report error, do not create shim unless config allows |

Tracked target files must be detected before writes:

```sh
git -C <repo> ls-files --error-unmatch -- CLAUDE.md
```

## State Database

Use SQLite for explainability, cleanup, and future watcher behavior.

Location should use platform data directories.

Tables:

```text
repositories
  id
  root_path
  git_dir
  exclude_path
  first_seen_at
  last_seen_at
  last_reconciled_at
  last_error

shims
  id
  repository_id
  adapter_name
  source_rel_path
  target_rel_path
  materialization
  target_kind
  content_hash
  created_at
  last_seen_at
  last_reconciled_at
  last_status
  last_error

events
  id
  occurred_at
  level
  repository_path
  adapter_name
  action
  message
```

The filesystem remains authoritative. The state database helps answer:

- Did Claudectomy create this?
- What changed last time?
- What should `clean` consider?
- What errors are recurring?

Never trust the database alone for destructive cleanup. Confirm ownership from
the filesystem state too.

## Reporting

Plain output should be concise and action-oriented:

```text
Scanned 42 repositories.
Created 12 shims.
Repaired 1 shim.
Refreshed 3 copies.
Skipped 2 conflicts.
Skipped 1 tracked target.
Updated 12 exclude files.
```

JSON output should include machine-readable per-repo records:

```json
{
  "summary": {
    "repos_scanned": 42,
    "created": 12,
    "repaired": 1,
    "refreshed": 3,
    "conflicts": 2,
    "tracked_conflicts": 1,
    "exclude_updates": 12,
    "errors": 0
  },
  "results": []
}
```

Exit codes:

```text
0 success, including clean skips
1 operational error
2 conflicts found
3 invalid configuration
```

`--dry-run` should return the code that the real run would return.

## Watcher

Watcher design:

1. Run full reconcile on startup.
2. Watch scoped roots.
3. Trigger targeted reconcile on relevant changes:
   - `AGENTS.md`
   - configured target files
   - `.git`
   - config file
4. Debounce rapid events.
5. Run periodic full reconcile.
6. Recover from watcher errors by falling back to periodic reconcile.

The watcher should not contain independent business logic. It should call the
same reconciler as `apply`.

## Platform Integration

Initial support:

- Direct CLI.
- Long-running `watch`.

Planned service wrappers:

| Platform | Integration |
| --- | --- |
| macOS | LaunchAgent |
| Linux | systemd user service and timer |
| Windows | Scheduled Task, optional service later |

Service installers should be separate commands or documented recipes. The core
reconciler must not depend on a service being installed.

## Safety Rules

1. No whole-machine scanning by default.
2. No writes outside discovered Git worktrees, config, state, and Git exclude files.
3. No discovery or writes outside configured scan scope.
4. No target writes when the target is tracked by Git.
5. No overwrites of unknown files.
6. No destructive cleanup based only on database records.
7. Dry-run must avoid all filesystem mutations.
8. Errors in one repo must not stop the whole run unless config says fail-fast.
9. Paths in reports should be clear enough to diagnose conflicts.

## Verification Plan

Use integration tests with temporary directories and real Git repositories.

Core cases:

1. Repo with `AGENTS.md` and no `CLAUDE.md` creates shim.
2. Second `apply` is idempotent.
3. Per-repo exclude block is created.
4. Exclude block is not duplicated.
5. Existing unknown `CLAUDE.md` is skipped.
6. Tracked `CLAUDE.md` is skipped.
7. Existing symlink to `AGENTS.md` is kept.
8. Broken managed symlink is repaired.
9. Managed copy is refreshed after `AGENTS.md` changes.
10. Missing `AGENTS.md` does nothing.
11. Source removed leaves managed shim by default.
12. `clean --remove-if-source-missing` removes only managed shims.
13. Linked Git worktree resolves the correct exclude path.
14. Nested repositories are deduplicated correctly.
15. Dry-run reports changes without mutating files.
16. JSON output is valid and stable.
17. Symlink failure falls back to managed copy in `auto` mode.
18. Forced symlink mode reports an error if symlink creation fails.
19. Scan roots and path filters are expanded and canonicalized.
20. Watch mode triggers reconciliation after `AGENTS.md` changes.

Manual smoke tests:

```sh
cargo test
cargo run -- apply ~/repos/claudectomy-fixtures --dry-run
cargo run -- apply ~/repos/claudectomy-fixtures
cargo run -- doctor
cargo run -- clean --dry-run
```

## Implementation Milestones

### Milestone 1: Project Skeleton

- Create Rust crate.
- Add CLI command structure.
- Add config loading with defaults.
- Add plain and JSON report types.
- Add test fixture helpers.

### Milestone 2: Discovery

- Implement root walking.
- Implement Git worktree validation.
- Implement deduplication.
- Add discovery tests, including linked worktrees.

### Milestone 3: Adapter and Reconciler Core

- Add adapter registry.
- Add Claude adapter.
- Model desired state.
- Model observed target state.
- Implement conflict classification.

### Milestone 4: Materialization

- Implement relative symlink creation.
- Implement managed copy fallback.
- Implement ownership detection.
- Implement repair and refresh.

### Milestone 5: Git Exclusion

- Resolve exclude file with Git.
- Add managed exclude blocks.
- Implement global exclude mode behind config.
- Test idempotency.

### Milestone 6: State

- Add SQLite schema and migrations.
- Record repositories, shims, and events.
- Keep filesystem checks authoritative.

### Milestone 7: Commands

- Finish `apply`.
- Finish `doctor`.
- Finish `clean`.
- Add `init`.
- Add exit codes.

### Milestone 8: Watch

- Add watcher loop.
- Add debouncing.
- Add periodic full reconciliation.
- Add config reload behavior.

### Milestone 9: Packaging Readiness

- Add README.
- Add examples.
- Add release build workflow.
- Add install notes for direct binary use.
- Defer package managers until the command behavior is stable.

## Acceptance Criteria

The first complete implementation is ready when:

1. `claudectomy apply <root>` safely reconciles real Git repositories.
2. A second identical run produces no changes.
3. Unknown and tracked `CLAUDE.md` files are never changed.
4. Generated shims are excluded from Git.
5. Symlink and copy materialization both work.
6. Linked Git worktrees are handled correctly.
7. `doctor` explains platform and config issues.
8. `clean` only touches files proven to be managed.
9. `watch` uses the same reconciler as `apply`.
10. The integration test suite covers the conflict matrix.

## Non-Goals For The First Complete Implementation

These are intentionally deferred:

- GUI or TUI.
- Remote repository management.
- Editing `AGENTS.md` content.
- Creating `AGENTS.md` automatically.
- Committing generated shims.
- Whole-machine background scanning.
- Complex template transforms.

## Open Decisions

1. Whether `clean` should require `--confirm` for actual deletion.
2. Whether global exclude mode should be implemented immediately or documented
   as planned.
3. Whether the default `init` scan roots should include only explicit user input
   or offer common suggestions.
4. Whether Windows should prefer copy by default even when symlink support is
   available.
5. Whether `watch` should be included in the first release or land immediately
   after `apply`, `doctor`, and `clean`.

The architecture should support all of these choices without changing the core
reconciler.

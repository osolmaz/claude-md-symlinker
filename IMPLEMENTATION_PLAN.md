# claude-md-symlinker Implementation Plan

Status: the zero-config, install-and-forget implementation is now the primary
product surface. The selected next change adds nonduplicating Agent Skills
compatibility to the same hook, reconciliation, state, and repair model. The
legacy root-scoped `init`, `apply`, `clean`, `watch`, and low-level `service`
commands remain available. The implemented instruction-file model is:

```text
Claude hook observes cwd paths -> tool records AGENTS.md directories -> service repairs recorded shims
```

Old root-scoped behavior can be removed, hidden, or kept only as internal test
support in later cleanup.

## Goal

Build claude-md-symlinker as a local compatibility manager that keeps
`AGENTS.md` and `.agents/skills` canonical while making Claude-compatible
`CLAUDE.md` file shims and `.claude/skills` directory links appear where Claude
Code expects them.

The core promise:

> Install once. When Claude enters a directory inside a Git repo,
> claude-md-symlinker checks that directory and its parents up to the repo root.
> For every `AGENTS.md` it finds on that path, it creates and maintains a
> sibling, local, ignored `CLAUDE.md` shim.

This keeps the system automatic without broad filesystem scanning.

## Product Contract

claude-md-symlinker must preserve these invariants:

1. `AGENTS.md` is the canonical source file.
2. `CLAUDE.md` is a generated local compatibility shim.
3. Generated shims are excluded from Git by default.
4. Unknown user files are never overwritten.
5. Normal shim repair never modifies tracked target files.
6. Normal use requires no claude-md-symlinker config file.
7. Normal scope is the observed instruction-directory set, learned from Claude
   hooks.
8. The service repairs only recorded instruction directories.
9. No command scans the whole machine by default.
10. Hooks and services only trigger the same reconciler; they do not implement
    separate business logic.
11. Re-running the same operation is safe and idempotent.
12. Existing user-owned `CLAUDE.md` files are detected and recorded.
13. Migration from `CLAUDE.md` to `AGENTS.md` is controlled by a local install
    choice.
14. Install asks whether safe auto-migration should be enabled, and the default
    answer is yes.
15. Auto-migration is still scoped to directories Claude enters; it never starts
    a global scan.
16. Explicit migration may rename tracked `CLAUDE.md` to `AGENTS.md`, stage the
    resulting `AGENTS.md`, and must never commit.
17. Skill state uses an in-place hard cutover to one active contract. Do not
    keep dual readers or writers, and preserve existing instruction-file
    records and behavior through the schema change.
18. `.agents/skills` is the canonical Agent Skills root.
19. Each managed skill is an immediate child directory with a valid, regular
    `SKILL.md` file.
20. Each Claude compatibility target is one relative directory symlink under
    `.claude/skills`; the complete `.claude/skills` root is never replaced or
    linked.
21. Skill contents are never copied in the steady state.
22. Existing unmanaged, plugin, and Claude-only skills must coexist with
    managed links.
23. The file adapter and the skill-collection adapter use the same ownership
    and safety principles, but remain separate resource types.
24. State alone is not proof that an on-disk skill target is managed.
25. A copied target directory is a conflict during normal reconciliation.
26. Copied skills may be replaced only by an explicit, verified, atomic
    migration.
27. One writer owns each compatibility target. The tool does not take over a
    target owned by another synchronizer.
28. Project and global skill support use the existing hooks and repair service.
    They do not add another daemon or a home-directory scan.

## Agent Skills Compatibility Plan

### Canonical Layout

Project skills use this mapping:

```text
.agents/skills/deploy/                 # canonical skill directory
.agents/skills/deploy/SKILL.md         # required skill file
.claude/skills/deploy -> ../../.agents/skills/deploy
```

Global skills use the equivalent mapping below the user's home directory:

```text
~/.agents/skills/deploy/
~/.agents/skills/deploy/SKILL.md
~/.claude/skills/deploy -> ../../.agents/skills/deploy
```

The relative target is calculated from each target parent. Repository moves do
not break a correctly generated project link.

The tool creates one link for each skill. It must not replace
`.claude/skills` with one root link. This allows managed skills to coexist with
unmanaged directories, Claude-only skills, and plugin-managed entries.

A skill link exposes the canonical directory directly. An edit to `SKILL.md`,
a script, a reference, or an asset is visible through the Claude path at once.
No refresh or content synchronization is required.

### Adapter Model

Keep the current instruction-file behavior. Add a separate skill-collection
adapter with these responsibilities:

- Read immediate skill children from a canonical `.agents/skills` root.
- Validate each source before creating a target.
- Map each valid source child to the child with the same name under the
  corresponding `.claude/skills` root.
- Classify target ownership and conflicts.
- Create, adopt, repair, report, and remove only per-skill directory links.

Do not make directory links use the file materialization fallback. Copy and
hardlink modes do not satisfy the nonduplicating skill contract.

### Source Discovery and Validation

A source skill is one immediate child directory of `.agents/skills` with a
valid, regular `SKILL.md`. Its complete tree is one unit. This includes optional
`scripts`, `references`, `assets`, and other nested files allowed by the Agent
Skills format.

A malformed or unsafe source must produce a clear report and no new target.
Validation must reject path escape and unsafe filesystem shapes. Validation
must not follow a source or target outside its allowed canonical boundary.

Project discovery is hook-directed:

1. Resolve the current directory to its Git repository.
2. Check the current directory and each parent through the repository root.
3. Reconcile `.agents/skills` at each directory on that path when it exists.
4. Record observed project skill roots for later repair.

Global discovery checks the fixed pair `~/.agents/skills` and
`~/.claude/skills`. It does not scan the user's home directory.

The Claude hook stays bounded. It does not scan an entire repository for skill
roots. The existing repair service revisits recorded project roots and the
fixed global root. Hooks, direct commands, watch mode, and the service must call
the same reconciler.

### Target Classification and Ownership

For each valid source skill, classify the target as one of these practical
states:

- Missing target.
- Exact link to the expected source.
- Proven managed link that needs repair.
- Unmanaged regular directory.
- Unmanaged file.
- Unmanaged symlink.
- Invalid or unsafe parent path.

Apply these rules:

- Create a relative directory symlink when the target is missing.
- Adopt an existing link only when it resolves to the exact expected source.
- Repair only a target whose state and on-disk identity prove that the tool
  owns it.
- Preserve every unmanaged directory, file, or symlink and report a conflict.
- Do not replace a target because SQLite state alone says it was managed.
- If the source is removed, remove a target only when ownership is still proven
  on disk.
- If a managed link is changed to point elsewhere, treat it as unmanaged and
  preserve it.
- Repair a broken managed link only when the expected source is valid again and
  ownership remains provable.

All operations must be idempotent. A second run against a correct target must
make no change.

### Safe Migration from Copied Skills

Normal reconciliation never replaces a copied skill directory. It reports the
copy as a conflict.

Provide an explicit migration path for users who want to replace copied skills
with managed links. A target is eligible only when:

- The source and target are both safe skill directory trees.
- Their relative entries and file contents match completely.
- Their executable modes match.
- No external synchronization record claims the target.

Migration uses this sequence:

1. Compare and validate the full source and target trees.
2. Rename the target atomically to a temporary sibling path.
3. Create the relative directory symlink.
4. Verify that the link resolves to the expected source and exposes the same
   tree.
5. Remove the temporary copy only after verification succeeds.
6. Restore the original target if link creation or verification fails.

A difference, unsupported filesystem entry, ownership conflict, or failed
verification leaves the original target unchanged.

### External Synchronizers

Some installations use a separate copy-based skill synchronizer, such as the
OnurPi Agents synchronizer. The tool must detect known external synchronization
state where it can do so safely.

It must not automatically adopt, replace, clean, or purge a target claimed by
another writer. `status` and `doctor` must explain the conflict and the
single-writer rule. The user must stop the other writer for a target before an
explicit migration can transfer ownership.

This repository does not change OnurPi, Agents, Claude Code, Codex, Pi, or any
other external synchronizer. A full cutover of an existing external copy-based
installation requires a separate change in the repository that owns that
synchronizer.

### State and Hard Cutover

Record one managed state entry for each skill target. The record must identify:

- Global or project scope.
- The repository and observed directory for project skills.
- The canonical source root and source skill.
- The Claude target root and target skill.
- The resource type.
- The expected relative link.
- The last classification, result, and error.

Extend the existing SQLite model in place. After the schema cutover, use one
active reader and writer for each state contract. Do not keep parallel legacy
and new state paths or indefinite dual behavior.

Existing instruction-file records and behavior must remain valid after the
cutover.

### Lifecycle Commands and Reporting

Extend normal product operations to include managed skill links:

- `install` configures the existing hooks and service, initializes state, and
  performs the same bounded initial observation.
- `observe` reconciles global skills and project skill roots on the current
  directory path.
- The repair service revisits recorded project skill roots and the fixed global
  root.
- `watch` reacts to relevant source-root and target changes through the shared
  reconciler.
- `status` reports skill coverage, managed links, conflicts, invalid sources,
  and external writers.
- `doctor` checks platform support, parent paths, state health, link ownership,
  and external-writer conflicts.
- `clean` removes stale entries only after it proves target ownership.
- `purge` removes only proven managed skill links and their managed Git exclude
  entries.
- `uninstall` leaves links by default and includes them when `--purge` is used.
- Plain and JSON reporting include skill actions without changing the meaning
  of existing instruction-file fields.

No lifecycle command may delete a canonical skill source.

### Git Behavior

For project skills, add only individual managed target paths to the repository's
private `.git/info/exclude` block. For example:

```text
/.claude/skills/deploy
```

Do not ignore `.claude/skills` as a whole. Do not hide unmanaged or tracked
Claude skill entries. If a managed target becomes tracked or loses ownership,
remove only its stale managed exclude entry when that operation is safe.

Global skills are outside a repository and do not need Git exclude entries.

### Platform Boundary

Support and test directory-link behavior on Linux and macOS. Keep generated
links relative on both platforms.

Do not claim Windows support until a directory-link mechanism is implemented
and verified for creation, discovery, ownership checks, migration, cleanup, and
Claude Code loading. An unsupported platform must fail clearly without falling
back to copied skill contents.

### Implementation Sequence

1. Add the skill-collection adapter and the skill source and target domain
   types without changing instruction-file semantics.
2. Add bounded global and hook-directed project discovery and source
   validation.
3. Add per-skill classification, relative directory-link creation, ownership
   checks, repair, and removal.
4. Cut the SQLite state and reporting model over to records that can represent
   both instruction files and skill links.
5. Connect skills to `observe`, the existing repair service, watch mode,
   install, status, doctor, clean, purge, uninstall, and Git exclude handling.
6. Add explicit copied-directory migration with complete comparison, atomic
   replacement, verification, and rollback.
7. Add external-writer detection and clear single-writer diagnostics.
8. Update installer behavior and user documentation after the implementation
   matches this contract.
9. Run the complete verification plan and all existing instruction-file
   regressions.

### Verification Plan

Automated tests must cover:

- Project and global relative skill links.
- Immediate visibility of changes to `SKILL.md`, scripts, references, assets,
  and nested files without reconciliation or copying.
- Valid and malformed source skills.
- Unsafe source paths and filesystem entries.
- Exact-link adoption.
- Unmanaged directory, file, symlink, tracked-target, and parent-path
  conflicts.
- Idempotent no-change reconciliation.
- Source deletion and rename.
- Broken links and links changed to another target.
- Repository moves with relative links.
- Individual Git exclude creation, repair, and removal.
- Complete and executable-mode-identical copied-directory migration.
- Migration differences, external ownership, failed verification, rollback,
  and original-data preservation.
- Hook-targeted current-directory and ancestor reconciliation.
- Fixed global-root reconciliation without a home scan.
- Watch and repair-service reconciliation.
- State hard cutover and existing instruction-record preservation.
- `status`, `doctor`, `clean`, `purge`, `uninstall`, and installer behavior.
- Plain and JSON reports.
- All current `AGENTS.md` to `CLAUDE.md` unit, integration, migration, Git,
  hook, service, installer, and regression tests.

Acceptance also requires:

- Rust formatting.
- Clippy with warnings denied.
- The complete Rust test suite.
- Installer tests using temporary homes and state directories.
- A real Claude Code check on each claimed operating system and surface when
  that client is available. The result must confirm that Claude discovers a
  linked skill directory. If the client has a symlink-discovery regression,
  report the exact version and do not replace the link design with copies.
- No writes to live `~/.agents`, `~/.claude`, service, or agent installations
  during tests.

## Target User Experience

Normal setup:

```sh
curl -fsSL https://github.com/osolmaz/claude-md-symlinker/releases/latest/download/claude-md-symlinker-installer.sh | sh
```

After install:

```text
Claude starts in ~/repos/alpha
  -> hook observes alpha
  -> the path from cwd to repo root is checked for AGENTS.md
  -> every directory on that path with AGENTS.md is recorded
  -> sibling CLAUDE.md shims are created for recorded AGENTS.md files

service runs in the background
  -> periodically repairs recorded instruction directories
  -> deleted managed CLAUDE.md files come back later
```

Normal commands:

```sh
claude-md-symlinker install
claude-md-symlinker status
claude-md-symlinker repos list
claude-md-symlinker repos remove <repo>
claude-md-symlinker repos prune
claude-md-symlinker migrate --dry-run
claude-md-symlinker settings set auto-migrate false # optional
claude-md-symlinker purge --dry-run
claude-md-symlinker uninstall
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

The implementation should remain a Rust CLI with focused modules.

```text
claude-md-symlinker
├── cli
├── install
│   ├── claude_hooks
│   └── systemd_user
├── observe
├── repos
├── migrate
├── daemon
├── core
│   ├── adapters
│   ├── reconciler
│   ├── materializer
│   ├── git_exclude
│   ├── state
│   ├── doctor
│   ├── cleaner
│   ├── migration
│   ├── purge
│   └── reporting
└── cutover
    └── state_archive
```

Recommended crates already fit this design:

```text
clap          CLI parsing
serde         JSON output and hook input parsing
rusqlite      local state database
directories   platform config/data directories
tracing       structured logs
tempfile      tests
```

Git operations should call the local `git` binary rather than guessing
repository internals. This keeps worktrees and platform differences boring.

## Data Flow

Install flow:

```text
claude-md-symlinker install
  -> resolve absolute binary path
  -> install managed Claude hook entries
  -> install and start user service
  -> observe the current cwd path if it is inside a Git repo
  -> print status summary
```

Hook flow:

```text
Claude hook event JSON
  -> observe reads cwd
  -> git rev-parse finds repo root
  -> walk cwd parents up to repo root
  -> every AGENTS.md directory on that path is inserted or refreshed in SQLite
  -> targeted reconcile runs for those instruction directories
  -> hook exits 0
```

Service flow:

```text
systemd --user service
  -> run startup repair over recorded instruction directories
  -> sleep with jitter
  -> repair recorded instruction directories periodically
  -> prune missing repos and missing instruction directories conservatively
```

No default flow walks `~`, `/`, or broad developer directories.

## Release Installer

Normal users should not need Rust, Cargo, or local compilation.

The public install path should be a one-line shell installer backed by GitHub
Releases:

```sh
curl -fsSL https://github.com/osolmaz/claude-md-symlinker/releases/latest/download/claude-md-symlinker-installer.sh | sh
```

Use `cargo-dist` for release packaging instead of a hand-rolled release
pipeline.

Primary distribution is GitHub Releases directly. Do not publish this release
to crates.io; `cargo install` is not part of the supported user install path.

Release behavior:

1. Build precompiled binaries for Linux and macOS.
2. Upload release artifacts to GitHub Releases.
3. Publish a shell installer that detects OS and CPU architecture.
4. Verify downloaded artifacts with release checksums.
5. Install the binary into a user-local bin directory.
   The default is `~/.local/bin`; the installer must not depend on Rust,
   Cargo, or `CARGO_HOME`.
6. Leave Claude hook and service setup to `claude-md-symlinker install`.

Required release targets:

```text
x86_64-unknown-linux-gnu
aarch64-unknown-linux-gnu
x86_64-apple-darwin
aarch64-apple-darwin
```

The public installer installs the binary and then explicitly runs the tool's
own setup command before editing Claude settings, installing a service, or
migrating files.

Non-goals for distribution:

- crates.io publishing
- asking users to install Rust or Cargo
- requiring local compilation

## CLI

### `claude-md-symlinker install`

Primary setup command.

Behavior:

- Does not require a user config file.
- Resolves the current executable to an absolute path.
- Installs managed Claude hook entries into `~/.claude/settings.json`.
- Preserves existing Claude settings and user hook entries.
- Creates a timestamped backup before changing Claude settings.
- Prompts for safe auto-migration of existing `CLAUDE.md` files found while
  Claude works through directories.
- Defaults that prompt to yes.
- Explains in the prompt that this does not scan the whole machine or whole
  repos; it only applies when Claude enters directories and a safe candidate is
  detected.
- Installs and starts the Linux `systemd --user` service.
- Opens or creates the SQLite state database.
- Observes the current working directory if it is inside a Git repo.
- Records every `AGENTS.md` directory on the cwd-to-repo-root path.
- Runs targeted reconciliation for those instruction directories when observed.
- Prints a concise status summary.

Options:

```sh
--no-service       Install hooks only
--no-hooks         Install service only
--auto-migrate     Enable safe auto-migration without prompting
--no-auto-migrate  Disable safe auto-migration without prompting
--dry-run          Show planned changes without writing
--json             Machine-readable report
```

Install prompt copy:

```text
Automatically migrate safe existing CLAUDE.md files to AGENTS.md when Claude
finds them while working through directories?

This does not scan your whole machine or whole repos. It only applies to
CLAUDE.md files found in directories Claude actually enters, and only when the
migration passes the safe checks.

Default: yes
```

### `claude-md-symlinker observe`

Internal hook command. It should still be safe to run manually.

Behavior:

- Reads Claude hook JSON from stdin.
- For `CwdChanged`, prefers `new_cwd`.
- For other hook events, uses `cwd`.
- Falls back to process cwd when no hook JSON is present.
- Ignores unknown hook fields.
- Treats malformed hook input as empty input unless `--strict` is passed.
- Runs `git -C <cwd> rev-parse --show-toplevel`.
- Skips non-Git directories and bare repos.
- Checks the cwd and each parent up to the Git repo root.
- Records each directory on that path that contains `AGENTS.md`.
- Records existing unmanaged sibling `CLAUDE.md` files detected on that path.
- Runs targeted reconciliation for recorded instruction directories by default.
- Runs safe migration only when global `auto_migrate` is enabled.
- Produces no stdout in hook mode.
- Logs errors quietly.
- Exits `0` by default, even on errors, so Claude is never broken by the hook.

Options:

```sh
--no-apply         Record instruction directories without reconciling them
--strict          Return nonzero on errors for manual debugging
--json            Print a machine-readable result
```

### `claude-md-symlinker daemon`

Long-running service command used by the systemd unit.

Behavior:

- Reads recorded instruction directories from SQLite.
- Runs reconcile immediately on startup.
- Periodically repairs recorded instruction directories every 10 minutes.
- Does not scan discovery roots.
- Does not need a user config file.
- Uses a process lock so multiple daemons do not run concurrently.
- Skips repos or instruction directories that no longer exist and records the
  error.
- Applies a per-tick work cap if the recorded instruction set grows large.

Defaults:

```text
repair_interval_minutes = 10
jitter_seconds = 30
max_instruction_dirs_per_tick = 500
```

These are fixed production defaults for this cutover, not user config.

### `claude-md-symlinker status`

Shows whether the system is installed and healthy.

Checks:

- Binary path used by installed hooks.
- Claude hook entries are present.
- User service is installed and active.
- SQLite state can be opened.
- Observed repo count.
- Recorded instruction directory count.
- Last reconcile time.
- Recent errors.

### `claude-md-symlinker repos list`

Lists observed repos and their recorded instruction directories.

Columns:

```text
repo
instruction_dirs
last_seen_at
last_reconciled_at
last_status
last_error
```

### `claude-md-symlinker migrate`

Migrates detected user-owned `CLAUDE.md` files to `AGENTS.md`.

Behavior:

- Works from detected `CLAUDE.md` records in SQLite.
- Does not scan whole repos.
- Automatic migration is controlled by the install-time choice or later
  `settings set auto-migrate ...`.
- Migrates only regular files that are not already managed shims.
- Skips symlinks, directories, generated copies, and unknown special files.
- Skips when sibling `AGENTS.md` already exists unless `--replace-existing` is
  passed.
- Skips when the content rewrite is not safe.
- Produces a dry-run plan with candidate, skipped, and needs-review groups.
- Does not commit.
- If the repo is a Git repo, stages the resulting `AGENTS.md` without staging
  generated `CLAUDE.md` shims.
- Re-runs reconciliation after migration so `CLAUDE.md` becomes a local shim.

Options:

```sh
--dry-run             Show planned migrations and content diffs
--auto-safe-only      Migrate only candidates with no warnings
--replace-existing   Allow replacing an existing AGENTS.md, still with checks
--no-git-add         Do not stage AGENTS.md after migration
--json               Machine-readable report
```

### `claude-md-symlinker settings`

Stores local tool preferences in SQLite, not in repo files. Install normally
sets this through a prompt.

Supported setting:

```text
auto_migrate = true | false
```

Behavior:

- `install` asks whether to enable auto-migration.
- The install prompt defaults to yes.
- `settings set auto-migrate true` enables it.
- `settings set auto-migrate false` disables it.
- When `false`, `observe` records migration candidates but does not migrate.
- When `true`, `observe` may run the same safe migration engine used by
  `migrate --auto-safe-only`.
- Auto migration never commits.
- Auto migration never runs candidates that need review.

### `claude-md-symlinker repos remove <repo>`

Stops managing a repo without deleting user data.

Behavior:

- Marks the observed repo inactive or removes it from the active observed set.
- Does not delete `CLAUDE.md` shims.
- Does not remove Git exclude blocks unless `--clean-exclude` is passed.
- Never touches unknown files.

### `claude-md-symlinker repos prune`

Removes missing repos from the active observed set.

Behavior:

- Reports repos whose root path no longer exists.
- Removes them from the active service scope.
- Removes missing instruction directories from the active repair scope.
- Keeps historical event rows unless `--forget-history` is passed.

### `claude-md-symlinker purge`

Removes managed shims created by claude-md-symlinker.

Behavior:

- Works from the SQLite ledger.
- Confirms ownership from filesystem state before deleting anything.
- Removes only files proven to be managed.
- Removes managed Git exclude entries when safe.
- Skips tracked files.
- Skips unknown regular files.
- Skips unknown symlinks.
- Supports `--dry-run`.

This is the explicit "delete all generated shims" command.

### `claude-md-symlinker uninstall`

Removes the integration.

Behavior:

- Stops and disables the user service.
- Removes only claude-md-symlinker-managed Claude hook entries.
- Leaves existing managed shims in place by default.
- Supports `--purge` to remove managed shims too.
- Supports `--dry-run`.

## Claude Hook Integration

Install managed hooks into `~/.claude/settings.json`.

Recommended hook events:

```text
SessionStart
CwdChanged
UserPromptSubmit
```

Reasoning:

- `SessionStart` catches the directory Claude starts in.
- `CwdChanged` catches every directory Claude enters during a session.
- `UserPromptSubmit` repairs the current cwd path on the next prompt if a shim
  was deleted while Claude was already sitting there.

The hook command should be absolute and quiet:

```sh
/abs/path/claude-md-symlinker observe >/dev/null 2>>"$HOME/.local/state/claude-md-symlinker/hooks.log" || true
```

Rules:

1. Preserve existing user hooks.
2. Add only versioned managed entries.
3. Do not duplicate entries on repeated install.
4. Remove only managed entries on uninstall.
5. Never make Claude fail because observe failed.
6. Keep hook work targeted to one cwd path and its parents, never a full repo
   tree.
7. Avoid stdout so Claude user prompts are not polluted.
8. Ignore unknown JSON fields so Claude hook payload changes do not break the
   tool.
9. In hook mode, malformed JSON should log and exit `0`; `--strict` is for
   manual debugging only.

## State Database

Use SQLite as the source of management scope and as the artifact ledger.

Location should use platform data directories.

This iteration is a clean state cutover:

1. If an old `state.sqlite3` exists with the previous schema, archive it before
   opening the new schema.
2. Archive name:

   ```text
   state.pre-observed-cutover.sqlite3
   ```

3. Create a fresh database with the new schema.
4. Do not migrate old root-scoped rows.
5. Set `PRAGMA user_version` for the new schema so future migrations can be
   deliberate.

Tables:

```text
observed_repositories
  id
  root_path
  git_dir
  exclude_path
  source              # claude-hook | manual
  first_seen_at
  last_seen_at
  last_observed_cwd
  last_reconciled_at
  last_status
  last_error
  active

observed_instruction_dirs
  id
  repository_id
  instruction_dir
  source_rel_path
  target_rel_path
  first_seen_at
  last_seen_at
  last_reconciled_at
  last_status
  last_error
  active

detected_claude_files
  id
  repository_id
  instruction_dir
  claude_path
  agents_path
  classification       # user_file | tracked_user_file | managed_shim | unknown
  content_hash
  first_seen_at
  last_seen_at
  last_migration_status
  last_error
  migrated_at

shims
  id
  instruction_dir_id
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
  removed_at

events
  id
  occurred_at
  level
  repository_path
  adapter_name
  action
  message

settings
  key
  value
  updated_at
```

The filesystem remains authoritative for destructive actions. The database
answers:

- Which repos are in scope?
- Which instruction directories are in scope inside each repo?
- Which existing `CLAUDE.md` files have been detected?
- Which shims did claude-md-symlinker create?
- Which detected files are migration candidates?
- What should the service repair?
- What can `purge` consider?
- What errors are recurring?

Never trust the database alone for cleanup. Confirm ownership from filesystem
state before deleting any file.

## Adapter Model

An adapter describes how a canonical file becomes a tool-specific shim.

Initial adapter:

```text
name: claude
source: AGENTS.md
target: CLAUDE.md
exclude: true
materialization: auto
```

Future adapters can be added without changing observed cwd discovery, state,
reporting, or service behavior.

Adapter source and target paths must be repository-relative, must not escape the
repository root, and must not point inside `.git`.

## Reconciler

The reconciler is the source of truth.

Inputs:

```text
repo root
git dir
exclude path
instruction directory
adapter
dry-run flag
```

The observed service should call the reconciler directly for known instruction
directories. It should not use root walking.

Rules:

1. Skip repos that are missing or no longer Git worktrees.
2. Skip bare repos.
3. Skip instruction directories that no longer exist.
4. Do nothing when `AGENTS.md` is missing in the instruction directory.
5. Create or repair sibling `CLAUDE.md` only when sibling `AGENTS.md` exists.
6. Never create shims outside the Git repo root.
7. Never overwrite unknown target files.
8. Never modify tracked target files during normal shim repair.
9. Keep generated targets ignored with per-repo Git excludes.
10. Record every result in SQLite.

Observed path expansion:

```text
cwd = /repo/apps/web/src/components
repo_root = /repo

check, in order:
  /repo/apps/web/src/components/AGENTS.md
  /repo/apps/web/src/AGENTS.md
  /repo/apps/web/AGENTS.md
  /repo/apps/AGENTS.md
  /repo/AGENTS.md
```

For every `AGENTS.md` found, reconcile a sibling `CLAUDE.md`:

```text
/repo/apps/web/AGENTS.md -> /repo/apps/web/CLAUDE.md
/repo/AGENTS.md          -> /repo/CLAUDE.md
```

The parent walk is bounded by path depth. It must never descend into
directories or scan the repo tree.

During the same parent walk, detect sibling `CLAUDE.md` files:

```text
/repo/apps/web/CLAUDE.md
/repo/CLAUDE.md
```

Classify each detected `CLAUDE.md` as:

```text
managed_shim
user_file
tracked_user_file
unknown
```

Only `user_file` and `tracked_user_file` are migration candidates.

Useful Git commands:

```sh
git -C <cwd> rev-parse --show-toplevel
git -C <repo> rev-parse --is-bare-repository
git -C <repo> rev-parse --git-dir
git -C <repo> rev-parse --git-path info/exclude
git -C <repo> ls-files --error-unmatch -- CLAUDE.md
```

Use `--git-path info/exclude` for the exclude file so linked worktrees and
nonstandard Git directories are handled correctly.

## Migration

Migration converts existing user-owned `CLAUDE.md` files into canonical
`AGENTS.md` files.

Primary command:

```sh
claude-md-symlinker migrate
```

The migration guide this plan is based on renames `CLAUDE.md` to `AGENTS.md`,
updates Claude-specific boilerplate to agent-agnostic language, and then uses
local `CLAUDE.md` symlinks for compatibility.

Production behavior should be more conservative than a shell script:

1. Work only from detected `CLAUDE.md` records.
2. Do not scan whole repos.
3. Preflight every candidate before writing.
4. Skip if sibling `AGENTS.md` already exists by default.
5. Skip if `CLAUDE.md` is a symlink, directory, generated copy, or special file.
6. Skip if the file cannot be read as UTF-8 Markdown.
7. Apply only versioned, exact-match cleanup rules.
8. Show a dry-run diff.
9. Perform the file move transactionally: preserve original content until the
   new `AGENTS.md` has been written and verified.
10. Write `AGENTS.md` atomically.
11. Replace `CLAUDE.md` with a managed local shim only after `AGENTS.md` is
    safely written.
12. Add `AGENTS.md` to the Git index when the directory is in a Git repo.
13. Never commit.

Safe content cleanup rules:

```text
# CLAUDE.md
  -> # AGENTS.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.
  -> This file provides guidance to AI agents when working with code in this repository.

Claude Code (claude.ai/code)
  -> AI agents
```

Rules must be exact and versioned. Do not run broad replacements like
`Claude -> AI agent` by default. Anthropic may change its default template, so
unknown Claude-specific wording should be reported as `needs_review`, not
guessed.

Git behavior:

- If `CLAUDE.md` is tracked, run a Git-aware rename such as
  `git mv -- CLAUDE.md AGENTS.md`, apply safe content cleanup to `AGENTS.md`,
  then run `git add -- AGENTS.md`.
- If `CLAUDE.md` is untracked, move it to `AGENTS.md` transactionally, apply
  safe content cleanup, and run `git add -- AGENTS.md` unless `--no-git-add` is
  passed.
- Never stage generated `CLAUDE.md` shims.
- Never stage `.git/info/exclude`; it is local Git metadata.
- Never commit.

Auto migration:

- `install` prompts for `auto_migrate`.
- The default install answer is yes.
- The prompt must say that this is not a global scan and only applies to
  `CLAUDE.md` files found along directories Claude enters while working.
- When enabled, auto migration runs only on candidates that pass the same
  no-warning preflight as `migrate --auto-safe-only`.
- Auto migration skips candidates with existing `AGENTS.md`, remaining
  Claude-specific text, Git conflicts, unreadable content, or unknown file type.
- Auto migration should log skipped candidates so `status` and `migrate
  --dry-run` can explain them later.

## Git Exclusion

Default mode: per-repo exclude.

For each managed target, ensure the repository-local exclude file contains:

```text
# claude-md-symlinker managed begin
/CLAUDE.md
# nested examples are also literal repo-relative paths:
/apps/web/CLAUDE.md
# claude-md-symlinker managed end
```

Rules:

1. Resolve the exclude file with `git rev-parse --git-path info/exclude`.
2. Create parent directories if needed.
3. Preserve existing user content.
4. Update only the managed block.
5. Do not create duplicate entries.
6. In dry-run mode, report the planned change only.
7. Escape Git ignore metacharacters so configured target paths are treated as
   literal paths, not glob patterns.

Global exclude mode is not part of the primary product. It hides user-owned
files too broadly.

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
<!-- claude-md-symlinker managed: source=AGENTS.md; adapter=claude; do not edit this file directly. -->
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
| `CLAUDE.md` is unknown regular file | Record as migration candidate, skip normal shim write |
| `CLAUDE.md` is unknown symlink | Skip and report conflict |
| `CLAUDE.md` is tracked by Git | Skip and report tracked conflict |
| `AGENTS.md` missing | Do nothing by default |
| `AGENTS.md` missing, user-owned `CLAUDE.md` exists | Record migration candidate |
| `migrate` candidate has safe content | Rename/write `AGENTS.md`, add to Git index, create local shim |
| `migrate` candidate has existing sibling `AGENTS.md` | Skip unless explicitly allowed |
| `migrate` candidate has unknown Claude-specific text | Skip and report `needs_review` |
| `AGENTS.md` removed, managed shim remains | Leave by default, clean only if requested |
| Exclude file is unwritable | Report error and avoid partial ownership claims |

Tracked target files must be detected before writes:

```sh
git -C <repo> ls-files --error-unmatch -- CLAUDE.md
```

## Service Design

Linux first:

```text
~/.config/systemd/user/claude-md-symlinker.service
```

The unit should run:

```sh
claude-md-symlinker daemon
```

Service behavior:

1. Start on login.
2. Restart on failure.
3. Reconcile recorded instruction directories on startup.
4. Reconcile recorded instruction directories every 10 minutes with +/- 30
   seconds of jitter.
5. Never walk broad roots.
6. Never require root.
7. Log concise errors.

Do not add a filesystem watcher. Periodic repair plus Claude hook-triggered
repair is enough and scales predictably.

Planned non-Linux wrappers:

| Platform | Integration |
| --- | --- |
| macOS | LaunchAgent |
| Windows | Scheduled Task, optional service later |

## Resource Model

The default system should scale with recorded instruction directory count, not
repo size or filesystem size.

Cost per hook:

```text
one cwd -> git repo detection -> parent walk to repo root -> reconcile found AGENTS.md dirs
```

Cost per service tick:

```text
recorded_instruction_dir_count * small directory checks
```

There is no default full machine walk and no default watch over large directory
trees.

Safeguards:

- Debounce repeated observations of the same repo path.
- Debounce repeated observations of the same instruction directory.
- Cap maximum instruction directories reconciled per service tick.
- Store `last_reconciled_at`.
- Skip inactive or missing repos and instruction directories.
- Use a process lock to avoid concurrent service loops.
- Keep hook command quiet and best-effort.

## Safety Rules

1. No whole-machine scanning by default.
2. No default home-directory scanning.
3. No writes outside recorded instruction directories, state, Claude settings,
   systemd user config, and Git exclude files.
4. No target writes when the target is tracked by Git during normal shim
   repair; explicit migration is the only exception.
5. No overwrites of unknown files.
6. No destructive cleanup based only on database records.
7. Hooks must exit `0` by default.
8. Hooks must not emit stdout in normal operation.
9. Dry-run must avoid all filesystem mutations.
10. Non-regular sources such as directories, FIFOs, and devices are errors.
11. Errors in one repo must not stop the whole service tick.
12. Paths in reports should be clear enough to diagnose conflicts.

## Reporting

Plain output should be concise and action-oriented:

```text
Installed Claude hooks.
Installed and started user service.
Observed 1 repo.
Created 1 shim.
Detected 1 migration candidate.
```

Dry-run output should be grouped by action:

```text
Would install Claude hooks.
Would install and start user service.
Auto migrate: enabled.
Would observe current cwd: /repo/apps/web.

Safe migrations: 2
Needs review: 1
Skipped: 3
Would create shims: 4
Would update Git excludes: 4
```

Migration dry-run should group candidates:

```text
Safe migrations
  /repo/CLAUDE.md -> /repo/AGENTS.md

Needs review
  /repo/apps/web/CLAUDE.md  unknown Claude-specific text

Skipped
  /repo/docs/CLAUDE.md      sibling AGENTS.md already exists
```

Status output:

```text
Hooks: installed
Service: active
Observed repos: 24
Instruction dirs: 38
Migration candidates: 3
Auto migrate: on
Last repair: 2026-06-07T21:54:00+08:00
Recent errors: 0
```

JSON output should include exact machine-readable summaries and per-record
details. Plain output is for humans; JSON is the stable interface.

Exit codes for manual commands:

```text
0 success
1 operational error
2 conflicts found
3 invalid configuration or invalid install state
```

Hook mode should exit `0` unless `--strict` is passed.

## Verification Plan

Use integration tests with temporary directories, real Git repositories, fake
Claude settings files, and fake `systemctl` commands where needed.

Core cases:

1. `install --dry-run` reports hook and service changes without writing.
2. `install` preserves existing Claude settings and hooks.
3. Re-running `install` does not duplicate hook entries.
4. `uninstall` removes only managed hook entries.
5. Existing old state is archived as `state.pre-observed-cutover.sqlite3`, and
   a fresh schema is created.
6. `observe` with a hook `cwd` inside a Git repo records the repo and any
   `AGENTS.md` directories on the cwd-to-repo-root path.
7. `CwdChanged` uses `new_cwd` before `cwd`.
8. Malformed hook JSON exits `0` in hook mode and nonzero with `--strict`.
9. Unknown hook JSON fields are ignored.
10. `observe` outside a Git repo exits successfully and records nothing.
11. `observe` against a bare repo records nothing.
12. `observe` applies immediately for every `AGENTS.md` found on the parent
   path.
13. `observe` does not scan sibling or child directories.
14. `observe` does nothing when no `AGENTS.md` exists on the parent path.
15. Service daemon reconciles only recorded instruction directories.
16. Service daemon ignores unobserved repos and unrecorded instruction
    directories.
17. Service daemon runs once on startup and then every 10 minutes with jitter.
18. Deleting a managed nested `CLAUDE.md` in a recorded instruction directory
    is repaired by the daemon.
19. Existing unknown `CLAUDE.md` is skipped.
20. Tracked `CLAUDE.md` is skipped.
21. Per-repo exclude block is created and not duplicated.
22. Nested targets are ignored with literal repo-relative exclude entries.
23. Linked Git worktree resolves the correct exclude path.
24. Unmanaged `CLAUDE.md` files on the cwd-to-repo-root path are recorded as
    detected files.
25. Managed symlinks and managed copies are not migration candidates.
26. `migrate --dry-run` groups safe migrations, needs-review candidates, and
    skipped candidates.
27. `migrate --dry-run` shows a safe content diff and planned Git index
    changes.
28. `migrate` renames/writes `AGENTS.md`, removes the original `CLAUDE.md`, and
    creates a managed local shim.
29. `migrate` adds `AGENTS.md` to the Git index in Git repos and never commits.
30. `migrate` skips candidates when sibling `AGENTS.md` already exists.
31. `migrate` skips candidates with unknown Claude-specific wording and reports
    `needs_review`.
32. `install` asks whether to enable `auto_migrate`, and the prompt defaults to
    yes.
33. The install prompt explains that auto-migration only applies to files Claude
    finds while working through directories, not a global scan.
34. `--no-auto-migrate` stores `auto_migrate = false`.
35. When `auto_migrate` is enabled, observe migrates only no-warning candidates.
36. `repos list` shows observed repos and instruction directory counts.
37. `repos remove` removes a repo from active service scope.
38. `repos prune` removes missing repo roots and missing instruction
    directories from active scope.
39. `purge --dry-run` reports managed shims without deleting them.
40. `purge` deletes only filesystem-proven managed shims.
41. `uninstall --purge` stops service, removes hooks, and purges managed shims.
42. No user config file is required for normal install, observe, status, or
    daemon behavior.

Manual smoke tests:

```sh
cargo test
cargo run -- install --dry-run
cargo run -- observe --json
cargo run -- status
cargo run -- repos list
cargo run -- migrate --dry-run
cargo run -- purge --dry-run
```

Linux service smoke:

```text
1. Install with a temporary Claude settings path and temporary systemd unit name.
2. Observe a temporary nested cwd whose repo root and parent directory contain
   `AGENTS.md`.
3. Start the service.
4. Confirm sibling `CLAUDE.md` shims exist and are ignored by Git.
5. Delete one nested `CLAUDE.md`.
6. Confirm the service recreates it on a later tick.
7. Uninstall and verify the unit is removed.
```

## Implementation Pass

Implement this as one coherent product pass, not as staged feature releases.
The end state should be usable with:

```sh
curl -fsSL https://github.com/osolmaz/claude-md-symlinker/releases/latest/download/claude-md-symlinker-installer.sh | sh
```

Single-pass checklist:

- Add `cargo-dist` release configuration.
- Add a GitHub release workflow that builds precompiled Linux and macOS
  binaries.
- Generate and publish `claude-md-symlinker-installer.sh` in GitHub Releases.
- Do not publish to crates.io.
- Do not document `cargo install` as a normal user install path.
- Add `observed_repositories` and `observed_instruction_dirs` state with
  active/inactive scope.
- Add `detected_claude_files` state and local `settings` state.
- Add clean state cutover: archive old DB and create the new schema with
  `PRAGMA user_version`.
- Add state APIs for observe, list, remove, prune, and service iteration.
- Extract a targeted reconcile API from the current root discovery flow.
- Extend the reconciler to target a specific instruction directory, not only the
  repo root.
- Make observed instruction directory repair call the same reconciler used by
  `apply`.
- Record shim creation, repair, conflict, purge, and errors in SQLite.
- Add `observe` to parse Claude hook JSON, resolve `cwd`, walk parents to the
  repo root, record every `AGENTS.md` directory found, and run targeted
  reconcile.
- Make hook parsing tolerant: prefer `new_cwd` for `CwdChanged`, fall back to
  `cwd`, ignore unknown fields, and treat malformed JSON as nonfatal unless
  `--strict`.
- Have `observe` detect and record unmanaged sibling `CLAUDE.md` files on the
  same parent path.
- Make `observe` quiet and best-effort by default, with `--strict` for manual
  debugging.
- Add Claude hook installer support for `~/.claude/settings.json`.
- Preserve unknown Claude settings and user hooks.
- Add versioned managed hook entries without duplicates.
- Remove only managed hook entries during uninstall.
- Create timestamped backups before modifying Claude settings.
- Add `daemon` to repair recorded instruction directories on startup and
  every 10 minutes with +/- 30 seconds of jitter.
- Add process locking, missing repo handling, and bounded per-tick work.
- Add top-level `install`, `status`, and `uninstall`.
- Wire `install` to hook installation, state creation, service installation,
  service start, and current-cwd observation.
- Add the install-time auto-migration prompt, defaulting to yes and explaining
  that auto-migration is limited to directories Claude enters.
- Add `--auto-migrate` and `--no-auto-migrate` for non-interactive install.
- Wire `uninstall` to service removal and hook removal.
- Add `repos list`, `repos remove <repo>`, and `repos prune`.
- Add `migrate`, `migrate --dry-run`, safe rewrite rules, and auto-migrate
  setting support.
- Add grouped dry-run output and stable JSON output.
- Add `purge` and `uninstall --purge`.
- Confirm purge ownership from filesystem state before deleting anything.
- Remove managed Git exclude entries when safe.
- Rewrite README around `install`, `status`, `repos`, and `uninstall`.
- Remove root-config-first usage from the main README.
- Document hook behavior, resource model, and no-default-scan safety.
- Document migration behavior, exact cleanup rules, and the fact that the tool
  never commits.
- Cover the full path with integration tests and a Linux service smoke test.

## Acceptance Criteria

The install-and-forget implementation is ready when:

1. A user can install the binary from GitHub Releases with the documented
   one-line shell installer without Rust, Cargo, or local compilation.
2. `claude-md-symlinker install` installs Claude hooks and starts the user
   service without requiring a user config file.
3. Starting Claude in a Git repo records that repo and every `AGENTS.md`
   directory on the cwd-to-repo-root path through `observe`.
4. Observing a cwd path creates ignored sibling `CLAUDE.md` shims for every
   `AGENTS.md` found between that cwd and the repo root.
5. Deleting a managed `CLAUDE.md` in a recorded instruction directory is
   repaired by the running service.
6. Repos and instruction directories Claude has never reached are not touched.
7. Unknown and tracked `CLAUDE.md` files are never changed during normal shim
   repair.
8. User-owned `CLAUDE.md` files detected on observed paths are recorded as
   migration candidates.
9. `migrate` safely converts detected `CLAUDE.md` files into `AGENTS.md`, adds
   `AGENTS.md` to Git when applicable, recreates a local shim, and never
   commits.
10. `auto_migrate` is chosen at install time, defaults to yes in the prompt, can
   be set to no, and only runs no-warning migrations when enabled.
11. `status` clearly reports hook, service, state, repo, and migration health.
12. `repos list/remove/prune` manage the observed repo and instruction directory
   set.
13. `purge` removes only shims proven to be managed.
14. `uninstall` removes managed hooks and service units without touching user
    files by default.
15. The integration test suite covers the hook, service, observed instruction
    directory, migration, and purge paths.

## Non-Goals For This Iteration

These are intentionally deferred:

- Whole-machine background scanning.
- Default `~/repos` or home-directory scanning.
- GUI or TUI.
- Remote repository management.
- Editing `AGENTS.md` content.
- Creating `AGENTS.md` from nothing.
- Creating `AGENTS.md` outside explicit migration or enabled safe auto-migrate.
- Committing generated shims.
- Committing migrations.
- Non-Linux service installers.
- Complex template transforms.

## Fixed Choices

1. Service interval is 10 minutes with +/- 30 seconds jitter.
2. No filesystem watcher.
3. Existing old SQLite state is archived and replaced with a fresh schema.
4. Hook parsing is tolerant by default and strict only when requested.
5. Dry-run output is grouped for humans; `--json` is the stable exact output.
6. Root-config-first behavior is removed from the main product surface.

## Resolved Decision

`uninstall --purge` does not prompt. Use `--dry-run` to preview managed shims
before deletion.

The architecture should support these choices without changing the core
observed-repo model.

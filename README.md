# claude-md-symlinker

`CLAUDE.md` is Claude Code's project instruction file. It is where Claude Code
expects to find repo-specific context like build commands, coding conventions,
architecture notes, and workflow rules.

[`AGENTS.md`](https://agents.md/) is the industry-wide accepted way to provide
context to agents. It is the same as CLAUDE.md, but your repo is NOT a billboard
for Anthropic. Imagine having to include a clippy.txt in your project...

Anthropic is obstinate like a goat and it is now too late for them to back down
on this convention. They say, "you can add a `CLAUDE.md` that imports `@AGENTS.md` 😉"
. That works, but it still leaves every repo carrying a Claude-branded 
compatibility file.

How about no?

claude-md-symlinker is a service and Claude Code hook that automatically creates
CLAUDE.md symlinks to AGENTS.md that are not tracked in git, 
so that you don't have to commit them manually. Everything just works without
heavy scanning of your filesystem for AGENTS.md files.

As Claude Code traverses your filesystem, any discovered AGENTS.md files will be
symlinked, which gives you automatic AGENTS.md compatibility if you are a Claude
Code user. If you choose to, it will also move the CLAUDE.md files to AGENTS.md
while keeping the same compatibility.

In other words:

```text
AGENTS.md                # canonical file, committed to Git
CLAUDE.md -> AGENTS.md   # local generated shim, ignored by Git
```

It also gives Claude Code the same Agent Skills that other tools read from
`.agents/skills`, without copying them:

```text
.agents/skills/deploy/                 # canonical skill directory
.claude/skills/deploy -> ../../.agents/skills/deploy
```

The tool creates one relative link per skill. It never replaces the complete
`.claude/skills` directory, so Claude-only, plugin, and unmanaged skills can
stay beside managed links.

Note: the initial version of this work was the
[`CLAUDE.md` to `AGENTS.md` migration guide](https://solmaz.io/log/2025/09/08/claude-md-agents-md-migration-guide/),
which documented a manual migration and symlink setup. This repo is the
follow-up that turns that workflow into an installable hook and service.

## Install

Requirements:

- Git
- Claude Code with hooks enabled
- Linux with `systemd --user` for the background repair service, or macOS in
  hook-only mode

Install:

```sh
curl -fsSL https://github.com/osolmaz/claude-md-symlinker/releases/latest/download/claude-md-symlinker-installer.sh | sh
```

This does not require Rust, Cargo, or compiling anything locally. It installs
the binary into `~/.local/bin` and connects it to Claude Code.

`install` does four things:

- Adds managed Claude hooks to `~/.claude/settings.json`.
- Starts a `systemd --user` repair service on Linux.
- Creates local SQLite state.
- Observes the current directory once.

It does not scan your whole machine, home directory, or all repos.

The one-line install asks whether to enable safe auto-migration of existing
`CLAUDE.md` files to `AGENTS.md` when Claude finds them while working. The
default answer is yes. Auto-migration is still scoped only to directories
Claude enters; it is not a global scan. In non-interactive installs, the
installer uses the default yes.

For manual setup choices after the binary is installed:

```sh
claude-md-symlinker install --auto-migrate
claude-md-symlinker install --no-auto-migrate
```

For partial installs:

```sh
claude-md-symlinker install --no-service
claude-md-symlinker install --no-hooks
```

On macOS, use hook-only mode until LaunchAgent support lands:

```sh
claude-md-symlinker install --no-service --auto-migrate
```

## How It Works

Claude hooks call:

```sh
claude-md-symlinker observe
```

When Claude starts in or moves to a directory inside a Git repo,
claude-md-symlinker checks that directory and each parent up to the repo root.
For every `AGENTS.md` found on that path, it records the directory and creates
or repairs the sibling `CLAUDE.md` shim. At the same checked directories, each
valid `.agents/skills/<name>/SKILL.md` is exposed through one managed relative
link at `.claude/skills/<name>`.

Example:

```text
cwd = /repo/apps/web/src/components

checked:
/repo/apps/web/src/components/AGENTS.md
/repo/apps/web/src/AGENTS.md
/repo/apps/web/AGENTS.md
/repo/apps/AGENTS.md
/repo/AGENTS.md
```

It does not descend into siblings or scan the repo tree. It also checks the
fixed global pair `~/.agents/skills` and `~/.claude/skills`; it does not scan the
rest of the home directory.

A skill's full directory is linked, including scripts, references, and assets.
Edits to the canonical directory are visible immediately. The tool does not
copy skill contents.

The background service runs:

```sh
claude-md-symlinker daemon
```

It repairs only instruction and skill directories already recorded by hooks,
plus the fixed global skill root. If a managed `CLAUDE.md` or skill link is
deleted, the service recreates it on a later repair tick. If a canonical source
is deleted, the service removes only an on-disk target whose link still proves
that the tool owns it. Managed copies, hardlinks, tracked files, changed links,
and unknown files are left in place.

## Commands

```sh
claude-md-symlinker status
```

Shows hook, service, state, repo, skill, conflict, and migration health.

```sh
claude-md-symlinker repos list
claude-md-symlinker repos remove <repo>
claude-md-symlinker repos remove <repo> --clean-exclude
claude-md-symlinker repos prune
```

Lists or trims the observed repo set. Removing a repo stops future service
repairs for it but does not delete files. `--clean-exclude` also removes
managed Git exclude entries for that repo.

```sh
claude-md-symlinker migrate --dry-run
claude-md-symlinker migrate
claude-md-symlinker migrate --replace-existing
claude-md-symlinker migrate --no-git-add
```

Migrates detected user-owned `CLAUDE.md` files into `AGENTS.md`. Migration works
only from files Claude has already encountered on observed paths. It never scans
whole repos and never commits. By default, successful migrations add
`AGENTS.md` to the Git index. `--no-git-add` leaves the index alone.
`--replace-existing` allows replacing a sibling `AGENTS.md`, still only after
the normal safe checks pass.

```sh
claude-md-symlinker settings set auto-migrate false
claude-md-symlinker settings set auto-migrate true
claude-md-symlinker settings get auto-migrate
```

Controls safe instruction-file migration.

```sh
claude-md-symlinker skills migrate --dry-run
claude-md-symlinker skills migrate
```

Replaces copied skill directories with relative links only when every path,
file byte, and executable mode matches the canonical skill. The replacement is
atomic and rolls back on failure. Normal reconciliation reports copied
directories as conflicts and never replaces them. Migration works on skill
roots that install, a hook, apply, watch, or the repair service has already
observed.

If another synchronizer has recorded ownership of a target, migration skips it.
Use only one writer for each Claude skill target.

```sh
claude-md-symlinker purge --dry-run
claude-md-symlinker purge
```

Deletes managed instruction shims and skill links recorded in state, after
confirming that each on-disk target still proves ownership.

```sh
claude-md-symlinker uninstall
claude-md-symlinker uninstall --purge
```

Removes managed Claude hooks and the user service. By default, uninstall leaves
generated shims and skill links in place. `--purge` also removes proven managed
targets.

Global options:

```sh
--dry-run   Show planned changes without writing
--json      Print machine-readable output
```

## Migration Rules

Migration is conservative.

Safe exact rewrites:

```text
# CLAUDE.md
  -> # AGENTS.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.
  -> This file provides guidance to AI agents when working with code in this repository.

Claude Code (claude.ai/code)
  -> AI agents
```

If unknown Claude-specific wording remains, the file is marked `needs_review`
instead of being changed. If `AGENTS.md` already exists beside the candidate,
migration skips it unless `--replace-existing` is passed.

When migration succeeds in a Git repo, `AGENTS.md` is added to the Git index by
default. Generated `CLAUDE.md` shims are kept local and ignored. The tool never
commits.

## Git Behavior

Managed shims are ignored with the repo-local exclude file:

```text
.git/info/exclude
```

claude-md-symlinker writes a managed block like:

```text
# claude-md-symlinker managed begin
/CLAUDE.md
/apps/web/CLAUDE.md
/.claude/skills/deploy
# claude-md-symlinker managed end
```

This file is private to your checkout and is not committed.

If `CLAUDE.md` already exists and is not managed:

- It is left untouched.
- It is recorded as a migration candidate when appropriate.
- No ignore entry is added for it during normal shim repair.
- Git continues to show it normally.

Tracked `CLAUDE.md` files and tracked Claude skill targets are never changed
during normal repair. Migration is the explicit path for converting instruction
files or verified copied skills.

Only individual managed project skill paths are ignored. The complete
`.claude/skills` directory is never ignored. Global skills are outside Git and
do not need exclude entries.

## Safety Model

claude-md-symlinker is intentionally boring:

- No whole-machine scan.
- No home-directory scan; only the fixed global skill roots are checked.
- No repo tree scan from hooks.
- No copied skill contents in the steady state.
- No whole-root `.claude/skills` link or ignore rule.
- No overwriting unknown files, directories, or links.
- No normal writes to tracked `CLAUDE.md`.
- No cleanup of tracked `CLAUDE.md`.
- No source-missing cleanup of managed copies or hardlinks.
- No generated shim commits.
- No migration commits.
- No cleanup based only on SQLite records.
- No takeover of targets recorded by another skill synchronizer.
- No Windows skill-link claim until directory links are implemented and tested.
- Hooks exit successfully by default so Claude is not broken by this tool.

The SQLite state directory can be overridden with:

```sh
CLAUDE_MD_SYMLINKER_DATA_DIR=/path/to/state
```

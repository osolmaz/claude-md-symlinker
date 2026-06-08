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

## Install

Requirements:

- Linux with `systemd --user`
- Rust stable
- Git
- Claude Code with hooks enabled

Install from GitHub:

```sh
cargo install --git https://github.com/dutifuldev/claude-md-symlinker
```

Then install the integration:

```sh
claude-md-symlinker install
```

`install` does four things:

- Adds managed Claude hooks to `~/.claude/settings.json`.
- Starts a `systemd --user` repair service.
- Creates local SQLite state.
- Observes the current directory once.

It does not scan your whole machine, home directory, or all repos.

During install, it asks whether to automatically migrate safe existing
`CLAUDE.md` files to `AGENTS.md` when Claude finds them while working. The
default is yes. Auto-migration is still scoped only to directories Claude
enters; it is not a global scan.

For noninteractive installs:

```sh
claude-md-symlinker install --auto-migrate
claude-md-symlinker install --no-auto-migrate
```

For partial installs:

```sh
claude-md-symlinker install --no-service
claude-md-symlinker install --no-hooks
```

## How It Works

Claude hooks call:

```sh
claude-md-symlinker observe
```

When Claude starts in or moves to a directory inside a Git repo,
claude-md-symlinker checks that directory and each parent up to the repo root.
For every `AGENTS.md` found on that path, it records the directory and creates
or repairs the sibling `CLAUDE.md` shim.

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

It does not descend into siblings or scan the repo tree.

The background service runs:

```sh
claude-md-symlinker daemon
```

It repairs only instruction directories already recorded by hooks. If a
managed `CLAUDE.md` is deleted, the service recreates it on a later repair
tick.

## Commands

```sh
claude-md-symlinker status
```

Shows hook, service, state, repo, and migration health.

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

Controls safe auto-migration.

```sh
claude-md-symlinker purge --dry-run
claude-md-symlinker purge
```

Deletes managed shims recorded in state, after confirming each file is still a
managed shim on disk.

```sh
claude-md-symlinker uninstall
claude-md-symlinker uninstall --purge
```

Removes managed Claude hooks and the user service. By default, uninstall leaves
generated shims in place. `--purge` also removes managed shims.

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
# claude-md-symlinker managed end
```

This file is private to your checkout and is not committed.

If `CLAUDE.md` already exists and is not managed:

- It is left untouched.
- It is recorded as a migration candidate when appropriate.
- No ignore entry is added for it during normal shim repair.
- Git continues to show it normally.

Tracked `CLAUDE.md` files are never changed during normal shim repair.
Migration is the explicit path for converting them.

## Safety Model

claude-md-symlinker is intentionally boring:

- No whole-machine scan.
- No default home-directory scan.
- No repo tree scan from hooks.
- No overwriting unknown files.
- No normal writes to tracked `CLAUDE.md`.
- No generated shim commits.
- No migration commits.
- No cleanup based only on SQLite records.
- Hooks exit successfully by default so Claude is not broken by this tool.

The SQLite state directory can be overridden with:

```sh
CLAUDE_MD_SYMLINKER_DATA_DIR=/path/to/state
```

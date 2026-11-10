# gh-arriba

[![Build](https://github.com/arriba-tools/gh-arriba/actions/workflows/build.yml/badge.svg)](https://github.com/arriba-tools/gh-arriba/actions/workflows/build.yml)
[![Lint](https://github.com/arriba-tools/gh-arriba/actions/workflows/lint.yml/badge.svg)](https://github.com/arriba-tools/gh-arriba/actions/workflows/lint.yml)
[![codecov](https://codecov.io/gh/arriba-tools/gh-arriba/graph/badge.svg)](https://codecov.io/gh/arriba-tools/gh-arriba)

A GitHub CLI extension for the chores `gh` doesn't cover: review threads, pending reviews,
CI retries, signed rebases, releases, Dependabot, and project config sync.

Every command takes `--json`, and every prompt has a flag that answers it, so scripts and coding
agents can drive the same workflows a person runs by hand.

## Contents

- [Install](#install)
- [Use with Claude](#use-with-claude)
- [Usage](#usage)
  - [Pull requests](#pull-requests)
  - [Uploads](#uploads)
  - [Workflows and releases](#workflows-and-releases)
  - [Issues](#issues)
  - [Dependabot](#dependabot)
  - [Runners](#runners)
  - [Worktrees](#worktrees)
  - [Notifications](#notifications)
  - [Inbox](#inbox)
  - [Config sync](#config-sync)
  - [Options](#options)
- [Examples](#examples)
- [Config sync](#config-sync-1)
  - [First machine](#first-machine)
  - [Every other machine](#every-other-machine)
  - [Which side wins](#which-side-wins)
  - [Platform-specific files](#platform-specific-files)
  - [Options](#options-1)
  - [Safety](#safety)
  - [Gist layout](#gist-layout)
- [Authentication](#authentication)
- [Development](#development)
- [License](#license)

## Install

```
gh extension install arriba-tools/gh-arriba
```

Upgrade with `gh extension upgrade arriba`.

## Use with Claude

Add this line to `~/.claude/CLAUDE.md` so Claude Code reaches for `gh arriba` where `gh` falls
short:

```
* Use `gh arriba` for what `gh` lacks, such as PR review threads, pending reviews, CI reruns, job logs, releases, and Dependabot. Run `gh arriba --help` for the commands.
```

To hear when Claude Code needs you, see [Claude Code notifications](#claude-code-notifications).

## Usage

```
gh arriba [options] <command>
```

### Pull requests

| Command | Description |
|---------|-------------|
| `pr threads [PR]` | List unresolved review threads from humans and bots |
| `pr reply <ID> [BODY]` | Reply to a review comment or thread, body from stdin with `-` |
| `pr resolve <ID>...` | Resolve or unresolve (`--undo`) review threads |
| `pr comment show <URL\|ID>` | Print one review comment, issue comment, or review by its link or ID |
| `pr comment edit <URL\|ID> [BODY]` | Replace the body of a comment or review you posted, body from stdin with `-` |
| `pr review lines [PR]` | Print the diff line ranges that accept inline comments |
| `pr review add [PR] <PATH:LINE[-END]> [BODY]` | Add an inline comment to your pending review |
| `pr review show [PR]` | Print the comments in your pending review |
| `pr review submit [PR]` | Submit your pending review as comment, approve, or request changes |
| `pr review discard [PR]` | Delete your pending review |
| `pr apply [PR]` | Apply reviewers' suggestion blocks to the working tree |
| `pr dump [PR]` | Write the whole discussion as grep-friendly text |
| `pr checks [PR]...` | Summarize state, mergeability, reviews, and checks, failing when a check failed |
| `pr retry [PR]` | Rerun failed and cancelled checks on a PR, on all your PRs with `--mine`, or on every open PR with `--all` |
| `pr failures [PR]` | Show failed checks with their failed steps and error lines, or `--grep` matches, saving full logs |
| `pr rebase [PR]` | Rebase onto the base branch, signed locally or server-side with `--remote` |
| `pr reviewers [PR]` | Suggest reviewers from top contributors and request one with `--add` |
| `pr labels [PR]` | List the repo's labels, `--suggest` some for the PR, and `--add` or `--remove` them by close match |

`PR` is a number, URL, `owner/repo#N`, or branch. It defaults to the PR for the current branch.
`ID` in `pr reply` and `pr resolve` is a comment ID, a `#discussion_r` comment link, or a
`PRRT_` thread ID.

### Uploads

| Command | Description |
|---------|-------------|
| `upload <FILE[#ALT]>...` | Upload images for use inline in comments and print the markdown that shows them |

Each image is stored against the repository from `-R` or the working directory, which needs write
access. PNG, JPEG, GIF, WebP, and SVG images up to 10 MiB are accepted. An upload cannot be undone,
and GitHub Enterprise Server has no upload endpoint.

### Workflows and releases

| Command | Description |
|---------|-------------|
| `run logs <RUN_OR_JOB>` | Download job logs to files for searching, picking jobs by name with `--job` and printing error lines with `--errors` or matches with `--grep` |
| `run watch [RUN]...` | Follow runs until their jobs finish, by default every run for the local HEAD |
| `run dispatch <WORKFLOW>` | Dispatch any workflow with `-f` inputs, printing its run and following it with `--watch` |
| `run artifacts [RUN]...` | List artifacts with size and expiry for runs, a commit, a branch, or a PR, download them with `--download`, or delete old ones with `--prune` |
| `run caches` | Show Actions cache usage against the repo's 10 GB limit, grouped by ref and key, or delete caches with `--prune` |
| `run stats` | Show each job's pass rate, flaky re-runs, and median and p90 run time over a branch's recent runs, one workflow with `-w` |
| `release` | Dispatch the repo's release workflow with a version bump, previewing with `--dry-run` |

### Issues

| Command | Description |
|---------|-------------|
| `issue watch <NUMBER>` | Wait for new comments on an issue or PR and print them, filtered with `--match` and `--not-match` |
| `issue watch --label <LABEL>` | Wait for open issues with a label and print them, skipping ones that also have `--not-label` |

`issue watch` exits 0 after printing what arrived, 3 when nothing arrived before `--timeout`, and 4
when GitHub could not be reached for five polls in a row. An unchanged issue costs nothing against
the rate limit, so the default 15 second interval is safe to leave running.

With `--state KEY` the watch remembers what was acknowledged. Printing a comment does not
acknowledge it. Restart with the `--ack` the command printed once the comments are handled, so a
caller that stops first sees them again. The first run under a key looks back 24 hours, and a watch
without `--state` reports only what arrives while it waits.

### Dependabot

| Command | Description |
|---------|-------------|
| `dependabot list` | Group open PRs into ready, needs rebase, and blocked, with lock-step sets |
| `dependabot merge [PR]...` | Approve and merge PRs, singly or as a lock-step group |
| `dependabot rebase [PR]...` | Ask Dependabot to rebase PRs |
| `dependabot alerts` | List open security alerts with their fix PR |
| `dependabot dismiss <ALERT>...` | Dismiss alerts with a reason |

### Runners

| Command | Description |
|---------|-------------|
| `runners` | List the repo's self-hosted runners with status, labels, and the job each busy one runs |
| `runners --org <ORG>` | List the organization's runners, scanning its repos for their jobs |
| `runners --queued` | Also list jobs waiting for a runner and how long they have waited |
| `runners --from-jobs` | Infer runners from the jobs of recent runs, without admin access |

Listing runners needs admin access: for an organization, an org admin token with the `admin:org`
scope (`gh auth refresh -s admin:org`). Without it, `runners` falls back to `--from-jobs`.

### Worktrees

| Command | Description |
|---------|-------------|
| `worktree list` | List worktrees with their PR, its state and checks, and uncommitted or unpushed work |
| `worktree prune` | Remove worktrees of your PRs that merged or closed, and their branches, keeping any with unpushed work |
| `worktree add <PR> [DIR]` | Check one of your PRs out into a new worktree beside the main one, or print the one it already has |

### Notifications

| Command | Description |
|---------|-------------|
| `notify <message>` | Show a desktop notification, subtitled and grouped by the git root unless `--subtitle` or `--group` say otherwise |
| `notify --hook` | Show the notification a Claude Code or Codex hook describes in the JSON it gets on stdin |
| `notify watch` | Poll your GitHub inbox and show a desktop notification for each new thread, until stopped |
| `notify watch --install` | Start the watch with the options given, now and at every login, on macOS and Linux |
| `notify watch --uninstall` | Stop the watch that starts at login, and stop starting it |

`--install` registers a LaunchAgent on macOS, logging to `~/Library/Logs/gh-arriba-notify-watch.log`,
or a systemd user service on Linux, logging to the journal. Either one restarts the watch a minute
after it exits, as when the first poll runs before the network is up, and runs it with the `PATH`
you installed from so it finds `gh` and the notifier. Run `--install` again to change the options.
When [Claude Code](#claude-code-notifications) or [Codex](#codex-notifications) is set up without
the hooks that run `notify`, `--install` also offers to add them, and `--uninstall` offers to remove
them. `--yes` answers both.

On macOS with [terminal-notifier](https://github.com/julienXX/terminal-notifier), clicking a
`notify <message>` notification returns to where it ran: the Ghostty 1.3+ terminal, the Terminal tab,
or the VS Code window. VS Code is matched by the window the Claude Code extension serves, or else by
the open folder that holds the working directory. Other apps only come to the front.

Notifications carry the GitHub mark. A macOS notification's icon is its sender's, so `notify`
makes a copy of terminal-notifier.app named gh arriba in `~/Library/Application Support/gh-arriba`,
which macOS asks you to allow the first time and lists in System Settings > Notifications. Until it
is allowed, the installed terminal-notifier shows the notification instead, so if you refused, turn
gh arriba on there.

macOS hides notifications after a few seconds by default. To keep them until dismissed, set
gh arriba to Persistent in System Settings > Notifications (Alerts on older macOS).

#### Claude Code notifications

`notify watch --install` offers a Notification hook in `~/.claude/settings.json` that tells you when
Claude Code needs you. With terminal-notifier installed, clicking the notification returns to its
terminal or VS Code window.

#### Codex notifications

`notify watch --install` also offers hooks in `~/.codex/hooks.json` that notify when Codex needs your
approval and when it finishes a turn. Codex has no hook for when it asks you a question. It runs a
hook only after you trust it, so open `/hooks` in Codex after adding them. The hooks run in the
background, which Codex supports from 0.148.0, as earlier releases skip them.

### Inbox

| Command | Description |
|---------|-------------|
| `inbox clean` | Mark done the inbox threads whose PR or issue merged or closed, or whose repo is gone, previewing with `--dry-run` |

Marking a thread done moves it to the Done tab without unsubscribing, and new activity brings it
back. Pass `-R owner/repo` to clean one repo's threads.

### Config sync

| Command | Description |
|---------|-------------|
| `config [sync] [files]` | Push or pull each tracked file, whichever side is newer (default) |
| `config status [files]` | Show what sync would push or pull, without changing anything |
| `config add [--platform] <files>` | Track files and push them, creating the gist on first use |
| `config remove [--platform] <files>` | Stop tracking files and delete them from the gist |
| `config restore [files]` | Overwrite local files with the gist versions |
| `config cleanup` | Delete gist files the manifest no longer lists |
| `config open` | Open the project's gist in a web browser |
| `config delete` | Delete the project's gist, after confirming |

See [Config sync](#config-sync-1) for how it works.

### Options

| Option | Description |
|--------|-------------|
| `-R`, `--repo <OWNER/REPO>` | Target repository (default: current repo) |
| `--json` | Print machine-readable JSON |
| `-y`, `--yes` | Skip confirmation prompts |
| `-h`, `--help` | Print help for any command |
| `-V`, `--version` | Print version information |

## Examples

Work through review feedback on the current branch's PR:

```
gh arriba pr threads
gh arriba pr apply --thread PRRT_kwDOAbc123
echo "Fixed." | gh arriba pr reply 1234567890 --resolve
```

Build up a review with inline comments, then submit it:

```
gh arriba pr review lines 42
gh arriba pr review add 42 src/inflate.c:120-124 "This drops the window check."
gh arriba pr review submit 42 --event request-changes --body "See inline comments."
```

Upload a screenshot and show it in a reply:

```
img=$(gh arriba upload './login.png#The login error')
gh arriba pr reply 1234567890 "Fixed, it now renders like this. $img"
```

Block until someone answers on an issue, then pick up where the last run stopped:

```
gh arriba issue watch 42 --state me --not-match '^/skip'
gh arriba issue watch 42 --state me --ack 1234567890 --not-match '^/skip'
```

Wait for an issue to be queued for this machine and not yet taken:

```
gh arriba issue watch --label win2 --not-label running
```

Check which labels exist or fit the PR, then add one by a loose name:

```
gh arriba pr labels
gh arriba pr labels 42 --suggest
gh arriba pr labels 42 --add ui
```

Rerun every failed check across all your open PRs:

```
gh arriba pr retry --mine
```

Rerun every failed check on every open PR in a repo, whoever authored it, after confirming the runs
it found:

```
gh arriba pr retry owner/repo --all --dry-run
gh arriba pr retry owner/repo --all
```

Runs on `main` or from a schedule belong to no PR, so `pr retry` leaves them alone.

Wait for CI after pushing, printing each check as it finishes and failing if any check fails:

```
git push && gh arriba pr checks --sha HEAD --wait --fail-fast
```

`--sha` waits for GitHub to see the push first, so the checks reported are the new commit's.

Add `--notify` for a desktop notification per PR when the checks finish, fail with `--fail-fast`, or
time out.

Download only the failed job logs from a run and search them:

```
gh arriba run logs https://github.com/owner/repo/actions/runs/123456 --failed -o logs
grep -rn "error:" logs
```

Or search them in place with `--grep`, which matches the lines without their timestamps and escape
codes and labels each match with its line in the saved log. `pr failures` takes it too:

```
gh arriba pr failures --grep 'undefined reference|LNK2019'
```

Logs of finished jobs are saved once, and later `run logs` and `pr failures` calls read the saved copy
instead of downloading it again. Pass `--refresh` to download them anyway.

Follow a run and print each job as it finishes, exiting non-zero if any fails:

```
gh arriba run watch https://github.com/owner/repo/actions/runs/123456
```

Or push and follow every run the push started, waiting for them to appear:

```
git push && gh arriba run watch
```

Add `--job "Export Check"` to return as soon as the matching jobs finish, ignoring the rest.

Add `--notify` to get a desktop notification when the runs finish, so you can switch away while
they go. It uses `terminal-notifier` or `osascript` on macOS, `notify-send` on Linux, and a toast on
Windows:

```
git push && gh arriba run watch --notify
```

Dispatch a workflow on a branch and follow the run it starts:

```
gh arriba run dispatch release.yaml --ref feature/self-update -f version=99.0.16 --watch
```

List the artifacts a PR's head commit produced, with their size and when they expire:

```
gh arriba run artifacts --pr 42
```

Or those of the latest green build of the default branch, skipping runs that uploaded none:

```
gh arriba run artifacts --success -w build
```

Add `--download` to extract them, picking artifacts with `-n NAME` or `-p 'GLOB'`:

```
gh arriba run artifacts --success -w build --download -p 'linux-*' -D dist
```

See what pruning would free across the repo, keeping the newest of each artifact name and anything
from the last week, then run it again without `--dry-run` to delete:

```
gh arriba run artifacts --prune --older-than 7d --keep-latest 1 --dry-run
```

See what fills the repo's Actions cache, by branch or PR and by key with its hash dropped:

```
gh arriba run caches
gh arriba run caches --ref main -k Linux-cargo
```

Delete caches nobody has used for a week, previewing first:

```
gh arriba run caches --prune --unused-for 7d --dry-run
```

Or those left behind by closed and merged PRs, which GitHub keeps until it needs the space:

```
gh arriba run caches --prune --closed
```

Find the jobs that fail or need a re-run on the default branch, over its last 100 runs:

```
gh arriba run stats --limit 100
```

Cut a minor release through the repo's Release workflow:

```
gh arriba release --bump minor
```

Merge every ready Dependabot PR, with lock-step groups merged together:

```
gh arriba dependabot merge --all-ready
```

Get desktop notifications for review requests and mentions while you work, polling as often as
GitHub allows. Pass `-R owner/repo` to watch one repo, and `--all` to also hear about what is already
unread:

```
gh arriba notify watch --reason review_requested --reason mention
```

Clear merged and closed PRs out of your inbox, previewing first:

```
gh arriba inbox clean --dry-run
gh arriba inbox clean
```

Or keep that watch running from every login:

```
gh arriba notify watch --install --reason review_requested --reason mention
```

Hear when a long build or test run ends, even from another window:

```
make test; gh arriba notify "tests done"
```

## Config sync

`config` keeps per-project files that you don't commit, such as `.vscode/settings.json` or
`.claude/CLAUDE.md`, in sync across machines. Each project gets one GitHub gist that holds the
files plus a manifest listing them.

### First machine

Track the files from anywhere inside the project:

```
gh arriba config add --private .vscode/settings.json .claude/CLAUDE.md
```

This creates the gist and pushes the files. Gists are public unless you pass `--private`, and the
flag only matters here, when the gist is created.

### Every other machine

Run `config` with no arguments inside the same project:

```
gh arriba config
```

It reads the manifest from the gist, pulls every tracked file, and pushes any you changed locally.
Run it again whenever you edit a tracked file. To track another file later, run `config add` again
from any machine.

### Which side wins

Sync works per file and never merges. It compares each file's local modification time with the time
the manifest recorded for it.

| Local file | Sync |
|------------|------|
| Newer than the manifest | Pushes it to the gist |
| Older than the manifest | Pulls the gist version |
| Missing | Pulls the gist version |
| Within a second of the manifest | Leaves it alone |

Edit a file on one machine at a time, since the newer copy overwrites the other. Run `config status`
first to see what would happen, or `config restore` to take the gist version regardless.

### Platform-specific files

Add a file with `--platform` when it only applies to the current OS, such as a toolchain path:

```
gh arriba config add --platform .vscode/cmake-variants.yaml
```

Run the same command on each OS and each keeps its own copy of that path in the gist. Sync only
touches the copy for the OS it runs on, `macos`, `linux`, or `windows`.

### Options

| Option | Description |
|--------|-------------|
| `--name <NAME>` | Gist name prefix (default: basename of the project root) |
| `--root <DIR>` | Directory that file paths are relative to (default: git root, or the current directory) |
| `--private` | Create the gist as secret instead of public |

### Safety

- Before pushing, `config` scans files for potential secrets such as API keys, tokens, and
  passwords, and aborts if it finds any.
- `config delete` asks for confirmation, or takes `--yes`.

### Gist layout

The gist's description is `"<project> config-sync"`, which is how `config` finds it again. Each file
is stored under a flat name with underscores for path separators, as in
`zlib-ng_.vscode_settings.json`, and platform-specific files tag the OS before the extension, as in
`zlib-ng_.vscode_settings[macos].json`. The manifest is `.<project>-config-sync.yaml`.

## Authentication

Uses the token from `gh auth token`, falling back to `GH_TOKEN` or `GITHUB_TOKEN`. Set
`GH_HOST` to target GitHub Enterprise Server.

## Development

```
git clone https://github.com/arriba-tools/gh-arriba.git
cd gh-arriba
cargo build --release
cp target/release/gh-arriba .
gh extension install .
```

The copied binary is ignored by git. Rebuild and copy it again to pick up changes.

## License

MIT

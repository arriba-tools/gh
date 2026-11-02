use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::api::Repo;
use crate::api::pulls::{self, BranchPr, fetch_pr};
use crate::cli::{WorktreeAddArgs, WorktreeCommand, WorktreePruneArgs};
use crate::context::{Ctx, git, git_in};
use crate::{output, prompt};

pub fn run(ctx: &Ctx, cmd: WorktreeCommand) -> Result<()> {
    match cmd {
        WorktreeCommand::List => list(ctx),
        WorktreeCommand::Prune(args) => prune(ctx, args),
        WorktreeCommand::Add(args) => add(ctx, args),
    }
}

/// One worktree from `git worktree list --porcelain`.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct Worktree {
    path: PathBuf,
    head: String,
    /// Short branch name, absent for a detached HEAD.
    branch: Option<String>,
    /// The worktree the repository was cloned into, which is never removed.
    main: bool,
    bare: bool,
    locked: bool,
    /// Git reports the directory as missing, so `git worktree prune` would drop it.
    prunable: bool,
}

/// Parse `git worktree list --porcelain`, whose first entry is the main worktree.
fn parse_list(text: &str) -> Vec<Worktree> {
    let mut out: Vec<Worktree> = Vec::new();
    for line in text.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key == "worktree" {
            out.push(Worktree {
                path: PathBuf::from(value),
                head: String::new(),
                branch: None,
                main: out.is_empty(),
                bare: false,
                locked: false,
                prunable: false,
            });
            continue;
        }
        let Some(wt) = out.last_mut() else {
            continue;
        };
        match key {
            "HEAD" => wt.head = value.to_string(),
            "branch" => {
                wt.branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_string(),
                )
            }
            "bare" => wt.bare = true,
            "locked" => wt.locked = true,
            "prunable" => wt.prunable = true,
            _ => {}
        }
    }
    out
}

/// What a worktree holds that isn't on the remote yet.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
struct Local {
    /// Uncommitted changes and untracked files.
    changes: usize,
    /// Commits ahead of and behind the branch's upstream, when it has one.
    ahead: u64,
    behind: u64,
    /// The upstream branch was deleted on the remote, as it usually is after a merge.
    upstream_gone: bool,
}

/// Parse `%(upstream:track)`, as in `[ahead 2, behind 1]` or `[gone]`.
fn parse_track(track: &str, local: &mut Local) {
    let inner = track.trim().trim_start_matches('[').trim_end_matches(']');
    for part in inner.split(", ") {
        match part.split_once(' ') {
            Some(("ahead", n)) => local.ahead = n.parse().unwrap_or(0),
            Some(("behind", n)) => local.behind = n.parse().unwrap_or(0),
            _ if part == "gone" => local.upstream_gone = true,
            _ => {}
        }
    }
}

fn local_state(root: &Path, wt: &Worktree) -> Local {
    let mut local = Local::default();
    if wt.prunable || wt.bare {
        return local;
    }
    local.changes = git_in(&wt.path, &["status", "--porcelain"])
        .map(|s| s.lines().count())
        .unwrap_or(0);
    if let Some(branch) = &wt.branch {
        let track = git_in(
            root,
            &[
                "for-each-ref",
                "--format=%(upstream:track)",
                &format!("refs/heads/{branch}"),
            ],
        )
        .unwrap_or_default();
        parse_track(&track, &mut local);
    }
    local
}

/// A worktree with its local state and the PR its branch is the head of.
#[derive(Debug, Serialize)]
struct Row {
    #[serde(flatten)]
    worktree: Worktree,
    local: Local,
    pr: Option<BranchPr>,
}

/// Git config key `worktree add` sets on a fork PR's branch to record the PR number, since the
/// branch has no upstream and may not keep the fork's branch name.
const PR_KEY: &str = "gh-arriba-pr";

/// What ties a local branch to its PR.
#[derive(Debug, Default)]
struct Origin {
    /// The PR number `worktree add` recorded on the branch.
    recorded: Option<u64>,
    /// OWNER/REPO the branch's upstream remote points at.
    upstream_repo: Option<String>,
}

fn origin(root: &Path, branch: &str) -> Origin {
    let config = |key: &str| {
        git_in(
            root,
            &["config", "--get", &format!("branch.{branch}.{key}")],
        )
        .ok()
        .filter(|v| !v.is_empty())
    };
    let upstream_repo = config("remote")
        .and_then(|remote| git_in(root, &["remote", "get-url", &remote]).ok())
        .and_then(|url| repo_of_url(&url));
    Origin {
        recorded: config(PR_KEY).and_then(|n| n.parse().ok()),
        upstream_repo,
    }
}

/// The PR of a branch, among the PRs with its name. Names repeat across forks, so a PR counts only
/// when its head lives in the repo the branch's upstream points at, or its head is the worktree's
/// commit. Among those the viewer's newest wins, then the newest.
fn pick_pr(
    prs: &[BranchPr],
    viewer: &str,
    upstream_repo: Option<&str>,
    head: &str,
) -> Option<BranchPr> {
    let ours = |p: &&BranchPr| {
        let same_repo = matches!(
            (p.head_repo.as_deref(), upstream_repo),
            (Some(a), Some(b)) if a.eq_ignore_ascii_case(b)
        );
        same_repo || p.head_oid == head
    };
    let matching: Vec<&BranchPr> = prs.iter().filter(ours).collect();
    matching
        .iter()
        .find(|p| p.author.eq_ignore_ascii_case(viewer))
        .or_else(|| matching.first())
        .map(|p| (*p).clone())
}

/// Every worktree of the repository at `root`, with local state and PRs.
fn rows(ctx: &Ctx, root: &Path) -> Result<Vec<Row>> {
    let list = git_in(root, &["worktree", "list", "--porcelain"])?;
    let worktrees = parse_list(&list);
    let origins: Vec<Origin> = worktrees
        .iter()
        .map(|w| {
            w.branch
                .as_deref()
                .map(|b| origin(root, b))
                .unwrap_or_default()
        })
        .collect();
    let by_name: Vec<String> = worktrees
        .iter()
        .zip(&origins)
        .filter(|(_, o)| o.recorded.is_none())
        .filter_map(|(w, _)| w.branch.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let any_branch = worktrees.iter().any(|w| w.branch.is_some());
    let (repo, prs, viewer) = if any_branch {
        let repo = ctx.repo()?;
        let prs = if by_name.is_empty() {
            Default::default()
        } else {
            pulls::prs_by_branch(&ctx.client, &repo, &by_name)?
        };
        (Some(repo), prs, ctx.client.viewer()?.to_string())
    } else {
        (None, Default::default(), String::new())
    };
    let mut rows = Vec::new();
    for (w, o) in worktrees.into_iter().zip(origins) {
        let pr = match (&w.branch, o.recorded, &repo) {
            // The number was recorded when the worktree was made, so it needs no matching.
            (Some(_), Some(n), Some(repo)) => pulls::pr_by_number(&ctx.client, repo, n)?,
            (Some(b), None, _) => prs
                .get(b)
                .and_then(|p| pick_pr(p, &viewer, o.upstream_repo.as_deref(), &w.head)),
            _ => None,
        };
        rows.push(Row {
            local: local_state(root, &w),
            worktree: w,
            pr,
        });
    }
    Ok(rows)
}

/// The main worktree's root, from anywhere inside any worktree of the repository.
fn repo_root() -> Result<PathBuf> {
    let common = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .context("Not in a git repository")?;
    let common = PathBuf::from(common);
    // The common dir is the main worktree's .git, or the repository itself when bare.
    Ok(match common.file_name() {
        Some(name) if name == ".git" => common.parent().map(Path::to_path_buf).unwrap_or(common),
        _ => common,
    })
}

fn list(ctx: &Ctx) -> Result<()> {
    let root = repo_root()?;
    let rows = rows(ctx, &root)?;
    if ctx.json {
        return output::print_json(&rows);
    }
    print!("{}", render(&rows));
    Ok(())
}

/// A worktree prune would remove, or the reason it keeps one it would otherwise remove.
#[derive(Debug, PartialEq, Serialize)]
struct Candidate {
    path: PathBuf,
    branch: String,
    /// Why the worktree is done with, as in `PR #42 merged`.
    reason: String,
    /// Why it stays anyway, such as uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    kept: Option<String>,
}

/// The worktrees of the viewer's own PRs that merged or closed. Worktrees of other people's PRs, and
/// ones with no PR to tell whose they are, are never candidates. Each
/// is kept when it is locked or holds work that isn't on the remote, unless `force` is set.
/// `on_remote` tells whether a commit is reachable from a remote branch.
fn plan_prune(
    rows: &[Row],
    viewer: &str,
    force: bool,
    on_remote: impl Fn(&str) -> bool,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for r in rows {
        let w = &r.worktree;
        let Some(branch) = &w.branch else {
            continue;
        };
        if w.main || w.bare || w.prunable {
            continue;
        }
        let Some(p) =
            r.pr.as_ref()
                .filter(|p| p.author.eq_ignore_ascii_case(viewer))
        else {
            continue;
        };
        let reason = match p.state.as_str() {
            "MERGED" => format!("PR #{} merged", p.number),
            "CLOSED" => format!("PR #{} closed", p.number),
            _ => continue,
        };
        let l = &r.local;
        let kept = if w.locked {
            Some("locked".to_string())
        } else if force {
            None
        } else if l.changes > 0 {
            Some(format!("{} uncommitted changes", l.changes))
        } else if l.ahead > 0 {
            Some(format!("{} unpushed commits", l.ahead))
        } else {
            // A merged PR's head, or any commit a remote branch has, is safe to drop locally.
            let pushed = p.head_oid == w.head || on_remote(&w.head);
            (!pushed).then(|| "commits on no remote branch".to_string())
        };
        out.push(Candidate {
            path: w.path.clone(),
            branch: branch.clone(),
            reason,
            kept,
        });
    }
    out
}

#[derive(Serialize)]
struct PruneReport {
    removed: Vec<Candidate>,
    kept: Vec<Candidate>,
    dry_run: bool,
    /// Worktrees whose removal failed, with the error.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failed: Vec<String>,
}

fn prune(ctx: &Ctx, args: WorktreePruneArgs) -> Result<()> {
    prune_at(ctx, &repo_root()?, &args)
}

fn prune_at(ctx: &Ctx, root: &Path, args: &WorktreePruneArgs) -> Result<()> {
    if !args.dry_run {
        // Drop the records of worktrees whose directories were already deleted.
        git_in(root, &["worktree", "prune"])?;
    }
    let rows = rows(ctx, root)?;
    let on_remote = |oid: &str| {
        git_in(root, &["branch", "-r", "--contains", oid]).is_ok_and(|s| !s.trim().is_empty())
    };
    let (kept, removed): (Vec<Candidate>, Vec<Candidate>) =
        plan_prune(&rows, ctx.client.viewer()?, args.force, on_remote)
            .into_iter()
            .partition(|c| c.kept.is_some());
    let mut report = PruneReport {
        removed,
        kept,
        dry_run: args.dry_run,
        failed: Vec::new(),
    };

    let say = |line: &str| {
        if ctx.json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };
    for c in &report.kept {
        say(&format!(
            "keep    {} ({}, {})",
            display_path(&c.path, &rows),
            c.reason,
            c.kept.as_deref().unwrap_or_default()
        ));
    }
    for c in &report.removed {
        say(&format!(
            "remove  {} ({})",
            display_path(&c.path, &rows),
            c.reason
        ));
    }
    if report.removed.is_empty() {
        say("Nothing to prune");
        return finish(ctx, &report);
    }
    if args.dry_run {
        return finish(ctx, &report);
    }
    let question = format!(
        "Remove {} and their local branches?",
        output::plural(report.removed.len(), "worktree")
    );
    if !prompt::confirm(&question, ctx.yes)? {
        say("Cancelled");
        return Ok(());
    }
    for c in &report.removed {
        let path = c.path.to_string_lossy();
        let mut remove = vec!["worktree", "remove"];
        if args.force {
            remove.push("--force");
        }
        remove.push(&path);
        // The branch of a squash-merged PR isn't merged as far as git knows, so it needs -D.
        let result = git_in(root, &remove).and_then(|_| git_in(root, &["branch", "-D", &c.branch]));
        if let Err(e) = result {
            eprintln!("failed to remove {path}: {e:#}");
            report.failed.push(path.to_string());
        }
    }
    say(&format!(
        "Removed {}",
        output::plural(report.removed.len() - report.failed.len(), "worktree")
    ));
    finish(ctx, &report)?;
    if !report.failed.is_empty() {
        anyhow::bail!(
            "{} could not be removed",
            output::plural(report.failed.len(), "worktree")
        );
    }
    Ok(())
}

fn finish(ctx: &Ctx, report: &PruneReport) -> Result<()> {
    if ctx.json {
        output::print_json(report)?;
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct Added {
    pr: String,
    path: PathBuf,
    branch: String,
    /// The PR already had a worktree, which is reported instead of making another.
    existing: bool,
}

fn add(ctx: &Ctx, args: WorktreeAddArgs) -> Result<()> {
    add_at(ctx, &repo_root()?, &args)
}

/// Check a PR out into a new worktree beside the main one, or report the worktree it already has.
fn add_at(ctx: &Ctx, root: &Path, args: &WorktreeAddArgs) -> Result<()> {
    let pr = ctx.pr(Some(&args.pr))?;
    let info = fetch_pr(&ctx.client, &pr)?;
    let viewer = ctx.client.viewer()?;
    if !info.author.eq_ignore_ascii_case(viewer) {
        anyhow::bail!(
            "{pr} is by {}, and worktree add only checks out your own PRs",
            info.author
        );
    }
    let worktrees = parse_list(&git_in(root, &["worktree", "list", "--porcelain"])?);
    // A fork's branch gets checked out as pr-N when its own name is taken locally.
    let fork_name = format!("pr-{}", info.number);
    let existing = worktrees.iter().find(|w| match w.branch.as_deref() {
        Some(b) if !info.cross_repository => b == info.head_ref,
        Some(b) => b == fork_name || (b == info.head_ref && w.head == info.head_oid),
        None => false,
    });
    let added = match existing {
        Some(w) => Added {
            pr: pr.to_string(),
            path: w.path.clone(),
            branch: w.branch.clone().unwrap_or_default(),
            existing: true,
        },
        None => {
            let main = worktrees
                .iter()
                .find(|w| w.main)
                .map(|w| w.path.clone())
                .unwrap_or_else(|| root.to_path_buf());
            let path = match &args.dir {
                Some(d) => d.clone(),
                None => default_dir(&main, info.number),
            };
            if path.exists() {
                anyhow::bail!("{} already exists", path.display());
            }
            let remote = remote_for(root, &pr.repo);
            let dir = path.to_string_lossy();
            let branch = if info.cross_repository {
                let branch = if branch_exists(root, &info.head_ref) {
                    fork_name
                } else {
                    info.head_ref.clone()
                };
                let pull = format!("pull/{}/head", info.number);
                git_in(root, &["fetch", "--quiet", &remote, &pull])?;
                git_in(
                    root,
                    &[
                        "worktree",
                        "add",
                        "--quiet",
                        "-b",
                        &branch,
                        &dir,
                        "FETCH_HEAD",
                    ],
                )?;
                let key = format!("branch.{branch}.{PR_KEY}");
                git_in(root, &["config", &key, &info.number.to_string()])?;
                branch
            } else {
                // An explicit destination, since a single-branch clone's refspec doesn't cover it.
                let refspec = format!("+refs/heads/{0}:refs/remotes/{remote}/{0}", info.head_ref);
                git_in(root, &["fetch", "--quiet", &remote, &refspec])?;
                if branch_exists(root, &info.head_ref) {
                    git_in(root, &["worktree", "add", "--quiet", &dir, &info.head_ref])?;
                } else {
                    let start = format!("{remote}/{}", info.head_ref);
                    git_in(
                        root,
                        &[
                            "worktree",
                            "add",
                            "--quiet",
                            "--no-track",
                            "-b",
                            &info.head_ref,
                            &dir,
                            &start,
                        ],
                    )?;
                    // Set the upstream by hand: --track refuses a ref the fetch refspec doesn't map.
                    let branch = &info.head_ref;
                    git_in(
                        root,
                        &["config", &format!("branch.{branch}.remote"), &remote],
                    )?;
                    let merge = format!("refs/heads/{branch}");
                    git_in(root, &["config", &format!("branch.{branch}.merge"), &merge])?;
                }
                info.head_ref.clone()
            };
            Added {
                pr: pr.to_string(),
                path,
                branch,
                existing: false,
            }
        }
    };
    if ctx.json {
        return output::print_json(&added);
    }
    if added.existing {
        eprintln!("{} already has a worktree on {}", added.pr, added.branch);
    }
    println!("{}", added.path.display());
    Ok(())
}

/// `<repo>-pr-<N>` beside the main worktree.
fn default_dir(main: &Path, number: u64) -> PathBuf {
    let name = main
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    main.parent()
        .unwrap_or(main)
        .join(format!("{name}-pr-{number}"))
}

/// The remote whose URL names `repo`, or `origin`.
fn remote_for(root: &Path, repo: &Repo) -> String {
    let remotes = git_in(root, &["remote", "-v"]).unwrap_or_default();
    remote_matching(&remotes, repo).unwrap_or_else(|| "origin".to_string())
}

/// The first remote in `git remote -v` output whose URL names `repo`.
fn remote_matching(remotes: &str, repo: &Repo) -> Option<String> {
    let want = repo.to_string();
    remotes.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let (name, url) = (parts.next()?, parts.next()?);
        repo_of_url(url)
            .filter(|r| r.eq_ignore_ascii_case(&want))
            .map(|_| name.to_string())
    })
}

/// OWNER/REPO from the last two segments of a remote URL, with or without `.git`.
fn repo_of_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let mut tail = url.rsplit([':', '/']);
    let (name, owner) = (tail.next()?, tail.next()?);
    (!name.is_empty() && !owner.is_empty()).then(|| format!("{owner}/{name}"))
}

fn branch_exists(root: &Path, branch: &str) -> bool {
    git_in(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

/// A path relative to the main worktree's parent, where sibling worktrees usually live.
fn display_path(path: &Path, rows: &[Row]) -> String {
    let parent = rows
        .iter()
        .find(|r| r.worktree.main)
        .and_then(|r| r.worktree.path.parent());
    match parent.and_then(|p| path.strip_prefix(p).ok()) {
        Some(rel) => rel.display().to_string(),
        None => path.display().to_string(),
    }
}

/// Uncommitted changes, unpushed commits, and a deleted upstream, or `clean`.
fn describe_local(row: &Row) -> String {
    let w = &row.worktree;
    if w.prunable {
        return "missing".to_string();
    }
    let l = &row.local;
    let mut parts = Vec::new();
    if l.changes > 0 {
        parts.push(format!("{} changed", l.changes));
    }
    if l.ahead > 0 {
        parts.push(format!("{} unpushed", l.ahead));
    }
    if l.behind > 0 {
        parts.push(format!("{} behind", l.behind));
    }
    if l.upstream_gone {
        parts.push("upstream gone".to_string());
    }
    if w.locked {
        parts.push("locked".to_string());
    }
    if parts.is_empty() {
        "clean".to_string()
    } else {
        parts.join(", ")
    }
}

fn render(rows: &[Row]) -> String {
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let w = &r.worktree;
            let branch = match (&w.branch, w.bare) {
                (_, true) => "(bare)".to_string(),
                (Some(b), _) => b.clone(),
                (None, _) => format!("(detached {})", &w.head[..w.head.len().min(8)]),
            };
            let (pr, state, checks) = match &r.pr {
                Some(p) => (
                    format!("#{}", p.number),
                    p.state.to_lowercase(),
                    p.checks
                        .as_deref()
                        .map(checks_word)
                        .unwrap_or("")
                        .to_string(),
                ),
                None => (String::new(), String::new(), String::new()),
            };
            vec![
                display_path(&w.path, rows),
                branch,
                pr,
                state,
                checks,
                describe_local(r),
            ]
        })
        .collect();
    output::table(&table, "") + "\n"
}

/// The rollup state as `pass`, `fail`, or `pending`.
fn checks_word(state: &str) -> &'static str {
    match state {
        "SUCCESS" => "pass",
        "FAILURE" | "ERROR" => "fail",
        _ => "pending",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use mockito::{Matcher, Server};
    use serde_json::json;

    const LIST: &str = "\
worktree /src/repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /src/repo-pr-42
HEAD 2222222222222222222222222222222222222222
branch refs/heads/fix/thing
locked agent

worktree /tmp/scratch
HEAD 3333333333333333333333333333333333333333
detached
prunable gitdir file points to non-existent location
";

    #[test]
    fn parses_porcelain_list() {
        let w = parse_list(LIST);
        assert_eq!(w.len(), 3);
        assert!(w[0].main && !w[1].main);
        assert_eq!(w[0].branch.as_deref(), Some("main"));
        assert_eq!(w[1].branch.as_deref(), Some("fix/thing"));
        assert!(w[1].locked && !w[1].prunable);
        assert_eq!(w[2].branch, None);
        assert!(w[2].prunable);
        assert_eq!(w[2].head, "3333333333333333333333333333333333333333");
    }

    #[test]
    fn repo_root_is_the_main_worktree() {
        // Tests run from the crate root, which is a git checkout.
        assert!(repo_root().unwrap().join("Cargo.toml").exists());
    }

    #[test]
    fn parses_upstream_tracking() {
        let mut l = Local::default();
        parse_track("[ahead 2, behind 1]", &mut l);
        assert_eq!((l.ahead, l.behind, l.upstream_gone), (2, 1, false));
        let mut l = Local::default();
        parse_track("[gone]", &mut l);
        assert!(l.upstream_gone);
        let mut l = Local::default();
        parse_track("", &mut l);
        assert_eq!(l, Local::default());
    }

    fn pr(number: u64, author: &str, state: &str) -> BranchPr {
        BranchPr {
            number,
            title: String::new(),
            url: String::new(),
            state: state.into(),
            author: author.into(),
            head_oid: "2222222222222222222222222222222222222222".into(),
            head_repo: Some("o/r".into()),
            checks: Some("SUCCESS".into()),
        }
    }

    #[test]
    fn picks_only_prs_tied_to_the_branch() {
        let head = "2222222222222222222222222222222222222222";
        let mut stranger = pr(9, "someone", "MERGED");
        stranger.head_repo = Some("someone/r".into());
        stranger.head_oid = "8888888888888888888888888888888888888888".into();
        let mine = pr(4, "Me", "OPEN");
        let other = pr(5, "teammate", "OPEN");
        let prs = vec![stranger.clone(), other, mine.clone()];
        // A fork's PR on the same name is never picked, and the viewer's wins among the rest.
        assert_eq!(pick_pr(&prs, "me", Some("O/R"), "x").unwrap().number, 4);
        assert_eq!(pick_pr(&prs, "nobody", Some("o/r"), "x").unwrap().number, 5);
        assert_eq!(
            pick_pr(std::slice::from_ref(&stranger), "me", Some("o/r"), head),
            None
        );
        // Without a matching upstream, the PR's head has to be the worktree's commit.
        assert_eq!(
            pick_pr(std::slice::from_ref(&mine), "me", None, head)
                .unwrap()
                .number,
            4
        );
        assert_eq!(pick_pr(&[mine], "me", None, "other"), None);
        assert_eq!(pick_pr(&[], "me", Some("o/r"), head), None);
    }

    #[test]
    fn renders_rows() {
        let worktrees = parse_list(LIST);
        let rows: Vec<Row> = worktrees
            .into_iter()
            .enumerate()
            .map(|(i, w)| Row {
                worktree: w,
                local: if i == 1 {
                    Local {
                        changes: 2,
                        ahead: 1,
                        behind: 0,
                        upstream_gone: true,
                    }
                } else {
                    Local::default()
                },
                pr: (i == 1).then(|| pr(42, "me", "MERGED")),
            })
            .collect();
        assert_eq!(
            render(&rows),
            "repo          main                                    clean\n\
             repo-pr-42    fix/thing            #42  merged  pass  2 changed, 1 unpushed, upstream gone, locked\n\
             /tmp/scratch  (detached 33333333)                     missing\n"
        );
    }

    /// A scratch repository with a bare remote, cleaned up on drop.
    pub(super) struct Scratch(pub PathBuf);

    impl Scratch {
        pub fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "gh-arriba-worktree-test-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("repo")).unwrap();
            let s = Scratch(dir);
            s.git(&[
                "init",
                "--quiet",
                "--bare",
                "--initial-branch=main",
                "../remote.git",
            ]);
            s.git(&["init", "--quiet", "--initial-branch=main"]);
            s.git(&["config", "user.name", "Test"]);
            s.git(&["config", "user.email", "test@example.com"]);
            s.git(&["config", "commit.gpgsign", "false"]);
            s.git(&["remote", "add", "origin", "../remote.git"]);
            s.commit("README.md", "hello\n");
            s.git(&["push", "--quiet", "-u", "origin", "main"]);
            s
        }

        pub fn root(&self) -> PathBuf {
            self.0.join("repo")
        }

        pub fn git(&self, args: &[&str]) -> String {
            git_in(&self.root(), args).unwrap()
        }

        pub fn commit(&self, file: &str, text: &str) {
            std::fs::write(self.root().join(file), text).unwrap();
            self.git(&["add", file]);
            self.git(&["commit", "--quiet", "-m", file]);
        }

        /// A worktree beside the repository on a new branch pushed to the remote.
        pub fn branch_worktree(&self, branch: &str) -> PathBuf {
            let path = self.0.join(branch.replace('/', "-"));
            self.git(&[
                "worktree",
                "add",
                "--quiet",
                "-b",
                branch,
                path.to_str().unwrap(),
            ]);
            git_in(&path, &["push", "--quiet", "-u", "origin", branch]).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(super) fn ctx(server: &Server, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    /// Answer the viewer query and the branch lookup with `prs` for branch `b0`.
    pub(super) fn mock_lookup(server: &mut Server, prs: serde_json::Value) {
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("viewer".into()))
            .with_body(r#"{"data": {"viewer": {"login": "me"}}}"#)
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("headRefName".into()))
            .with_body(json!({"data": {"repository": {"b0": {"nodes": prs}}}}).to_string())
            .create();
    }

    #[test]
    fn rows_join_git_state_and_prs() {
        let s = Scratch::new("rows");
        let path = s.branch_worktree("fix/thing");
        std::fs::write(path.join("dirty.txt"), "x").unwrap();
        let head = git_in(&path, &["rev-parse", "HEAD"]).unwrap();
        let mut server = Server::new();
        // Branches are looked up sorted, so fix/thing is b0 ahead of main.
        mock_lookup(
            &mut server,
            json!([{
                "number": 42, "state": "OPEN", "headRefOid": head,
                "author": {"login": "me"}, "commits": {"nodes": []}
            }]),
        );
        let rows = rows(&ctx(&server, true), &s.root()).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].worktree.main);
        assert_eq!(rows[0].local, Local::default());
        let wt = &rows[1];
        assert_eq!(wt.worktree.branch.as_deref(), Some("fix/thing"));
        assert_eq!(wt.local.changes, 1);
        assert_eq!(wt.pr.as_ref().unwrap().number, 42);
        assert!(render(&rows).contains("#42"));
    }

    fn row(branch: &str, pr_state: Option<&str>, local: Local) -> Row {
        Row {
            worktree: Worktree {
                path: PathBuf::from(format!("/src/{branch}")),
                head: "2222222222222222222222222222222222222222".into(),
                branch: Some(branch.into()),
                main: false,
                bare: false,
                locked: false,
                prunable: false,
            },
            local,
            pr: pr_state.map(|s| pr(42, "me", s)),
        }
    }

    #[test]
    fn plan_prune_picks_finished_and_keeps_unsaved_work() {
        let dirty = Local {
            changes: 3,
            ..Local::default()
        };
        let ahead = Local {
            ahead: 2,
            ..Local::default()
        };
        let gone = Local {
            upstream_gone: true,
            ..Local::default()
        };
        let mut theirs = row("theirs", Some("MERGED"), Local::default());
        theirs.pr.as_mut().unwrap().author = "teammate".into();
        let mut moved = row("moved", Some("MERGED"), Local::default());
        moved.worktree.head = "9999999999999999999999999999999999999999".into();
        let mut locked = row("locked", Some("CLOSED"), Local::default());
        locked.worktree.locked = true;
        let mut main = row("main", Some("MERGED"), Local::default());
        main.worktree.main = true;
        let rows = vec![
            main,
            row("merged", Some("MERGED"), Local::default()),
            row("closed", Some("CLOSED"), Local::default()),
            row("open", Some("OPEN"), Local::default()),
            row("dirty", Some("MERGED"), dirty),
            row("ahead", Some("MERGED"), ahead),
            moved,
            locked,
            row("gone", None, gone),
            row("nopr", None, Local::default()),
            theirs,
        ];
        let summary = |plan: Vec<Candidate>| -> Vec<(String, String, Option<String>)> {
            plan.into_iter()
                .map(|c| (c.branch, c.reason, c.kept))
                .collect()
        };
        let s = |v: &str| v.to_string();
        assert_eq!(
            summary(plan_prune(&rows, "Me", false, |_| false)),
            vec![
                (s("merged"), s("PR #42 merged"), None),
                (s("closed"), s("PR #42 closed"), None),
                (
                    s("dirty"),
                    s("PR #42 merged"),
                    Some(s("3 uncommitted changes"))
                ),
                (
                    s("ahead"),
                    s("PR #42 merged"),
                    Some(s("2 unpushed commits"))
                ),
                (
                    s("moved"),
                    s("PR #42 merged"),
                    Some(s("commits on no remote branch"))
                ),
                (s("locked"), s("PR #42 closed"), Some(s("locked"))),
            ]
        );
        // A remote branch holding the commit, and --force, each let more go, but never another
        // person's worktree or one with no PR.
        let plan = summary(plan_prune(&rows, "me", true, |_| true));
        assert!(plan.contains(&(s("moved"), s("PR #42 merged"), None)));
        assert!(plan.contains(&(s("dirty"), s("PR #42 merged"), None)));
        assert!(plan.contains(&(s("locked"), s("PR #42 closed"), Some(s("locked")))));
        assert!(
            !plan
                .iter()
                .any(|(b, _, _)| ["open", "nopr", "gone", "theirs", "main"].contains(&b.as_str()))
        );
    }

    fn prune_args(dry_run: bool) -> WorktreePruneArgs {
        WorktreePruneArgs {
            dry_run,
            force: false,
        }
    }

    #[test]
    fn prune_removes_merged_worktrees_and_their_branches() {
        let s = Scratch::new("prune");
        let path = s.branch_worktree("fix/done");
        let head = git_in(&path, &["rev-parse", "HEAD"]).unwrap();
        let mut server = Server::new();
        mock_lookup(
            &mut server,
            json!([{
                "number": 42, "state": "MERGED", "headRefOid": head,
                "author": {"login": "me"}, "commits": {"nodes": []}
            }]),
        );
        // A dry run changes nothing.
        prune_at(&ctx(&server, true), &s.root(), &prune_args(true)).unwrap();
        assert!(path.exists());
        prune_at(&ctx(&server, false), &s.root(), &prune_args(false)).unwrap();
        assert!(!path.exists());
        assert!(s.git(&["branch", "--list", "fix/done"]).is_empty());
        assert!(!s.git(&["worktree", "list"]).contains("fix/done"));
    }

    #[test]
    fn prune_keeps_worktrees_with_changes() {
        let s = Scratch::new("keep");
        let path = s.branch_worktree("fix/wip");
        std::fs::write(path.join("notes.md"), "todo").unwrap();
        let head = git_in(&path, &["rev-parse", "HEAD"]).unwrap();
        let mut server = Server::new();
        mock_lookup(
            &mut server,
            json!([{
                "number": 7, "state": "CLOSED", "headRefOid": head,
                "author": {"login": "me"}, "commits": {"nodes": []}
            }]),
        );
        prune_at(&ctx(&server, true), &s.root(), &prune_args(false)).unwrap();
        assert!(path.join("notes.md").exists());
        let forced = WorktreePruneArgs {
            force: true,
            ..prune_args(false)
        };
        prune_at(&ctx(&server, true), &s.root(), &forced).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn remotes_match_by_owner_and_name() {
        let remotes = "\
fork\tgit@github.com:me/r.git (fetch)
fork\tgit@github.com:me/r.git (push)
upstream\thttps://github.com/O/R (fetch)
upstream\thttps://github.com/O/R (push)
";
        let repo = Repo::parse("o/r").unwrap();
        assert_eq!(remote_matching(remotes, &repo).as_deref(), Some("upstream"));
        let mine = Repo::parse("me/r").unwrap();
        assert_eq!(remote_matching(remotes, &mine).as_deref(), Some("fork"));
        assert_eq!(remote_matching(remotes, &Repo::parse("x/y").unwrap()), None);
        assert_eq!(remote_matching("bad", &repo), None);
        assert_eq!(
            repo_of_url("https://github.com/o/r.git/").as_deref(),
            Some("o/r")
        );
        assert_eq!(repo_of_url("git@github.com:o/r").as_deref(), Some("o/r"));
        assert_eq!(repo_of_url("r"), None);
    }

    #[test]
    fn default_dir_sits_beside_the_main_worktree() {
        assert_eq!(
            default_dir(Path::new("/src/gh-arriba"), 42),
            PathBuf::from("/src/gh-arriba-pr-42")
        );
    }

    fn mock_pr(server: &mut Server, head: &str, oid: &str, fork: bool) {
        mock_pr_by(server, head, oid, fork, "me");
    }

    fn mock_pr_by(server: &mut Server, head: &str, oid: &str, fork: bool, author: &str) {
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("viewer".into()))
            .with_body(r#"{"data": {"viewer": {"login": "me"}}}"#)
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("isCrossRepository".into()))
            .with_body(
                json!({"data": {"repository": {"pullRequest": {
                    "id": "PR_7", "number": 7, "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z", "baseRefName": "main",
                    "headRefName": head, "headRefOid": oid, "isCrossRepository": fork,
                    "author": {"login": author},
                    "headRepository": {"nameWithOwner": if fork { "fork/r" } else { "o/r" }}
                }}}})
                .to_string(),
            )
            .create();
    }

    fn add_args(dir: Option<PathBuf>) -> WorktreeAddArgs {
        WorktreeAddArgs {
            pr: "7".into(),
            dir,
        }
    }

    #[test]
    fn add_checks_out_a_branch_from_the_remote() {
        let s = Scratch::new("add");
        s.git(&["checkout", "--quiet", "-b", "fix/x"]);
        s.commit("fix.txt", "fix\n");
        s.git(&["push", "--quiet", "origin", "fix/x"]);
        let oid = s.git(&["rev-parse", "HEAD"]);
        s.git(&["checkout", "--quiet", "main"]);
        s.git(&["branch", "--quiet", "-D", "fix/x"]);
        let mut server = Server::new();
        mock_pr(&mut server, "fix/x", &oid, false);

        let ctx = ctx(&server, true);
        add_at(&ctx, &s.root(), &add_args(None)).unwrap();
        let path = s.0.join("repo-pr-7");
        assert_eq!(git_in(&path, &["rev-parse", "HEAD"]).unwrap(), oid);
        assert_eq!(
            git_in(&path, &["rev-parse", "--abbrev-ref", "@{u}"]).unwrap(),
            "origin/fix/x"
        );
        // A second call finds the worktree instead of making another.
        add_at(&ctx, &s.root(), &add_args(Some(s.0.join("other")))).unwrap();
        assert!(!s.0.join("other").exists());
    }

    #[test]
    fn add_fetches_a_fork_pr_by_number() {
        let s = Scratch::new("fork");
        s.git(&["checkout", "--quiet", "-b", "feature"]);
        s.commit("fork.txt", "fork\n");
        let oid = s.git(&["rev-parse", "HEAD"]);
        s.git(&["push", "--quiet", "origin", "HEAD:refs/pull/7/head"]);
        s.git(&["checkout", "--quiet", "main"]);
        // The fork's branch name is taken locally, so it lands on pr-7.
        let mut server = Server::new();
        mock_pr(&mut server, "feature", &oid, true);
        let dir = s.0.join("fork-wt");
        add_at(
            &ctx(&server, false),
            &s.root(),
            &add_args(Some(dir.clone())),
        )
        .unwrap();
        assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]).unwrap(), oid);
        assert_eq!(
            git_in(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(),
            "pr-7"
        );
    }

    #[test]
    fn add_refuses_other_peoples_prs() {
        let s = Scratch::new("theirs");
        let mut server = Server::new();
        mock_pr_by(&mut server, "fix/q", "abc", false, "teammate");
        let err = add_at(&ctx(&server, true), &s.root(), &add_args(None)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "o/r#7 is by teammate, and worktree add only checks out your own PRs"
        );
        assert!(!s.0.join("repo-pr-7").exists());
    }

    #[test]
    fn add_refuses_an_existing_directory() {
        let s = Scratch::new("exists");
        let mut server = Server::new();
        mock_pr(&mut server, "fix/y", "abc", false);
        let err = add_at(&ctx(&server, true), &s.root(), &add_args(Some(s.root()))).unwrap_err();
        assert!(err.to_string().ends_with("already exists"));
    }

    #[test]
    fn prune_ignores_another_forks_pr_on_the_same_branch_name() {
        let s = Scratch::new("stranger");
        let path = s.branch_worktree("develop");
        let mut server = Server::new();
        mock_lookup(
            &mut server,
            json!([{
                "number": 3, "state": "MERGED", "author": {"login": "someone"},
                "headRefOid": "8888888888888888888888888888888888888888",
                "headRepository": {"nameWithOwner": "someone/r"}, "commits": {"nodes": []}
            }]),
        );
        let ctx = ctx(&server, true);
        let rows = rows(&ctx, &s.root()).unwrap();
        assert!(rows.iter().all(|r| r.pr.is_none()));
        let forced = WorktreePruneArgs {
            force: true,
            ..prune_args(false)
        };
        prune_at(&ctx, &s.root(), &forced).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn prune_finds_a_fork_worktree_by_its_recorded_pr() {
        let s = Scratch::new("forkprune");
        s.git(&["checkout", "--quiet", "-b", "feature"]);
        s.commit("fork.txt", "fork\n");
        let oid = s.git(&["rev-parse", "HEAD"]);
        s.git(&["push", "--quiet", "origin", "HEAD:refs/pull/7/head"]);
        s.git(&["checkout", "--quiet", "main"]);
        let mut server = Server::new();
        mock_pr(&mut server, "feature", &oid, true);
        let dir = s.0.join("fork-wt");
        let ctx = ctx(&server, true);
        add_at(&ctx, &s.root(), &add_args(Some(dir.clone()))).unwrap();
        assert_eq!(s.git(&["config", "--get", "branch.pr-7.gh-arriba-pr"]), "7");

        // Only the main worktree's branch is looked up by name; pr-7 is looked up by number.
        mock_lookup(&mut server, json!([]));
        let by_number = server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex(
                r"pullRequest\(number: \$number\) \{ number".into(),
            ))
            .with_body(
                json!({"data": {"repository": {"pullRequest": {
                    "number": 7, "state": "MERGED", "headRefOid": oid,
                    "author": {"login": "me"},
                    "headRepository": {"nameWithOwner": "fork/r"}, "commits": {"nodes": []}
                }}}})
                .to_string(),
            )
            .create();
        prune_at(&ctx, &s.root(), &prune_args(false)).unwrap();
        assert!(!dir.exists());
        assert!(s.git(&["branch", "--list", "pr-7"]).is_empty());
        by_number.assert();
    }

    #[test]
    fn add_works_in_a_single_branch_clone() {
        let s = Scratch::new("single");
        s.git(&["checkout", "--quiet", "-b", "fix/z"]);
        s.commit("z.txt", "z\n");
        s.git(&["push", "--quiet", "origin", "fix/z"]);
        let oid = s.git(&["rev-parse", "HEAD"]);
        s.git(&["checkout", "--quiet", "main"]);
        s.git(&["branch", "--quiet", "-D", "fix/z"]);
        s.git(&["update-ref", "-d", "refs/remotes/origin/fix/z"]);
        // As `git clone --single-branch` leaves it, the refspec only covers main.
        s.git(&[
            "config",
            "remote.origin.fetch",
            "+refs/heads/main:refs/remotes/origin/main",
        ]);
        let mut server = Server::new();
        mock_pr(&mut server, "fix/z", &oid, false);
        let dir = s.0.join("single-wt");
        add_at(&ctx(&server, true), &s.root(), &add_args(Some(dir.clone()))).unwrap();
        assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]).unwrap(), oid);
    }
}

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::api::pulls::{self, PrInfo, fetch_pr};
use crate::api::{PrRef, Repo};
use crate::cli::RebaseArgs;
use crate::context::{Ctx, git, git_in};
use crate::output;

#[derive(Debug, Serialize)]
struct Outcome {
    pr: String,
    base: String,
    head: String,
    /// "local", "remote", or "check".
    method: &'static str,
    /// "rebased", "up-to-date", "clean", or "conflicts".
    result: &'static str,
    conflicts: Vec<String>,
    new_head: Option<String>,
    commented: bool,
}

pub fn run(ctx: &Ctx, args: RebaseArgs) -> Result<()> {
    let pr = ctx.pr(args.pr.as_deref())?;
    let info = fetch_pr(&ctx.client, &pr)?;
    if info.state != "OPEN" {
        bail!("{pr} is {}", info.state.to_lowercase());
    }

    let mut outcome = if args.remote {
        remote(ctx, &pr, &info)?
    } else {
        local(&pr, &info, args.check)?
    };

    if outcome.result == "rebased" && !args.no_comment {
        pulls::add_issue_comment(&ctx.client, &pr, "Rebased.")?;
        outcome.commented = true;
    }

    if ctx.json {
        output::print_json(&outcome)?;
    } else {
        print_outcome(&outcome);
    }
    if outcome.result == "conflicts" {
        bail!(
            "Rebase onto {} has conflicts, resolve them locally",
            outcome.base
        );
    }
    Ok(())
}

fn print_outcome(o: &Outcome) {
    match o.result {
        "up-to-date" => println!("{} is already up to date with {}", o.pr, o.base),
        "clean" => println!("{} rebases cleanly onto {}", o.pr, o.base),
        "conflicts" => {
            println!("{} conflicts with {} in:", o.pr, o.base);
            for f in &o.conflicts {
                println!("  {f}");
            }
        }
        _ => {
            let short = o
                .new_head
                .as_deref()
                .map(|h| &h[..h.len().min(12)])
                .unwrap_or("");
            let how = if o.method == "remote" {
                "server-side, commits are unsigned"
            } else {
                "locally"
            };
            println!(
                "Rebased {} onto {} {how}, head is now {short}",
                o.pr, o.base
            );
            if o.commented {
                println!("Commented \"Rebased.\" on {}", o.pr);
            }
        }
    }
}

fn outcome(pr: &PrRef, info: &PrInfo, method: &'static str, result: &'static str) -> Outcome {
    Outcome {
        pr: pr.to_string(),
        base: info.base_ref.clone(),
        head: info.head_ref.clone(),
        method,
        result,
        conflicts: Vec::new(),
        new_head: None,
        commented: false,
    }
}

/// Rebase server-side, which leaves the commits unsigned.
fn remote(ctx: &Ctx, pr: &PrRef, info: &PrInfo) -> Result<Outcome> {
    let behind = pulls::behind_by(&ctx.client, &pr.repo, &info.base_ref, &info.head_oid)?;
    if behind == Some(0) {
        return Ok(outcome(pr, info, "remote", "up-to-date"));
    }
    let mut o = outcome(pr, info, "remote", "rebased");
    o.new_head = pulls::rebase_branch(&ctx.client, &info.node_id, &info.head_oid)?;
    Ok(o)
}

/// Rebase with the local git config, so commits keep the user's signature.
fn local(pr: &PrRef, info: &PrInfo, check: bool) -> Result<Outcome> {
    let root = PathBuf::from(
        git(&["rev-parse", "--show-toplevel"])
            .with_context(|| format!("Run inside a clone of {}", pr.repo))?,
    );
    let remotes = git_in(&root, &["remote", "-v"])?;
    let remote = find_remote(&remotes, &pr.repo, "(fetch)").with_context(|| {
        format!(
            "No git remote points at {}. Run inside a clone of it.",
            pr.repo
        )
    })?;
    // --check pushes nothing, so it needs no remote for the fork.
    let push = if check {
        remote.clone()
    } else {
        push_remote(&remotes, pr, info)?
    };

    // The pull ref serves the head for forks too, which --check relies on.
    let base_ref = format!("refs/remotes/{remote}/{}", info.base_ref);
    git_in(
        &root,
        &[
            "fetch",
            "--quiet",
            &remote,
            &format!("+refs/heads/{}:{base_ref}", info.base_ref),
            &format!("+refs/pull/{}/head", pr.number),
        ],
    )?;
    if !git_ok(
        &root,
        &["cat-file", "-e", &format!("{}^{{commit}}", info.head_oid)],
    ) {
        bail!(
            "The PR head {} is not in the fetched refs. The PR may have changed, run again.",
            info.head_oid
        );
    }
    let base_oid = git_in(&root, &["rev-parse", &base_ref])?;
    if git_ok(
        &root,
        &["merge-base", "--is-ancestor", &base_oid, &info.head_oid],
    ) {
        return Ok(outcome(
            pr,
            info,
            if check { "check" } else { "local" },
            "up-to-date",
        ));
    }

    let current = git_in(&root, &["branch", "--show-current"]).unwrap_or_default();
    if !check && checkout_is_head(&root, &current, &push, info) {
        return in_place(pr, info, &root, &push, &base_oid);
    }
    in_worktree(pr, info, &root, &push, &base_oid, check)
}

/// Whether the checked-out branch is the PR's head. Branch names repeat across forks, so a fork's
/// branch must also track the remote the head is pushed to.
fn checkout_is_head(root: &Path, current: &str, push: &str, info: &PrInfo) -> bool {
    current == info.head_ref
        && (!info.cross_repository
            || git_in(root, &["config", &format!("branch.{current}.remote")])
                .is_ok_and(|r| r == push))
}

/// The remote to push the head to, chosen by push URL since that is where git sends the push.
/// A fork's head goes to the fork, while the base is still fetched from the PR's repo.
fn push_remote(remotes: &str, pr: &PrRef, info: &PrInfo) -> Result<String> {
    if !info.cross_repository {
        return find_remote(remotes, &pr.repo, "(push)").with_context(|| {
            format!(
                "No git remote pushes to {}. Add one, or rebase with --remote.",
                pr.repo
            )
        });
    }
    let fork = info
        .head_repo
        .as_deref()
        .with_context(|| format!("{pr} comes from a fork that no longer exists"))?;
    find_remote(remotes, &Repo::parse(fork)?, "(push)").with_context(|| {
        format!("{pr} comes from {fork}, and no git remote pushes to it. Add one, or rebase with --remote.")
    })
}

/// Rebase the checked-out head branch itself.
fn in_place(
    pr: &PrRef,
    info: &PrInfo,
    root: &Path,
    remote: &str,
    base_oid: &str,
) -> Result<Outcome> {
    let dirty = git_in(root, &["status", "--porcelain", "--untracked-files=no"])?;
    if !dirty.is_empty() {
        bail!(
            "Commit or stash your changes before rebasing {}",
            info.head_ref
        );
    }
    // Pushing a branch that lacks the PR head would drop commits others pushed.
    if !git_ok(
        root,
        &["merge-base", "--is-ancestor", &info.head_oid, "HEAD"],
    ) {
        bail!(
            "Your {} branch is missing commits that are on {pr}. Pull them first.",
            info.head_ref
        );
    }
    let extra = git_in(
        root,
        &["rev-list", "--count", &format!("{}..HEAD", info.head_oid)],
    )?;
    if extra != "0" {
        eprintln!("note: {extra} local commit(s) not on the PR will be pushed too");
    }

    let conflicts = rebase_at(root, base_oid)?;
    if !conflicts.is_empty() {
        let mut o = outcome(pr, info, "local", "conflicts");
        o.conflicts = conflicts;
        return Ok(o);
    }
    let mut o = outcome(pr, info, "local", "rebased");
    o.new_head = Some(push_from(root, remote, info)?);
    Ok(o)
}

/// Removes the temporary worktree however the rebase ends.
struct TempWorktree<'a> {
    root: &'a Path,
    dir: PathBuf,
}

impl Drop for TempWorktree<'_> {
    fn drop(&mut self) {
        let dir = self.dir.to_string_lossy();
        let _ = git_in(self.root, &["worktree", "remove", "--force", &dir]);
    }
}

/// Rebase a detached copy of the PR head so the user's checkout is left alone.
fn in_worktree(
    pr: &PrRef,
    info: &PrInfo,
    root: &Path,
    remote: &str,
    base_oid: &str,
    check: bool,
) -> Result<Outcome> {
    let dir = std::env::temp_dir().join(format!(
        "gh-arriba-rebase-{}-{}",
        info.number,
        std::process::id()
    ));
    git_in(
        root,
        &[
            "worktree",
            "add",
            "--detach",
            "--quiet",
            &dir.to_string_lossy(),
            &info.head_oid,
        ],
    )?;
    let worktree = TempWorktree { root, dir };

    let method = if check { "check" } else { "local" };
    let conflicts = rebase_at(&worktree.dir, base_oid)?;
    if !conflicts.is_empty() {
        let mut o = outcome(pr, info, method, "conflicts");
        o.conflicts = conflicts;
        return Ok(o);
    }
    if check {
        return Ok(outcome(pr, info, method, "clean"));
    }
    let mut o = outcome(pr, info, method, "rebased");
    o.new_head = Some(push_from(&worktree.dir, remote, info)?);
    Ok(o)
}

/// Rebase HEAD in `dir` onto `onto`. Returns the conflicting files, after aborting, if any.
fn rebase_at(dir: &Path, onto: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["rebase", "--quiet", onto])
        .output()
        .context("Failed to run git rebase")?;
    if output.status.success() {
        return Ok(Vec::new());
    }
    let conflicts: Vec<String> = git_in(dir, &["diff", "--name-only", "--diff-filter=U"])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let _ = git_in(dir, &["rebase", "--abort"]);
    if conflicts.is_empty() {
        // Not a conflict, such as a failed commit signature.
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git rebase failed: {}", stderr.trim());
    }
    Ok(conflicts)
}

/// Force-push HEAD to the PR branch, leased on the head we rebased so newer pushes are kept.
fn push_from(dir: &Path, remote: &str, info: &PrInfo) -> Result<String> {
    let new_head = git_in(dir, &["rev-parse", "HEAD"])?;
    git_in(
        dir,
        &[
            "push",
            "--quiet",
            &format!(
                "--force-with-lease=refs/heads/{}:{}",
                info.head_ref, info.head_oid
            ),
            remote,
            &format!("HEAD:refs/heads/{}", info.head_ref),
        ],
    )
    .context("Push rejected. Someone may have pushed to the PR since it was fetched")?;
    Ok(new_head)
}

/// Name of the remote whose `kind` URL, "(fetch)" or "(push)", points at `repo`, preferring origin.
fn find_remote(remotes: &str, repo: &Repo, kind: &str) -> Option<String> {
    let matching: Vec<&str> = remotes
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let (name, url, line_kind) = (parts.next()?, parts.next()?, parts.next()?);
            let parsed = Repo::parse(url).ok()?;
            // GitHub owner and repo names are case-insensitive.
            let same = parsed.owner.eq_ignore_ascii_case(&repo.owner)
                && parsed.name.eq_ignore_ascii_case(&repo.name);
            (line_kind == kind && same).then_some(name)
        })
        .collect();
    matching
        .iter()
        .find(|n| **n == "origin")
        .or(matching.first())
        .map(|n| n.to_string())
}

/// Run git in `dir` and report only whether it succeeded.
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use mockito::{Matcher, Mock, Server};
    use serde_json::{Value, json};

    fn ctx(server: &Server, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    fn args(remote: bool, no_comment: bool) -> RebaseArgs {
        RebaseArgs {
            pr: Some("7".into()),
            remote,
            check: false,
            no_comment,
        }
    }

    fn mock_pr(server: &mut Server, state: &str) -> Mock {
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("isCrossRepository".into()))
            .with_body(
                json!({"data": {"repository": {"pullRequest": {
                    "id": "PR_7", "number": 7, "state": state,
                    "createdAt": "2026-01-01T00:00:00Z", "baseRefName": "main",
                    "headRefName": "fix", "headRefOid": "abc123"
                }}}})
                .to_string(),
            )
            .create()
    }

    fn mock_compare(server: &mut Server, behind: u64) -> Mock {
        server
            .mock("GET", "/repos/o/r/compare/main...abc123")
            .with_body(json!({"behind_by": behind}).to_string())
            .create()
    }

    fn mock_rebase(server: &mut Server) -> Mock {
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({"variables": {"id": "PR_7", "oid": "abc123"}}),
            ))
            .with_body(
                r#"{"data": {"updatePullRequestBranch": {"pullRequest": {
                    "headRefOid": "def4567890abcdef"}}}}"#,
            )
            .create()
    }

    fn mock_comment(server: &mut Server) -> Mock {
        server
            .mock("POST", "/repos/o/r/issues/7/comments")
            .match_body(Matcher::Json(json!({"body": "Rebased."})))
            .with_body("{}")
            .create()
    }

    #[test]
    fn closed_prs_are_not_rebased() {
        let mut server = Server::new();
        mock_pr(&mut server, "MERGED");
        let compare = mock_compare(&mut server, 1).expect(0);
        let err = run(&ctx(&server, true), args(true, false))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "o/r#7 is merged");
        compare.assert();
    }

    #[test]
    fn remote_skips_an_up_to_date_branch() {
        let mut server = Server::new();
        mock_pr(&mut server, "OPEN");
        let compare = mock_compare(&mut server, 0).expect(2);
        let rebase = mock_rebase(&mut server).expect(0);
        let comment = mock_comment(&mut server).expect(0);
        run(&ctx(&server, true), args(true, false)).unwrap();
        run(&ctx(&server, false), args(true, false)).unwrap();
        compare.assert();
        rebase.assert();
        comment.assert();
    }

    #[test]
    fn remote_rebases_and_comments() {
        let mut server = Server::new();
        mock_pr(&mut server, "OPEN");
        mock_compare(&mut server, 3);
        let rebase = mock_rebase(&mut server);
        let comment = mock_comment(&mut server);
        run(&ctx(&server, false), args(true, false)).unwrap();
        rebase.assert();
        comment.assert();
    }

    #[test]
    fn remote_rebase_without_a_comment() {
        let mut server = Server::new();
        mock_pr(&mut server, "OPEN");
        mock_compare(&mut server, 3);
        let rebase = mock_rebase(&mut server);
        let comment = mock_comment(&mut server).expect(0);
        run(&ctx(&server, true), args(true, true)).unwrap();
        rebase.assert();
        comment.assert();
    }

    #[test]
    fn outcomes_serialize_for_scripts() {
        let info: PrInfo = serde_json::from_value(json!({
            "number": 7, "createdAt": "2026-01-01T00:00:00Z",
            "baseRefName": "main", "headRefName": "fix"
        }))
        .unwrap();
        let pr = PrRef {
            repo: Repo::parse("o/r").unwrap(),
            number: 7,
        };
        let mut o = outcome(&pr, &info, "check", "conflicts");
        o.conflicts = vec!["a.c".into()];
        print_outcome(&o);
        let v: Value = serde_json::to_value(&o).unwrap();
        assert_eq!(
            v,
            json!({
                "pr": "o/r#7", "base": "main", "head": "fix", "method": "check",
                "result": "conflicts", "conflicts": ["a.c"], "new_head": null,
                "commented": false
            })
        );
    }

    /// A scratch repository with `main` holding `a.txt` and branch `topic` changing `file`.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "gh-arriba-rebase-test-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let s = Scratch(dir);
            s.git(&["init", "--quiet", "--initial-branch=main"]);
            s.git(&["config", "user.name", "Test"]);
            s.git(&["config", "user.email", "test@example.com"]);
            s.git(&["config", "commit.gpgsign", "false"]);
            s.commit("a.txt", "one\n");
            s
        }

        fn git(&self, args: &[&str]) -> String {
            git_in(&self.0, args).unwrap()
        }

        fn commit(&self, file: &str, text: &str) {
            std::fs::write(self.0.join(file), text).unwrap();
            self.git(&["add", file]);
            self.git(&["commit", "--quiet", "-m", file]);
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rebase_at_replays_clean_changes() {
        let s = Scratch::new("clean");
        s.git(&["checkout", "--quiet", "-b", "topic"]);
        s.commit("b.txt", "topic\n");
        s.git(&["checkout", "--quiet", "main"]);
        s.commit("c.txt", "main\n");
        let main = s.git(&["rev-parse", "main"]);
        s.git(&["checkout", "--quiet", "topic"]);
        assert!(rebase_at(&s.0, &main).unwrap().is_empty());
        assert!(git_ok(
            &s.0,
            &["merge-base", "--is-ancestor", &main, "HEAD"]
        ));
    }

    #[test]
    fn rebase_at_reports_conflicts_and_aborts() {
        let s = Scratch::new("conflict");
        s.git(&["checkout", "--quiet", "-b", "topic"]);
        s.commit("a.txt", "topic\n");
        s.git(&["checkout", "--quiet", "main"]);
        s.commit("a.txt", "main\n");
        let main = s.git(&["rev-parse", "main"]);
        s.git(&["checkout", "--quiet", "topic"]);
        let before = s.git(&["rev-parse", "HEAD"]);
        assert_eq!(rebase_at(&s.0, &main).unwrap(), vec!["a.txt"]);
        // Aborted, so the branch is as it was.
        assert_eq!(s.git(&["rev-parse", "HEAD"]), before);
        assert!(!git_ok(
            &s.0,
            &["merge-base", "--is-ancestor", &main, "HEAD"]
        ));
    }

    #[test]
    fn rebase_at_fails_without_conflicts_on_a_bad_target() {
        let s = Scratch::new("bad");
        let err = rebase_at(&s.0, "no-such-ref").unwrap_err().to_string();
        assert!(err.starts_with("git rebase failed: "), "{err}");
    }

    #[test]
    fn remote_matched_by_fetch_url() {
        let repo = Repo::parse("zlib-ng/zlib-ng").unwrap();
        let remotes = "\
fork\tgit@github.com:nmoinvaz/zlib-ng.git (fetch)
fork\tgit@github.com:nmoinvaz/zlib-ng.git (push)
upstream\thttps://github.com/Zlib-NG/zlib-ng (fetch)
upstream\thttps://github.com/Zlib-NG/zlib-ng (push)";
        assert_eq!(
            find_remote(remotes, &repo, "(fetch)").as_deref(),
            Some("upstream")
        );
        assert_eq!(find_remote("", &repo, "(fetch)"), None);
    }

    #[test]
    fn origin_preferred_when_several_match() {
        let repo = Repo::parse("a/b").unwrap();
        let remotes = "\
mirror\thttps://github.com/a/b (fetch)
origin\tgit@github.com:a/b.git (fetch)";
        assert_eq!(
            find_remote(remotes, &repo, "(fetch)").as_deref(),
            Some("origin")
        );
    }

    fn zlib_ng_pr() -> (PrRef, PrInfo) {
        let pr = PrRef {
            repo: Repo::parse("zlib-ng/zlib-ng").unwrap(),
            number: 7,
        };
        let info: PrInfo = serde_json::from_value(json!({
            "number": 7, "createdAt": "2026-01-01T00:00:00Z",
            "baseRefName": "develop", "headRefName": "fix"
        }))
        .unwrap();
        (pr, info)
    }

    #[test]
    fn fork_prs_push_to_the_fork_remote() {
        let remotes = "\
origin\tgit@github.com:nmoinvaz/zlib-ng (fetch)
origin\tgit@github.com:nmoinvaz/zlib-ng (push)
upstream\tgit@github.com:zlib-ng/zlib-ng (fetch)
upstream\tgit@github.com:zlib-ng/zlib-ng (push)";
        let (pr, mut info) = zlib_ng_pr();
        assert_eq!(push_remote(remotes, &pr, &info).unwrap(), "upstream");

        info.cross_repository = true;
        info.head_repo = Some("NMoinvaz/zlib-ng".into());
        assert_eq!(push_remote(remotes, &pr, &info).unwrap(), "origin");

        info.head_repo = Some("someone/zlib-ng".into());
        let err = push_remote(remotes, &pr, &info).unwrap_err().to_string();
        assert_eq!(
            err,
            "zlib-ng/zlib-ng#7 comes from someone/zlib-ng, and no git remote pushes to it. \
             Add one, or rebase with --remote."
        );

        info.head_repo = None;
        let err = push_remote(remotes, &pr, &info).unwrap_err().to_string();
        assert_eq!(
            err,
            "zlib-ng/zlib-ng#7 comes from a fork that no longer exists"
        );
    }

    #[test]
    fn push_url_alone_does_not_match() {
        let repo = Repo::parse("a/b").unwrap();
        let remotes = "\
origin\thttps://github.com/x/y (fetch)
origin\thttps://github.com/a/b (push)";
        assert_eq!(find_remote(remotes, &repo, "(fetch)"), None);
    }

    #[test]
    fn push_remote_follows_push_urls() {
        // upstream fetches the PR's repo but pushes to the fork, so neither push may pick it.
        let remotes = "\
upstream\tgit@github.com:zlib-ng/zlib-ng (fetch)
upstream\tgit@github.com:someone/zlib-ng (push)";
        let (pr, mut info) = zlib_ng_pr();
        let err = push_remote(remotes, &pr, &info).unwrap_err().to_string();
        assert_eq!(
            err,
            "No git remote pushes to zlib-ng/zlib-ng. Add one, or rebase with --remote."
        );

        info.cross_repository = true;
        info.head_repo = Some("someone/zlib-ng".into());
        assert_eq!(push_remote(remotes, &pr, &info).unwrap(), "upstream");
    }

    #[test]
    fn fork_checkouts_must_track_the_push_remote() {
        let s = Scratch::new("tracking");
        s.git(&["checkout", "--quiet", "-b", "fix"]);
        let (_, mut info) = zlib_ng_pr();
        assert!(checkout_is_head(&s.0, "fix", "origin", &info));
        assert!(!checkout_is_head(&s.0, "main", "origin", &info));

        // A fork's branch of the same name counts only when it tracks the push remote.
        info.cross_repository = true;
        assert!(!checkout_is_head(&s.0, "fix", "origin", &info));
        s.git(&["config", "branch.fix.remote", "other"]);
        assert!(!checkout_is_head(&s.0, "fix", "origin", &info));
        s.git(&["config", "branch.fix.remote", "origin"]);
        assert!(checkout_is_head(&s.0, "fix", "origin", &info));
    }

    /// A scratch repo with `topic` branched from `main`, each with one commit touching `file`,
    /// plus a bare remote holding `topic`. Returns the repo, the remote, the base oid, and the PR
    /// info. Each test takes its own PR number, which names the temporary worktree.
    fn diverged(tag: &str, number: u64, topic_file: &str) -> (Scratch, Scratch, String, PrInfo) {
        let s = Scratch::new(tag);
        s.git(&["checkout", "--quiet", "-b", "topic"]);
        s.commit(topic_file, "topic\n");
        let head = s.git(&["rev-parse", "HEAD"]);
        s.git(&["checkout", "--quiet", "main"]);
        s.commit("a.txt", "main\n");
        let base = s.git(&["rev-parse", "main"]);

        let remote = Scratch(s.0.with_extension("remote"));
        let _ = std::fs::remove_dir_all(&remote.0);
        git_in(
            &s.0,
            &["init", "--quiet", "--bare", &remote.0.to_string_lossy()],
        )
        .unwrap();
        s.git(&["push", "--quiet", &remote.0.to_string_lossy(), "topic"]);

        let info: PrInfo = serde_json::from_value(json!({
            "number": number, "createdAt": "2026-01-01T00:00:00Z", "state": "OPEN",
            "baseRefName": "main", "headRefName": "topic", "headRefOid": head
        }))
        .unwrap();
        (s, remote, base, info)
    }

    fn pr() -> PrRef {
        PrRef {
            repo: Repo::parse("o/r").unwrap(),
            number: 7,
        }
    }

    #[test]
    fn check_in_a_worktree_leaves_the_checkout_alone() {
        let (s, remote, base, info) = diverged("check", 1, "b.txt");
        let remote_url = remote.0.to_string_lossy().to_string();
        let o = in_worktree(&pr(), &info, &s.0, &remote_url, &base, true).unwrap();
        assert_eq!((o.method, o.result), ("check", "clean"));
        // Nothing was pushed, and the temporary worktree is gone.
        assert_eq!(
            git_in(&remote.0, &["rev-parse", "topic"]).unwrap(),
            info.head_oid
        );
        assert_eq!(s.git(&["worktree", "list"]).lines().count(), 1);

        let (s, remote, base, info) = diverged("check-conflict", 2, "a.txt");
        let remote_url = remote.0.to_string_lossy().to_string();
        let o = in_worktree(&pr(), &info, &s.0, &remote_url, &base, true).unwrap();
        assert_eq!(o.result, "conflicts");
        assert_eq!(o.conflicts, vec!["a.txt"]);
    }

    #[test]
    fn worktree_rebase_pushes_with_a_lease() {
        let (s, remote, base, info) = diverged("push", 3, "b.txt");
        let remote_url = remote.0.to_string_lossy().to_string();
        let o = in_worktree(&pr(), &info, &s.0, &remote_url, &base, false).unwrap();
        assert_eq!((o.method, o.result), ("local", "rebased"));
        let pushed = git_in(&remote.0, &["rev-parse", "topic"]).unwrap();
        assert_eq!(o.new_head.as_deref(), Some(pushed.as_str()));
        assert!(git_ok(
            &remote.0,
            &["merge-base", "--is-ancestor", &base, "topic"]
        ));

        // Someone pushed after the PR was fetched, so a push leased on the old head is refused.
        s.git(&["checkout", "--quiet", "topic"]);
        s.commit("d.txt", "other\n");
        s.git(&["push", "--quiet", "--force", &remote_url, "topic"]);
        let err = in_worktree(&pr(), &info, &s.0, &remote_url, &base, false)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("Push rejected."), "{err}");
    }

    #[test]
    fn in_place_rebases_the_checked_out_branch() {
        let (s, remote, base, info) = diverged("in-place", 4, "b.txt");
        let remote_url = remote.0.to_string_lossy().to_string();
        s.git(&["checkout", "--quiet", "topic"]);
        let o = in_place(&pr(), &info, &s.0, &remote_url, &base).unwrap();
        assert_eq!(o.result, "rebased");
        assert_eq!(
            o.new_head.as_deref(),
            Some(s.git(&["rev-parse", "HEAD"]).as_str())
        );
    }

    #[test]
    fn in_place_refuses_dirty_or_stale_branches() {
        let (s, remote, base, info) = diverged("refuse", 5, "b.txt");
        let remote_url = remote.0.to_string_lossy().to_string();
        s.git(&["checkout", "--quiet", "topic"]);
        std::fs::write(s.0.join("b.txt"), "edited\n").unwrap();
        let err = in_place(&pr(), &info, &s.0, &remote_url, &base)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Commit or stash your changes before rebasing topic");

        s.git(&["checkout", "--quiet", "--", "b.txt"]);
        s.git(&["reset", "--quiet", "--hard", "main"]);
        let err = in_place(&pr(), &info, &s.0, &remote_url, &base)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "Your topic branch is missing commits that are on o/r#7. Pull them first."
        );
    }

    #[test]
    fn in_place_conflicts_keep_the_branch() {
        let (s, remote, base, info) = diverged("in-place-conflict", 6, "a.txt");
        let remote_url = remote.0.to_string_lossy().to_string();
        s.git(&["checkout", "--quiet", "topic"]);
        let o = in_place(&pr(), &info, &s.0, &remote_url, &base).unwrap();
        assert_eq!(o.result, "conflicts");
        assert_eq!(s.git(&["rev-parse", "HEAD"]), info.head_oid);
    }
}

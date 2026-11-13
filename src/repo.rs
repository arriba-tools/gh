//! Waits on a branch for commits that add or change files, so a script or an agent can block until
//! a repository has something new for it.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::api::{self, Client, Repo, encode_path};
use crate::cli::{RepoCommand, RepoWatchArgs};
use crate::context::Ctx;
use crate::wait::{
    Filter, LOOKBACK, Outcome, Store, Wait, filter_flags, finish, poll_until, progress,
    reply_flags, wake_flag,
};
use crate::wake;

/// The most files GitHub lists when comparing two commits.
const COMPARE_LIMIT: usize = 300;

pub fn run(ctx: &Ctx, cmd: RepoCommand) -> Result<()> {
    match cmd {
        RepoCommand::Watch(args) => watch(ctx, &args),
    }
}

/// One file a commit added or changed, as it is at `commit`.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct Changed {
    path: String,
    status: String,
    commit: String,
    body: String,
}

/// What a named watch remembers between runs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct State {
    /// Commits up to this one were acknowledged, or held nothing wanted.
    #[serde(default)]
    cursor: Option<String>,
}

/// Where a scan starts.
#[derive(Clone, Debug, PartialEq)]
enum Base {
    /// The commit after which files are new.
    Commit(String),
    /// No commit is old enough, so every file on the branch is new.
    Everything,
}

/// Reads a branch's head again and again, for free while it has not moved.
struct Head<'a> {
    client: &'a Client,
    path: String,
    etag: Option<String>,
    sha: Option<String>,
}

impl<'a> Head<'a> {
    fn new(client: &'a Client, repo: &Repo, branch: &str) -> Self {
        Self {
            client,
            path: format!("{}/git/ref/heads/{}", repo.api(), encode_path(branch)),
            etag: None,
            sha: None,
        }
    }

    /// The commit the branch points at now.
    fn read(&mut self) -> Result<String> {
        let (body, etag) = self
            .client
            .get_if_changed(&self.path, self.etag.as_deref())?;
        if let Some(body) = body {
            let sha = body["object"]["sha"]
                .as_str()
                .with_context(|| format!("No commit in {}", self.path))?;
            self.sha = Some(sha.to_string());
            self.etag = etag;
        }
        self.sha
            .clone()
            .context("Branch head missing from response")
    }
}

/// The newest commit on `branch` at least a day old at `now`, where a named watch starts the
/// first time it runs, so it does not replay the branch's whole history.
fn lookback(client: &Client, repo: &Repo, branch: &str, now: DateTime<Utc>) -> Result<Base> {
    let until = (now - LOOKBACK).format("%Y-%m-%dT%H:%M:%SZ");
    let commits = client.get(&format!(
        "{}/commits?sha={}&until={until}&per_page=1",
        repo.api(),
        encode_path(branch)
    ))?;
    Ok(match commits[0]["sha"].as_str() {
        Some(sha) => Base::Commit(sha.to_string()),
        None => Base::Everything,
    })
}

/// Whether a compare status means the file exists at the head with new contents.
fn arrived(status: &str) -> bool {
    matches!(
        status,
        "added" | "modified" | "renamed" | "copied" | "changed"
    )
}

/// The paths under `prefix` that are new or changed at `head` relative to `base`, with the status
/// of each, sorted by path.
fn changed_paths(
    client: &Client,
    repo: &Repo,
    base: &Base,
    head: &str,
    prefix: &str,
) -> Result<Vec<(String, String)>> {
    let mut paths: Vec<(String, String)> = match base {
        Base::Commit(sha) if sha == head => Vec::new(),
        Base::Commit(sha) => {
            // One commit per page keeps the answer small, since only the file list is read.
            let diff = client.get(&format!("{}/compare/{sha}...{head}?per_page=1", repo.api()))?;
            let files = diff["files"].as_array().cloned().unwrap_or_default();
            if files.len() >= COMPARE_LIMIT {
                eprintln!(
                    "More than {COMPARE_LIMIT} files changed since {sha}, so some are not listed"
                );
            }
            files
                .iter()
                .filter_map(|f| {
                    let status = f["status"].as_str()?;
                    arrived(status)
                        .then(|| Some((f["filename"].as_str()?.to_string(), status.to_string())))?
                })
                .collect()
        }
        Base::Everything => {
            let tree = client.get(&format!("{}/git/trees/{head}?recursive=1", repo.api()))?;
            let Some(entries) = tree["tree"].as_array() else {
                bail!("No tree in the response for {head}");
            };
            entries
                .iter()
                .filter(|e| e["type"] == "blob")
                .filter_map(|e| Some((e["path"].as_str()?.to_string(), "added".to_string())))
                .collect()
        }
    };
    paths.retain(|(path, _)| path.starts_with(prefix));
    paths.sort();
    Ok(paths)
}

/// The files under `prefix` that arrived between `base` and `head` and that the filter wants.
fn scan(
    client: &Client,
    repo: &Repo,
    base: &Base,
    head: &str,
    prefix: &str,
    filter: &Filter,
) -> Result<Vec<Changed>> {
    let mut found = Vec::new();
    for (path, status) in changed_paths(client, repo, base, head, prefix)? {
        let body = client.get_raw(&format!(
            "{}/contents/{}?ref={head}",
            repo.api(),
            encode_path(&path)
        ))?;
        if filter.wants(&body) {
            found.push(Changed {
                path,
                status,
                commit: head.to_string(),
                body,
            });
        }
    }
    Ok(found)
}

/// Wait for wanted files to arrive on the branch after `base`. A head that brings nothing wanted
/// moves `base` up to it, so no commit is scanned twice. A head that brings something leaves
/// `base` where it was, since finding a file is not acknowledging it.
fn watch_branch(
    head: &mut Head,
    repo: &Repo,
    base: &mut Base,
    prefix: &str,
    filter: &Filter,
    wait: &Wait,
) -> Result<Outcome<Changed>> {
    let client = head.client;
    poll_until(wait, || {
        let now = head.read()?;
        let found = scan(client, repo, base, &now, prefix, filter)?;
        if found.is_empty() {
            *base = Base::Commit(now);
        }
        Ok(found)
    })
}

fn watch(ctx: &Ctx, args: &RepoWatchArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let client = &ctx.client;
    let wait = Wait {
        interval: Duration::from_secs(args.interval),
        timeout: Duration::from_secs(args.timeout * 60),
        offline: Duration::from_secs(args.offline * 60),
        wake: wake::resolve(client, &repo, args.wake.as_deref())?,
    };
    let filter = Filter::new(&args.matches, &args.not_match)?.replying_to(&args.re)?;
    let prefix = args.path.as_deref().unwrap_or_default();
    let branch = match &args.branch {
        Some(branch) => branch.clone(),
        None => api::default_branch(client, &repo)?,
    };
    let store = args
        .state
        .as_deref()
        .map(|key| Store::open("repo-watch", &repo, &format!("{branch}__{prefix}"), key))
        .transpose()?;
    let mut state: State = store.as_ref().map(Store::load).unwrap_or_default();
    if let Some(sha) = &args.ack {
        state.cursor = Some(sha.clone());
    }

    let mut head = Head::new(client, &repo, &branch);
    let mut base = match &state.cursor {
        Some(sha) => Base::Commit(sha.clone()),
        // A named watch starts a day back the first time. An unnamed one reports only what
        // arrives while it waits.
        None if store.is_some() => lookback(client, &repo, &branch, Utc::now())?,
        None => Base::Commit(head.read()?),
    };
    let from = match &base {
        Base::Commit(sha) => format!("after {}", short(sha)),
        Base::Everything => "from its first commit".to_string(),
    };
    progress(ctx, &format!("Watching {repo} branch {branch} {from}"));

    let outcome = watch_branch(&mut head, &repo, &mut base, prefix, &filter, &wait)?;
    // Saved before reporting, since a quiet or unreachable end exits the process.
    if let (Some(store), Base::Commit(sha)) = (&store, &base) {
        state.cursor = Some(sha.clone());
        store.save(&state)?;
    }
    let found = finish(ctx, outcome, &wait, |f: &Changed| {
        format!(
            "--- {} ({}) at {}\n{}",
            f.path,
            f.status,
            short(&f.commit),
            f.body
        )
    })?;
    if let (Some(key), Some(last)) = (&args.state, found.first()) {
        let path = args
            .path
            .as_ref()
            .map(|p| format!(" --path {p}"))
            .unwrap_or_default();
        progress(
            ctx,
            &format!(
                "\nOnce these are handled, restart with:\n  gh arriba repo watch -R {repo} \
                 --branch {branch}{path} --state {key} --ack {}{}{}{}",
                last.commit,
                reply_flags(&args.re),
                filter_flags(&args.matches, &args.not_match),
                wake_flag(&args.wake)
            ),
        );
    }
    Ok(())
}

/// A commit hash cut to the length people read.
fn short(sha: &str) -> &str {
    sha.get(..10).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server, ServerGuard};
    use regex::Regex;
    use serde_json::{Value, json};

    fn repo() -> Repo {
        Repo::parse("o/r").unwrap()
    }

    fn no_wait() -> Wait {
        Wait {
            interval: Duration::ZERO,
            timeout: Duration::from_secs(60),
            offline: Duration::ZERO,
            wake: None,
        }
    }

    fn once() -> Wait {
        Wait {
            interval: Duration::from_secs(60),
            timeout: Duration::ZERO,
            offline: Duration::ZERO,
            wake: None,
        }
    }

    fn head_at(server: &mut ServerGuard, sha: &str) -> mockito::Mock {
        server
            .mock("GET", "/repos/o/r/git/ref/heads/main")
            .with_body(json!({"object": {"sha": sha}}).to_string())
            .create()
    }

    fn serve_file(server: &mut ServerGuard, path: &str, at: &str, body: &str) -> mockito::Mock {
        server
            .mock("GET", format!("/repos/o/r/contents/{path}").as_str())
            .match_query(Matcher::UrlEncoded("ref".into(), at.into()))
            .with_body(body)
            .create()
    }

    fn serve_compare(server: &mut ServerGuard, range: &str, files: Value) -> mockito::Mock {
        server
            .mock("GET", format!("/repos/o/r/compare/{range}").as_str())
            .match_query(Matcher::Any)
            .with_body(json!({ "files": files }).to_string())
            .create()
    }

    #[test]
    fn head_is_read_for_free_until_it_moves() {
        let mut server = Server::new();
        let first = server
            .mock("GET", "/repos/o/r/git/ref/heads/topic/x")
            .match_header("if-none-match", Matcher::Missing)
            .with_header("ETag", "\"a\"")
            .with_body(json!({"object": {"sha": "aaa"}}).to_string())
            .expect(1)
            .create();
        let unchanged = server
            .mock("GET", "/repos/o/r/git/ref/heads/topic/x")
            .match_header("if-none-match", "\"a\"")
            .with_status(304)
            .expect(1)
            .create();
        let client = Client::for_tests(&server.url());
        let mut head = Head::new(&client, &repo(), "topic/x");
        assert_eq!(head.read().unwrap(), "aaa");
        assert_eq!(head.read().unwrap(), "aaa");
        first.assert();
        unchanged.assert();
    }

    #[test]
    fn changed_paths_keep_what_arrived_under_the_prefix() {
        let mut server = Server::new();
        serve_compare(
            &mut server,
            "aaa...bbb",
            json!([
                {"filename": "chan/2.md", "status": "added"},
                {"filename": "chan/1.md", "status": "modified"},
                {"filename": "chan/gone.md", "status": "removed"},
                {"filename": "other/3.md", "status": "added"}
            ]),
        );
        let client = Client::for_tests(&server.url());
        let base = Base::Commit("aaa".into());
        let paths = changed_paths(&client, &repo(), &base, "bbb", "chan/").unwrap();
        assert_eq!(
            paths,
            [
                ("chan/1.md".to_string(), "modified".to_string()),
                ("chan/2.md".to_string(), "added".to_string())
            ]
        );
        // A head equal to the base asks GitHub nothing.
        let same = changed_paths(&client, &repo(), &base, "aaa", "").unwrap();
        assert!(same.is_empty());
    }

    #[test]
    fn everything_lists_the_whole_tree() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/git/trees/bbb")
            .match_query(Matcher::UrlEncoded("recursive".into(), "1".into()))
            .with_body(
                json!({"tree": [
                    {"path": "chan", "type": "tree"},
                    {"path": "chan/1.md", "type": "blob"},
                    {"path": "README.md", "type": "blob"}
                ]})
                .to_string(),
            )
            .create();
        let client = Client::for_tests(&server.url());
        let paths = changed_paths(&client, &repo(), &Base::Everything, "bbb", "chan/").unwrap();
        assert_eq!(paths, [("chan/1.md".to_string(), "added".to_string())]);
    }

    #[test]
    fn lookback_starts_at_a_day_old_commit_or_at_the_beginning() {
        let mut server = Server::new();
        let now: DateTime<Utc> = "2026-01-02T00:00:00Z".parse().unwrap();
        let old = server
            .mock("GET", "/repos/o/r/commits")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("sha".into(), "main".into()),
                Matcher::UrlEncoded("until".into(), "2026-01-01T00:00:00Z".into()),
            ]))
            .with_body(json!([{"sha": "old"}]).to_string())
            .expect(1)
            .create();
        let client = Client::for_tests(&server.url());
        let base = lookback(&client, &repo(), "main", now).unwrap();
        assert_eq!(base, Base::Commit("old".into()));
        old.assert();

        let mut young = Server::new();
        young
            .mock("GET", "/repos/o/r/commits")
            .match_query(Matcher::Any)
            .with_body("[]")
            .create();
        let client = Client::for_tests(&young.url());
        assert_eq!(
            lookback(&client, &repo(), "main", now).unwrap(),
            Base::Everything
        );
    }

    #[test]
    fn finding_a_file_does_not_move_the_base() {
        let mut server = Server::new();
        head_at(&mut server, "bbb");
        serve_compare(
            &mut server,
            "aaa...bbb",
            json!([
                {"filename": "chan/1.md", "status": "added"},
                {"filename": "chan/2.md", "status": "added"}
            ]),
        );
        serve_file(&mut server, "chan/1.md", "bbb", "to: mac\n\nhello");
        serve_file(&mut server, "chan/2.md", "bbb", "to: win\n\nnot yours");
        let client = Client::for_tests(&server.url());
        let filter = Filter {
            matches: Some(Regex::new(r"(?m)^to:.*\b(mac|all)\b").unwrap()),
            not_match: None,
            replies: None,
        };
        let mut head = Head::new(&client, &repo(), "main");
        let mut base = Base::Commit("aaa".into());
        let outcome =
            watch_branch(&mut head, &repo(), &mut base, "chan/", &filter, &no_wait()).unwrap();
        let Outcome::Found(found) = outcome else {
            panic!("expected a file, got {outcome:?}");
        };
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "chan/1.md");
        assert_eq!(found[0].commit, "bbb");
        assert_eq!(found[0].body, "to: mac\n\nhello");
        assert_eq!(base, Base::Commit("aaa".into()));
    }

    #[test]
    fn a_head_with_nothing_wanted_moves_the_base() {
        let mut server = Server::new();
        head_at(&mut server, "bbb");
        serve_compare(
            &mut server,
            "aaa...bbb",
            json!([{"filename": "chan/2.md", "status": "added"}]),
        );
        serve_file(&mut server, "chan/2.md", "bbb", "to: win\n\nnot yours");
        let client = Client::for_tests(&server.url());
        let filter = Filter {
            matches: Some(Regex::new(r"(?m)^to: mac").unwrap()),
            not_match: None,
            replies: None,
        };
        let mut head = Head::new(&client, &repo(), "main");
        let mut base = Base::Commit("aaa".into());
        let outcome =
            watch_branch(&mut head, &repo(), &mut base, "chan/", &filter, &once()).unwrap();
        assert_eq!(outcome, Outcome::TimedOut);
        assert_eq!(base, Base::Commit("bbb".into()));
    }

    #[test]
    fn an_unknown_base_fails_at_once() {
        let mut server = Server::new();
        head_at(&mut server, "bbb");
        server
            .mock("GET", "/repos/o/r/compare/nope...bbb")
            .match_query(Matcher::Any)
            .with_status(404)
            .with_body(r#"{"message": "Not Found"}"#)
            .expect(1)
            .create();
        let client = Client::for_tests(&server.url());
        let mut head = Head::new(&client, &repo(), "main");
        let mut base = Base::Commit("nope".into());
        let err = watch_branch(
            &mut head,
            &repo(),
            &mut base,
            "",
            &Filter::default(),
            &no_wait(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.starts_with("GitHub API 404"), "{err}");
    }

    fn args(branch: Option<&str>) -> RepoWatchArgs {
        RepoWatchArgs {
            branch: branch.map(str::to_string),
            path: Some("chan/".into()),
            matches: None,
            not_match: None,
            re: Vec::new(),
            state: None,
            ack: None,
            interval: 1,
            timeout: 1,
            offline: 15,
            wake: None,
        }
    }

    #[test]
    fn an_unnamed_watch_reports_only_what_arrives() {
        let mut server = Server::new();
        // The head is read once to fix the start, then it has moved on the first poll.
        let start = server
            .mock("GET", "/repos/o/r/git/ref/heads/main")
            .match_header("if-none-match", Matcher::Missing)
            .with_header("ETag", "\"a\"")
            .with_body(json!({"object": {"sha": "aaa"}}).to_string())
            .expect(1)
            .create();
        let moved = server
            .mock("GET", "/repos/o/r/git/ref/heads/main")
            .match_header("if-none-match", "\"a\"")
            .with_header("ETag", "\"b\"")
            .with_body(json!({"object": {"sha": "bbb"}}).to_string())
            .expect(1)
            .create();
        serve_compare(
            &mut server,
            "aaa...bbb",
            json!([{"filename": "chan/1.md", "status": "added"}]),
        );
        serve_file(&mut server, "chan/1.md", "bbb", "hello");
        let ctx = Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json: true,
            yes: true,
        };
        run(&ctx, RepoCommand::Watch(args(Some("main")))).unwrap();
        start.assert();
        moved.assert();
    }

    #[test]
    fn the_default_branch_is_used_when_none_is_named() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r")
            .with_body(r#"{"default_branch": "trunk"}"#)
            .create();
        let first = server
            .mock("GET", "/repos/o/r/git/ref/heads/trunk")
            .match_header("if-none-match", Matcher::Missing)
            .with_header("ETag", "\"a\"")
            .with_body(json!({"object": {"sha": "aaa"}}).to_string())
            .create();
        server
            .mock("GET", "/repos/o/r/git/ref/heads/trunk")
            .match_header("if-none-match", "\"a\"")
            .with_header("ETag", "\"b\"")
            .with_body(json!({"object": {"sha": "bbb"}}).to_string())
            .create();
        serve_compare(
            &mut server,
            "aaa...bbb",
            json!([{"filename": "chan/1.md", "status": "added"}]),
        );
        serve_file(&mut server, "chan/1.md", "bbb", "hello");
        let ctx = Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json: false,
            yes: true,
        };
        run(&ctx, RepoCommand::Watch(args(None))).unwrap();
        first.assert();
    }

    #[test]
    fn short_hashes() {
        assert_eq!(short("0123456789abcdef"), "0123456789");
        assert_eq!(short("abc"), "abc");
    }
}

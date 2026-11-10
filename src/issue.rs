//! Waits on an issue for new comments, or on a repository for labelled issues, so a script or an
//! agent can block until something needs it instead of polling by hand.

use anyhow::{Result, bail};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::api::{Client, Repo, encode_segment};
use crate::cli::{IssueCommand, IssueWatchArgs};
use crate::context::Ctx;
use crate::wait::{
    Filter, LOOKBACK, Outcome, Store, Wait, filter_flags, finish, poll_until, progress,
};

pub fn run(ctx: &Ctx, cmd: IssueCommand) -> Result<()> {
    match cmd {
        IssueCommand::Watch(args) => watch(ctx, &args),
    }
}

/// One issue comment.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct Comment {
    id: u64,
    author: String,
    created_at: String,
    body: String,
    url: String,
}

impl Comment {
    fn parse(v: &Value) -> Option<Self> {
        Some(Self {
            id: v["id"].as_u64()?,
            author: v["user"]["login"].as_str().unwrap_or_default().to_string(),
            created_at: v["created_at"].as_str().unwrap_or_default().to_string(),
            body: v["body"].as_str().unwrap_or_default().to_string(),
            url: v["html_url"].as_str().unwrap_or_default().to_string(),
        })
    }
}

/// One open issue that carries the wanted labels.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct Labelled {
    number: u64,
    title: String,
    url: String,
}

/// What a named watch remembers between runs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct State {
    /// Comments up to this ID were acknowledged.
    #[serde(default)]
    acked: u64,
    /// Comments are listed from this time on.
    #[serde(default)]
    since: Option<String>,
    /// The newest comment printed and its time, so acknowledging it can move `since` forward.
    #[serde(default)]
    delivered: Option<(u64, String)>,
    /// When each labelled issue was last reported.
    #[serde(default)]
    announced: BTreeMap<u64, DateTime<Utc>>,
}

impl State {
    /// Acknowledge comments up to `id`. The cursor never moves back.
    fn ack(&mut self, id: u64) {
        self.acked = self.acked.max(id);
        if let Some((delivered, at)) = &self.delivered
            && *delivered <= self.acked
        {
            self.since = Some(at.clone());
        }
    }

    /// Note the comments just printed. Printing is not acknowledging, so the cursor stays.
    fn deliver(&mut self, comments: &[Comment]) {
        if let Some(last) = comments.iter().max_by_key(|c| c.id) {
            self.delivered = Some((last.id, last.created_at.clone()));
        }
    }
}

/// The comments past the cursor that the filter wants, oldest first.
fn unread(mut comments: Vec<Comment>, filter: &Filter, acked: u64) -> Vec<Comment> {
    comments.retain(|c| c.id > acked && filter.wants(&c.body));
    comments.sort_by_key(|c| c.id);
    comments
}

/// The time to list comments from. A named watch without a cursor looks back a day, so its first
/// run does not replay the issue's whole history. An unnamed watch reports only what arrives.
fn start_time(state: &State, now: DateTime<Utc>, named: bool) -> String {
    match &state.since {
        Some(since) => since.clone(),
        None => (if named { now - LOOKBACK } else { now })
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
    }
}

/// The labelled issues to report now, each recorded as reported. A named watch reports an issue
/// once per `window`, so one that stays labelled does not end every restart at once.
fn fresh(
    issues: &[Labelled],
    announced: &mut BTreeMap<u64, DateTime<Utc>>,
    now: DateTime<Utc>,
    window: TimeDelta,
    named: bool,
) -> Vec<Labelled> {
    let due: Vec<Labelled> = issues
        .iter()
        .filter(|i| {
            !named
                || announced
                    .get(&i.number)
                    .is_none_or(|at| now - *at >= window)
        })
        .cloned()
        .collect();
    for issue in &due {
        announced.insert(issue.number, now);
    }
    due
}

/// Reads a list endpoint again and again, for free while it has not changed.
struct Feed<'a> {
    client: &'a Client,
    path: String,
    etag: Option<String>,
    last: Vec<Value>,
}

impl<'a> Feed<'a> {
    fn new(client: &'a Client, path: String) -> Self {
        Self {
            client,
            path,
            etag: None,
            last: Vec::new(),
        }
    }

    /// The list as it is now.
    fn read(&mut self) -> Result<&[Value]> {
        let (body, etag) = self
            .client
            .get_if_changed(&self.path, self.etag.as_deref())?;
        if let Some(body) = body {
            let Value::Array(page) = body else {
                bail!("Expected a JSON array from {}", self.path);
            };
            // A full page may have more behind it. Only the first page's tag is known, and a new
            // item lands on a later page, so a paged list is read whole on every poll.
            if page.len() >= 100 {
                self.last = self.client.get_all(&self.path)?;
                self.etag = None;
            } else {
                self.last = page;
                self.etag = etag;
            }
        }
        Ok(&self.last)
    }
}

/// Wait for comments on issue `number` past the cursor in `state`.
fn watch_comments(
    client: &Client,
    repo: &Repo,
    number: u64,
    filter: &Filter,
    state: &State,
    named: bool,
    wait: &Wait,
) -> Result<Outcome<Comment>> {
    let since = start_time(state, Utc::now(), named);
    let path = format!(
        "{}/issues/{number}/comments?per_page=100&since={since}",
        repo.api()
    );
    let mut feed = Feed::new(client, path);
    poll_until(wait, || {
        let comments = feed.read()?.iter().filter_map(Comment::parse).collect();
        Ok(unread(comments, filter, state.acked))
    })
}

/// Wait for open issues that carry every label in `labels` and none in `not_labels`.
fn watch_labels(
    client: &Client,
    repo: &Repo,
    labels: &[String],
    not_labels: &[String],
    state: &mut State,
    named: bool,
    wait: &Wait,
) -> Result<Outcome<Labelled>> {
    let wanted: Vec<String> = labels.iter().map(|l| encode_segment(l)).collect();
    let path = format!(
        "{}/issues?state=open&per_page=100&labels={}",
        repo.api(),
        wanted.join(",")
    );
    let window = TimeDelta::from_std(wait.timeout).unwrap_or(TimeDelta::MAX);
    let mut feed = Feed::new(client, path);
    poll_until(wait, || {
        let issues: Vec<Labelled> = feed
            .read()?
            .iter()
            // The issues endpoint lists pull requests too.
            .filter(|i| i.get("pull_request").is_none())
            .filter(|i| {
                let has = |label: &String| {
                    i["labels"]
                        .as_array()
                        .is_some_and(|ls| ls.iter().any(|l| l["name"].as_str() == Some(label)))
                };
                !not_labels.iter().any(has)
            })
            .filter_map(|i| {
                Some(Labelled {
                    number: i["number"].as_u64()?,
                    title: i["title"].as_str().unwrap_or_default().to_string(),
                    url: i["html_url"].as_str().unwrap_or_default().to_string(),
                })
            })
            .collect();
        Ok(fresh(
            &issues,
            &mut state.announced,
            Utc::now(),
            window,
            named,
        ))
    })
}

fn watch(ctx: &Ctx, args: &IssueWatchArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let wait = Wait {
        interval: Duration::from_secs(args.interval),
        timeout: Duration::from_secs(args.timeout * 60),
        offline: Duration::from_secs(args.offline * 60),
    };
    let named = args.state.is_some();

    let Some(number) = args.number else {
        let mut labels = args.label.clone();
        labels.sort();
        let store = args
            .state
            .as_deref()
            .map(|key| {
                Store::open(
                    "issue-watch",
                    &repo,
                    &format!("label-{}", labels.join("+")),
                    key,
                )
            })
            .transpose()?;
        let mut state: State = store.as_ref().map(Store::load).unwrap_or_default();
        progress(
            ctx,
            &format!(
                "Watching {repo} for open issues labelled {}",
                labels.join(", ")
            ),
        );
        let outcome = watch_labels(
            &ctx.client,
            &repo,
            &labels,
            &args.not_label,
            &mut state,
            named,
            &wait,
        )?;
        if let Some(store) = &store {
            store.save(&state)?;
        }
        finish(ctx, outcome, &wait, |i: &Labelled| {
            format!("#{} {}\n{}", i.number, i.title, i.url)
        })?;
        return Ok(());
    };

    let filter = Filter::new(&args.matches, &args.not_match)?;
    let store = args
        .state
        .as_deref()
        .map(|key| Store::open("issue-watch", &repo, &number.to_string(), key))
        .transpose()?;
    let mut state: State = store.as_ref().map(Store::load).unwrap_or_default();
    if let Some(id) = args.ack {
        state.ack(id);
    }
    // Fixed on the first run, so later runs list from the same point until an ack moves it.
    state.since = Some(start_time(&state, Utc::now(), named));
    if let Some(store) = &store {
        store.save(&state)?;
    }
    progress(
        ctx,
        &format!(
            "Watching {repo}#{number} for comments after {}",
            state.acked
        ),
    );
    let outcome = watch_comments(&ctx.client, &repo, number, &filter, &state, named, &wait)?;
    let found = finish(ctx, outcome, &wait, |c: &Comment| {
        format!(
            "--- comment {} by {} at {}\n{}",
            c.id, c.author, c.created_at, c.body
        )
    })?;
    if let (Some(store), Some(key), Some(last)) =
        (&store, &args.state, found.iter().map(|c| c.id).max())
    {
        state.deliver(&found);
        store.save(&state)?;
        progress(
            ctx,
            &format!(
                "\nOnce these are handled, restart with:\n  gh arriba issue watch {number} -R {repo} \
                 --state {key} --ack {last}{}",
                filter_flags(&args.matches, &args.not_match)
            ),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use regex::Regex;
    use serde_json::json;
    use std::fs;

    fn comment(id: u64, body: &str) -> Comment {
        Comment {
            id,
            author: "someone".into(),
            created_at: format!("2026-01-01T00:00:{id:02}Z"),
            body: body.into(),
            url: String::new(),
        }
    }

    fn raw(id: u64, body: &str) -> Value {
        json!({"id": id, "user": {"login": "someone"}, "created_at": "2026-01-01T00:00:00Z",
               "body": body, "html_url": format!("https://github.com/o/r/issues/1#issuecomment-{id}")})
    }

    fn filter(matches: Option<&str>, not_match: Option<&str>) -> Filter {
        Filter {
            matches: matches.map(|p| Regex::new(p).unwrap()),
            not_match: not_match.map(|p| Regex::new(p).unwrap()),
        }
    }

    fn no_wait() -> Wait {
        Wait {
            interval: Duration::ZERO,
            timeout: Duration::from_secs(60),
            offline: Duration::ZERO,
        }
    }

    fn repo() -> Repo {
        Repo::parse("o/r").unwrap()
    }

    fn ids(comments: &[Comment]) -> Vec<u64> {
        comments.iter().map(|c| c.id).collect()
    }

    #[test]
    fn unread_keeps_what_is_past_the_cursor_and_wanted() {
        let all = vec![
            comment(3, "mac to coord\nreply"),
            comment(1, "coord to win2, mac\nold"),
            comment(2, "coord to win, mac\nnew"),
            comment(4, "coord to all\nx"),
            comment(5, "no header here"),
        ];
        // Addressed to mac or all on the first line, and not sent by mac.
        let mine = filter(Some(r"^\S+ to .*\b(mac|all)\b"), Some(r"^mac to "));
        assert_eq!(ids(&unread(all.clone(), &mine, 0)), [1, 2, 4]);
        assert_eq!(ids(&unread(all.clone(), &mine, 2)), [4]);
        assert_eq!(
            ids(&unread(all.clone(), &Filter::default(), 0)),
            [1, 2, 3, 4, 5]
        );
        assert!(unread(all, &mine, 4).is_empty());
    }

    #[test]
    fn printing_does_not_acknowledge() {
        let mut state = State {
            acked: 1,
            since: Some("2026-01-01T00:00:00Z".into()),
            ..State::default()
        };
        state.deliver(&[comment(7, ""), comment(9, "")]);
        assert_eq!(state.acked, 1);
        assert_eq!(state.since.as_deref(), Some("2026-01-01T00:00:00Z"));

        // Acknowledging what was printed moves both the cursor and the listing's start.
        state.ack(9);
        assert_eq!(state.acked, 9);
        assert_eq!(state.since.as_deref(), Some("2026-01-01T00:00:09Z"));

        // The cursor never moves back.
        state.ack(4);
        assert_eq!(state.acked, 9);
    }

    #[test]
    fn a_partial_ack_keeps_the_listing_start() {
        let mut state = State {
            since: Some("2026-01-01T00:00:00Z".into()),
            ..State::default()
        };
        state.deliver(&[comment(7, ""), comment(9, "")]);
        state.ack(7);
        assert_eq!(state.acked, 7);
        assert_eq!(state.since.as_deref(), Some("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn start_time_looks_back_a_day_only_when_named() {
        let now = "2026-01-02T00:00:00Z".parse().unwrap();
        let empty = State::default();
        assert_eq!(start_time(&empty, now, true), "2026-01-01T00:00:00Z");
        assert_eq!(start_time(&empty, now, false), "2026-01-02T00:00:00Z");
        let stored = State {
            since: Some("2025-12-01T00:00:00Z".into()),
            ..State::default()
        };
        assert_eq!(start_time(&stored, now, true), "2025-12-01T00:00:00Z");
    }

    #[test]
    fn labelled_issues_are_reported_once_per_window_when_named() {
        let issue = Labelled {
            number: 7,
            title: "Task".into(),
            url: String::new(),
        };
        let now: DateTime<Utc> = "2026-01-02T00:00:00Z".parse().unwrap();
        let window = TimeDelta::minutes(110);
        let mut announced = BTreeMap::new();
        let issues = std::slice::from_ref(&issue);
        assert_eq!(fresh(issues, &mut announced, now, window, true).len(), 1);
        let soon = now + TimeDelta::minutes(5);
        assert!(fresh(issues, &mut announced, soon, window, true).is_empty());
        let later = now + TimeDelta::minutes(111);
        assert_eq!(fresh(issues, &mut announced, later, window, true).len(), 1);
        // An unnamed watch has no memory, so it reports every time.
        assert_eq!(fresh(issues, &mut announced, later, window, false).len(), 1);
    }

    #[test]
    fn watch_comments_polls_for_free_until_one_arrives() {
        let mut server = Server::new();
        let since = Matcher::UrlEncoded("since".into(), "2026-01-01T00:00:00Z".into());
        let first = server
            .mock("GET", "/repos/o/r/issues/1/comments")
            .match_query(since.clone())
            .match_header("if-none-match", Matcher::Missing)
            .with_header("ETag", "\"a\"")
            .with_body(json!([raw(5, "old")]).to_string())
            .expect(1)
            .create();
        let unchanged = server
            .mock("GET", "/repos/o/r/issues/1/comments")
            .match_query(since.clone())
            .match_header("if-none-match", "\"a\"")
            .with_status(304)
            .expect(2)
            .create();
        let changed = server
            .mock("GET", "/repos/o/r/issues/1/comments")
            .match_query(since)
            .match_header("if-none-match", "\"a\"")
            .with_header("ETag", "\"b\"")
            .with_body(json!([raw(5, "old"), raw(6, "skip me"), raw(8, "new")]).to_string())
            .expect(1)
            .create();
        let client = Client::for_tests(&server.url());
        let state = State {
            acked: 5,
            since: Some("2026-01-01T00:00:00Z".into()),
            ..State::default()
        };
        let outcome = watch_comments(
            &client,
            &repo(),
            1,
            &filter(None, Some("^skip")),
            &state,
            true,
            &no_wait(),
        )
        .unwrap();
        let Outcome::Found(found) = outcome else {
            panic!("expected comments, got {outcome:?}");
        };
        assert_eq!(ids(&found), [8]);
        assert_eq!(found[0].author, "someone");
        first.assert();
        unchanged.assert();
        changed.assert();
    }

    #[test]
    fn a_paged_feed_is_read_whole_on_every_poll() {
        let mut server = Server::new();
        let page: Vec<Value> = (1..=100).map(|id| raw(id, "x")).collect();
        let next = format!(
            "<{}/repos/o/r/issues/1/comments?page=2>; rel=\"next\"",
            server.url()
        );
        // Each read asks for the first page twice, once to see whether it changed and once to
        // follow its pages. A stored tag would answer 304 here and hide the second page.
        let first = server
            .mock("GET", "/repos/o/r/issues/1/comments")
            .match_query(Matcher::UrlEncoded("since".into(), "t".into()))
            .match_header("if-none-match", Matcher::Missing)
            .with_header("ETag", "\"a\"")
            .with_header("link", &next)
            .with_body(Value::Array(page).to_string())
            .expect(4)
            .create();
        let second = server
            .mock("GET", "/repos/o/r/issues/1/comments")
            .match_query(Matcher::UrlEncoded("page".into(), "2".into()))
            .with_body(json!([raw(101, "new")]).to_string())
            .expect(2)
            .create();
        let client = Client::for_tests(&server.url());
        let path = "repos/o/r/issues/1/comments?per_page=100&since=t".to_string();
        let mut feed = Feed::new(&client, path);
        assert_eq!(feed.read().unwrap().len(), 101);
        assert_eq!(feed.etag, None);
        assert_eq!(feed.read().unwrap().len(), 101);
        first.assert();
        second.assert();
    }

    #[test]
    fn watch_comments_fails_at_once_on_a_missing_issue() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/issues/1/comments")
            .match_query(Matcher::Any)
            .with_status(404)
            .with_body(r#"{"message": "Not Found"}"#)
            .expect(1)
            .create();
        let client = Client::for_tests(&server.url());
        let err = watch_comments(
            &client,
            &repo(),
            1,
            &Filter::default(),
            &State::default(),
            false,
            &no_wait(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.starts_with("GitHub API 404"), "{err}");
    }

    #[test]
    fn watch_labels_skips_excluded_and_remembers() {
        let mut server = Server::new();
        let issue = |number: u64, labels: &[&str]| {
            let labels: Vec<Value> = labels.iter().map(|l| json!({"name": l})).collect();
            json!({"number": number, "title": format!("Task {number}"), "labels": labels,
                   "html_url": format!("https://github.com/o/r/issues/{number}")})
        };
        server
            .mock("GET", "/repos/o/r/issues")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("state".into(), "open".into()),
                Matcher::UrlEncoded("labels".into(), "win 2".into()),
            ]))
            .with_body(
                json!([
                    issue(3, &["win 2", "running"]),
                    issue(4, &["win 2"]),
                    {"number": 5, "title": "A pull request", "labels": [{"name": "win 2"}],
                     "pull_request": {"url": "https://api.github.com/repos/o/r/pulls/5"}}
                ])
                .to_string(),
            )
            .create();
        let client = Client::for_tests(&server.url());
        let labels = ["win 2".to_string()];
        let skip = ["running".to_string()];
        let mut state = State::default();
        let outcome = watch_labels(
            &client,
            &repo(),
            &labels,
            &skip,
            &mut state,
            true,
            &no_wait(),
        )
        .unwrap();
        let Outcome::Found(found) = outcome else {
            panic!("expected an issue, got {outcome:?}");
        };
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].number, 4);
        assert!(state.announced.contains_key(&4));

        // The same issue again within the window is not news.
        let once = Wait {
            interval: Duration::from_secs(60),
            timeout: Duration::from_secs(60),
            offline: Duration::ZERO,
        };
        let again =
            watch_labels(&client, &repo(), &labels, &skip, &mut state, true, &once).unwrap();
        assert_eq!(again, Outcome::TimedOut);
    }

    #[test]
    fn state_round_trips_and_survives_a_bad_file() {
        let dir = std::env::temp_dir().join(format!("gh-arriba-issue-{}", std::process::id()));
        let store = Store(dir.join("nested").join("state.json"));
        assert_eq!(store.load::<State>(), State::default());
        let mut state = State {
            acked: 9,
            since: Some("2026-01-01T00:00:00Z".into()),
            ..State::default()
        };
        state
            .announced
            .insert(4, "2026-01-02T00:00:00Z".parse().unwrap());
        state.deliver(&[comment(12, "")]);
        store.save(&state).unwrap();
        assert_eq!(store.load::<State>(), state);
        fs::write(&store.0, "not json").unwrap();
        assert_eq!(store.load::<State>(), State::default());
        fs::remove_dir_all(&dir).unwrap();
    }

    fn ctx(server: &mockito::ServerGuard, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    fn args(number: Option<u64>, label: &[&str]) -> IssueWatchArgs {
        IssueWatchArgs {
            number,
            label: label.iter().map(|l| l.to_string()).collect(),
            not_label: vec![],
            matches: None,
            not_match: None,
            state: None,
            ack: None,
            interval: 1,
            timeout: 1,
            offline: 15,
        }
    }

    #[test]
    fn watch_prints_comments_as_text_or_json() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/issues/1/comments")
            .match_query(Matcher::Any)
            .with_body(json!([raw(8, "hello")]).to_string())
            .expect(2)
            .create();
        run(
            &ctx(&server, false),
            IssueCommand::Watch(args(Some(1), &[])),
        )
        .unwrap();
        run(&ctx(&server, true), IssueCommand::Watch(args(Some(1), &[]))).unwrap();
    }

    #[test]
    fn watch_prints_labelled_issues() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/issues")
            .match_query(Matcher::Any)
            .with_body(
                json!([{"number": 4, "title": "Task", "labels": [{"name": "b"}]}]).to_string(),
            )
            .expect(2)
            .create();
        run(
            &ctx(&server, false),
            IssueCommand::Watch(args(None, &["b", "a"])),
        )
        .unwrap();
        run(&ctx(&server, true), IssueCommand::Watch(args(None, &["b"]))).unwrap();
    }
}

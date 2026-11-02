//! Pull requests: their metadata, checks, review threads, comments, reviews, labels, and the
//! calls that change them.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::api::{Client, PrRef, Repo};

mod branch;
mod comments;
mod heads;
mod labels;
mod pending;
mod rollup;
mod threads;

pub use branch::{behind_by, file_at, rebase_branch};
pub use comments::{
    RestComment, edit_issue_comment, edit_review, edit_review_comment, find_issue_comment,
    find_review_comment, issue_comment, review, review_comment,
};
pub use heads::{BranchPr, open_prs, pr_by_number, prs_by_branch};
pub use labels::{
    Contributor, RepoLabel, add_labels, contributors, pr_labels, remove_label, repo_labels,
    request_reviewers, requested_reviewers,
};
pub use pending::{
    NewThread, add_pending_review, add_review_thread, create_review, delete_review, fetch_pending,
    pr_files, submit_review,
};
pub use rollup::{CheckRun, StatusContext, StatusFields, checks_with_status, checks_with_title};
#[cfg(test)]
pub use threads::Comment;
pub use threads::{
    IssueComment, PrThreads, ResolvedThread, Review, Thread, add_issue_comment, fetch_threads,
    is_bot, issue_comments, last_review_time, reply_to_comment, reply_to_thread, reviews,
    set_thread_resolved,
};

/// State of a pull request, "open" or "closed", from REST without the rest of `fetch_pr`.
pub fn pull_state(client: &Client, repo: &Repo, number: u64) -> Result<String> {
    #[derive(Deserialize)]
    struct Pull {
        state: Option<String>,
    }
    let pull: Pull = serde_json::from_value(client.get(&format!("{}/pulls/{number}", repo.api()))?)
        .context("Failed to parse pull request")?;
    Ok(pull.state.unwrap_or_default())
}

/// Pull request metadata most commands need.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(from = "PrNode")]
pub struct PrInfo {
    pub node_id: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub draft: bool,
    pub body: String,
    pub author: String,
    pub created_at: DateTime<Utc>,
    pub base_ref: String,
    pub head_ref: String,
    pub head_oid: String,
    /// OWNER/REPO the head branch lives in, which differs from the base for forks.
    pub head_repo: Option<String>,
    pub cross_repository: bool,
}

/// The pull request as `PR_QUERY` returns it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrNode {
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    number: u64,
    #[serde(default, deserialize_with = "nullable")]
    title: String,
    #[serde(default, deserialize_with = "nullable")]
    url: String,
    #[serde(default, deserialize_with = "nullable")]
    state: String,
    #[serde(default, deserialize_with = "nullable")]
    is_draft: bool,
    #[serde(default, deserialize_with = "nullable")]
    body: String,
    author: Option<Actor>,
    created_at: DateTime<Utc>,
    #[serde(default, deserialize_with = "nullable")]
    base_ref_name: String,
    #[serde(default, deserialize_with = "nullable")]
    head_ref_name: String,
    #[serde(default, deserialize_with = "nullable")]
    head_ref_oid: String,
    #[serde(default, deserialize_with = "nullable")]
    is_cross_repository: bool,
    head_repository: Option<NameWithOwner>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NameWithOwner {
    name_with_owner: Option<String>,
}

impl From<PrNode> for PrInfo {
    fn from(p: PrNode) -> Self {
        PrInfo {
            node_id: p.id,
            number: p.number,
            title: p.title,
            url: p.url,
            state: p.state,
            draft: p.is_draft,
            body: p.body,
            author: login_or_ghost(p.author.as_ref()),
            created_at: p.created_at,
            base_ref: p.base_ref_name,
            head_ref: p.head_ref_name,
            head_oid: p.head_ref_oid,
            head_repo: p.head_repository.and_then(|r| r.name_with_owner),
            cross_repository: p.is_cross_repository,
        }
    }
}

const PR_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      id number title url state isDraft body createdAt
      author { login }
      baseRefName headRefName headRefOid
      isCrossRepository
      headRepository { nameWithOwner }
    }
  }
}"#;

pub fn fetch_pr(client: &Client, pr: &PrRef) -> Result<PrInfo> {
    let data: RepoData<Option<PrInfo>> = query(client, PR_QUERY, vars(pr))?;
    data.repository
        .pull_request
        .with_context(|| format!("Pull request {pr} not found"))
}

/// A GraphQL user, bot, or other actor. Null for deleted accounts.
#[derive(Clone, Debug, Deserialize)]
struct Actor {
    #[serde(rename = "__typename")]
    pub typename: Option<String>,
    pub login: Option<String>,
}

/// Login of an actor, or "ghost" for deleted accounts.
fn login_or_ghost(actor: Option<&Actor>) -> String {
    actor
        .and_then(|a| a.login.clone())
        .unwrap_or_else(|| "ghost".to_string())
}

/// A REST user object. Null for deleted accounts.
#[derive(Clone, Debug, Deserialize)]
pub struct User {
    pub login: Option<String>,
}

/// Login of a REST user, or "ghost" for deleted accounts.
pub fn user_login(user: Option<&User>) -> String {
    user.and_then(|u| u.login.clone())
        .unwrap_or_else(|| "ghost".to_string())
}

/// `data.repository` of a query on one repository.
#[derive(Deserialize)]
struct RepoData<T> {
    repository: PullRequestData<T>,
}

/// `repository.pullRequest`, null when the PR does not exist.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestData<T> {
    pull_request: T,
}

/// One page of a GraphQL connection.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", bound(deserialize = "T: Deserialize<'de>"))]
struct Connection<T> {
    page_info: PageInfo,
    #[serde(default, deserialize_with = "nullable")]
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    #[serde(default, deserialize_with = "nullable")]
    has_next_page: bool,
    end_cursor: Option<String>,
}

impl PageInfo {
    /// Cursor of the next page, or None on the last one.
    fn next_cursor(&self) -> Option<String> {
        if self.has_next_page {
            self.end_cursor.clone()
        } else {
            None
        }
    }
}

const MY_PRS_QUERY: &str = r#"
  query($q: String!, $cursor: String) {
    search(query: $q, type: ISSUE, first: 100, after: $cursor) {
      pageInfo { hasNextPage endCursor }
      nodes { ... on PullRequest { number } }
    }
  }"#;

#[derive(Deserialize)]
struct SearchData {
    search: Connection<SearchNode>,
}

/// A search hit. Hits that are not pull requests come back empty.
#[derive(Deserialize)]
struct SearchNode {
    number: Option<u64>,
}

/// Open PRs authored by the viewer in `repo`, by number.
pub fn my_open_prs(client: &Client, repo: &Repo) -> Result<Vec<PrRef>> {
    let login = client.viewer()?;
    let q = format!("is:pr is:open author:{login} repo:{repo}");
    let mut prs = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let data: SearchData = query(client, MY_PRS_QUERY, json!({ "q": q, "cursor": cursor }))?;
        prs.extend(
            data.search
                .nodes
                .iter()
                .filter_map(|n| n.number)
                .map(|number| PrRef {
                    repo: repo.clone(),
                    number,
                }),
        );
        match data.search.page_info.next_cursor() {
            Some(c) => cursor = Value::String(c),
            None => break,
        }
    }
    prs.sort_by_key(|p| p.number);
    Ok(prs)
}

/// GET every page of a REST list and deserialize its items.
fn get_all<T: DeserializeOwned>(client: &Client, path: &str) -> Result<Vec<T>> {
    client
        .get_all(path)?
        .into_iter()
        .map(|v| serde_json::from_value(v).with_context(|| format!("Failed to parse {path}")))
        .collect()
}

/// Run a GraphQL query and deserialize its `data`.
fn query<T: DeserializeOwned>(client: &Client, query: &str, variables: Value) -> Result<T> {
    let data = client.graphql(query, variables)?;
    serde_json::from_value(data).context("Failed to parse GraphQL response")
}

/// Variables naming a pull request, as every PR query takes them.
fn vars(pr: &PrRef) -> Value {
    json!({
        "owner": pr.repo.owner,
        "repo": pr.repo.name,
        "number": pr.number,
    })
}

/// Treat null like an absent field, as reading `v["field"]` with a fallback did.
fn nullable<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    fn pr() -> PrRef {
        PrRef {
            repo: Repo::parse("o/r").unwrap(),
            number: 7,
        }
    }

    #[test]
    fn pull_state_reads_rest_state() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/pulls/7")
            .with_body(r#"{"state": "closed", "title": "x"}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/pulls/8")
            .with_body(r#"{"state": null}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        assert_eq!(pull_state(&client, &repo, 7).unwrap(), "closed");
        assert_eq!(pull_state(&client, &repo, 8).unwrap(), "");
    }

    #[test]
    fn fetch_pr_sends_the_pr_variables() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({
                "variables": { "owner": "o", "repo": "r", "number": 7 }
            })))
            .with_body(
                json!({ "data": { "repository": { "pullRequest": {
                    "id": "PR_7", "number": 7, "title": "Fix", "url": "u", "state": "OPEN",
                    "isDraft": false, "body": null, "createdAt": "2026-01-02T03:04:05Z",
                    "author": { "login": "alice" }, "baseRefName": "main",
                    "headRefName": "fix", "headRefOid": "abc", "isCrossRepository": false,
                    "headRepository": null
                }}}})
                .to_string(),
            )
            .create();
        let client = Client::for_tests(&server.url());
        let info = fetch_pr(&client, &pr()).unwrap();
        mock.assert();
        assert_eq!(info.author, "alice");
        assert_eq!(info.body, "");
        assert_eq!(info.head_repo, None);
    }

    #[test]
    fn fetch_pr_missing_is_an_error() {
        let mut server = Server::new();
        server
            .mock("POST", "/graphql")
            .with_body(r#"{"data": {"repository": {"pullRequest": null}}}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let err = fetch_pr(&client, &pr()).unwrap_err().to_string();
        assert_eq!(err, "Pull request o/r#7 not found");
    }

    #[test]
    fn my_open_prs_pages_and_sorts() {
        let mut server = Server::new();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({"query": "query { viewer { login } }"}),
            ))
            .with_body(r#"{"data": {"viewer": {"login": "me"}}}"#)
            .create();
        let q = "is:pr is:open author:me repo:o/r";
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({"variables": {"q": q, "cursor": null}}),
            ))
            .with_body(
                r#"{"data": {"search": {
                    "pageInfo": {"hasNextPage": true, "endCursor": "c1"},
                    "nodes": [{"number": 9}, {}]}}}"#,
            )
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({"variables": {"q": q, "cursor": "c1"}}),
            ))
            .with_body(
                r#"{"data": {"search": {
                    "pageInfo": {"hasNextPage": false, "endCursor": null},
                    "nodes": [{"number": 3}]}}}"#,
            )
            .create();
        let client = Client::for_tests(&server.url());
        let prs = my_open_prs(&client, &Repo::parse("o/r").unwrap()).unwrap();
        let numbers: Vec<u64> = prs.iter().map(|p| p.number).collect();
        assert_eq!(numbers, vec![3, 9]);
    }

    #[test]
    fn user_login_falls_back_to_ghost() {
        assert_eq!(user_login(None), "ghost");
        let user = User {
            login: Some("bob".into()),
        };
        assert_eq!(user_login(Some(&user)), "bob");
    }

    #[test]
    fn pr_info_from_graphql() {
        let node = json!({
            "id": "PR_1", "number": 7, "title": "Fix", "url": "u", "state": "OPEN",
            "isDraft": true, "body": "b", "createdAt": "2026-01-02T03:04:05Z",
            "author": null, "baseRefName": "main", "headRefName": "fix",
            "headRefOid": "abc", "isCrossRepository": true,
            "headRepository": { "nameWithOwner": "fork/r" }
        });
        let info: PrInfo = serde_json::from_value(node).unwrap();
        assert_eq!(info.node_id, "PR_1");
        assert_eq!(info.author, "ghost");
        assert!(info.draft && info.cross_repository);
        assert_eq!(info.head_repo.as_deref(), Some("fork/r"));
        let out = serde_json::to_value(&info).unwrap();
        assert_eq!(out["base_ref"], "main");
        assert_eq!(out["created_at"], "2026-01-02T03:04:05Z");
    }

    #[test]
    fn missing_pr_is_null() {
        let data: RepoData<Option<PrInfo>> =
            serde_json::from_value(json!({ "repository": { "pullRequest": null } })).unwrap();
        assert!(data.repository.pull_request.is_none());
    }
}

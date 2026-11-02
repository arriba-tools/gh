//! Pull requests looked up by their head branch name, for matching local branches to PRs.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

use super::{Connection, nullable};
use crate::api::{Client, Repo};

/// A pull request whose head is a given branch name.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(from = "HeadNode")]
pub struct BranchPr {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// OPEN, CLOSED, or MERGED.
    pub state: String,
    pub author: String,
    /// The commit the PR's head points at, to tell whether a local branch has more.
    pub head_oid: String,
    /// OWNER/REPO the head branch lives in, to tell a fork's PR from one on the same branch name.
    pub head_repo: Option<String>,
    /// SUCCESS, FAILURE, ERROR, PENDING, or EXPECTED for the head's checks, if it has any.
    pub checks: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadNode {
    number: u64,
    #[serde(default, deserialize_with = "nullable")]
    title: String,
    #[serde(default, deserialize_with = "nullable")]
    url: String,
    #[serde(default, deserialize_with = "nullable")]
    state: String,
    author: Option<Login>,
    #[serde(default, deserialize_with = "nullable")]
    head_ref_oid: String,
    head_repository: Option<NameWithOwner>,
    #[serde(default, deserialize_with = "nullable")]
    commits: Nodes<CommitNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NameWithOwner {
    name_with_owner: String,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
struct Nodes<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
}

impl<T> Default for Nodes<T> {
    fn default() -> Self {
        Self { nodes: Vec::new() }
    }
}

#[derive(Deserialize)]
struct CommitNode {
    commit: Option<Commit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    status_check_rollup: Option<Rollup>,
}

#[derive(Deserialize)]
struct Rollup {
    state: Option<String>,
}

impl From<HeadNode> for BranchPr {
    fn from(n: HeadNode) -> Self {
        let checks = n
            .commits
            .nodes
            .into_iter()
            .next()
            .and_then(|c| c.commit)
            .and_then(|c| c.status_check_rollup)
            .and_then(|r| r.state);
        BranchPr {
            number: n.number,
            title: n.title,
            url: n.url,
            state: n.state,
            author: n.author.map(|a| a.login).unwrap_or_else(|| "ghost".into()),
            head_oid: n.head_ref_oid,
            head_repo: n.head_repository.map(|r| r.name_with_owner),
            checks,
        }
    }
}

/// The fields of a PR that worktree commands show and decide on.
const FIELDS: &str = "number title url state headRefOid author { login } \
     headRepository { nameWithOwner } \
     commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }";

/// Branches looked up per query, each as one aliased field.
const BATCH: usize = 25;

/// The query for one batch: an aliased `pullRequests(headRefName:)` per branch, newest first.
fn batch_query(count: usize) -> String {
    let params: Vec<String> = (0..count).map(|i| format!("$b{i}: String!")).collect();
    let mut query = format!(
        "query($owner: String!, $repo: String!, {}) {{\n  repository(owner: $owner, name: $repo) {{\n",
        params.join(", ")
    );
    for i in 0..count {
        query.push_str(&format!(
            "    b{i}: pullRequests(headRefName: $b{i}, first: 10, \
             orderBy: {{field: UPDATED_AT, direction: DESC}}) {{ ...Head }}\n"
        ));
    }
    query.push_str(&format!(
        "  }}\n}}\nfragment Head on PullRequestConnection {{\n  nodes {{ {FIELDS} }}\n}}"
    ));
    query
}

/// The PRs whose head branch is each of `branches`, newest first, in any state. A branch with no PR
/// maps to an empty list. Names match across forks, so a list can hold other people's PRs.
pub fn prs_by_branch(
    client: &Client,
    repo: &Repo,
    branches: &[String],
) -> Result<HashMap<String, Vec<BranchPr>>> {
    let mut found = HashMap::new();
    for batch in branches.chunks(BATCH) {
        let mut variables = Map::new();
        variables.insert("owner".into(), json!(repo.owner));
        variables.insert("repo".into(), json!(repo.name));
        for (i, branch) in batch.iter().enumerate() {
            variables.insert(format!("b{i}"), json!(branch));
        }
        let data = client.graphql(&batch_query(batch.len()), Value::Object(variables))?;
        let repository = &data["repository"];
        for (i, branch) in batch.iter().enumerate() {
            let nodes: Option<Nodes<BranchPr>> =
                serde_json::from_value(repository[format!("b{i}")].clone())
                    .with_context(|| format!("Failed to parse the PRs for {branch}"))?;
            found.insert(branch.clone(), nodes.map(|n| n.nodes).unwrap_or_default());
        }
    }
    Ok(found)
}

/// One PR by number, or None when the repo has no such PR.
pub fn pr_by_number(client: &Client, repo: &Repo, number: u64) -> Result<Option<BranchPr>> {
    let query = format!(
        "query($owner: String!, $repo: String!, $number: Int!) {{\n  \
         repository(owner: $owner, name: $repo) {{ pullRequest(number: $number) {{ {FIELDS} }} }}\n}}"
    );
    let data = client.graphql(
        &query,
        json!({ "owner": repo.owner, "repo": repo.name, "number": number }),
    )?;
    serde_json::from_value(data["repository"]["pullRequest"].clone())
        .with_context(|| format!("Failed to parse PR #{number}"))
}

/// Every open PR in `repo` by anyone, by number. Listing the repo's PRs avoids the 1000-result cap
/// of search.
pub fn open_prs(client: &Client, repo: &Repo) -> Result<Vec<BranchPr>> {
    let query = format!(
        "query($owner: String!, $repo: String!, $cursor: String) {{\n  \
         repository(owner: $owner, name: $repo) {{\n    \
         pullRequests(states: OPEN, first: 100, after: $cursor) {{\n      \
         pageInfo {{ hasNextPage endCursor }}\n      nodes {{ {FIELDS} }}\n    }}\n  }}\n}}"
    );
    let mut prs = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let data = client.graphql(
            &query,
            json!({ "owner": repo.owner, "repo": repo.name, "cursor": cursor }),
        )?;
        let page: Connection<BranchPr> =
            serde_json::from_value(data["repository"]["pullRequests"].clone())
                .with_context(|| format!("Failed to parse the open PRs of {repo}"))?;
        prs.extend(page.nodes);
        match page.page_info.next_cursor() {
            Some(c) => cursor = Value::String(c),
            None => break,
        }
    }
    prs.sort_by_key(|p| p.number);
    Ok(prs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    #[test]
    fn query_aliases_one_field_per_branch() {
        let q = batch_query(2);
        assert!(q.contains("$b0: String!, $b1: String!"));
        assert!(q.contains("b1: pullRequests(headRefName: $b1"));
        assert!(!q.contains("$b2"));
        assert!(q.contains("headRepository { nameWithOwner }"));
    }

    #[test]
    fn looks_up_one_pr_by_number() {
        let mut server = Server::new();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"variables": {"number": 7}})))
            .with_body(
                json!({"data": {"repository": {"pullRequest": {
                    "number": 7, "state": "CLOSED", "headRefOid": "abc",
                    "headRepository": {"nameWithOwner": "fork/r"}, "commits": {"nodes": []}
                }}}})
                .to_string(),
            )
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"variables": {"number": 8}})))
            .with_body(r#"{"data": {"repository": {"pullRequest": null}}}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        let pr = pr_by_number(&client, &repo, 7).unwrap().unwrap();
        assert_eq!(pr.head_repo.as_deref(), Some("fork/r"));
        assert_eq!(pr_by_number(&client, &repo, 8).unwrap(), None);
    }

    #[test]
    fn open_prs_pages_and_sorts() {
        let mut server = Server::new();
        let page = |next: Option<&str>, number: u64, author: Value| {
            json!({"data": {"repository": {"pullRequests": {
                "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next},
                "nodes": [{
                    "number": number, "state": "OPEN", "author": author,
                    "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE"}}}]}
                }]
            }}}})
            .to_string()
        };
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({"variables": {"owner": "o", "repo": "r", "cursor": null}}),
            ))
            .with_body(page(Some("c1"), 9, json!({"login": "someone"})))
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"variables": {"cursor": "c1"}})))
            .with_body(page(None, 3, json!(null)))
            .create();
        let client = Client::for_tests(&server.url());
        let prs = open_prs(&client, &Repo::parse("o/r").unwrap()).unwrap();
        let got: Vec<(u64, &str)> = prs.iter().map(|p| (p.number, p.author.as_str())).collect();
        assert_eq!(got, vec![(3, "ghost"), (9, "someone")]);
        assert_eq!(prs[0].checks.as_deref(), Some("FAILURE"));
    }

    #[test]
    fn looks_up_branches_in_batches() {
        let mut server = Server::new();
        let branches: Vec<String> = (0..26).map(|i| format!("fix/{i}")).collect();
        let pr = json!({
            "number": 42, "title": "Fix", "url": "https://github.com/o/r/pull/42",
            "state": "MERGED", "headRefOid": "abc", "author": {"login": "me"},
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "SUCCESS"}}}]}
        });
        let mut first = serde_json::Map::new();
        for i in 0..25 {
            first.insert(format!("b{i}"), json!({"nodes": []}));
        }
        first.insert("b3".into(), json!({"nodes": [pr]}));
        let one = server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({"variables": {"owner": "o", "repo": "r", "b0": "fix/0", "b24": "fix/24"}}),
            ))
            .with_body(json!({"data": {"repository": first}}).to_string())
            .create();
        let two = server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"variables": {"b0": "fix/25"}})))
            .with_body(
                json!({"data": {"repository": {"b0": {"nodes": [{
                    "number": 7, "state": "OPEN", "author": null, "commits": {"nodes": []}
                }]}}}})
                .to_string(),
            )
            .create();
        let client = Client::for_tests(&server.url());
        let found = prs_by_branch(&client, &Repo::parse("o/r").unwrap(), &branches).unwrap();
        assert_eq!(found.len(), 26);
        assert!(found["fix/0"].is_empty());
        let merged = &found["fix/3"][0];
        assert_eq!(
            (merged.number, merged.state.as_str(), merged.author.as_str()),
            (42, "MERGED", "me")
        );
        assert_eq!(merged.checks.as_deref(), Some("SUCCESS"));
        let open = &found["fix/25"][0];
        assert_eq!((open.number, open.author.as_str()), (7, "ghost"));
        assert_eq!(open.checks, None);
        one.assert();
        two.assert();
    }
}

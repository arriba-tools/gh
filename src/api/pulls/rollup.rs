//! The status-check rollup of a PR's head commit: check runs and commit statuses.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{Connection, RepoData, nullable, query, vars};
use crate::api::{Client, PrRef};

/// One check run on the PR's head commit.
#[derive(Clone, Debug, Deserialize)]
#[serde(from = "CheckRunNode")]
pub struct CheckRun {
    pub name: String,
    pub workflow: String,
    pub event: String,
    /// Actions workflow run, absent for checks from other apps.
    pub run_id: Option<u64>,
    /// When the workflow run was created, which reruns of it leave unchanged.
    pub run_created_at: Option<DateTime<Utc>>,
    pub status: String,
    pub conclusion: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    /// Page with the check's details, such as the Actions job.
    pub url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckRunNode {
    #[serde(default, deserialize_with = "nullable")]
    name: String,
    #[serde(default, deserialize_with = "nullable")]
    status: String,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    details_url: Option<String>,
    check_suite: Option<CheckSuite>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckSuite {
    workflow_run: Option<WorkflowRun>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRun {
    database_id: Option<u64>,
    #[serde(default, deserialize_with = "nullable")]
    event: String,
    created_at: Option<String>,
    workflow: Option<Workflow>,
}

#[derive(Deserialize)]
struct Workflow {
    #[serde(default, deserialize_with = "nullable")]
    name: String,
}

impl From<CheckRunNode> for CheckRun {
    fn from(node: CheckRunNode) -> Self {
        let run = node.check_suite.and_then(|s| s.workflow_run);
        let (workflow, event, run_id, run_created_at) = match run {
            Some(r) => (
                r.workflow.map(|w| w.name).unwrap_or_default(),
                r.event,
                r.database_id,
                lenient_time(r.created_at.as_deref()),
            ),
            None => (String::new(), String::new(), None, None),
        };
        CheckRun {
            name: node.name,
            workflow,
            event,
            run_id,
            run_created_at,
            status: node.status,
            conclusion: node.conclusion,
            started_at: lenient_time(node.started_at.as_deref()),
            completed_at: lenient_time(node.completed_at.as_deref()),
            url: node.details_url,
        }
    }
}

/// An RFC 3339 timestamp, or None when it is missing or malformed.
fn lenient_time(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

/// A commit status from an external service.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusContext {
    #[serde(default, deserialize_with = "nullable")]
    pub context: String,
    /// SUCCESS, FAILURE, ERROR, PENDING, or EXPECTED.
    #[serde(default, deserialize_with = "nullable")]
    pub state: String,
    pub target_url: Option<String>,
}

/// The checks on a PR's head commit, plus the PR fields the caller asked for.
pub struct Rollup<T> {
    /// The PR fields from the first page.
    pub pr: T,
    pub runs: Vec<CheckRun>,
    pub statuses: Vec<StatusContext>,
}

/// PR fields `pr checks` reports alongside the checks.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusFields {
    #[serde(default, deserialize_with = "nullable")]
    pub title: String,
    #[serde(default, deserialize_with = "nullable")]
    pub url: String,
    /// OPEN, CLOSED, or MERGED.
    pub state: Option<String>,
    pub is_draft: Option<bool>,
    pub mergeable: Option<String>,
    pub merge_state_status: Option<String>,
    pub review_decision: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub head_ref_oid: String,
    pub commits: Option<TotalCount>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TotalCount {
    pub total_count: u64,
}

const STATUS_FIELDS: &str = "title url state isDraft mergeable mergeStateStatus reviewDecision headRefOid commits { totalCount }";

/// The PR's title, as `pr retry` shows it.
#[derive(Clone, Debug, Deserialize)]
pub struct TitleFields {
    #[serde(default, deserialize_with = "nullable")]
    pub title: String,
}

/// Every check on the PR's head commit, with the fields `pr checks` reports.
pub fn checks_with_status(client: &Client, pr: &PrRef) -> Result<Rollup<StatusFields>> {
    fetch(client, pr, STATUS_FIELDS)
}

/// Every check on the PR's head commit, with the PR's title.
pub fn checks_with_title(client: &Client, pr: &PrRef) -> Result<Rollup<TitleFields>> {
    fetch(client, pr, "title")
}

/// `__FIELDS__` is replaced with the PR fields each caller needs alongside the checks.
const ROLLUP_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      __FIELDS__
      last: commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              contexts(first: 100, after: $cursor) {
                pageInfo { hasNextPage endCursor }
                nodes {
                  __typename
                  ... on CheckRun {
                    name status conclusion startedAt completedAt detailsUrl
                    checkSuite { workflowRun { databaseId event createdAt workflow { name } } }
                  }
                  ... on StatusContext { context state targetUrl }
                }
              }
            }
          }
        }
      }
    }
  }
}"#;

/// One page of the rollup, with the requested PR fields beside it.
#[derive(Deserialize)]
struct Page<T> {
    #[serde(flatten)]
    fields: T,
    last: LastCommit,
}

#[derive(Deserialize)]
struct LastCommit {
    #[serde(default, deserialize_with = "nullable")]
    nodes: Vec<CommitNode>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: Commit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    status_check_rollup: Option<CheckRollup>,
}

#[derive(Deserialize)]
struct CheckRollup {
    contexts: Connection<ContextNode>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum ContextNode {
    CheckRun(CheckRun),
    StatusContext(StatusContext),
    #[serde(other)]
    Other,
}

/// Fetch every check run and commit status on the PR's head commit, with `fields` selected on
/// the pull request, such as `title url`.
fn fetch<T: DeserializeOwned>(client: &Client, pr: &PrRef, fields: &str) -> Result<Rollup<T>> {
    let query_text = ROLLUP_QUERY.replace("__FIELDS__", fields);
    let mut pr_fields = None;
    let mut runs = Vec::new();
    let mut statuses = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let mut variables = vars(pr);
        variables["cursor"] = cursor;
        let data: RepoData<Option<Page<T>>> = query(client, &query_text, variables)?;
        let page = data
            .repository
            .pull_request
            .with_context(|| format!("Pull request {pr} not found"))?;
        // A commit with no checks at all has a null rollup, which has no next page either.
        let contexts = page
            .last
            .nodes
            .into_iter()
            .next()
            .and_then(|n| n.commit.status_check_rollup)
            .map(|r| r.contexts);
        let mut next = None;
        if let Some(contexts) = contexts {
            for node in contexts.nodes {
                match node {
                    ContextNode::CheckRun(run) => runs.push(run),
                    ContextNode::StatusContext(status) => statuses.push(status),
                    ContextNode::Other => {}
                }
            }
            next = contexts.page_info.next_cursor();
        }
        if pr_fields.is_none() {
            pr_fields = Some(page.fields);
        }
        match next {
            Some(c) => cursor = Value::String(c),
            None => break,
        }
    }
    Ok(Rollup {
        pr: pr_fields.context("Empty response for checks")?,
        runs,
        statuses,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Repo;
    use mockito::{Matcher, Server};
    use serde_json::json;

    fn pr() -> PrRef {
        PrRef {
            repo: Repo::parse("o/r").unwrap(),
            number: 7,
        }
    }

    #[test]
    fn check_run_carries_its_workflow_run_creation_time() {
        let node = json!({
            "name": "Ubuntu", "status": "COMPLETED", "conclusion": "FAILURE",
            "checkSuite": {"workflowRun": {
                "databaseId": 10, "event": "push", "createdAt": "2026-01-02T03:04:05Z"
            }}
        });
        let run: CheckRun = serde_json::from_value(node).unwrap();
        assert_eq!(
            run.run_created_at,
            Some("2026-01-02T03:04:05Z".parse().unwrap())
        );
        let app: CheckRun = serde_json::from_value(json!({"name": "coderabbit"})).unwrap();
        assert_eq!(app.run_created_at, None);
    }

    fn rollup_page(fields: Value, next: Option<&str>, nodes: Value) -> String {
        let mut pr = fields;
        pr["last"] = json!({ "nodes": [{ "commit": { "statusCheckRollup": { "contexts": {
            "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next },
            "nodes": nodes
        }}}}]});
        json!({ "data": { "repository": { "pullRequest": pr } } }).to_string()
    }

    #[test]
    fn checks_with_title_pages_through_contexts() {
        let mut server = Server::new();
        let first = server
            .mock("POST", "/graphql")
            .match_body(Matcher::AllOf(vec![
                // Only the title is selected beside the checks.
                Matcher::Regex(r"\{\\n +title\\n +last:".into()),
                Matcher::PartialJson(json!({"variables": {"number": 7, "cursor": null}})),
            ]))
            .with_body(rollup_page(
                json!({"title": "Fix"}),
                Some("c1"),
                json!([{ "__typename": "CheckRun", "name": "build", "status": "COMPLETED",
                         "conclusion": "SUCCESS" }]),
            ))
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"variables": {"cursor": "c1"}})))
            .with_body(rollup_page(
                json!({"title": "Ignored"}),
                None,
                json!([{ "__typename": "StatusContext", "context": "ci", "state": "PENDING" }]),
            ))
            .create();
        let client = Client::for_tests(&server.url());
        let rollup = checks_with_title(&client, &pr()).unwrap();
        first.assert();
        assert_eq!(rollup.pr.title, "Fix");
        assert_eq!(rollup.runs.len(), 1);
        assert_eq!(rollup.statuses[0].context, "ci");
    }

    #[test]
    fn checks_with_status_on_a_commit_without_checks() {
        let mut server = Server::new();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("mergeStateStatus reviewDecision".into()))
            .with_body(
                json!({"data": {"repository": {"pullRequest": {
                    "title": "Fix", "url": "u", "state": "OPEN", "isDraft": true,
                    "headRefOid": "abc", "commits": {"totalCount": 1},
                    "last": {"nodes": [{"commit": {"statusCheckRollup": null}}]}
                }}}})
                .to_string(),
            )
            .expect(1)
            .create();
        let client = Client::for_tests(&server.url());
        let rollup = checks_with_status(&client, &pr()).unwrap();
        assert_eq!(rollup.pr.is_draft, Some(true));
        assert!(rollup.runs.is_empty() && rollup.statuses.is_empty());
    }

    #[test]
    fn checks_of_missing_pr() {
        let mut server = Server::new();
        server
            .mock("POST", "/graphql")
            .with_body(r#"{"data": {"repository": {"pullRequest": null}}}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let err = checks_with_title(&client, &pr()).err().unwrap().to_string();
        assert_eq!(err, "Pull request o/r#7 not found");
    }

    #[test]
    fn page_splits_runs_statuses_and_fields() {
        let page = json!({
            "title": "Fix",
            "last": { "nodes": [{ "commit": { "statusCheckRollup": { "contexts": {
                "pageInfo": { "hasNextPage": true, "endCursor": "c1" },
                "nodes": [
                    {
                        "__typename": "CheckRun", "name": "Ubuntu", "status": "COMPLETED",
                        "conclusion": "FAILURE", "startedAt": "2026-01-01T00:00:00Z",
                        "completedAt": "bad", "detailsUrl": "https://d",
                        "checkSuite": { "workflowRun": {
                            "databaseId": 9, "event": "pull_request", "workflow": { "name": "CI" }
                        }}
                    },
                    {
                        "__typename": "CheckRun", "name": "coderabbit", "status": "COMPLETED",
                        "conclusion": "SUCCESS", "checkSuite": { "workflowRun": null }
                    },
                    { "__typename": "StatusContext", "context": "ci/jenkins", "state": "ERROR",
                      "targetUrl": null },
                    { "__typename": "Unknown" }
                ]
            }}}}]}
        });
        let page: Page<TitleFields> = serde_json::from_value(page).unwrap();
        assert_eq!(page.fields.title, "Fix");
        let contexts = page.last.nodes.into_iter().next().unwrap();
        let contexts = contexts.commit.status_check_rollup.unwrap().contexts;
        assert_eq!(contexts.page_info.next_cursor().as_deref(), Some("c1"));
        let [
            ContextNode::CheckRun(ubuntu),
            ContextNode::CheckRun(app),
            ContextNode::StatusContext(status),
            ContextNode::Other,
        ] = contexts.nodes.as_slice()
        else {
            panic!("unexpected nodes");
        };
        assert_eq!(
            (
                ubuntu.workflow.as_str(),
                ubuntu.event.as_str(),
                ubuntu.run_id
            ),
            ("CI", "pull_request", Some(9))
        );
        assert!(ubuntu.started_at.is_some());
        assert_eq!(ubuntu.completed_at, None);
        assert_eq!((app.workflow.as_str(), app.run_id), ("", None));
        assert_eq!(status.context, "ci/jenkins");
        assert_eq!(status.target_url, None);
    }

    #[test]
    fn commit_without_checks_has_null_rollup() {
        let page = json!({
            "title": "Fix",
            "last": { "nodes": [{ "commit": { "statusCheckRollup": null } }] }
        });
        let page: Page<TitleFields> = serde_json::from_value(page).unwrap();
        assert!(page.last.nodes[0].commit.status_check_rollup.is_none());
    }

    #[test]
    fn status_fields_tolerate_nulls() {
        let fields: StatusFields = serde_json::from_value(json!({
            "title": "Fix", "url": "u", "state": "OPEN", "isDraft": false,
            "mergeable": "UNKNOWN", "mergeStateStatus": null, "reviewDecision": null,
            "headRefOid": "abc", "commits": { "totalCount": 2 }
        }))
        .unwrap();
        assert_eq!(fields.commits.map(|c| c.total_count), Some(2));
        assert_eq!(fields.review_decision, None);
    }
}

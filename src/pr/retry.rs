use anyhow::{Context, Result};
use chrono::{TimeDelta, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};

use crate::api::pulls::{self, CheckRun, StatusContext};
use crate::api::{Client, PrRef, Repo, is_failed, parse_pr_ref};
use crate::cli::RetryArgs;
use crate::context::{Ctx, git};
use crate::pr::rollup::latest;
use crate::{output, parallel, prompt};

/// GitHub allows a workflow run to be rerun for this long after its initial run.
const RERUN_WINDOW_DAYS: i64 = 30;

struct Checks {
    title: String,
    runs: Vec<CheckRun>,
    /// Commit statuses from external services, which cannot be rerun.
    statuses: Vec<StatusContext>,
}

fn fetch_checks(client: &Client, pr: &PrRef) -> Result<Checks> {
    let rollup = pulls::checks_with_title(client, pr)?;
    Ok(Checks {
        title: rollup.pr.title,
        runs: rollup.runs,
        statuses: rollup.statuses,
    })
}

#[derive(Debug, Serialize)]
struct FailedJob {
    name: String,
    conclusion: String,
}

#[derive(Debug, Serialize)]
struct RunPlan {
    run_id: u64,
    workflow: String,
    jobs: Vec<FailedJob>,
    /// "rerun", "already rerun", "dry-run", or the error from the rerun request.
    result: String,
}

#[derive(Debug, Serialize)]
struct PrPlan {
    repo: String,
    number: u64,
    title: String,
    runs: Vec<RunPlan>,
    /// Failed checks that can't be rerun, such as external commit statuses or expired runs.
    skipped: Vec<String>,
    pending: usize,
    /// GitHub Actions status when checks are stuck pending with no failures.
    #[serde(skip_serializing_if = "Option::is_none")]
    actions_status: Option<String>,
}

/// Group failed check runs by workflow run, and collect what cannot be rerun.
fn plan(pr: &PrRef, checks: Checks) -> PrPlan {
    let runs = latest(checks.runs);
    let pending = runs.iter().filter(|r| r.status != "COMPLETED").count()
        + checks
            .statuses
            .iter()
            .filter(|s| matches!(s.state.as_str(), "PENDING" | "EXPECTED"))
            .count();

    let cutoff = Utc::now() - TimeDelta::days(RERUN_WINDOW_DAYS);
    let mut groups: BTreeMap<(String, u64), Vec<FailedJob>> = BTreeMap::new();
    let mut skipped = Vec::new();
    for run in &runs {
        if !is_failed(run.conclusion.as_deref()) {
            continue;
        }
        let conclusion = run.conclusion.as_deref().unwrap_or_default().to_lowercase();
        match run.run_id {
            Some(id) if run.run_created_at.is_some_and(|t| t < cutoff) => {
                // One entry per run, however many of its jobs failed.
                let entry = format!(
                    "{} ({id}, older than {RERUN_WINDOW_DAYS} days)",
                    run.workflow
                );
                if !skipped.contains(&entry) {
                    skipped.push(entry);
                }
            }
            Some(id) => groups
                .entry((run.workflow.clone(), id))
                .or_default()
                .push(FailedJob {
                    name: run.name.clone(),
                    conclusion,
                }),
            None => skipped.push(format!("{} ({conclusion})", run.name)),
        }
    }
    for status in &checks.statuses {
        if matches!(status.state.as_str(), "FAILURE" | "ERROR") {
            skipped.push(format!(
                "{} ({})",
                status.context,
                status.state.to_lowercase()
            ));
        }
    }

    PrPlan {
        repo: pr.repo.to_string(),
        number: pr.number,
        title: checks.title,
        runs: groups
            .into_iter()
            .map(|((workflow, run_id), jobs)| RunPlan {
                run_id,
                workflow,
                jobs,
                result: String::new(),
            })
            .collect(),
        skipped,
        pending,
        actions_status: None,
    }
}

/// Render a PR's failed runs as a tree.
fn render(plan: &PrPlan) -> String {
    let mut out = format!("PR #{} {}\n", plan.number, plan.title);
    let n = plan.runs.len();
    for (i, run) in plan.runs.iter().enumerate() {
        let last = i + 1 == n;
        let (branch, stem) = if last {
            ("└── ", "    ")
        } else {
            ("├── ", "│   ")
        };
        out.push_str(&format!("{branch}{} ({})\n", run.workflow, run.run_id));
        let m = run.jobs.len();
        for (j, job) in run.jobs.iter().enumerate() {
            let leaf = if j + 1 == m {
                "└── "
            } else {
                "├── "
            };
            out.push_str(&format!("{stem}{leaf}{} ({})\n", job.name, job.conclusion));
        }
    }
    out
}

/// What the positional argument names.
#[derive(Debug, PartialEq)]
enum Target {
    Pr(Option<String>),
    Repo(Option<Repo>),
}

/// A PR reference, or OWNER/REPO and repo URLs which mean every PR of yours in that repo, or every
/// PR with `--all`. A two-part name that is also a local branch is taken as the branch. `sweep`
/// names the flag that asked for a whole repo.
fn classify(
    arg: Option<&str>,
    sweep: Option<&str>,
    is_branch: impl Fn(&str) -> bool,
) -> Result<Target> {
    let Some(a) = arg else {
        return Ok(if sweep.is_some() {
            Target::Repo(None)
        } else {
            Target::Pr(None)
        });
    };
    if parse_pr_ref(a).is_some() || a.trim_start_matches('#').parse::<u64>().is_ok() {
        if let Some(flag) = sweep {
            anyhow::bail!("{flag} takes a repository, not a PR");
        }
        return Ok(Target::Pr(Some(a.to_string())));
    }
    let looks_like_repo =
        a.contains("://") || (a.split('/').count() == 2 && !a.starts_with('/') && !is_branch(a));
    if looks_like_repo {
        return Ok(Target::Repo(Some(Repo::parse(a)?)));
    }
    if let Some(flag) = sweep {
        anyhow::bail!("{flag} takes a repository, not a branch");
    }
    Ok(Target::Pr(Some(a.to_string())))
}

fn local_branch_exists(name: &str) -> bool {
    git(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/heads/{name}"),
    ])
    .is_ok()
}

/// Open PRs in `repo`, by anyone, that may have something to rerun. A passing rollup has nothing,
/// nor does a head commit without checks.
fn unsettled_prs(client: &Client, repo: &Repo) -> Result<Vec<PrRef>> {
    Ok(pulls::open_prs(client, repo)?
        .into_iter()
        .filter(|p| p.checks.as_deref().is_some_and(|c| c != "SUCCESS"))
        .map(|p| PrRef {
            repo: repo.clone(),
            number: p.number,
        })
        .collect())
}

/// The question to ask before rerunning across a repo, or None with nothing to rerun.
fn sweep_question(plans: &[PrPlan]) -> Option<String> {
    let with_runs: Vec<&PrPlan> = plans.iter().filter(|p| !p.runs.is_empty()).collect();
    // PRs on one head commit share its runs.
    let runs: HashSet<u64> = with_runs
        .iter()
        .flat_map(|p| p.runs.iter().map(|r| r.run_id))
        .collect();
    with_runs.first().map(|first| {
        format!(
            "Rerun {} across {} in {}?",
            output::plural(runs.len(), "workflow run"),
            output::plural(with_runs.len(), "PR"),
            first.repo
        )
    })
}

/// Status of the Actions component on githubstatus.com.
fn actions_status() -> Result<String> {
    let body: Value = reqwest::blocking::get("https://www.githubstatus.com/api/v2/components.json")
        .context("Failed to reach githubstatus.com")?
        .json()
        .context("Failed to parse githubstatus.com response")?;
    body["components"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"].as_str() == Some("Actions"))
        .and_then(|c| c["status"].as_str())
        .map(str::to_string)
        .context("Actions component missing from githubstatus.com")
}

pub fn run(ctx: &Ctx, args: RetryArgs) -> Result<()> {
    let client = &ctx.client;
    let sweep = match (args.all, args.mine) {
        (true, _) => Some("--all"),
        (_, true) => Some("--mine"),
        _ => None,
    };
    let prs = match classify(args.target.as_deref(), sweep, local_branch_exists)? {
        Target::Pr(arg) => vec![ctx.pr(arg.as_deref())?],
        Target::Repo(repo) if args.all => {
            let repo = ctx.repo_or(repo)?;
            let prs = unsettled_prs(client, &repo)?;
            if prs.is_empty() && !ctx.json {
                println!("Nothing to retry on the open PRs in {repo}");
            }
            prs
        }
        Target::Repo(repo) => {
            let repo = ctx.repo_or(repo)?;
            let prs = pulls::my_open_prs(client, &repo)?;
            if prs.is_empty() {
                println!("No open PRs by {} in {repo}", client.viewer()?);
                return Ok(());
            }
            prs
        }
    };

    let mut plans = Vec::new();
    let mut status_checked: Option<String> = None;
    let fetched = parallel::map(&prs, |pr| fetch_checks(client, pr));
    for (pr, checks) in prs.iter().zip(fetched) {
        let mut plan = plan(pr, checks?);

        if plan.runs.is_empty() && plan.pending > 0 {
            // Checks stuck pending with nothing failed often means an Actions outage.
            let status = match &status_checked {
                Some(s) => s.clone(),
                None => actions_status().unwrap_or_else(|e| format!("unknown ({e})")),
            };
            status_checked = Some(status.clone());
            plan.actions_status = Some(status);
        }

        if !ctx.json {
            print_summary(&plan);
        }
        plans.push(plan);
    }

    // --all reruns other people's CI, so it asks once against the whole tree.
    if args.all
        && !args.dry_run
        && let Some(question) = sweep_question(&plans)
        && !prompt::confirm(&question, ctx.yes)?
    {
        if ctx.json {
            eprintln!("Cancelled");
        } else {
            println!("Cancelled");
        }
        return Ok(());
    }

    // PRs on one head commit share its runs. A refused run stays eligible for the next PR.
    let mut sent = HashSet::new();
    for (pr, plan) in prs.iter().zip(&mut plans) {
        for run in &mut plan.runs {
            if !args.dry_run && sent.contains(&run.run_id) {
                run.result = "already rerun".to_string();
                continue;
            }
            run.result = if args.dry_run {
                "dry-run".to_string()
            } else {
                let path = format!(
                    "{}/actions/runs/{}/rerun-failed-jobs",
                    pr.repo.api(),
                    run.run_id
                );
                match client.post(&path, &json!({})) {
                    Ok(_) => {
                        sent.insert(run.run_id);
                        "rerun".to_string()
                    }
                    Err(e) => format!("{e:#}"),
                }
            };
            if !ctx.json && !args.dry_run {
                match run.result.as_str() {
                    "rerun" => println!("rerun {} ({})", run.workflow, run.run_id),
                    err => eprintln!("failed to rerun {} ({}): {err}", run.workflow, run.run_id),
                }
            }
        }
    }

    if ctx.json {
        return output::print_json(&plans);
    }
    Ok(())
}

/// The state of a PR with no runs to rerun. Skipped checks still failed, so it is not passing.
fn idle_state(plan: &PrPlan) -> String {
    if !plan.skipped.is_empty() {
        "nothing to rerun".to_string()
    } else if plan.pending > 0 {
        format!("no failures, {} pending", plan.pending)
    } else {
        "all checks passing".to_string()
    }
}

fn print_summary(plan: &PrPlan) {
    if plan.runs.is_empty() {
        println!("PR #{} {}: {}", plan.number, plan.title, idle_state(plan));
    } else {
        print!("{}", render(plan));
    }
    for s in &plan.skipped {
        println!("  skipped {s}");
    }
    if let Some(status) = plan
        .actions_status
        .as_deref()
        .filter(|s| *s != "operational")
    {
        println!("  warning: GitHub Actions is {status}, runs may be stuck");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Mock, Server};

    fn ctx(server: &Server, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    fn node(name: &str, workflow: &str, run_id: u64, conclusion: &str) -> Value {
        json!({
            "__typename": "CheckRun", "name": name, "status": "COMPLETED",
            "conclusion": conclusion, "startedAt": "2026-01-01T00:00:00Z",
            "checkSuite": {"workflowRun": {
                "databaseId": run_id, "event": "pull_request", "workflow": {"name": workflow}
            }}
        })
    }

    fn mock_rollup(server: &mut Server, number: u64, nodes: Vec<Value>) -> Mock {
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({"variables": {"number": number}}),
            ))
            .with_body(
                json!({"data": {"repository": {"pullRequest": {
                    "title": "Fix",
                    "last": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
                        "pageInfo": {"hasNextPage": false}, "nodes": nodes
                    }}}}]}
                }}}})
                .to_string(),
            )
            .create()
    }

    fn failing() -> Vec<Value> {
        vec![
            node("Ubuntu", "Build", 10, "FAILURE"),
            node("Lint", "Lint", 11, "TIMED_OUT"),
            node("Docs", "Docs", 12, "SUCCESS"),
            json!({"__typename": "StatusContext", "context": "ci/jenkins", "state": "ERROR"}),
        ]
    }

    fn args(target: &str, mine: bool, dry_run: bool) -> RetryArgs {
        RetryArgs {
            target: Some(target.into()),
            mine,
            all: false,
            dry_run,
        }
    }

    /// `--all` on the -R repo of [`ctx`].
    fn sweep(dry_run: bool) -> RetryArgs {
        RetryArgs {
            target: None,
            mine: false,
            all: true,
            dry_run,
        }
    }

    fn mock_open_prs(server: &mut Server, rollups: &[(u64, Option<&str>)]) -> Mock {
        let nodes: Vec<Value> = rollups
            .iter()
            .map(|(number, state)| {
                let rollup = state.map(|s| json!({"state": s}));
                json!({
                    "number": number, "state": "OPEN",
                    "commits": {"nodes": [{"commit": {"statusCheckRollup": rollup}}]}
                })
            })
            .collect();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex(r"pullRequests\(states: OPEN".into()))
            .with_body(
                json!({"data": {"repository": {"pullRequests": {
                    "pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": nodes
                }}}})
                .to_string(),
            )
            .create()
    }

    #[test]
    fn dry_run_reruns_nothing() {
        let mut server = Server::new();
        mock_rollup(&mut server, 7, failing());
        let rerun = server
            .mock("POST", Matcher::Regex("rerun-failed-jobs$".into()))
            .expect(0)
            .create();
        run(&ctx(&server, true), args("7", false, true)).unwrap();
        run(&ctx(&server, false), args("o/r#7", false, true)).unwrap();
        rerun.assert();
    }

    #[test]
    fn reruns_each_failed_workflow_run_once() {
        let mut server = Server::new();
        mock_rollup(&mut server, 7, failing());
        let build = server
            .mock("POST", "/repos/o/r/actions/runs/10/rerun-failed-jobs")
            .with_status(201)
            .expect(2)
            .create();
        // A refused rerun is reported, and the others still go out.
        let lint = server
            .mock("POST", "/repos/o/r/actions/runs/11/rerun-failed-jobs")
            .with_status(403)
            .with_body(r#"{"message": "Must have admin rights"}"#)
            .expect(2)
            .create();
        run(&ctx(&server, false), args("7", false, false)).unwrap();
        run(&ctx(&server, true), args("7", false, false)).unwrap();
        build.assert();
        lint.assert();
    }

    #[test]
    fn mine_covers_every_open_pr_in_the_repo() {
        let mut server = Server::new();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex(r"query \{ viewer".into()))
            .with_body(r#"{"data": {"viewer": {"login": "me"}}}"#)
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"variables": {
                "q": "is:pr is:open author:me repo:a/b"
            }})))
            .with_body(
                r#"{"data": {"search": {"pageInfo": {"hasNextPage": false},
                    "nodes": [{"number": 2}, {"number": 1}]}}}"#,
            )
            .create();
        let one = mock_rollup(&mut server, 1, vec![node("Ubuntu", "Build", 10, "SUCCESS")]);
        let two = mock_rollup(&mut server, 2, vec![node("Ubuntu", "Build", 20, "SUCCESS")]);
        run(
            &ctx(&server, false),
            args("https://github.com/a/b", true, false),
        )
        .unwrap();
        one.assert();
        two.assert();
    }

    #[test]
    fn mine_without_open_prs() {
        let mut server = Server::new();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex(r"query \{ viewer".into()))
            .with_body(r#"{"data": {"viewer": {"login": "me"}}}"#)
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("search".into()))
            .with_body(r#"{"data": {"search": {"pageInfo": {"hasNextPage": false}, "nodes": []}}}"#)
            .create();
        run(
            &ctx(&server, true),
            args("https://github.com/a/b", true, false),
        )
        .unwrap();
        let err = run(&ctx(&server, true), args("7", true, false))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "--mine takes a repository, not a PR");
    }

    fn check(
        name: &str,
        workflow: &str,
        run_id: Option<u64>,
        conclusion: Option<&str>,
        at: &str,
    ) -> CheckRun {
        CheckRun {
            name: name.to_string(),
            workflow: workflow.to_string(),
            event: "pull_request".to_string(),
            run_id,
            run_created_at: None,
            status: if conclusion.is_some() {
                "COMPLETED"
            } else {
                "IN_PROGRESS"
            }
            .to_string(),
            conclusion: conclusion.map(str::to_string),
            started_at: Some(at.parse().unwrap()),
            completed_at: None,
            url: None,
        }
    }

    fn pr() -> PrRef {
        PrRef {
            repo: Repo::parse("zlib-ng/zlib-ng").unwrap(),
            number: 7,
        }
    }

    #[test]
    fn queued_rerun_supersedes_failure() {
        let mut queued = check("Build", "CI", Some(2), None, "2026-01-01T01:00:00Z");
        queued.started_at = None;
        let failed = check(
            "Build",
            "CI",
            Some(1),
            Some("FAILURE"),
            "2026-01-01T00:00:00Z",
        );
        for runs in [vec![failed.clone(), queued.clone()], vec![queued, failed]] {
            let kept = latest(runs);
            assert_eq!(kept.len(), 1);
            assert_eq!(kept[0].run_id, Some(2));
        }
    }

    #[test]
    fn rerun_attempt_supersedes_failure() {
        let runs = vec![
            check(
                "Build",
                "CI",
                Some(1),
                Some("FAILURE"),
                "2026-01-01T00:00:00Z",
            ),
            check(
                "Build",
                "CI",
                Some(2),
                Some("SUCCESS"),
                "2026-01-01T01:00:00Z",
            ),
        ];
        let kept = latest(runs);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].conclusion.as_deref(), Some("SUCCESS"));
    }

    #[test]
    fn plan_groups_failures_by_run_and_skips_external() {
        let checks = Checks {
            title: "Fix".to_string(),
            runs: vec![
                check(
                    "Ubuntu",
                    "Build",
                    Some(10),
                    Some("FAILURE"),
                    "2026-01-01T00:00:00Z",
                ),
                check(
                    "macOS",
                    "Build",
                    Some(10),
                    Some("CANCELLED"),
                    "2026-01-01T00:00:00Z",
                ),
                check(
                    "Windows",
                    "Build",
                    Some(10),
                    Some("SUCCESS"),
                    "2026-01-01T00:00:00Z",
                ),
                check(
                    "Lint",
                    "Lint",
                    Some(11),
                    Some("TIMED_OUT"),
                    "2026-01-01T00:00:00Z",
                ),
                check("Docs", "Docs", Some(12), None, "2026-01-01T00:00:00Z"),
                check(
                    "coderabbit",
                    "",
                    None,
                    Some("FAILURE"),
                    "2026-01-01T00:00:00Z",
                ),
            ],
            statuses: vec![StatusContext {
                context: "ci/jenkins".to_string(),
                state: "ERROR".to_string(),
                target_url: None,
            }],
        };
        let plan = plan(&pr(), checks);
        let runs: Vec<(u64, usize)> = plan.runs.iter().map(|r| (r.run_id, r.jobs.len())).collect();
        assert_eq!(runs, vec![(10, 2), (11, 1)]);
        assert_eq!(plan.pending, 1);
        assert_eq!(
            plan.skipped,
            vec!["coderabbit (failure)", "ci/jenkins (error)"]
        );
        assert_eq!(
            render(&plan),
            "PR #7 Fix\n\
             ├── Build (10)\n\
             │   ├── macOS (cancelled)\n\
             │   └── Ubuntu (failure)\n\
             └── Lint (11)\n    \
             └── Lint (timed_out)\n"
        );
    }

    #[test]
    fn all_reruns_failures_on_every_open_pr() {
        let mut server = Server::new();
        // Rollups for 2 and 3 are unmocked, so fetching either fails the run.
        mock_open_prs(
            &mut server,
            &[
                (1, Some("FAILURE")),
                (2, Some("SUCCESS")),
                (3, None),
                (4, Some("PENDING")),
            ],
        );
        mock_rollup(&mut server, 1, vec![node("Ubuntu", "Build", 10, "FAILURE")]);
        mock_rollup(&mut server, 4, vec![node("Lint", "Lint", 40, "CANCELLED")]);
        let build = server
            .mock("POST", "/repos/o/r/actions/runs/10/rerun-failed-jobs")
            .with_status(201)
            .create();
        let lint = server
            .mock("POST", "/repos/o/r/actions/runs/40/rerun-failed-jobs")
            .with_status(201)
            .create();
        run(&ctx(&server, false), sweep(false)).unwrap();
        build.assert();
        lint.assert();
    }

    #[test]
    fn all_dry_run_neither_asks_nor_reruns() {
        let mut server = Server::new();
        mock_open_prs(&mut server, &[(1, Some("FAILURE"))]);
        mock_rollup(&mut server, 1, vec![node("Ubuntu", "Build", 10, "FAILURE")]);
        let rerun = server
            .mock("POST", Matcher::Regex("rerun-failed-jobs$".into()))
            .expect(0)
            .create();
        let unconfirmed = Ctx {
            yes: false,
            ..ctx(&server, false)
        };
        run(&unconfirmed, sweep(true)).unwrap();
        rerun.assert();
    }

    #[test]
    fn all_reruns_nothing_unconfirmed() {
        // A terminal would block on the prompt.
        if prompt::interactive() {
            return;
        }
        let mut server = Server::new();
        mock_open_prs(&mut server, &[(1, Some("FAILURE"))]);
        mock_rollup(&mut server, 1, vec![node("Ubuntu", "Build", 10, "FAILURE")]);
        let rerun = server
            .mock("POST", Matcher::Regex("rerun-failed-jobs$".into()))
            .expect(0)
            .create();
        let unconfirmed = Ctx {
            yes: false,
            ..ctx(&server, false)
        };
        let err = run(&unconfirmed, sweep(false)).unwrap_err().to_string();
        assert_eq!(
            err,
            "Rerun 1 workflow run across 1 PR in o/r? Pass --yes to confirm without a prompt."
        );
        rerun.assert();
    }

    /// Two failing PRs on one head commit, so their rollups both report run 10.
    fn mock_shared_run(server: &mut Server) {
        mock_open_prs(server, &[(1, Some("FAILURE")), (2, Some("FAILURE"))]);
        mock_rollup(server, 1, vec![node("Ubuntu", "Build", 10, "FAILURE")]);
        mock_rollup(server, 2, vec![node("Ubuntu", "Build", 10, "FAILURE")]);
    }

    #[test]
    fn a_run_shared_by_two_prs_reruns_once() {
        let mut server = Server::new();
        mock_shared_run(&mut server);
        let rerun = server
            .mock("POST", "/repos/o/r/actions/runs/10/rerun-failed-jobs")
            .with_status(201)
            .expect(1)
            .create();
        run(&ctx(&server, false), sweep(false)).unwrap();
        rerun.assert();
    }

    #[test]
    fn a_refused_shared_run_is_tried_again() {
        let mut server = Server::new();
        mock_shared_run(&mut server);
        let rerun = server
            .mock("POST", "/repos/o/r/actions/runs/10/rerun-failed-jobs")
            .with_status(403)
            .with_body(r#"{"message": "Must have admin rights"}"#)
            .expect(2)
            .create();
        run(&ctx(&server, false), sweep(false)).unwrap();
        rerun.assert();
    }

    #[test]
    fn all_with_nothing_to_retry() {
        let mut server = Server::new();
        mock_open_prs(&mut server, &[(1, Some("SUCCESS")), (2, None)]);
        // Nothing is rerun, so nothing needs confirming either.
        let unconfirmed = Ctx {
            yes: false,
            ..ctx(&server, true)
        };
        run(&unconfirmed, sweep(false)).unwrap();
        run(&ctx(&server, false), sweep(false)).unwrap();
    }

    #[test]
    fn sweep_question_counts_only_prs_with_runs() {
        let plan_of = |number, runs: &[u64]| PrPlan {
            repo: "o/r".to_string(),
            number,
            title: "Fix".to_string(),
            runs: runs
                .iter()
                .map(|&run_id| RunPlan {
                    run_id,
                    workflow: "Build".to_string(),
                    jobs: Vec::new(),
                    result: String::new(),
                })
                .collect(),
            skipped: Vec::new(),
            pending: 0,
            actions_status: None,
        };
        assert_eq!(sweep_question(&[plan_of(1, &[])]), None);
        assert_eq!(
            sweep_question(&[plan_of(1, &[10, 11]), plan_of(2, &[]), plan_of(3, &[30])]).unwrap(),
            "Rerun 3 workflow runs across 2 PRs in o/r?"
        );
    }

    #[test]
    fn sweep_question_counts_a_shared_run_once() {
        let plan_of = |number| PrPlan {
            repo: "o/r".to_string(),
            number,
            title: "Fix".to_string(),
            runs: vec![RunPlan {
                run_id: 10,
                workflow: "Build".to_string(),
                jobs: Vec::new(),
                result: String::new(),
            }],
            skipped: Vec::new(),
            pending: 0,
            actions_status: None,
        };
        assert_eq!(
            sweep_question(&[plan_of(1), plan_of(2)]).unwrap(),
            "Rerun 1 workflow run across 2 PRs in o/r?"
        );
    }

    #[test]
    fn plan_skips_runs_past_the_rerun_window() {
        let aged = |name: &str, workflow: &str, run_id: u64, days: i64| {
            let mut run = check(
                name,
                workflow,
                Some(run_id),
                Some("FAILURE"),
                "2026-01-01T00:00:00Z",
            );
            run.run_created_at = Some(Utc::now() - TimeDelta::days(days));
            run
        };
        let checks = Checks {
            title: "Fix".to_string(),
            runs: vec![
                aged("Ubuntu", "Build", 10, 31),
                aged("macOS", "Build", 10, 31),
                aged("Lint", "Lint", 11, 29),
            ],
            statuses: Vec::new(),
        };
        let plan = plan(&pr(), checks);
        let runs: Vec<u64> = plan.runs.iter().map(|r| r.run_id).collect();
        assert_eq!(runs, vec![11]);
        assert_eq!(plan.skipped, vec!["Build (10, older than 30 days)"]);
    }

    #[test]
    fn idle_state_never_calls_a_skipped_failure_passing() {
        let state = |skipped: &[&str], pending| {
            idle_state(&PrPlan {
                repo: "o/r".to_string(),
                number: 7,
                title: "Fix".to_string(),
                runs: Vec::new(),
                skipped: skipped.iter().map(|s| s.to_string()).collect(),
                pending,
                actions_status: None,
            })
        };
        assert_eq!(state(&[], 0), "all checks passing");
        assert_eq!(state(&[], 2), "no failures, 2 pending");
        assert_eq!(
            state(&["Build (10, older than 30 days)"], 0),
            "nothing to rerun"
        );
        assert_eq!(state(&["ci/jenkins (error)"], 2), "nothing to rerun");
    }

    #[test]
    fn classify_targets() {
        let no_branch = |_: &str| false;
        assert_eq!(classify(None, None, no_branch).unwrap(), Target::Pr(None));
        assert_eq!(
            classify(None, Some("--mine"), no_branch).unwrap(),
            Target::Repo(None)
        );
        assert_eq!(
            classify(Some("12"), None, no_branch).unwrap(),
            Target::Pr(Some("12".to_string()))
        );
        assert_eq!(
            classify(Some("a/b#12"), None, no_branch).unwrap(),
            Target::Pr(Some("a/b#12".to_string()))
        );
        assert_eq!(
            classify(Some("a/b"), None, no_branch).unwrap(),
            Target::Repo(Some(Repo::parse("a/b").unwrap()))
        );
        assert_eq!(
            classify(Some("https://github.com/a/b"), None, no_branch).unwrap(),
            Target::Repo(Some(Repo::parse("a/b").unwrap()))
        );
        // A two-part name that exists as a local branch is the branch.
        assert_eq!(
            classify(Some("fix/thing"), None, |_| true).unwrap(),
            Target::Pr(Some("fix/thing".to_string()))
        );
        assert_eq!(
            classify(Some("nathan/dev/feature"), None, no_branch).unwrap(),
            Target::Pr(Some("nathan/dev/feature".to_string()))
        );
        assert!(classify(Some("12"), Some("--mine"), no_branch).is_err());
        assert_eq!(
            classify(Some("a/b"), Some("--all"), no_branch).unwrap(),
            Target::Repo(Some(Repo::parse("a/b").unwrap()))
        );
        assert_eq!(
            classify(Some("12"), Some("--all"), no_branch)
                .unwrap_err()
                .to_string(),
            "--all takes a repository, not a PR"
        );
        assert_eq!(
            classify(Some("fix/thing"), Some("--all"), |_| true)
                .unwrap_err()
                .to_string(),
            "--all takes a repository, not a branch"
        );
    }
}

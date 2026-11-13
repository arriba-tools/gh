use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::Mutex;

use crate::api::actions::{self, OrgRepo, WorkflowRun};
use crate::api::{Client, Repo};
use crate::cli::{RunnersArgs, RunnersCommand};
use crate::context::Ctx;
use crate::dependabot::Scope;
use crate::output::table;
use crate::{output, parallel};

mod watch;

/// A self-hosted runner as the runners API reports it.
#[derive(Debug, PartialEq, Serialize)]
struct Runner {
    id: u64,
    name: String,
    /// "online" or "offline".
    status: String,
    busy: bool,
    os: String,
    labels: Vec<String>,
    /// The job a busy runner is running, when it was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    job: Option<Job>,
}

impl Runner {
    /// Status column: busy, idle, or offline.
    fn state(&self) -> &str {
        if self.status != "online" {
            "offline"
        } else if self.busy {
            "busy"
        } else {
            "idle"
        }
    }
}

impl From<actions::Runner> for Runner {
    fn from(r: actions::Runner) -> Self {
        Runner {
            id: r.id,
            name: r.name,
            status: r.status,
            busy: r.busy,
            os: r.os,
            labels: r.labels.into_iter().map(|l| l.name).collect(),
            job: None,
        }
    }
}

/// Runners registered to the scope.
fn scope_runners(client: &Client, scope: &Scope) -> Result<Vec<actions::Runner>> {
    match scope {
        Scope::Repo(r) => actions::repo_runners(client, r),
        Scope::Org(o) => actions::org_runners(client, o),
    }
}

/// Whether an API error means the token lacks the access the runners API needs.
fn is_forbidden(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    // A 403 for the rate limit says nothing about access.
    !text.contains("rate limit")
        && (text.contains("GitHub API 403") || text.contains("GitHub API 404"))
}

/// What to do when the runners API refuses the token.
fn access_hint(scope: &Scope) -> String {
    match scope {
        Scope::Org(o) => format!(
            "Listing runners for {o} needs an org admin and the admin:org scope. Run `gh auth refresh -s admin:org`."
        ),
        Scope::Repo(r) => format!("Listing runners for {r} needs admin access to the repo."),
    }
}

/// A workflow job, usually one running or waiting for a runner.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct Job {
    repo: String,
    workflow: String,
    name: String,
    run_id: u64,
    job_id: u64,
    /// "in_progress", "queued", or "completed".
    status: String,
    /// Runner the job was assigned to, absent while queued.
    runner: Option<String>,
    /// Labels the job asked for in `runs-on`.
    labels: Vec<String>,
    created_at: Option<DateTime<Utc>>,
    started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<DateTime<Utc>>,
    url: String,
}

/// A job of `repo`, falling back to the run's `workflow` name when the job leaves it out.
fn job_of(j: actions::Job, repo: &Repo, workflow: &str) -> Job {
    Job {
        repo: repo.to_string(),
        workflow: j.workflow_name.unwrap_or_else(|| workflow.to_string()),
        name: j.name,
        run_id: j.run_id,
        job_id: j.id,
        status: j.status,
        runner: j.runner_name.filter(|s| !s.is_empty()),
        labels: j.labels,
        created_at: j.created_at,
        started_at: j.started_at,
        completed_at: j.completed_at,
        url: j.html_url,
    }
}

/// Bodies of earlier GETs with their entity tags, so reading an unchanged listing again costs a
/// 304, which GitHub does not count against the rate limit.
#[derive(Default)]
struct Cache {
    entries: Mutex<HashMap<String, (String, Value)>>,
    /// Paths read since the last sweep.
    read: Mutex<HashSet<String>>,
}

impl Cache {
    /// The body at `path`, from the cache when GitHub says it has not changed.
    fn get(&self, client: &Client, path: &str) -> Result<Value> {
        let tag = self
            .entries
            .lock()
            .unwrap()
            .get(path)
            .map(|(t, _)| t.clone());
        let (body, tag) = match client.get_if_changed(path, tag.as_deref()) {
            Ok(answer) => answer,
            // A blip keeps the last answer, so a listing that fails once does not read as an
            // empty queue. A 4xx is no blip.
            Err(e) if !format!("{e:#}").contains("GitHub API 4") => {
                let kept = self
                    .entries
                    .lock()
                    .unwrap()
                    .get(path)
                    .map(|(_, b)| b.clone());
                let Some(body) = kept else {
                    return Err(e);
                };
                eprintln!("warning: kept the last answer for {path}: {e:#}");
                self.read.lock().unwrap().insert(path.to_string());
                return Ok(body);
            }
            Err(e) => return Err(e),
        };
        self.read.lock().unwrap().insert(path.to_string());
        let mut entries = self.entries.lock().unwrap();
        match (body, tag) {
            (Some(body), Some(tag)) => {
                entries.insert(path.to_string(), (tag, body.clone()));
                Ok(body)
            }
            (Some(body), None) => {
                entries.remove(path);
                Ok(body)
            }
            (None, _) => entries
                .get(path)
                .map(|(_, body)| body.clone())
                .with_context(|| format!("GitHub answered 304 for {path}, which was never read")),
        }
    }

    /// Forget what was not read since the last sweep, as the runs that finished.
    fn sweep(&self) {
        let read = std::mem::take(&mut *self.read.lock().unwrap());
        self.entries
            .lock()
            .unwrap()
            .retain(|path, _| read.contains(path));
    }
}

/// The items in `key` of one page of a listing, or None when the listing has more pages.
fn one_page<T: DeserializeOwned>(page: &Value, key: &str, what: &str) -> Result<Option<Vec<T>>> {
    let items = page[key]
        .as_array()
        .with_context(|| format!("No {key} in the {what} listing"))?;
    if page["total_count"]
        .as_u64()
        .is_some_and(|n| n > items.len() as u64)
    {
        return Ok(None);
    }
    items
        .iter()
        .map(|v| T::deserialize(v).with_context(|| format!("Failed to parse {what}")))
        .collect::<Result<_>>()
        .map(Some)
}

/// Runs of `repo` with `status`, read as one cached page, or every page when there are more.
fn cached_runs(
    client: &Client,
    cache: &Cache,
    repo: &Repo,
    status: &str,
) -> Result<Vec<WorkflowRun>> {
    let path = format!("{}/actions/runs?status={status}&per_page=100", repo.api());
    match one_page(&cache.get(client, &path)?, "workflow_runs", "workflow run")? {
        Some(runs) => Ok(runs),
        None => actions::runs_with_status(client, repo, status),
    }
}

/// Jobs of a run's latest attempt, read as one cached page, or every page when there are more.
fn cached_jobs(
    client: &Client,
    cache: &Cache,
    repo: &Repo,
    run_id: u64,
) -> Result<Vec<actions::Job>> {
    let path = format!(
        "{}/actions/runs/{run_id}/jobs?filter=latest&per_page=100",
        repo.api()
    );
    match one_page(&cache.get(client, &path)?, "jobs", "job")? {
        Some(jobs) => Ok(jobs),
        None => actions::run_jobs(client, repo, run_id),
    }
}

/// Running and queued jobs in a repo. Queued jobs often sit inside in-progress runs, as with a
/// matrix that outgrows its runners, so both kinds of run are scanned.
fn active_jobs(client: &Client, cache: &Cache, repo: &Repo) -> Result<Vec<Job>> {
    let mut seen = HashSet::new();
    let mut jobs = Vec::new();
    for status in ["in_progress", "queued"] {
        for run in cached_runs(client, cache, repo, status)? {
            if !seen.insert(run.id) {
                continue;
            }
            for j in cached_jobs(client, cache, repo, run.id)? {
                let job = job_of(j, repo, &run.name);
                if matches!(job.status.as_str(), "in_progress" | "queued") {
                    jobs.push(job);
                }
            }
        }
    }
    Ok(jobs)
}

/// Repositories of an organization that can still run workflows.
fn org_repos(client: &Client, org: &str) -> Result<Vec<Repo>> {
    Ok(active_repos(actions::org_repos(client, org)?).collect())
}

/// Repositories that are not archived, in the order listed.
fn active_repos(repos: Vec<OrgRepo>) -> impl Iterator<Item = Repo> {
    repos
        .into_iter()
        .filter(|r| !r.archived)
        .filter_map(|r| Repo::parse(&r.full_name).ok())
}

/// Run `f` over every item a few at a time. An item that fails is skipped with a warning, so one
/// repo without Actions doesn't hide the rest.
fn in_parallel<I: Sync + fmt::Display, T: Send>(
    items: &[I],
    f: impl Fn(&I) -> Result<Vec<T>> + Sync,
) -> Vec<T> {
    let mut out = Vec::new();
    for (item, result) in items.iter().zip(parallel::map(items, f)) {
        match result {
            Ok(found) => out.extend(found),
            Err(e) => eprintln!("warning: skipped {item}: {e:#}"),
        }
    }
    out
}

/// Attach each running job to its runner and return the jobs still waiting for one, oldest first.
fn assign(runners: &mut [Runner], jobs: Vec<Job>) -> Vec<Job> {
    let mut queued = Vec::new();
    for job in jobs {
        if job.status == "queued" {
            queued.push(job);
            continue;
        }
        if let Some(r) = runners
            .iter_mut()
            .find(|r| job.runner.as_deref() == Some(r.name.as_str()))
        {
            r.job = Some(job);
        }
    }
    queued.sort_by_key(|j| j.created_at);
    queued
}

/// A workflow run whose jobs show which runners picked them up.
struct RunRef {
    repo: Repo,
    id: u64,
    workflow: String,
}

impl fmt::Display for RunRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} run {}", self.repo, self.id)
    }
}

/// A self-hosted runner seen in recent jobs, for tokens the runners API refuses.
#[derive(Debug, PartialEq, Serialize)]
struct SeenRunner {
    name: String,
    /// Labels its jobs asked for, which may be fewer than the runner has.
    labels: Vec<String>,
    /// Whether one of its jobs is in progress.
    running: bool,
    last_seen: Option<DateTime<Utc>>,
    last_job: Job,
}

/// The most recent `runs` runs of a repo.
fn recent_runs(client: &Client, repo: &Repo, runs: usize) -> Result<Vec<RunRef>> {
    Ok(actions::recent_runs(client, repo, runs)?
        .into_iter()
        .map(|r| RunRef {
            repo: repo.clone(),
            id: r.id,
            workflow: r.name,
        })
        .collect())
}

/// The `limit` most recently pushed repositories of an organization.
fn recent_org_repos(client: &Client, org: &str, limit: usize) -> Result<Vec<Repo>> {
    Ok(
        active_repos(actions::recently_pushed_org_repos(client, org)?)
            .take(limit)
            .collect(),
    )
}

/// Every job of the recent runs in `repos`.
fn recent_jobs(client: &Client, repos: &[Repo], runs: usize) -> Vec<Job> {
    let refs = in_parallel(repos, |repo| recent_runs(client, repo, runs));
    in_parallel(&refs, |run| {
        Ok(actions::run_jobs(client, &run.repo, run.id)?
            .into_iter()
            .map(|j| job_of(j, &run.repo, &run.workflow))
            .collect())
    })
}

/// GitHub-hosted runners are named `GitHub Actions N` and aren't worth listing.
fn is_hosted(runner: &str) -> bool {
    runner.starts_with("GitHub Actions ")
}

/// When a job last showed its runner was alive.
fn seen_at(job: &Job) -> Option<DateTime<Utc>> {
    job.completed_at.or(job.started_at)
}

/// Group jobs by the self-hosted runner that ran them, keeping each runner's latest job.
fn infer(jobs: &[Job]) -> Vec<SeenRunner> {
    let mut by_name: BTreeMap<String, SeenRunner> = BTreeMap::new();
    for job in jobs {
        let Some(name) = job.runner.as_deref().filter(|n| !is_hosted(n)) else {
            continue;
        };
        let seen = by_name
            .entry(name.to_string())
            .or_insert_with(|| SeenRunner {
                name: name.to_string(),
                labels: Vec::new(),
                running: false,
                last_seen: None,
                last_job: job.clone(),
            });
        for label in &job.labels {
            if !seen.labels.contains(label) {
                seen.labels.push(label.clone());
            }
        }
        // A running job is the most recent one a runner can have.
        let running = job.status == "in_progress";
        if running || (!seen.running && seen_at(job) >= seen.last_seen) {
            seen.last_job = job.clone();
            seen.last_seen = seen_at(job);
        }
        seen.running |= running;
    }
    by_name.into_values().collect()
}

#[derive(Serialize)]
struct InferredReport {
    scope: String,
    /// Always true, so scripts can tell this from the runners API listing.
    inferred: bool,
    runners: Vec<SeenRunner>,
    queued: Vec<Job>,
}

/// Runners seen in recent jobs, then the jobs still queued among them.
fn render_inferred(report: &InferredReport, runs: usize, now: DateTime<Utc>) -> String {
    let mut out = format!(
        "Inferred from the jobs of the last {runs} runs, not the runners API, so idle and offline runners look alike.\n"
    );
    if report.runners.is_empty() {
        out.push_str(&format!(
            "No self-hosted runners ran those jobs in {}.\n",
            report.scope
        ));
    } else {
        let mut rows = vec![vec![
            "NAME".to_string(),
            "LAST SEEN".to_string(),
            "LAST JOB".to_string(),
            "LABELS".to_string(),
        ]];
        for r in &report.runners {
            let seen = match (r.running, r.last_seen) {
                (true, _) => "running".to_string(),
                (false, Some(t)) => format!("{} ago", elapsed(t, now)),
                (false, None) => "unknown".to_string(),
            };
            let j = &r.last_job;
            rows.push(vec![
                r.name.clone(),
                seen,
                format!("{}  {} / {}", j.repo, j.workflow, j.name),
                r.labels.join(", "),
            ]);
        }
        out.push_str(&table(&rows, ""));
        out.push('\n');
    }
    out.push_str(&render_queue(&report.queued, now));
    out
}

#[derive(Serialize)]
struct Report {
    scope: String,
    runners: Vec<Runner>,
    /// Jobs waiting for a runner. Absent when jobs were not looked up.
    #[serde(skip_serializing_if = "Option::is_none")]
    queued: Option<Vec<Job>>,
}

pub fn run(ctx: &Ctx, args: RunnersArgs) -> Result<()> {
    if let Some(RunnersCommand::Watch(watch_args)) = args.command {
        return watch::run(ctx, &watch_args);
    }
    let client = &ctx.client;
    let scope = Scope::new(ctx, args.org.clone())?;
    if args.from_jobs {
        return run_inferred(ctx, &scope, &args);
    }
    let mut runners: Vec<Runner> = match scope_runners(client, &scope) {
        Ok(list) => list.into_iter().map(Runner::from).collect(),
        Err(e) if is_forbidden(&e) => {
            eprintln!(
                "warning: {} Falling back to recent jobs.",
                access_hint(&scope)
            );
            return run_inferred(ctx, &scope, &args);
        }
        Err(e) => return Err(e),
    };
    runners.sort_by_key(|r| r.name.to_lowercase());

    // Scanning an organization costs a few requests per repo, so only do it when there is
    // something to find.
    let cache = Cache::default();
    let jobs = match &scope {
        Scope::Repo(repo) => Some(active_jobs(client, &cache, repo)?),
        Scope::Org(org) if args.queued || runners.iter().any(|r| r.busy) => {
            let repos = org_repos(client, org)?;
            Some(in_parallel(&repos, |repo| {
                active_jobs(client, &cache, repo)
            }))
        }
        Scope::Org(_) => None,
    };
    let queued = jobs.map(|jobs| assign(&mut runners, jobs));

    let report = Report {
        scope: scope.to_string(),
        runners,
        queued,
    };
    if ctx.json {
        return output::print_json(&report);
    }
    print!("{}", render(&report, &scope, Utc::now()));
    Ok(())
}

/// List runners from the jobs of recent runs, for tokens without admin access.
fn run_inferred(ctx: &Ctx, scope: &Scope, args: &RunnersArgs) -> Result<()> {
    let client = &ctx.client;
    let repos = match scope {
        Scope::Repo(r) => vec![r.clone()],
        Scope::Org(o) => recent_org_repos(client, o, args.repos)?,
    };
    if repos.is_empty() {
        bail!("No repositories to scan in {scope}");
    }
    let jobs = recent_jobs(client, &repos, args.runs);
    let mut queued: Vec<Job> = jobs
        .iter()
        .filter(|j| j.status == "queued")
        .cloned()
        .collect();
    queued.sort_by_key(|j| j.created_at);
    let report = InferredReport {
        scope: scope.to_string(),
        inferred: true,
        runners: infer(&jobs),
        queued,
    };
    if ctx.json {
        return output::print_json(&report);
    }
    print!("{}", render_inferred(&report, args.runs, Utc::now()));
    Ok(())
}

/// Runner table and count summary, then busy runners' jobs and the queue.
fn render(report: &Report, scope: &Scope, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    if report.runners.is_empty() {
        let hint = match scope {
            Scope::Repo(_) => " Runners registered to the organization show with --org ORG.",
            Scope::Org(_) => "",
        };
        out.push_str(&format!(
            "No self-hosted runners in {}.{hint}\n",
            report.scope
        ));
    } else {
        let mut rows = vec![vec![
            "NAME".to_string(),
            "STATUS".to_string(),
            "OS".to_string(),
            "LABELS".to_string(),
        ]];
        for r in &report.runners {
            rows.push(vec![
                r.name.clone(),
                r.state().to_string(),
                r.os.clone(),
                r.labels.join(", "),
            ]);
        }
        out.push_str(&format!(
            "{}\n{}\n",
            table(&rows, ""),
            summary(&report.runners)
        ));
    }

    // Jobs are only known when they were looked up.
    let Some(queued) = &report.queued else {
        return out;
    };
    let busy: Vec<&Runner> = report
        .runners
        .iter()
        .filter(|r| r.status == "online" && r.busy)
        .collect();
    if !busy.is_empty() {
        out.push_str("\nRunning:\n");
        let rows: Vec<Vec<String>> = busy
            .iter()
            .map(|r| {
                let detail = match &r.job {
                    Some(j) => {
                        let since = j
                            .started_at
                            .map(|t| format!("  for {}", elapsed(t, now)))
                            .unwrap_or_default();
                        format!("{}  {} / {}{since}", j.repo, j.workflow, j.name)
                    }
                    None => "job not found".to_string(),
                };
                vec![r.name.clone(), detail]
            })
            .collect();
        out.push_str(&table(&rows, "  "));
        out.push('\n');
    }
    out.push_str(&render_queue(queued, now));
    out
}

/// Jobs waiting for a runner, with how long they've waited and the labels they asked for.
fn render_queue(queued: &[Job], now: DateTime<Utc>) -> String {
    if queued.is_empty() {
        return String::new();
    }
    let rows: Vec<Vec<String>> = queued
        .iter()
        .map(|j| {
            let waited = j
                .created_at
                .map(|t| format!("waiting {}", elapsed(t, now)))
                .unwrap_or_else(|| "waiting".to_string());
            vec![
                j.repo.clone(),
                format!("{} / {}", j.workflow, j.name),
                format!("{waited} for {}", j.labels.join(", ")),
            ]
        })
        .collect();
    format!("\nQueued ({}):\n{}\n", queued.len(), table(&rows, "  "))
}

/// Counts line, as `12 runners: 9 online (4 busy), 3 offline`.
fn summary(runners: &[Runner]) -> String {
    let online = runners.iter().filter(|r| r.status == "online").count();
    let busy = runners
        .iter()
        .filter(|r| r.status == "online" && r.busy)
        .count();
    let offline = runners.len() - online;
    let noun = if runners.len() == 1 {
        "runner"
    } else {
        "runners"
    };
    format!(
        "{} {noun}: {online} online ({busy} busy), {offline} offline",
        runners.len()
    )
}

/// Time since `from`, as `45s`, `12m`, or `2h 5m`.
fn elapsed(from: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - from).num_seconds().max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server, ServerGuard};
    use serde_json::json;

    fn runner(name: &str, status: &str, busy: bool) -> Runner {
        let api: actions::Runner = serde_json::from_value(json!({
            "id": 1, "name": name, "os": "Linux", "status": status, "busy": busy,
            "labels": [
                { "id": 1, "name": "self-hosted", "type": "read-only" },
                { "id": 2, "name": "linux", "type": "custom" }
            ]
        }))
        .unwrap();
        api.into()
    }

    fn job(name: &str, status: &str, runner: Option<&str>, at: &str) -> Job {
        let api: actions::Job = serde_json::from_value(json!({
            "id": 7, "run_id": 3, "name": name, "status": status,
            "workflow_name": "CI", "runner_name": runner,
            "labels": ["self-hosted", "windows"],
            "created_at": at, "started_at": at,
            "html_url": "https://github.com/a/b/actions/runs/3/job/7"
        }))
        .unwrap();
        job_of(api, &Repo::parse("a/b").unwrap(), "fallback")
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn parses_runner_and_state() {
        let r = runner("build-1", "online", true);
        assert_eq!(r.labels, vec!["self-hosted", "linux"]);
        assert_eq!(r.state(), "busy");
        assert_eq!(runner("b", "online", false).state(), "idle");
        // An offline runner can still report busy from its last job.
        assert_eq!(runner("c", "offline", true).state(), "offline");
    }

    #[test]
    fn summary_counts() {
        let runners = vec![
            runner("a", "online", true),
            runner("b", "online", false),
            runner("c", "offline", true),
        ];
        assert_eq!(summary(&runners), "3 runners: 2 online (1 busy), 1 offline");
        assert_eq!(
            summary(&runners[..1]),
            "1 runner: 1 online (1 busy), 0 offline"
        );
    }

    #[test]
    fn parses_job_and_drops_empty_runner() {
        let j = job("Build", "queued", Some(""), "2026-01-01T00:00:00Z");
        assert_eq!(j.runner, None);
        assert_eq!(j.workflow, "CI");
        assert_eq!(j.labels, vec!["self-hosted", "windows"]);
        assert_eq!(j.created_at, Some(at("2026-01-01T00:00:00Z")));
    }

    #[test]
    fn assign_matches_runner_names_and_sorts_queue() {
        let mut runners = vec![runner("a", "online", true), runner("b", "online", false)];
        let jobs = vec![
            job("Late", "queued", None, "2026-01-01T00:10:00Z"),
            job("Build", "in_progress", Some("a"), "2026-01-01T00:00:00Z"),
            job("Early", "queued", None, "2026-01-01T00:05:00Z"),
            job(
                "Hosted",
                "in_progress",
                Some("GitHub Actions 3"),
                "2026-01-01T00:00:00Z",
            ),
        ];
        let queued = assign(&mut runners, jobs);
        assert_eq!(runners[0].job.as_ref().unwrap().name, "Build");
        assert!(runners[1].job.is_none());
        let names: Vec<&str> = queued.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, vec!["Early", "Late"]);
    }

    #[test]
    fn render_table_jobs_and_queue() {
        let scope = Scope::Repo(Repo::parse("a/b").unwrap());
        let mut runners = vec![
            runner("build-1", "online", true),
            runner("build-2", "online", true),
            runner("build-3", "online", false),
        ];
        let queued = assign(
            &mut runners,
            vec![
                job(
                    "Build",
                    "in_progress",
                    Some("build-1"),
                    "2026-01-01T00:00:00Z",
                ),
                job("Test", "queued", None, "2026-01-01T00:46:00Z"),
            ],
        );
        let report = Report {
            scope: "a/b".to_string(),
            runners,
            queued: Some(queued),
        };
        assert_eq!(
            render(&report, &scope, at("2026-01-01T01:05:00Z")),
            "NAME     STATUS  OS     LABELS\n\
             build-1  busy    Linux  self-hosted, linux\n\
             build-2  busy    Linux  self-hosted, linux\n\
             build-3  idle    Linux  self-hosted, linux\n\
             3 runners: 3 online (2 busy), 0 offline\n\
             \n\
             Running:\n\
             \x20 build-1  a/b  CI / Build  for 1h 5m\n\
             \x20 build-2  job not found\n\
             \n\
             Queued (1):\n\
             \x20 a/b  CI / Test  waiting 19m for self-hosted, windows\n"
        );
    }

    #[test]
    fn render_without_job_lookup_or_runners() {
        let scope = Scope::Repo(Repo::parse("a/b").unwrap());
        let report = Report {
            scope: "a/b".to_string(),
            runners: vec![runner("build-1", "online", false)],
            queued: None,
        };
        assert_eq!(
            render(&report, &scope, Utc::now()),
            "NAME     STATUS  OS     LABELS\n\
             build-1  idle    Linux  self-hosted, linux\n\
             1 runner: 1 online (0 busy), 0 offline\n"
        );
        let empty = Report {
            scope: "a/b".to_string(),
            runners: Vec::new(),
            queued: Some(Vec::new()),
        };
        assert!(render(&empty, &scope, Utc::now()).contains("--org ORG"));
    }

    fn done(name: &str, runner: &str, labels: &[&str], at: &str) -> Job {
        let mut j = job(name, "completed", Some(runner), at);
        j.labels = labels.iter().map(|l| l.to_string()).collect();
        j.completed_at = j.started_at;
        j
    }

    #[test]
    fn infer_groups_by_runner_and_prefers_running_jobs() {
        let jobs = vec![
            done("Old", "box-1", &["self-hosted"], "2026-01-01T00:00:00Z"),
            done(
                "New",
                "box-1",
                &["self-hosted", "windows"],
                "2026-01-01T01:00:00Z",
            ),
            job("Live", "in_progress", Some("box-2"), "2026-01-01T00:30:00Z"),
            done("Later", "box-2", &["self-hosted"], "2026-01-01T02:00:00Z"),
            done(
                "Hosted",
                "GitHub Actions 12",
                &["ubuntu-latest"],
                "2026-01-01T00:00:00Z",
            ),
            job("Waiting", "queued", None, "2026-01-01T00:00:00Z"),
        ];
        let seen = infer(&jobs);
        let summary: Vec<(&str, bool, &str)> = seen
            .iter()
            .map(|r| (r.name.as_str(), r.running, r.last_job.name.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![("box-1", false, "New"), ("box-2", true, "Live")]
        );
        assert_eq!(seen[0].labels, vec!["self-hosted", "windows"]);
        assert_eq!(seen[0].last_seen, Some(at("2026-01-01T01:00:00Z")));
    }

    #[test]
    fn render_inferred_marks_running_and_age() {
        let jobs = vec![
            done("Build", "box-1", &["self-hosted"], "2026-01-01T00:00:00Z"),
            job("Test", "in_progress", Some("box-2"), "2026-01-01T00:50:00Z"),
        ];
        let report = InferredReport {
            scope: "a/b".to_string(),
            inferred: true,
            runners: infer(&jobs),
            queued: Vec::new(),
        };
        assert_eq!(
            render_inferred(&report, 30, at("2026-01-01T01:00:00Z")),
            "Inferred from the jobs of the last 30 runs, not the runners API, so idle and offline runners look alike.\n\
             NAME   LAST SEEN  LAST JOB         LABELS\n\
             box-1  1h 0m ago  a/b  CI / Build  self-hosted\n\
             box-2  running    a/b  CI / Test   self-hosted, windows\n"
        );
    }

    #[test]
    fn elapsed_units() {
        let t = at("2026-01-01T00:00:00Z");
        assert_eq!(elapsed(t, at("2026-01-01T00:00:45Z")), "45s");
        assert_eq!(elapsed(t, at("2026-01-01T00:12:30Z")), "12m");
        assert_eq!(elapsed(t, at("2026-01-01T02:05:00Z")), "2h 5m");
        assert_eq!(elapsed(at("2026-01-01T00:01:00Z"), t), "0s");
    }

    #[test]
    fn forbidden_detection() {
        assert!(is_forbidden(&anyhow::anyhow!(
            "GitHub API 403 on GET orgs/x/actions/runners: Must have admin rights"
        )));
        assert!(!is_forbidden(&anyhow::anyhow!(
            "GitHub API 500 on GET x: oops"
        )));
        assert!(!is_forbidden(&anyhow::anyhow!(
            "GitHub API 403 on GET orgs/x/actions/runners: API rate limit exceeded for user ID 1."
        )));
        assert!(access_hint(&Scope::Org("x".to_string())).contains("admin:org"));
    }

    fn ctx(server: &ServerGuard, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    fn runners_args(org: Option<&str>) -> RunnersArgs {
        RunnersArgs {
            command: None,
            org: org.map(str::to_string),
            queued: false,
            from_jobs: false,
            runs: 30,
            repos: 10,
        }
    }

    fn status(s: &str) -> Matcher {
        Matcher::UrlEncoded("status".into(), s.into())
    }

    /// Run 1 is in progress with a job on runner `a`, run 2 is queued with a waiting job.
    fn serve_active_runs(server: &mut ServerGuard, repo: &str) -> mockito::Mock {
        server
            .mock("GET", format!("/repos/{repo}/actions/runs").as_str())
            .match_query(status("in_progress"))
            .with_body(r#"{"workflow_runs": [{"id": 1, "name": "CI"}]}"#)
            .create();
        // A run can move from queued to in progress between the two listings.
        server
            .mock("GET", format!("/repos/{repo}/actions/runs").as_str())
            .match_query(status("queued"))
            .with_body(
                r#"{"workflow_runs": [{"id": 1, "name": "CI"}, {"id": 2, "name": "Nightly"}]}"#,
            )
            .create();
        server
            .mock("GET", format!("/repos/{repo}/actions/runs/2/jobs").as_str())
            .match_query(Matcher::Any)
            .with_body(
                r#"{"jobs": [{"id": 20, "run_id": 2, "name": "Test", "status": "queued",
                    "labels": ["gpu"], "created_at": "2026-01-01T00:00:00Z"}]}"#,
            )
            .create();
        server
            .mock("GET", format!("/repos/{repo}/actions/runs/1/jobs").as_str())
            .match_query(Matcher::Any)
            .with_body(
                r#"{"jobs": [
                    {"id": 10, "run_id": 1, "name": "Build", "status": "in_progress",
                     "runner_name": "a", "workflow_name": "CI"},
                    {"id": 11, "run_id": 1, "name": "Lint", "status": "completed"}
                ]}"#,
            )
            .create()
    }

    #[test]
    fn active_jobs_scan_running_and_queued_runs_once() {
        let mut server = Server::new();
        let run_1 = serve_active_runs(&mut server, "o/r");
        let client = Client::for_tests(&server.url());
        let jobs = active_jobs(&client, &Cache::default(), &Repo::parse("o/r").unwrap()).unwrap();
        run_1.assert();
        let rows: Vec<(&str, &str, &str)> = jobs
            .iter()
            .map(|j| (j.workflow.as_str(), j.name.as_str(), j.status.as_str()))
            .collect();
        // The job's own workflow name wins, else the run's.
        assert_eq!(
            rows,
            vec![
                ("CI", "Build", "in_progress"),
                ("Nightly", "Test", "queued")
            ]
        );
    }

    #[test]
    fn the_cache_answers_a_304_with_the_body_it_kept_until_swept() {
        let mut server = Server::new();
        let first = server
            .mock("GET", "/items")
            .match_header("if-none-match", Matcher::Missing)
            .with_header("ETag", "\"v1\"")
            .with_body(r#"{"n": 1}"#)
            .expect(2)
            .create();
        let unchanged = server
            .mock("GET", "/items")
            .match_header("if-none-match", "\"v1\"")
            .with_status(304)
            .expect(1)
            .create();
        let client = Client::for_tests(&server.url());
        let cache = Cache::default();
        assert_eq!(cache.get(&client, "items").unwrap(), json!({"n": 1}));
        assert_eq!(cache.get(&client, "items").unwrap(), json!({"n": 1}));
        // Read since the last sweep, so kept, then unread through a sweep, so forgotten.
        cache.sweep();
        cache.sweep();
        assert_eq!(cache.get(&client, "items").unwrap(), json!({"n": 1}));
        first.assert();
        unchanged.assert();
    }

    #[test]
    fn the_cache_keeps_its_answer_through_a_blip_but_not_a_refusal() {
        let mut server = Server::new();
        for path in ["/blip", "/gone"] {
            server
                .mock("GET", path)
                .match_header("if-none-match", Matcher::Missing)
                .with_header("ETag", "\"v1\"")
                .with_body(r#"{"n": 1}"#)
                .create();
        }
        server
            .mock("GET", "/blip")
            .match_header("if-none-match", "\"v1\"")
            .with_status(502)
            .create();
        server
            .mock("GET", "/gone")
            .match_header("if-none-match", "\"v1\"")
            .with_status(404)
            .with_body(r#"{"message": "Not Found"}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let cache = Cache::default();
        for path in ["blip", "gone"] {
            cache.get(&client, path).unwrap();
        }
        assert_eq!(cache.get(&client, "blip").unwrap(), json!({"n": 1}));
        let err = cache.get(&client, "gone").unwrap_err().to_string();
        assert_eq!(err, "GitHub API 404 on GET gone: Not Found");
        // Nothing kept, nothing to fall back on.
        assert!(Cache::default().get(&client, "missing").is_err());
    }

    #[test]
    fn a_listing_longer_than_a_page_is_read_whole() {
        let mut server = Server::new();
        let next = format!(
            "<{}/repos/o/r/actions/runs?status=queued&per_page=100&page=2>; rel=\"next\"",
            server.url()
        );
        // Read once into the cache, where the count shows a second page, then again from the top.
        let page_1 = server
            .mock("GET", "/repos/o/r/actions/runs")
            .match_query(Matcher::Exact("status=queued&per_page=100".into()))
            .with_header("link", &next)
            .with_body(r#"{"total_count": 2, "workflow_runs": [{"id": 1}]}"#)
            .expect(2)
            .create();
        let page_2 = server
            .mock("GET", "/repos/o/r/actions/runs")
            .match_query(Matcher::UrlEncoded("page".into(), "2".into()))
            .with_body(r#"{"total_count": 2, "workflow_runs": [{"id": 2}]}"#)
            .expect(1)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        let runs = cached_runs(&client, &Cache::default(), &repo, "queued").unwrap();
        assert_eq!(runs.iter().map(|r| r.id).collect::<Vec<_>>(), [1, 2]);
        page_1.assert();
        page_2.assert();
    }

    #[test]
    fn repo_runners_with_their_jobs() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/runners")
            .match_query(Matcher::Any)
            .with_body(
                r#"{"runners": [
                    {"id": 2, "name": "b", "status": "offline"},
                    {"id": 1, "name": "a", "status": "online", "busy": true}
                ]}"#,
            )
            .expect(2)
            .create();
        serve_active_runs(&mut server, "o/r");
        run(&ctx(&server, true), runners_args(None)).unwrap();
        run(&ctx(&server, false), runners_args(None)).unwrap();
    }

    #[test]
    fn org_scans_repos_only_when_there_is_something_to_find() {
        let mut server = Server::new();
        server
            .mock("GET", "/orgs/o/actions/runners")
            .match_query(Matcher::Any)
            .with_body(r#"{"runners": [{"id": 1, "name": "a", "status": "online"}]}"#)
            .create();
        let repos = server
            .mock("GET", "/orgs/o/repos")
            .match_query(Matcher::Exact("type=all&per_page=100".into()))
            .with_body(
                r#"[{"full_name": "o/r"}, {"full_name": "o/old", "archived": true},
                    {"full_name": "o/broken"}]"#,
            )
            .expect(1)
            .create();
        serve_active_runs(&mut server, "o/r");
        // A repo whose runs can't be read is skipped with a warning.
        server
            .mock("GET", "/repos/o/broken/actions/runs")
            .match_query(Matcher::Any)
            .with_status(500)
            .create();
        let archived = server
            .mock("GET", Matcher::Regex("^/repos/o/old/".into()))
            .expect(0)
            .create();
        // No runner is busy, so nothing is scanned.
        run(&ctx(&server, true), runners_args(Some("o"))).unwrap();
        let mut args = runners_args(Some("o"));
        args.queued = true;
        run(&ctx(&server, true), args).unwrap();
        repos.assert();
        archived.assert();
    }

    /// Recent runs of `repo`, whose one job ran on self-hosted `box-1`.
    fn serve_recent_jobs(server: &mut ServerGuard, repo: &str) {
        server
            .mock("GET", format!("/repos/{repo}/actions/runs").as_str())
            .match_query(Matcher::Exact("per_page=30".into()))
            .with_body(r#"{"workflow_runs": [{"id": 5, "name": "CI"}]}"#)
            .create();
        server
            .mock("GET", format!("/repos/{repo}/actions/runs/5/jobs").as_str())
            .match_query(Matcher::Any)
            .with_body(
                r#"{"jobs": [
                    {"id": 50, "run_id": 5, "name": "Build", "status": "completed",
                     "runner_name": "box-1", "completed_at": "2026-01-01T00:00:00Z"},
                    {"id": 51, "run_id": 5, "name": "Wait", "status": "queued"}
                ]}"#,
            )
            .create();
    }

    #[test]
    fn falls_back_to_recent_jobs_without_admin_access() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/runners")
            .match_query(Matcher::Any)
            .with_status(403)
            .with_body(r#"{"message": "Resource not accessible by integration"}"#)
            .expect(2)
            .create();
        serve_recent_jobs(&mut server, "o/r");
        run(&ctx(&server, true), runners_args(None)).unwrap();
        run(&ctx(&server, false), runners_args(None)).unwrap();
    }

    #[test]
    fn other_runner_errors_are_not_hidden() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/runners")
            .match_query(Matcher::Any)
            .with_status(500)
            .with_body(r#"{"message": "Server Error"}"#)
            .create();
        let err = run(&ctx(&server, true), runners_args(None))
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("GitHub API 500"), "{err}");
    }

    #[test]
    fn from_jobs_reads_the_most_recently_pushed_repos() {
        let mut server = Server::new();
        server
            .mock("GET", "/orgs/o/repos")
            .match_query(Matcher::UrlEncoded("sort".into(), "pushed".into()))
            .with_body(r#"[{"full_name": "o/a"}, {"full_name": "o/b"}]"#)
            .create();
        serve_recent_jobs(&mut server, "o/a");
        let skipped = server
            .mock("GET", Matcher::Regex("^/repos/o/b/".into()))
            .expect(0)
            .create();
        let client = Client::for_tests(&server.url());
        let repos = recent_org_repos(&client, "o", 1).unwrap();
        let jobs = recent_jobs(&client, &repos, 30);
        let seen = infer(&jobs);
        assert_eq!(seen.len(), 1);
        assert_eq!(
            (seen[0].name.as_str(), seen[0].last_job.repo.as_str()),
            ("box-1", "o/a")
        );

        let mut args = runners_args(Some("o"));
        args.from_jobs = true;
        args.repos = 1;
        run(&ctx(&server, true), args).unwrap();
        skipped.assert();

        let mut empty = Server::new();
        empty
            .mock("GET", "/orgs/e/repos")
            .match_query(Matcher::Any)
            .with_body("[]")
            .create();
        let mut args = runners_args(Some("e"));
        args.from_jobs = true;
        assert_eq!(
            run(&ctx(&empty, true), args).unwrap_err().to_string(),
            "No repositories to scan in e"
        );
    }

    #[test]
    fn render_inferred_without_runners_lists_the_queue() {
        let report = InferredReport {
            scope: "o/r".to_string(),
            inferred: true,
            runners: Vec::new(),
            queued: vec![job("Test", "queued", None, "2026-01-01T00:00:00Z")],
        };
        assert_eq!(
            render_inferred(&report, 5, at("2026-01-01T00:00:30Z")),
            "Inferred from the jobs of the last 5 runs, not the runners API, so idle and offline runners look alike.\n\
             No self-hosted runners ran those jobs in o/r.\n\
             \n\
             Queued (1):\n\
             \x20 a/b  CI / Test  waiting 30s for self-hosted, windows\n"
        );
    }
}

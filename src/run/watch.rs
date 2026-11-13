use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::api::actions::{self, Job, WorkflowRun};
use crate::api::{Repo, is_failed};
use crate::context::Ctx;
use crate::notify::{self, Note};
use crate::output;
use crate::wake::Wake;

/// How often to poll, how long to keep at it, and which jobs to wait for.
pub struct Wait {
    pub interval: Duration,
    pub timeout: Duration,
    /// Case-insensitive substring of the job names to wait for. None waits for every job.
    pub job: Option<String>,
    /// Show a desktop notification when the wait ends.
    pub notify: bool,
    /// Ends the pause between polls early, when given.
    pub wake: Option<Wake>,
}

impl From<&crate::cli::WaitArgs> for Wait {
    fn from(args: &crate::cli::WaitArgs) -> Self {
        Self {
            interval: Duration::from_secs(args.interval),
            timeout: Duration::from_secs(args.timeout * 60),
            job: args.job.as_ref().map(|j| j.to_lowercase()),
            notify: args.notify,
            // Resolved by the caller, which knows the repository of the runs.
            wake: None,
        }
    }
}

impl Default for Wait {
    /// The `run watch` defaults: poll every 20 seconds for up to an hour, for every job.
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(20),
            timeout: Duration::from_secs(60 * 60),
            job: None,
            notify: false,
            wake: None,
        }
    }
}

impl Wait {
    fn wants(&self, job: &str) -> bool {
        self.job
            .as_deref()
            .is_none_or(|p| job.to_lowercase().contains(p))
    }
}

/// A job queued this long is reported once, since it is likely waiting on a busy runner.
const QUEUED_TOO_LONG: i64 = 5 * 60;

#[derive(Clone, Debug, Serialize)]
pub struct JobState {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    /// Wall time from start to completion.
    pub seconds: Option<i64>,
    pub labels: Vec<String>,
    pub url: String,
    #[serde(skip)]
    queued_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunState {
    pub repo: String,
    pub id: u64,
    pub workflow: String,
    pub title: String,
    pub url: String,
    pub status: String,
    pub conclusion: Option<String>,
    /// Only the jobs being waited for, when --job narrows them.
    pub jobs: Vec<JobState>,
    #[serde(skip)]
    filtered: bool,
}

impl RunState {
    /// Done when the run completes, or with --job, once every matching job has.
    fn finished(&self) -> bool {
        self.status == "completed"
            || (self.filtered
                && !self.jobs.is_empty()
                && self.jobs.iter().all(|j| j.status == "completed"))
    }

    /// Whether any job failed, or without --job the run itself did, as when its workflow file
    /// is invalid.
    fn failed(&self) -> bool {
        self.jobs.iter().any(|j| is_failed(j.conclusion.as_deref()))
            || (!self.filtered && is_failed(self.conclusion.as_deref()))
    }
}

fn job_state(job: Job) -> JobState {
    let seconds = match (job.started_at, job.completed_at) {
        (Some(s), Some(c)) => Some((c - s).num_seconds().max(0)),
        _ => None,
    };
    JobState {
        id: job.id,
        name: job.name,
        status: job.status,
        conclusion: job.conclusion,
        seconds,
        labels: job.labels,
        url: job.html_url,
        queued_at: job.created_at.or(job.started_at),
    }
}

fn run_state(repo: &Repo, run: WorkflowRun, jobs: Vec<Job>) -> RunState {
    RunState {
        repo: repo.to_string(),
        id: run.id,
        workflow: run.name,
        title: run.display_title,
        url: run.html_url,
        status: run.status,
        conclusion: run.conclusion,
        jobs: jobs.into_iter().map(job_state).collect(),
        filtered: false,
    }
}

fn fetch(ctx: &Ctx, repo: &Repo, id: u64, wait: &Wait) -> Result<RunState> {
    let run = actions::workflow_run(&ctx.client, repo, id)?;
    let jobs = actions::run_jobs(&ctx.client, repo, id)?;
    let mut state = run_state(repo, run, jobs);
    state.jobs.retain(|j| wait.wants(&j.name));
    state.filtered = wait.job.is_some();
    Ok(state)
}

/// One progress line for a job that just finished.
fn finished_line(run: &RunState, job: &JobState) -> String {
    let conclusion = job.conclusion.as_deref().unwrap_or("completed");
    let took = job
        .seconds
        .map(|s| format!(" ({})", output::duration(s)))
        .unwrap_or_default();
    format!("{conclusion:<10} {}: {}{took}", run.workflow, job.name)
}

/// One line for a job that has sat in the queue, naming the runner labels it waits for.
fn queued_line(run: &RunState, job: &JobState, waited: i64) -> String {
    let labels = if job.labels.is_empty() {
        String::new()
    } else {
        format!(", waiting for a runner labeled {}", job.labels.join(", "))
    };
    format!(
        "{:<10} {}: {} for {}{labels}",
        "queued",
        run.workflow,
        job.name,
        output::duration(waited)
    )
}

/// Progress goes to stderr under --json so stdout carries only the final document.
pub fn progress(ctx: &Ctx, line: &str) {
    if ctx.json {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

/// Poll runs until every one completes, printing each job as it finishes. Fails on timeout.
pub fn watch(ctx: &Ctx, runs: &[(Repo, u64)], wait: &Wait) -> Result<Vec<RunState>> {
    let deadline = Instant::now() + wait.timeout;
    let mut states: Vec<Option<RunState>> = vec![None; runs.len()];
    let mut reported: HashSet<u64> = HashSet::new();
    let mut warned: HashSet<u64> = HashSet::new();
    loop {
        let now = Utc::now();
        for ((repo, id), slot) in runs.iter().zip(states.iter_mut()) {
            if slot.as_ref().is_some_and(RunState::finished) {
                continue;
            }
            let state = fetch(ctx, repo, *id, wait)?;
            // A run already done when first seen goes straight to the summary.
            let quiet = slot.is_none() && state.finished();
            for job in &state.jobs {
                if quiet {
                    reported.insert(job.id);
                } else if job.status == "completed" {
                    if reported.insert(job.id) {
                        progress(ctx, &finished_line(&state, job));
                    }
                } else if job.status == "queued"
                    && let Some(waited) = job.queued_at.map(|t| (now - t).num_seconds())
                    && waited >= QUEUED_TOO_LONG
                    && warned.insert(job.id)
                {
                    progress(ctx, &queued_line(&state, job, waited));
                }
            }
            *slot = Some(state);
        }

        let states_now: Vec<&RunState> = states.iter().flatten().collect();
        if states_now.iter().all(|s| s.finished()) {
            break;
        }
        if Instant::now() + wait.interval > deadline {
            let pending: Vec<String> = states_now
                .iter()
                .filter(|s| !s.finished())
                .map(|s| format!("{} ({})", s.workflow, s.id))
                .collect();
            bail!(
                "Timed out after {} with runs still going: {}",
                output::duration(wait.timeout.as_secs() as i64),
                pending.join(", ")
            );
        }
        crate::wait::rest(wait.wake.as_ref(), wait.interval);
    }
    let states: Vec<RunState> = states.into_iter().flatten().collect();
    if let Some(pattern) = &wait.job
        && states.iter().all(|s| s.jobs.is_empty())
    {
        bail!("No jobs match {pattern:?}");
    }
    Ok(states)
}

/// Watch the runs, print the summary, and fail if any failed. With --notify, also show a desktop
/// notification per run, or one for the error that ended the wait.
pub fn watch_and_report(ctx: &Ctx, runs: &[(Repo, u64)], wait: &Wait) -> Result<()> {
    let states = watch(ctx, runs, wait);
    if wait.notify {
        match &states {
            Ok(states) => states.iter().for_each(|s| notify::send(&note(s))),
            Err(e) => notify::send(&Note::new("Run watch stopped", format!("{e:#}"), None)),
        }
    }
    report(ctx, &states?)
}

/// The notification for a finished run.
fn note(run: &RunState) -> Note {
    let outcome = if run.failed() {
        "failed"
    } else {
        match run.conclusion.as_deref() {
            Some("success") | None => "passed",
            Some(other) => other,
        }
    };
    Note::new(
        format!("{} {outcome}", run.workflow),
        format!("{}: {}", run.repo, run.title),
        Some(run.url.clone()),
    )
}

/// Per-run summary with a line for each job.
pub fn render(runs: &[RunState]) -> String {
    let mut out = String::new();
    for run in runs {
        let state = run.conclusion.as_deref().unwrap_or(&run.status);
        out.push_str(&format!(
            "{} ({}): {state}  {}\n",
            run.workflow, run.id, run.url
        ));
        let width = run.jobs.iter().map(|j| j.name.len()).max().unwrap_or(0);
        for job in &run.jobs {
            let state = job.conclusion.as_deref().unwrap_or(&job.status);
            let took = job.seconds.map(output::duration).unwrap_or_default();
            let line = format!("  {state:<10} {:<width$}  {took}", job.name);
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    out
}

/// Print the summary, then fail if any run or job failed.
pub fn report(ctx: &Ctx, runs: &[RunState]) -> Result<()> {
    if ctx.json {
        output::print_json(runs)?;
    } else {
        print!("{}", render(runs));
    }
    check(runs)
}

/// Fail when any run or job failed, naming the failed jobs.
pub fn check(runs: &[RunState]) -> Result<()> {
    let failed: Vec<String> = runs
        .iter()
        .filter(|r| r.failed())
        .flat_map(|r| {
            let jobs: Vec<String> = r
                .jobs
                .iter()
                .filter(|j| is_failed(j.conclusion.as_deref()))
                .map(|j| format!("{}: {}", r.workflow, j.name))
                .collect();
            if jobs.is_empty() {
                vec![format!("{} ({})", r.workflow, r.id)]
            } else {
                jobs
            }
        })
        .collect();
    if !failed.is_empty() {
        bail!("Failed: {}", failed.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use mockito::{Matcher, Server, ServerGuard};
    use serde_json::json;

    fn repo() -> Repo {
        Repo::parse("zlib-ng/zlib-ng").unwrap()
    }

    fn job(id: u64, name: &str, status: &str, conclusion: Option<&str>) -> Job {
        serde_json::from_value(json!({
            "id": id, "name": name, "status": status, "conclusion": conclusion,
            "created_at": "2026-01-01T00:00:00Z",
            "started_at": "2026-01-01T00:01:00Z",
            "completed_at": if status == "completed" { json!("2026-01-01T00:04:12Z") } else { json!(null) },
            "labels": ["self-hosted", "windows"],
            "html_url": format!("https://github.com/zlib-ng/zlib-ng/actions/runs/9/job/{id}"),
        }))
        .unwrap()
    }

    fn run(status: &str, conclusion: Option<&str>, jobs: &[Job]) -> RunState {
        let run = serde_json::from_value(json!({
            "id": 9, "name": "CI", "display_title": "Fix inflate",
            "html_url": "https://github.com/zlib-ng/zlib-ng/actions/runs/9",
            "status": status, "conclusion": conclusion,
        }))
        .unwrap();
        run_state(&repo(), run, jobs.to_vec())
    }

    #[test]
    fn job_parses_duration_and_labels() {
        let state = run(
            "in_progress",
            None,
            &[job(1, "Ubuntu", "completed", Some("success"))],
        );
        let j = &state.jobs[0];
        assert_eq!(j.seconds, Some(192));
        assert_eq!(j.labels, vec!["self-hosted", "windows"]);
        assert_eq!(finished_line(&state, j), "success    CI: Ubuntu (3m12s)");
        assert_eq!(
            queued_line(&state, j, 420),
            "queued     CI: Ubuntu for 7m00s, waiting for a runner labeled self-hosted, windows"
        );
    }

    #[test]
    fn failure_from_jobs_or_run() {
        let ok = run(
            "completed",
            Some("success"),
            &[job(1, "Ubuntu", "completed", Some("success"))],
        );
        assert!(ok.finished());
        assert!(!ok.failed());
        assert!(check(&[ok]).is_ok());

        let bad = run(
            "completed",
            Some("failure"),
            &[
                job(1, "Ubuntu", "completed", Some("success")),
                job(2, "macOS", "completed", Some("timed_out")),
            ],
        );
        assert!(bad.failed());
        assert_eq!(check(&[bad]).unwrap_err().to_string(), "Failed: CI: macOS");

        // An invalid workflow fails with no jobs at all.
        let startup = run("completed", Some("startup_failure"), &[]);
        assert_eq!(check(&[startup]).unwrap_err().to_string(), "Failed: CI (9)");
    }

    #[test]
    fn job_filter_finishes_early_and_ignores_the_rest() {
        let wait = Wait {
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(1),
            job: Some("export".to_string()),
            notify: false,
            wake: None,
        };
        assert!(wait.wants("Export Check (gcc)"));
        assert!(!wait.wants("Ubuntu"));

        let mut state = run(
            "in_progress",
            None,
            &[
                job(1, "Export Check", "completed", Some("success")),
                job(2, "Ubuntu", "completed", Some("failure")),
                job(3, "macOS", "in_progress", None),
            ],
        );
        assert!(!state.finished());
        state.jobs.retain(|j| wait.wants(&j.name));
        state.filtered = true;
        assert!(state.finished());
        assert!(!state.failed());

        // With nothing matching yet, it waits for the run itself.
        state.jobs.clear();
        assert!(!state.finished());
    }

    #[test]
    fn render_lists_jobs_under_runs() {
        let state = run(
            "in_progress",
            None,
            &[
                job(1, "Ubuntu GCC", "completed", Some("failure")),
                job(2, "macOS", "queued", None),
            ],
        );
        assert_eq!(
            render(&[state]),
            "CI (9): in_progress  https://github.com/zlib-ng/zlib-ng/actions/runs/9\n  \
             failure    Ubuntu GCC  3m12s\n  \
             queued     macOS\n"
        );
    }

    fn ctx(server: &ServerGuard) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json: true,
            yes: true,
        }
    }

    /// Serve run 9 of o/r with `status` and `jobs`, `times` times.
    fn serve_run(server: &mut ServerGuard, status: &str, jobs: serde_json::Value, times: usize) {
        server
            .mock("GET", "/repos/o/r/actions/runs/9")
            .with_body(json!({"id": 9, "name": "CI", "status": status}).to_string())
            .expect(times)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/runs/9/jobs")
            .match_query(Matcher::Any)
            .with_body(json!({ "jobs": jobs }).to_string())
            .expect(times)
            .create();
    }

    fn no_wait(job: Option<&str>) -> Wait {
        Wait {
            interval: Duration::ZERO,
            timeout: Duration::from_secs(60),
            job: job.map(str::to_string),
            notify: false,
            wake: None,
        }
    }

    #[test]
    fn wait_from_args() {
        let wait: Wait = (&crate::cli::WaitArgs {
            interval: 5,
            timeout: 2,
            job: Some("Export".to_string()),
            notify: true,
            wake: Some("WAKE_URL".to_string()),
        })
            .into();
        // The caller resolves the wake, since only it knows the repository.
        assert!(wait.wake.is_none());
        assert_eq!(wait.interval, Duration::from_secs(5));
        assert_eq!(wait.timeout, Duration::from_secs(120));
        assert_eq!(wait.job.as_deref(), Some("export"));
        assert!(wait.notify);
        assert_eq!(Wait::default().interval, Duration::from_secs(20));
    }

    #[test]
    fn watch_polls_until_the_run_completes() {
        let mut server = Server::new();
        // Queued long ago at first, then finished on the next poll.
        serve_run(
            &mut server,
            "in_progress",
            json!([
                {"id": 1, "name": "Build", "status": "queued", "labels": ["big"],
                 "created_at": "2020-01-01T00:00:00Z"},
                {"id": 2, "name": "Lint", "status": "completed", "conclusion": "success"}
            ]),
            1,
        );
        serve_run(
            &mut server,
            "completed",
            json!([
                {"id": 1, "name": "Build", "status": "completed", "conclusion": "success",
                 "started_at": "2026-01-01T00:00:00Z", "completed_at": "2026-01-01T00:01:00Z"},
                {"id": 2, "name": "Lint", "status": "completed", "conclusion": "success"}
            ]),
            1,
        );
        let ctx = ctx(&server);
        let states = watch(&ctx, &[(Repo::parse("o/r").unwrap(), 9)], &no_wait(None)).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].status, "completed");
        assert_eq!(states[0].jobs[0].seconds, Some(60));
        report(&ctx, &states).unwrap();
    }

    #[test]
    fn watch_times_out_with_runs_still_going() {
        let mut server = Server::new();
        serve_run(
            &mut server,
            "in_progress",
            json!([{"id": 1, "name": "Build", "status": "in_progress"}]),
            1,
        );
        let ctx = ctx(&server);
        let wait = Wait {
            interval: Duration::from_secs(60),
            timeout: Duration::ZERO,
            job: None,
            notify: false,
            wake: None,
        };
        let err = watch(&ctx, &[(Repo::parse("o/r").unwrap(), 9)], &wait)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Timed out after 0s with runs still going: CI (9)");
    }

    #[test]
    fn watch_for_matching_jobs_only() {
        let mut server = Server::new();
        // The run is still going, but the job being waited for is done.
        serve_run(
            &mut server,
            "in_progress",
            json!([
                {"id": 1, "name": "Export Check", "status": "completed", "conclusion": "failure"},
                {"id": 2, "name": "Ubuntu", "status": "in_progress"}
            ]),
            1,
        );
        let repo = Repo::parse("o/r").unwrap();
        let states = watch(
            &ctx(&server),
            &[(repo.clone(), 9)],
            &no_wait(Some("export")),
        )
        .unwrap();
        let names: Vec<&str> = states[0].jobs.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, vec!["Export Check"]);
        assert_eq!(
            check(&states).unwrap_err().to_string(),
            "Failed: CI: Export Check"
        );

        let mut server = Server::new();
        serve_run(
            &mut server,
            "completed",
            json!([{"id": 2, "name": "Ubuntu", "status": "completed"}]),
            1,
        );
        let err = watch(&ctx(&server), &[(repo, 9)], &no_wait(Some("export")))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "No jobs match \"export\"");
    }
}

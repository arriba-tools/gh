pub mod artifacts;
pub mod caches;
pub mod stats;
pub mod watch;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::api::actions::{self, Job, WorkflowRun};
use crate::api::{self, Repo, is_failed};
use crate::cli::{DispatchArgs, LogsArgs, RunCommand, WatchArgs};
use crate::context::{Ctx, git};
use crate::log::{self, Excerpt};
use crate::{output, parallel, wake, workflow};

pub fn run(ctx: &Ctx, cmd: RunCommand) -> Result<()> {
    match cmd {
        RunCommand::Logs(args) => logs(ctx, args),
        RunCommand::Watch(args) => watch_runs(ctx, args),
        RunCommand::Dispatch(args) => dispatch(ctx, args),
        RunCommand::Artifacts(args) => artifacts::run(ctx, args),
        RunCommand::Caches(args) => caches::run(ctx, args),
        RunCommand::Stats(args) => stats::run(ctx, &args),
    }
}

/// What a RUN_OR_JOB argument names.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    Run {
        id: u64,
        attempt: Option<u64>,
    },
    Job(u64),
    /// A bare number, which may be either.
    Unknown(u64),
}

/// Parse a number or an Actions URL. A URL also yields its repository.
fn parse_target(s: &str) -> Result<(Option<Repo>, Target)> {
    let s = s.trim();
    if let Ok(id) = s.parse() {
        return Ok((None, Target::Unknown(id)));
    }
    let path = s
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, path)| path)
        .with_context(|| format!("Expected a run ID, job ID, or Actions URL, got {s:?}"))?;
    let path = path.split(['#', '?']).next().unwrap_or_default();
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let bad = || anyhow::anyhow!("Expected an Actions run or job URL, got {s:?}");
    let [owner, name, "actions", "runs", run, rest @ ..] = parts.as_slice() else {
        return Err(bad());
    };
    let repo = Repo {
        owner: owner.to_string(),
        name: name.to_string(),
    };
    let run: u64 = run.parse().map_err(|_| bad())?;
    let target = match rest {
        ["job", job, ..] => Target::Job(job.parse().map_err(|_| bad())?),
        ["attempts", attempt, ..] => Target::Run {
            id: run,
            attempt: Some(attempt.parse().map_err(|_| bad())?),
        },
        _ => Target::Run {
            id: run,
            attempt: None,
        },
    };
    Ok((Some(repo), target))
}

#[derive(Serialize)]
struct Downloaded {
    job_id: u64,
    name: String,
    conclusion: Option<String>,
    path: PathBuf,
    /// Whether the log was saved by an earlier call instead of downloaded now.
    cached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    excerpt: Option<Excerpt>,
}

/// Whether a job name contains any of `patterns`, ignoring case. No patterns keeps every job.
pub(crate) fn name_matches(name: &str, patterns: &[String]) -> bool {
    let name = name.to_lowercase();
    patterns.is_empty() || patterns.iter().any(|p| name.contains(&p.to_lowercase()))
}

/// Turn a job name into a file name, keeping it readable.
fn sanitize(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "job".to_string()
    } else {
        out.to_string()
    }
}

fn logs(ctx: &Ctx, args: LogsArgs) -> Result<()> {
    let (url_repo, target) = parse_target(&args.target)?;
    let repo = ctx.repo_or(url_repo)?;
    let client = &ctx.client;

    // A bare number is tried as a run, then as a job.
    let target = match target {
        Target::Unknown(id) => {
            if actions::workflow_run_opt(client, &repo, id)?.is_some() {
                Target::Run { id, attempt: None }
            } else if actions::job_opt(client, &repo, id)?.is_some() {
                Target::Job(id)
            } else {
                anyhow::bail!("No run or job {id} in {repo}");
            }
        }
        other => other,
    };

    let (run_id, jobs) = match target {
        Target::Job(id) => {
            let job = actions::job(client, &repo, id)?;
            (job.run_id, vec![job])
        }
        Target::Run { id, attempt } => {
            let jobs = match attempt {
                Some(a) => actions::attempt_jobs(client, &repo, id, a)?,
                None => actions::run_jobs(client, &repo, id)?,
            };
            let jobs: Vec<Job> = jobs
                .into_iter()
                .filter(|j| !args.failed || is_failed(j.conclusion.as_deref()))
                .collect();
            (id, jobs)
        }
        Target::Unknown(_) => unreachable!("resolved above"),
    };
    let jobs: Vec<Job> = jobs
        .into_iter()
        .filter(|j| name_matches(&j.name, &args.jobs))
        .collect();

    if jobs.is_empty() {
        let what = if args.failed { "failed jobs" } else { "jobs" };
        let named = if args.jobs.is_empty() {
            String::new()
        } else {
            format!(" matching {}", args.jobs.join(", "))
        };
        anyhow::bail!("No {what}{named} in run {run_id}");
    }

    let base = args.out.unwrap_or_else(log::default_dir);
    let dir = base.join(run_id.to_string());
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;

    let refresh = args.refresh;
    let mut downloaded = Vec::new();
    // Each log is a redirect to blob storage, so fetch a few at a time.
    let results = parallel::map(&jobs, |job| {
        // A running job's log is still growing, so only finished ones are reused.
        let reuse = !refresh && job.status == "completed";
        download(ctx, &repo, job.id, &dir, &job.name, reuse)
    });
    for (job, result) in jobs.iter().zip(results) {
        let id = job.id;
        let name = job.name.clone();
        let conclusion = job.conclusion.clone();
        match result {
            Ok((path, cached)) => {
                let excerpt = if args.errors || args.grep.is_some() {
                    let text = fs::read_to_string(&path)
                        .with_context(|| format!("Failed to read {}", path.display()))?;
                    Some(log::select(
                        &text,
                        args.grep.as_ref(),
                        args.context,
                        args.max,
                    ))
                } else {
                    None
                };
                if !ctx.json {
                    let label = conclusion.as_deref().unwrap_or("running");
                    let note = if cached { " (saved earlier)" } else { "" };
                    println!("{label:<10} {}{note}", path.display());
                    if let Some(x) = &excerpt {
                        let pattern = args.grep.as_ref().map(|re| re.as_str());
                        print!("{}", log::render(x, pattern, "  "));
                    }
                }
                downloaded.push(Downloaded {
                    job_id: id,
                    name,
                    conclusion,
                    path,
                    cached,
                    excerpt,
                });
            }
            Err(e) => eprintln!("warning: skipped {name} ({id}): {}", short_error(&e)),
        }
    }

    if ctx.json {
        output::print_json(&downloaded)?;
    }
    Ok(())
}

/// One-line reason for a failed download. Jobs cancelled before they start have no log blob.
pub(crate) fn short_error(e: &anyhow::Error) -> String {
    let text = format!("{e:#}");
    if text.contains("404") {
        return "no log available, the job may not have started".to_string();
    }
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(120) {
        Some((i, _)) => format!("{}...", &line[..i]),
        None => line.to_string(),
    }
}

/// Save a job's log under `dir` and return its path, plus whether a copy saved earlier was used.
/// A finished job's log never changes and a rerun gets a new job ID, so with `reuse` set an
/// existing file is kept instead of downloading it again.
pub(crate) fn download(
    ctx: &Ctx,
    repo: &Repo,
    job_id: u64,
    dir: &Path,
    name: &str,
    reuse: bool,
) -> Result<(PathBuf, bool)> {
    let path = dir.join(format!("{}-{job_id}.log", sanitize(name)));
    if reuse && has_saved_copy(&path) {
        return Ok((path, true));
    }
    let text = actions::job_log(&ctx.client, repo, job_id)?;
    // Write beside the final name and rename, so an interrupted download is never reused.
    let part = path.with_extension("log.part");
    fs::write(&part, text).with_context(|| format!("Failed to write {}", part.display()))?;
    fs::rename(&part, &path).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok((path, false))
}

/// Whether a non-empty log was saved at `path` by an earlier download.
fn has_saved_copy(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
}

fn watch_runs(ctx: &Ctx, args: WatchArgs) -> Result<()> {
    let mut wait: watch::Wait = (&args.wait).into();
    let runs = if args.runs.is_empty() {
        let repo = ctx.repo()?;
        let commit = match (&args.sha, &args.branch) {
            (Some(sha), _) => Commit::Sha(sha.clone()),
            (None, Some(branch)) => Commit::Branch(branch.clone()),
            (None, None) => Commit::Sha(git(&["rev-parse", "HEAD"])?),
        };
        find_runs(ctx, &repo, &commit, args.workflow.as_deref(), Some(&wait))?
            .into_iter()
            .map(|id| (repo.clone(), id))
            .collect()
    } else {
        args.runs
            .iter()
            .map(|r| resolve_run(ctx, r))
            .collect::<Result<Vec<_>>>()?
    };
    if let Some((repo, _)) = runs.first() {
        wait.wake = wake::resolve(&ctx.client, repo, args.wait.wake.as_deref())?;
    }
    watch::watch_and_report(ctx, &runs, &wait)
}

/// Which commit's runs to watch.
pub(crate) enum Commit {
    Sha(String),
    /// The newest commit on the branch that has runs.
    Branch(String),
}

/// How long to wait for runs to show up after a push.
const RUNS_GRACE: Duration = Duration::from_secs(120);

/// Run IDs for a commit. With `wait`, polls for a while since runs appear some seconds after a
/// push; without it, looks once.
pub(crate) fn find_runs(
    ctx: &Ctx,
    repo: &Repo,
    commit: &Commit,
    workflow: Option<&str>,
    wait: Option<&watch::Wait>,
) -> Result<Vec<u64>> {
    let what = match commit {
        Commit::Sha(sha) => format!("commit {}", &sha[..sha.len().min(10)]),
        Commit::Branch(b) => format!("branch {b}"),
    };
    let for_workflow = workflow
        .map(|w| format!(" for workflow {w}"))
        .unwrap_or_default();
    let deadline = Instant::now() + wait.map_or(Duration::ZERO, |_| RUNS_GRACE);
    let mut announced = false;
    loop {
        let runs = match commit {
            Commit::Sha(sha) => actions::commit_runs(&ctx.client, repo, sha)?,
            // Only the newest page matters, older commits are not watched.
            Commit::Branch(b) => actions::branch_runs(&ctx.client, repo, b)?,
        };
        let ids = select_runs(&runs, workflow);
        if !ids.is_empty() {
            return Ok(ids);
        }
        if Instant::now() >= deadline {
            let seen: BTreeSet<&str> = runs
                .iter()
                .map(|r| r.name.as_str())
                .filter(|n| !n.is_empty())
                .collect();
            let hint = if seen.is_empty() {
                " Push the commit first, or pass --sha or --branch.".to_string()
            } else {
                format!(" Workflows with runs: {}", Vec::from_iter(seen).join(", "))
            };
            anyhow::bail!("No runs{for_workflow} on {what} in {repo}.{hint}");
        }
        if !announced {
            watch::progress(ctx, &format!("Waiting for runs{for_workflow} on {what}..."));
            announced = true;
        }
        let interval = wait.map_or(Duration::ZERO, |w| w.interval);
        std::thread::sleep(interval.min(Duration::from_secs(10)));
    }
}

/// Runs of the newest commit in `runs`, which the API lists newest first, narrowed to a workflow.
fn select_runs(runs: &[WorkflowRun], workflow: Option<&str>) -> Vec<u64> {
    let runs: Vec<&WorkflowRun> = runs
        .iter()
        .filter(|r| workflow.is_none_or(|w| crate::workflow::matches(&r.name, &r.path, w)))
        .collect();
    let Some(sha) = runs.first().map(|r| r.head_sha.as_str()) else {
        return Vec::new();
    };
    let mut ids: Vec<u64> = runs
        .iter()
        .filter(|r| r.head_sha == sha)
        .map(|r| r.id)
        .collect();
    ids.sort_unstable();
    ids
}

/// The repository and run a RUN argument names. A job ID or URL stands for the run it belongs to.
pub(crate) fn resolve_run(ctx: &Ctx, s: &str) -> Result<(Repo, u64)> {
    let (url_repo, target) = parse_target(s)?;
    let repo = ctx.repo_or(url_repo)?;
    let job_run = |id: u64| -> Result<Option<u64>> {
        Ok(actions::job_opt(&ctx.client, &repo, id)?.map(|job| job.run_id))
    };
    let id = match target {
        Target::Run { id, .. } => id,
        Target::Job(id) => job_run(id)?.with_context(|| format!("No job {id} in {repo}"))?,
        Target::Unknown(id) => {
            if actions::workflow_run_opt(&ctx.client, &repo, id)?.is_some() {
                id
            } else {
                job_run(id)?.with_context(|| format!("No run or job {id} in {repo}"))?
            }
        }
    };
    Ok((repo, id))
}

#[derive(Serialize)]
struct Dispatched {
    repo: String,
    workflow: String,
    #[serde(rename = "ref")]
    git_ref: String,
    run: Option<workflow::NewRun>,
}

fn dispatch(ctx: &Ctx, args: DispatchArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let client = &ctx.client;
    let workflows = workflow::list(client, &repo)?;
    let wf = workflow::find(&workflows, &args.workflow)?;
    let git_ref = match args.git_ref {
        Some(r) => r,
        None => api::default_branch(client, &repo)?,
    };

    let inputs = workflow::inputs_at(client, &repo, wf, &git_ref)?;
    let fields = workflow::parse_fields(&args.fields, &inputs)?;
    let missing = workflow::missing(&inputs, &fields);
    if !missing.is_empty() {
        anyhow::bail!(
            "Required inputs have no value: {}. Set them with -f KEY=VALUE.",
            missing.join(", ")
        );
    }
    let sent = fields
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    let run = workflow::dispatch(client, &repo, wf, &git_ref, sent)?;

    let line = match &run {
        Some(r) => format!(
            "Dispatched {} on {git_ref}: run {} {}",
            wf.name, r.id, r.url
        ),
        None => format!(
            "Dispatched {} on {git_ref}. The run is not visible yet, see https://github.com/{repo}/actions/workflows/{}",
            wf.name,
            wf.file()
        ),
    };
    if !args.watch {
        if ctx.json {
            return output::print_json(&Dispatched {
                repo: repo.to_string(),
                workflow: wf.name.clone(),
                git_ref,
                run,
            });
        }
        println!("{line}");
        return Ok(());
    }

    // The watch summary is the JSON document, so the dispatch line goes with the progress.
    watch::progress(ctx, &line);
    let run = run.context("Can't watch, the dispatched run did not appear in time")?;
    let mut wait: watch::Wait = (&args.wait).into();
    wait.wake = wake::resolve(&ctx.client, &repo, args.wait.wake.as_deref())?;
    watch::watch_and_report(ctx, &[(repo, run.id)], &wait)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use crate::cli::WaitArgs;
    use mockito::{Matcher, Server, ServerGuard};

    fn repo() -> Option<Repo> {
        Some(Repo {
            owner: "zlib-ng".to_string(),
            name: "zlib-ng".to_string(),
        })
    }

    #[test]
    fn bare_number_is_ambiguous() {
        assert_eq!(parse_target("123").unwrap(), (None, Target::Unknown(123)));
    }

    #[test]
    fn run_url_variants() {
        let base = "https://github.com/zlib-ng/zlib-ng/actions/runs/39608462685";
        assert_eq!(
            parse_target(base).unwrap(),
            (
                repo(),
                Target::Run {
                    id: 39608462685,
                    attempt: None
                }
            )
        );
        assert_eq!(
            parse_target(&format!("{base}/attempts/2")).unwrap(),
            (
                repo(),
                Target::Run {
                    id: 39608462685,
                    attempt: Some(2)
                }
            )
        );
        assert_eq!(
            parse_target(&format!("{base}/job/119251542374?pr=2480")).unwrap(),
            (repo(), Target::Job(119251542374))
        );
    }

    #[test]
    fn non_actions_url_is_rejected() {
        assert!(parse_target("https://github.com/zlib-ng/zlib-ng/pull/1").is_err());
        assert!(parse_target("main").is_err());
    }

    #[test]
    fn sanitize_collapses_separators() {
        assert_eq!(
            sanitize("CMake / Ubuntu GCC PPC64"),
            "CMake-Ubuntu-GCC-PPC64"
        );
        assert_eq!(sanitize("build (x86_64, 1.2)"), "build-x86_64-1.2");
        assert_eq!(sanitize("///"), "job");
    }

    #[test]
    fn saved_copy_needs_a_non_empty_file() {
        let dir = std::env::temp_dir().join(format!("gh-arriba-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("build-1.log");
        assert!(!has_saved_copy(&path));
        fs::write(&path, "").unwrap();
        assert!(!has_saved_copy(&path));
        fs::write(&path, "log").unwrap();
        assert!(has_saved_copy(&path));
        assert!(!has_saved_copy(&dir));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn job_name_filter() {
        let patterns = vec!["windows".to_string(), "Lint".to_string()];
        assert!(name_matches("Build Windows MSVC", &patterns));
        assert!(name_matches("lint", &patterns));
        assert!(!name_matches("Ubuntu GCC", &patterns));
        assert!(name_matches("anything", &[]));
    }

    #[test]
    fn runs_of_newest_commit_and_workflow() {
        use serde_json::json;
        let runs: Vec<WorkflowRun> = serde_json::from_value(json!([
            {"id": 30, "name": "Lint", "path": ".github/workflows/lint.yml", "head_sha": "bbb"},
            {"id": 20, "name": "Build", "path": ".github/workflows/build.yml", "head_sha": "bbb"},
            {"id": 10, "name": "Build", "path": ".github/workflows/build.yml", "head_sha": "aaa"},
        ]))
        .unwrap();
        assert_eq!(select_runs(&runs, None), vec![20, 30]);
        assert_eq!(select_runs(&runs, Some("build.yml")), vec![20]);
        assert!(select_runs(&runs, Some("deploy")).is_empty());
        assert!(select_runs(&[], None).is_empty());
    }

    fn ctx(server: &ServerGuard, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    fn temp_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gh-arriba-run-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn logs_args(target: &str, out: &Path) -> LogsArgs {
        LogsArgs {
            target: target.to_string(),
            failed: false,
            out: Some(out.to_path_buf()),
            jobs: Vec::new(),
            errors: false,
            grep: None,
            context: log::CONTEXT,
            max: log::MAX,
            refresh: false,
        }
    }

    fn wait_args() -> WaitArgs {
        WaitArgs {
            interval: 1,
            timeout: 1,
            job: None,
            notify: false,
            wake: None,
        }
    }

    /// A run 9 with a failed and a passing job, each with a log.
    fn run_with_jobs(server: &mut ServerGuard) {
        server
            .mock("GET", "/repos/o/r/actions/runs/9")
            .with_body(r#"{"id": 9, "name": "CI", "status": "completed"}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/runs/9/jobs")
            .match_query(Matcher::Any)
            .with_body(
                r#"{"jobs": [
                    {"id": 1, "run_id": 9, "name": "Build / Linux", "status": "completed",
                     "conclusion": "failure"},
                    {"id": 2, "run_id": 9, "name": "Lint", "status": "completed",
                     "conclusion": "success"}
                ]}"#,
            )
            .create();
    }

    #[test]
    fn logs_saves_failed_jobs_and_reuses_them() {
        let mut server = Server::new();
        run_with_jobs(&mut server);
        let log1 = server
            .mock("GET", "/repos/o/r/actions/jobs/1/logs")
            .with_body("2026-01-01T00:00:00Z compiling\n2026-01-01T00:00:01Z error: boom\n")
            .expect(2)
            .create();
        let log2 = server
            .mock("GET", "/repos/o/r/actions/jobs/2/logs")
            .expect(0)
            .create();
        let ctx = ctx(&server, true);
        let out = temp_dir("logs");
        let path = out.join("9").join("Build-Linux-1.log");

        let mut args = logs_args("9", &out);
        args.failed = true;
        args.grep = Some(regex::Regex::new("boom").unwrap());
        logs(&ctx, args).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "2026-01-01T00:00:00Z compiling\n2026-01-01T00:00:01Z error: boom\n"
        );
        assert!(!out.join("9").join("Build-Linux-1.log.part").exists());

        // A finished job's saved log is reused instead of downloaded again...
        let mut args = logs_args("9", &out);
        args.failed = true;
        args.errors = true;
        logs(&ctx, args).unwrap();
        // ...unless asked to refresh it.
        let mut args = logs_args("9", &out);
        args.failed = true;
        args.refresh = true;
        logs(&ctx, args).unwrap();
        log1.assert();
        log2.assert();
        fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn logs_by_job_url_and_name_filter() {
        let mut server = Server::new();
        run_with_jobs(&mut server);
        server
            .mock("GET", "/repos/o/r/actions/jobs/2")
            .with_body(r#"{"id": 2, "run_id": 9, "name": "Lint", "status": "completed"}"#)
            .create();
        let lint = server
            .mock("GET", "/repos/o/r/actions/jobs/2/logs")
            .with_body("ok\n")
            .expect(2)
            .create();
        let ctx = ctx(&server, false);
        let out = temp_dir("logs-job");

        logs(
            &ctx,
            logs_args("https://github.com/o/r/actions/runs/9/job/2", &out),
        )
        .unwrap();
        assert!(out.join("9").join("Lint-2.log").exists());

        let mut args = logs_args("9", &out);
        args.jobs = vec!["lint".to_string()];
        args.refresh = true;
        logs(&ctx, args).unwrap();

        let mut args = logs_args("9", &out);
        args.jobs = vec!["windows".to_string()];
        let err = logs(&ctx, args).unwrap_err().to_string();
        assert_eq!(err, "No jobs matching windows in run 9");
        lint.assert();
        fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn logs_of_a_bare_job_id_and_unknown_ids() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/runs/1")
            .with_status(404)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/runs/5")
            .with_status(404)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/jobs/5")
            .with_status(404)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/jobs/1")
            .with_body(r#"{"id": 1, "run_id": 9, "name": "Build", "status": "in_progress"}"#)
            .create();
        // A job that never started has no log, which is skipped with a warning.
        server
            .mock("GET", "/repos/o/r/actions/jobs/1/logs")
            .with_status(404)
            .create();
        let ctx = ctx(&server, true);
        let out = temp_dir("logs-bare");
        logs(&ctx, logs_args("1", &out)).unwrap();
        assert!(!out.join("9").join("Build-1.log").exists());
        let err = logs(&ctx, logs_args("5", &out)).unwrap_err().to_string();
        assert_eq!(err, "No run or job 5 in o/r");
        fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn logs_with_no_failed_jobs() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/runs/9/attempts/2/jobs")
            .match_query(Matcher::Any)
            .with_body(r#"{"jobs": [{"id": 1, "name": "Lint", "conclusion": "success"}]}"#)
            .create();
        let ctx = ctx(&server, true);
        let out = temp_dir("logs-none");
        let mut args = logs_args("https://github.com/o/r/actions/runs/9/attempts/2", &out);
        args.failed = true;
        let err = logs(&ctx, args).unwrap_err().to_string();
        assert_eq!(err, "No failed jobs in run 9");
        assert!(!out.exists());
    }

    #[test]
    fn short_errors() {
        let not_found = anyhow::anyhow!("GitHub API 404 on GET x: Not Found");
        assert_eq!(
            short_error(&not_found),
            "no log available, the job may not have started"
        );
        let long = anyhow::anyhow!("{}\nsecond line", "x".repeat(200));
        assert_eq!(short_error(&long), format!("{}...", "x".repeat(120)));
        assert_eq!(short_error(&anyhow::anyhow!("timed out")), "timed out");
    }

    #[test]
    fn resolve_run_from_runs_jobs_and_urls() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/runs/9")
            .with_body(r#"{"id": 9}"#)
            .create();
        for id in [1, 3] {
            server
                .mock("GET", format!("/repos/o/r/actions/runs/{id}").as_str())
                .with_status(404)
                .create();
        }
        server
            .mock("GET", "/repos/o/r/actions/jobs/1")
            .with_body(r#"{"id": 1, "run_id": 9}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/jobs/3")
            .with_status(404)
            .create();
        let ctx = ctx(&server, true);
        let o_r = Repo::parse("o/r").unwrap();
        assert_eq!(resolve_run(&ctx, "9").unwrap(), (o_r.clone(), 9));
        assert_eq!(resolve_run(&ctx, "1").unwrap(), (o_r.clone(), 9));
        assert_eq!(
            resolve_run(&ctx, "https://github.com/o/r/actions/runs/9/job/1").unwrap(),
            (o_r, 9)
        );
        // A URL names its own repository, which is not looked up.
        assert_eq!(
            resolve_run(&ctx, "https://github.com/a/b/actions/runs/4").unwrap(),
            (Repo::parse("a/b").unwrap(), 4)
        );
        assert_eq!(
            resolve_run(&ctx, "3").unwrap_err().to_string(),
            "No run or job 3 in o/r"
        );
        assert_eq!(
            resolve_run(&ctx, "https://github.com/o/r/actions/runs/9/job/3")
                .unwrap_err()
                .to_string(),
            "No job 3 in o/r"
        );
    }

    #[test]
    fn find_runs_once_names_what_it_saw() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/runs")
            .match_query(Matcher::UrlEncoded("head_sha".into(), "abc".into()))
            .with_body(
                r#"{"workflow_runs": [
                    {"id": 2, "name": "Lint", "path": ".github/workflows/lint.yml", "head_sha": "abc"},
                    {"id": 1, "name": "CI", "path": ".github/workflows/ci.yml", "head_sha": "abc"}
                ]}"#,
            )
            .create();
        server
            .mock("GET", "/repos/o/r/actions/runs")
            .match_query(Matcher::UrlEncoded("head_sha".into(), "def".into()))
            .with_body(r#"{"workflow_runs": []}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/runs")
            .match_query(Matcher::UrlEncoded("branch".into(), "main".into()))
            .with_body(r#"{"workflow_runs": [{"id": 5, "name": "CI", "head_sha": "x"}]}"#)
            .create();
        let ctx = ctx(&server, true);
        let repo = Repo::parse("o/r").unwrap();
        let sha = |s: &str| Commit::Sha(s.to_string());
        assert_eq!(
            find_runs(&ctx, &repo, &sha("abc"), None, None).unwrap(),
            vec![1, 2]
        );
        assert_eq!(
            find_runs(&ctx, &repo, &sha("abc"), Some("ci"), None).unwrap(),
            vec![1]
        );
        assert_eq!(
            find_runs(&ctx, &repo, &sha("abc"), Some("deploy"), None)
                .unwrap_err()
                .to_string(),
            "No runs for workflow deploy on commit abc in o/r. Workflows with runs: CI, Lint"
        );
        assert_eq!(
            find_runs(&ctx, &repo, &sha("def"), None, None)
                .unwrap_err()
                .to_string(),
            "No runs on commit def in o/r. Push the commit first, or pass --sha or --branch."
        );
        let branch = Commit::Branch("main".to_string());
        assert_eq!(
            find_runs(&ctx, &repo, &branch, None, None).unwrap(),
            vec![5]
        );
    }

    #[test]
    fn watch_named_runs_that_already_finished() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/runs/9")
            .with_body(r#"{"id": 9, "name": "CI", "status": "completed", "conclusion": "failure"}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/runs/9/jobs")
            .match_query(Matcher::Any)
            .with_body(
                r#"{"jobs": [{"id": 1, "name": "Build", "status": "completed",
                    "conclusion": "failure"}]}"#,
            )
            .create();
        let ctx = ctx(&server, true);
        let args = WatchArgs {
            runs: vec!["https://github.com/o/r/actions/runs/9".to_string()],
            sha: None,
            branch: None,
            workflow: None,
            wait: wait_args(),
        };
        let err = watch_runs(&ctx, args).unwrap_err().to_string();
        assert_eq!(err, "Failed: CI: Build");

        // --notify shows one notification per run.
        let args = WatchArgs {
            runs: vec!["9".to_string()],
            sha: None,
            branch: None,
            workflow: None,
            wait: WaitArgs {
                notify: true,
                ..wait_args()
            },
        };
        assert!(watch_runs(&ctx, args).is_err());
        let notes = crate::notify::sent();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "CI failed");

        // Watching a commit finds its runs first.
        server
            .mock("GET", "/repos/o/r/actions/runs")
            .match_query(Matcher::UrlEncoded("head_sha".into(), "abc".into()))
            .with_body(r#"{"workflow_runs": [{"id": 9, "name": "CI", "head_sha": "abc"}]}"#)
            .create();
        let args = WatchArgs {
            runs: Vec::new(),
            sha: Some("abc".to_string()),
            branch: None,
            workflow: None,
            wait: wait_args(),
        };
        assert!(watch_runs(&ctx, args).is_err());
    }

    /// A repository with a release workflow that takes a required `version` input.
    fn release_workflow(server: &mut ServerGuard) {
        server
            .mock("GET", "/repos/o/r/actions/workflows")
            .match_query(Matcher::Any)
            .with_body(
                r#"{"workflows": [{"id": 7, "name": "Release",
                    "path": ".github/workflows/release.yml", "state": "active"}]}"#,
            )
            .create();
        server
            .mock("GET", "/repos/o/r")
            .with_body(r#"{"default_branch": "main"}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/contents/.github/workflows/release.yml")
            .match_query(Matcher::UrlEncoded("ref".into(), "main".into()))
            .with_body(
                "on:\n  workflow_dispatch:\n    inputs:\n      version:\n        required: true\n      \
                 channel:\n        default: stable\n",
            )
            .create();
    }

    fn dispatch_args(fields: &[&str]) -> DispatchArgs {
        DispatchArgs {
            workflow: "release".to_string(),
            git_ref: None,
            fields: fields.iter().map(|f| f.to_string()).collect(),
            watch: false,
            wait: wait_args(),
        }
    }

    #[test]
    fn dispatch_validates_inputs_before_sending() {
        let mut server = Server::new();
        release_workflow(&mut server);
        let sent = server
            .mock("POST", "/repos/o/r/actions/workflows/7/dispatches")
            .expect(0)
            .create();
        let ctx = ctx(&server, true);
        assert_eq!(
            dispatch(&ctx, dispatch_args(&[])).unwrap_err().to_string(),
            "Required inputs have no value: version. Set them with -f KEY=VALUE."
        );
        assert_eq!(
            dispatch(&ctx, dispatch_args(&["version=1.0", "target=prod"]))
                .unwrap_err()
                .to_string(),
            "target is not an input of this workflow. Inputs: version, channel"
        );
        let mut args = dispatch_args(&["version=1.0"]);
        args.workflow = "deploy".to_string();
        assert!(
            dispatch(&ctx, args)
                .unwrap_err()
                .to_string()
                .starts_with("No workflow matches \"deploy\"")
        );
        sent.assert();
    }

    #[test]
    fn dispatch_sends_inputs_and_finds_the_new_run() {
        let mut server = Server::new();
        release_workflow(&mut server);
        // The run listing before dispatching, then once the new run shows up.
        server
            .mock("GET", "/repos/o/r/actions/workflows/7/runs")
            .match_query(Matcher::Any)
            .with_body(r#"{"workflow_runs": [{"id": 1, "actor": {"login": "me"}}]}"#)
            .expect(1)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/workflows/7/runs")
            .match_query(Matcher::Any)
            .with_body(
                r#"{"workflow_runs": [
                    {"id": 3, "actor": {"login": "someone"}},
                    {"id": 2, "actor": {"login": "me"}, "html_url": "https://h/2"},
                    {"id": 1, "actor": {"login": "me"}}
                ]}"#,
            )
            .create();
        let sent = server
            .mock("POST", "/repos/o/r/actions/workflows/7/dispatches")
            .match_body(Matcher::Json(
                serde_json::json!({"ref": "main", "inputs": {"version": "1.0"}}),
            ))
            .with_status(204)
            .create();
        server
            .mock("POST", "/graphql")
            .with_body(r#"{"data": {"viewer": {"login": "me"}}}"#)
            .create();
        let ctx = ctx(&server, true);
        dispatch(&ctx, dispatch_args(&["version=1.0"])).unwrap();
        sent.assert();
    }
}

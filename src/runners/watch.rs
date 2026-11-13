//! `runners watch`: polls runners and the jobs waiting for them, and shows a desktop notification
//! when a runner goes offline, sits idle while a job it could run waits, or a job waits too long.

use anyhow::Result;
use chrono::{DateTime, Local, TimeDelta, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use super::{
    Cache, Job, Runner, access_hint, active_jobs, elapsed, in_parallel, is_forbidden, org_repos,
    scope_runners,
};
use crate::cli::RunnersWatchArgs;
use crate::context::Ctx;
use crate::dependabot::Scope;
use crate::notify::{self, Note};
use crate::startup::{self, Service};
use crate::wait;

/// The runners watch, as it starts at login.
const SERVICE: Service = Service {
    label: "com.github.nmoinvaz.gh-arriba.runners-watch",
    unit: "gh-arriba-runners-watch.service",
    log: "gh-arriba-runners-watch.log",
    description: "gh arriba runners watch",
};

/// Unless an idle runner could run a job the last scan saw waiting, the queue is scanned only every
/// this many polls, often enough to see a job wait too long.
const SCAN_EVERY: u64 = 5;

/// More events than this in one poll make a single summary notification.
const MAX_NOTES: usize = 3;

/// Scans pause while fewer requests than this are left in the hour, keeping them for other work.
const RESERVE: u64 = 1000;

pub fn run(ctx: &Ctx, args: &RunnersWatchArgs) -> Result<()> {
    if args.uninstall {
        if !startup::uninstall(&SERVICE)? {
            println!("No runners watch was set to start at login");
        }
        return Ok(());
    }
    let scope = Scope::new(ctx, args.org.clone())?;
    if args.install {
        // A wrong scope fails here rather than in a service that restarts every minute. The
        // runners API answers 404 to a missing scope and a token without access alike.
        match &scope {
            Scope::Org(org) => ctx.client.get(&format!("orgs/{org}"))?,
            Scope::Repo(repo) => ctx.client.get(&repo.api())?,
        };
        if let Err(e) = scope_runners(&ctx.client, &scope) {
            if !is_forbidden(&e) {
                return Err(e);
            }
            eprintln!(
                "warning: {} Until then, only jobs that wait too long are reported.",
                access_hint(&scope)
            );
        }
        // The service runs outside any checkout, so it is given the scope found here.
        return startup::install(&SERVICE, |exe| watch_argv(exe, args, &scope, ctx.json));
    }
    watch(ctx, args, &scope)
}

/// The command line the service runs: this executable, the scope, and the watch options given now.
fn watch_argv(exe: &Path, args: &RunnersWatchArgs, scope: &Scope, json: bool) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        exe.to_string_lossy().into_owned(),
        "runners".into(),
        "watch".into(),
        "--interval".into(),
        args.interval.to_string(),
        "--max-wait".into(),
        args.max_wait.to_string(),
    ];
    match scope {
        Scope::Org(org) => argv.extend(["--org".into(), org.clone()]),
        Scope::Repo(repo) => argv.extend(["-R".into(), repo.to_string()]),
    }
    if json {
        argv.push("--json".into());
    }
    argv
}

fn watch(ctx: &Ctx, args: &RunnersWatchArgs, scope: &Scope) -> Result<()> {
    let max_wait = TimeDelta::minutes(args.max_wait as i64);
    let cache = Cache::default();
    let mut state = State::default();
    wait::progress(
        ctx,
        &format!(
            "Watching the runners of {scope} every {}s, for jobs waiting over {} min",
            args.interval, args.max_wait
        ),
    );
    loop {
        match poll(ctx, scope, &cache, &mut state) {
            Ok(seen) => {
                let now = Utc::now();
                let events = step(&mut state, &seen, now, max_wait);
                announce(ctx, scope, &events, now, args.max_wait)?;
            }
            // A wrong org or token shows on the first poll; later failures are likely transient.
            Err(e) if state.polls == 0 => return Err(e),
            Err(e) => eprintln!("warning: {e:#}"),
        }
        std::thread::sleep(Duration::from_secs(args.interval));
    }
}

/// What one poll saw.
#[derive(Debug, Default)]
struct Seen {
    /// The scope's runners, or None when the token may not list them.
    runners: Option<Vec<Runner>>,
    /// Jobs waiting for a runner, oldest first, or None when the queue was not scanned.
    queued: Option<Vec<Job>>,
}

/// What the watch remembers between polls.
#[derive(Debug, Default)]
struct State {
    /// Runners reported offline.
    offline: BTreeSet<String>,
    /// Runners idle beside a job they could run on the last scan.
    idle: BTreeSet<String>,
    /// Runners reported as stalled, until they stop being idle beside work.
    stalled: BTreeSet<String>,
    /// Label sets reported as waiting too long, until none of their jobs is.
    waiting: BTreeSet<String>,
    /// A stall or long wait was reported and has not cleared.
    alerted: bool,
    /// The label sets the last scan saw jobs waiting for.
    last_queue: BTreeSet<Vec<String>>,
    /// The runners API refused the token.
    no_access: bool,
    /// Scans are paused while the hour's requests run low.
    paused: bool,
    polls: u64,
}

/// Read the runners, and the queue when it could show something: on every poll while an idle
/// runner could run a job the last scan saw waiting, else on every SCAN_EVERY-th, and never while
/// the hour's requests run low.
fn poll(ctx: &Ctx, scope: &Scope, cache: &Cache, state: &mut State) -> Result<Seen> {
    let client = &ctx.client;
    let runners = match scope_runners(client, scope) {
        Ok(list) => {
            if state.no_access {
                wait::progress(ctx, "The runners can be listed now");
                state.no_access = false;
            }
            Some(list.into_iter().map(Runner::from).collect::<Vec<_>>())
        }
        Err(e) if is_forbidden(&e) => {
            if !state.no_access {
                eprintln!(
                    "warning: {} Until then, only jobs that wait too long are reported.",
                    access_hint(scope)
                );
                state.no_access = true;
            }
            None
        }
        Err(e) => return Err(e),
    };
    // An idle runner says nothing about a job it cannot run, as a Linux box idle beside a Windows
    // backlog.
    let idle_beside_work = runners.as_ref().is_some_and(|rs| {
        rs.iter().any(|r| {
            r.state() == "idle" && state.last_queue.iter().any(|labels| has_labels(r, labels))
        })
    });
    let due = idle_beside_work || state.polls.is_multiple_of(SCAN_EVERY);
    let queued = if due {
        match scan(ctx, scope, cache, state) {
            Ok(queued) => queued,
            // A wrong org shows on the first poll. Later, a failed scan leaves the queue as it was,
            // and the runners seen still count.
            Err(e) if state.polls == 0 => return Err(e),
            Err(e) => {
                eprintln!("warning: skipped a scan: {e:#}");
                None
            }
        }
    } else {
        None
    };
    state.polls += 1;
    Ok(Seen { runners, queued })
}

/// The jobs waiting for a runner, oldest first, or None while the hour's requests run low.
fn scan(ctx: &Ctx, scope: &Scope, cache: &Cache, state: &mut State) -> Result<Option<Vec<Job>>> {
    if !budget_left(ctx, state)? {
        return Ok(None);
    }
    let client = &ctx.client;
    let repos = match scope {
        Scope::Repo(repo) => vec![repo.clone()],
        Scope::Org(org) => org_repos(client, org)?,
    };
    let jobs = in_parallel(&repos, |repo| active_jobs(client, cache, repo));
    cache.sweep();
    let mut queued: Vec<Job> = jobs.into_iter().filter(|j| j.status == "queued").collect();
    queued.sort_by_key(|j| j.created_at);
    state.last_queue = queued.iter().map(|j| j.labels.clone()).collect();
    Ok(Some(queued))
}

/// Whether at least RESERVE requests are left this hour, saying so when that changes. Reading the
/// rate limit does not count against it, and a server without one has no limit to keep.
fn budget_left(ctx: &Ctx, state: &mut State) -> Result<bool> {
    let remaining = ctx
        .client
        .get_opt("rate_limit")?
        .and_then(|limit| limit["resources"]["core"]["remaining"].as_u64());
    let left = remaining.is_none_or(|n| n >= RESERVE);
    if left == state.paused {
        if left {
            wait::progress(ctx, "Scans resume, the hour's requests have reset");
        } else {
            eprintln!(
                "warning: Scans pause while fewer than {RESERVE} of the hour's requests are left"
            );
        }
        state.paused = !left;
    }
    Ok(left)
}

/// Something worth a notification.
#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    /// A runner went offline.
    Offline { runner: String },
    /// A runner reported offline came back.
    Online { runner: String },
    /// A runner sat idle on two scans in a row while jobs it could run waited.
    Stalled {
        runner: String,
        jobs: usize,
        /// When the oldest of those jobs was queued.
        since: DateTime<Utc>,
        url: String,
    },
    /// Jobs asking for `labels` have waited longer than --max-wait.
    Waiting {
        labels: Vec<String>,
        jobs: usize,
        since: DateTime<Utc>,
        url: String,
        /// Whether any runner has the labels, None when the runners are unknown.
        served: Option<bool>,
    },
    /// No runner is stalled and no job waits too long any more.
    Recovered,
}

/// Whether `runner` has every label `job` asks for.
fn fits(job: &Job, runner: &Runner) -> bool {
    has_labels(runner, &job.labels)
}

/// Whether `runner` has all of `labels`, none being no match. GitHub matches labels without case.
fn has_labels(runner: &Runner, labels: &[String]) -> bool {
    !labels.is_empty()
        && labels
            .iter()
            .all(|l| runner.labels.iter().any(|r| r.eq_ignore_ascii_case(l)))
}

/// The labels of `job` as one key, the same for any order or case.
fn label_key(job: &Job) -> String {
    let mut labels: Vec<String> = job.labels.iter().map(|l| l.to_lowercase()).collect();
    labels.sort();
    labels.dedup();
    labels.join(",")
}

/// Compare what a poll saw with what the watch remembers, and return what changed.
fn step(state: &mut State, seen: &Seen, now: DateTime<Utc>, max_wait: TimeDelta) -> Vec<Event> {
    let mut events = Vec::new();
    if let Some(runners) = &seen.runners {
        let offline: BTreeSet<String> = runners
            .iter()
            .filter(|r| r.state() == "offline")
            .map(|r| r.name.clone())
            .collect();
        for name in offline.difference(&state.offline) {
            events.push(Event::Offline {
                runner: name.clone(),
            });
        }
        // A runner removed from the scope is gone, not back.
        for name in state.offline.difference(&offline) {
            if runners.iter().any(|r| &r.name == name) {
                events.push(Event::Online {
                    runner: name.clone(),
                });
            }
        }
        state.offline = offline;
    }

    let Some(queued) = &seen.queued else {
        // A stall needs two scans in a row, so an unscanned poll restarts the count. A stall ends
        // once its runner is busy, gone, or offline.
        state.idle.clear();
        if let Some(runners) = &seen.runners {
            state.stalled.retain(|name| {
                runners
                    .iter()
                    .any(|r| &r.name == name && r.state() == "idle")
            });
        }
        return events;
    };

    if let Some(runners) = &seen.runners {
        let mut idle = BTreeSet::new();
        let mut stalled = BTreeSet::new();
        for runner in runners.iter().filter(|r| r.state() == "idle") {
            let work: Vec<&Job> = queued.iter().filter(|j| fits(j, runner)).collect();
            let Some(oldest) = work.first() else {
                continue;
            };
            idle.insert(runner.name.clone());
            if state.stalled.contains(&runner.name) {
                stalled.insert(runner.name.clone());
            } else if state.idle.contains(&runner.name) {
                // Idle on two scans in a row, a poll apart, is past the second GitHub takes to assign.
                events.push(Event::Stalled {
                    runner: runner.name.clone(),
                    jobs: work.len(),
                    since: oldest.created_at.unwrap_or(now),
                    url: oldest.url.clone(),
                });
                stalled.insert(runner.name.clone());
                state.alerted = true;
            }
        }
        state.idle = idle;
        state.stalled = stalled;
    } else {
        // Both scans in a row must see the runner, so a gap in access restarts the count.
        state.idle.clear();
    }

    let mut late: BTreeMap<String, Vec<&Job>> = BTreeMap::new();
    for job in queued {
        if job.created_at.is_some_and(|t| now - t >= max_wait) {
            late.entry(label_key(job)).or_default().push(job);
        }
    }
    for (key, jobs) in &late {
        if state.waiting.contains(key) {
            continue;
        }
        let oldest = jobs[0];
        events.push(Event::Waiting {
            labels: oldest.labels.clone(),
            jobs: jobs.len(),
            since: oldest.created_at.unwrap_or(now),
            url: oldest.url.clone(),
            served: seen
                .runners
                .as_ref()
                .map(|rs| rs.iter().any(|r| fits(oldest, r))),
        });
        state.alerted = true;
    }
    state.waiting = late.into_keys().collect();

    if state.alerted && state.stalled.is_empty() && state.waiting.is_empty() {
        events.push(Event::Recovered);
        state.alerted = false;
    }
    events
}

/// `n` of `noun`, as in "1 job" or "3 jobs".
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

impl Event {
    /// What happened, in a sentence.
    fn text(&self, now: DateTime<Utc>, max_wait: u64) -> String {
        match self {
            Event::Offline { runner } => format!("{runner} went offline"),
            Event::Online { runner } => format!("{runner} is back online"),
            Event::Stalled {
                runner,
                jobs,
                since,
                ..
            } => format!(
                "{runner} is idle while {} it can run {}, the oldest for {}",
                count(*jobs, "job"),
                if *jobs == 1 { "waits" } else { "wait" },
                elapsed(*since, now)
            ),
            Event::Waiting {
                labels,
                jobs,
                since,
                served,
                ..
            } => {
                let mut text = format!(
                    "{} for {} {} waited over {max_wait} min, the oldest for {}",
                    count(*jobs, "job"),
                    labels.join(", "),
                    if *jobs == 1 { "has" } else { "have" },
                    elapsed(*since, now)
                );
                if *served == Some(false) {
                    text.push_str(". No runner has these labels");
                }
                text
            }
            Event::Recovered => {
                format!("No runner is stalled and no job has waited over {max_wait} min")
            }
        }
    }

    fn title(&self) -> &'static str {
        match self {
            Event::Offline { .. } => "runner offline",
            Event::Online { .. } => "runner back online",
            Event::Stalled { .. } => "runner idle with work waiting",
            Event::Waiting { .. } => "jobs waiting too long",
            Event::Recovered => "runners caught up",
        }
    }

    /// The page a click opens: the oldest job waiting, or the runners' settings.
    fn url(&self, scope: &Scope) -> String {
        match self {
            Event::Stalled { url, .. } | Event::Waiting { url, .. } if !url.is_empty() => {
                url.clone()
            }
            _ => runners_page(scope),
        }
    }
}

/// The settings page that lists the scope's runners.
fn runners_page(scope: &Scope) -> String {
    match scope {
        Scope::Org(org) => {
            format!("https://github.com/organizations/{org}/settings/actions/runners")
        }
        Scope::Repo(repo) => format!("https://github.com/{repo}/settings/actions/runners"),
    }
}

/// One line of JSON output per event.
#[derive(Serialize)]
struct Line<'a> {
    at: DateTime<Utc>,
    scope: String,
    #[serde(flatten)]
    event: &'a Event,
}

/// Print a line per event and show its notification, or one summary when there are many.
fn announce(
    ctx: &Ctx,
    scope: &Scope,
    events: &[Event],
    now: DateTime<Utc>,
    max_wait: u64,
) -> Result<()> {
    for event in events {
        if ctx.json {
            let line = Line {
                at: now,
                scope: scope.to_string(),
                event,
            };
            println!("{}", serde_json::to_string(&line)?);
        } else {
            println!(
                "{} {}",
                now.with_timezone(&Local).format("%Y-%m-%d %H:%M"),
                event.text(now, max_wait)
            );
        }
    }
    for note in notes(scope, events, now, max_wait) {
        notify::send(&note);
    }
    Ok(())
}

fn notes(scope: &Scope, events: &[Event], now: DateTime<Utc>, max_wait: u64) -> Vec<Note> {
    if events.len() <= MAX_NOTES {
        return events
            .iter()
            .map(|e| {
                Note::new(
                    format!("{scope}: {}", e.title()),
                    e.text(now, max_wait),
                    Some(e.url(scope)),
                )
            })
            .collect();
    }
    let lines: Vec<String> = events
        .iter()
        .take(MAX_NOTES)
        .map(|e| e.text(now, max_wait))
        .collect();
    let more = events.len() - lines.len();
    vec![Note::new(
        format!("{scope}: {} runner alerts", events.len()),
        format!("{}\nand {more} more", lines.join("\n")),
        Some(runners_page(scope)),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::actions;
    use crate::api::{Client, Repo};
    use mockito::{Matcher, Server, ServerGuard};
    use serde_json::json;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    const NOW: &str = "2026-11-12T03:00:00Z";

    fn runner(name: &str, status: &str, busy: bool, labels: &[&str]) -> Runner {
        let labels: Vec<_> = labels.iter().map(|l| json!({ "name": l })).collect();
        let api: actions::Runner = serde_json::from_value(json!({
            "id": 1, "name": name, "status": status, "busy": busy, "labels": labels
        }))
        .unwrap();
        api.into()
    }

    /// A job queued `minutes` before NOW asking for `labels`.
    fn queued(id: u64, labels: &[&str], minutes: i64) -> Job {
        Job {
            repo: "o/r".into(),
            workflow: "CI".into(),
            name: format!("job {id}"),
            run_id: 1,
            job_id: id,
            status: "queued".into(),
            runner: None,
            labels: labels.iter().map(|l| l.to_string()).collect(),
            created_at: Some(at(NOW) - TimeDelta::minutes(minutes)),
            started_at: None,
            completed_at: None,
            url: format!("https://github.com/o/r/actions/runs/1/job/{id}"),
        }
    }

    const WIN11: &[&str] = &["self-hosted", "Windows", "build", "11"];

    fn seen(runners: Vec<Runner>, queue: Option<Vec<Job>>) -> Seen {
        Seen {
            runners: Some(runners),
            queued: queue,
        }
    }

    fn step_at(state: &mut State, seen: &Seen) -> Vec<Event> {
        step(state, seen, at(NOW), TimeDelta::minutes(30))
    }

    #[test]
    fn labels_fit_without_case_or_order() {
        let r = runner("w", "online", false, WIN11);
        assert!(fits(&queued(1, &["windows", "11"], 0), &r));
        assert!(fits(&queued(1, &["11", "self-hosted"], 0), &r));
        assert!(!fits(&queued(1, &["windows", "direct7"], 0), &r));
        assert!(!fits(&queued(1, &[], 0), &r));
        assert_eq!(
            label_key(&queued(1, &["Windows", "build", "windows"], 0)),
            "build,windows"
        );
    }

    #[test]
    fn offline_and_back_once_each() {
        let mut state = State::default();
        let up = |s| runner("a", s, false, &[]);
        assert_eq!(step_at(&mut state, &seen(vec![up("online")], None)), []);
        let gone = step_at(&mut state, &seen(vec![up("offline")], None));
        assert_eq!(gone, [Event::Offline { runner: "a".into() }]);
        assert_eq!(step_at(&mut state, &seen(vec![up("offline")], None)), []);
        let back = step_at(&mut state, &seen(vec![up("online")], None));
        assert_eq!(back, [Event::Online { runner: "a".into() }]);

        // A runner already offline at start is reported, and one removed while offline is not back.
        let mut state = State::default();
        let first = step_at(&mut state, &seen(vec![up("offline")], None));
        assert_eq!(first, [Event::Offline { runner: "a".into() }]);
        assert_eq!(step_at(&mut state, &seen(vec![], None)), []);
    }

    #[test]
    fn a_runner_idle_beside_work_on_two_scans_is_stalled_once() {
        let mut state = State::default();
        let idle = || vec![runner("w11", "online", false, WIN11)];
        let work = || Some(vec![queued(1, &["windows", "11"], 5)]);
        // Once could be the moment before GitHub assigns the job.
        assert_eq!(step_at(&mut state, &seen(idle(), work())), []);
        let stalled = step_at(&mut state, &seen(idle(), work()));
        assert_eq!(
            stalled,
            [Event::Stalled {
                runner: "w11".into(),
                jobs: 1,
                since: at(NOW) - TimeDelta::minutes(5),
                url: "https://github.com/o/r/actions/runs/1/job/1".into(),
            }]
        );
        assert_eq!(step_at(&mut state, &seen(idle(), work())), []);
        // Picking the job up ends the stall.
        let busy = vec![runner("w11", "online", true, WIN11)];
        assert_eq!(
            step_at(&mut state, &seen(busy, Some(vec![]))),
            [Event::Recovered]
        );
    }

    #[test]
    fn an_idle_runner_without_matching_work_is_not_stalled() {
        let mut state = State::default();
        let w10 = || vec![runner("w10", "online", false, &["self-hosted", "Windows"])];
        let work = || Some(vec![queued(1, &["windows", "11"], 5)]);
        step_at(&mut state, &seen(w10(), work()));
        assert_eq!(step_at(&mut state, &seen(w10(), work())), []);
    }

    #[test]
    fn a_poll_without_a_scan_restarts_the_count() {
        let mut state = State::default();
        let idle = || vec![runner("w11", "online", false, WIN11)];
        let work = || Some(vec![queued(1, &["11"], 5)]);
        step_at(&mut state, &seen(idle(), work()));
        // Every runner busy, so the queue was not scanned.
        step_at(
            &mut state,
            &seen(vec![runner("w11", "online", true, WIN11)], None),
        );
        assert_eq!(step_at(&mut state, &seen(idle(), work())), []);
    }

    #[test]
    fn a_scan_without_the_runners_restarts_the_count() {
        let mut state = State::default();
        let idle = || vec![runner("w11", "online", false, WIN11)];
        let work = || Some(vec![queued(1, &["11"], 5)]);
        step_at(&mut state, &seen(idle(), work()));
        let blind = Seen {
            runners: None,
            queued: work(),
        };
        step_at(&mut state, &blind);
        assert_eq!(step_at(&mut state, &seen(idle(), work())), []);
    }

    #[test]
    fn a_stall_outlasts_an_unscanned_poll_while_its_runner_stays_idle() {
        let mut state = State::default();
        let idle = || vec![runner("w11", "online", false, WIN11)];
        let work = || Some(vec![queued(1, &["11"], 5)]);
        step_at(&mut state, &seen(idle(), work()));
        assert_eq!(step_at(&mut state, &seen(idle(), work())).len(), 1);
        // Unscanned, as while scans pause, then scanned again with the runner still idle.
        step_at(&mut state, &seen(idle(), None));
        assert_eq!(step_at(&mut state, &seen(idle(), work())), []);
    }

    #[test]
    fn long_waits_are_reported_once_per_label_set() {
        let mut state = State::default();
        let runners = || vec![runner("w11", "online", true, WIN11)];
        let queue = || {
            Some(vec![
                queued(1, &["Windows", "11"], 90),
                queued(2, &["11", "windows"], 45),
                queued(3, &["linux"], 40),
                queued(4, &["windows", "11"], 10),
            ])
        };
        let events = step_at(&mut state, &seen(runners(), queue()));
        let summary: Vec<(String, usize, Option<bool>)> = events
            .iter()
            .map(|e| match e {
                Event::Waiting {
                    labels,
                    jobs,
                    served,
                    ..
                } => (labels.join(","), *jobs, *served),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        // Keyed by the sorted labels, so 11,windows comes before linux.
        assert_eq!(
            summary,
            [
                ("Windows,11".to_string(), 2, Some(true)),
                ("linux".to_string(), 1, Some(false)),
            ]
        );
        assert_eq!(step_at(&mut state, &seen(runners(), queue())), []);
        // The Linux job ran, then the Windows ones did.
        let windows = Some(vec![queued(1, &["Windows", "11"], 91)]);
        assert_eq!(step_at(&mut state, &seen(runners(), windows)), []);
        assert_eq!(
            step_at(&mut state, &seen(runners(), Some(vec![]))),
            [Event::Recovered]
        );
    }

    #[test]
    fn without_the_runners_only_long_waits_are_reported() {
        let mut state = State::default();
        let blind = Seen {
            runners: None,
            queued: Some(vec![queued(1, &["11"], 45)]),
        };
        let events = step_at(&mut state, &blind);
        assert!(matches!(
            events.as_slice(),
            [Event::Waiting { served: None, .. }]
        ));
    }

    #[test]
    fn events_read_as_sentences() {
        let now = at(NOW);
        let since = now - TimeDelta::minutes(67);
        let stalled = Event::Stalled {
            runner: "w11".into(),
            jobs: 3,
            since,
            url: String::new(),
        };
        assert_eq!(
            stalled.text(now, 30),
            "w11 is idle while 3 jobs it can run wait, the oldest for 1h 7m"
        );
        let waiting = Event::Waiting {
            labels: vec!["self-hosted".into(), "gpu".into()],
            jobs: 1,
            since,
            url: String::new(),
            served: Some(false),
        };
        assert_eq!(
            waiting.text(now, 30),
            "1 job for self-hosted, gpu has waited over 30 min, the oldest for 1h 7m. \
             No runner has these labels"
        );
        let org = Scope::Org("o".into());
        // Without a job to open, a click opens the runners' settings.
        assert_eq!(
            waiting.url(&org),
            "https://github.com/organizations/o/settings/actions/runners"
        );
        let repo = Scope::Repo(Repo::parse("o/r").unwrap());
        assert_eq!(
            runners_page(&repo),
            "https://github.com/o/r/settings/actions/runners"
        );
    }

    #[test]
    fn json_lines_carry_the_event_beside_its_time_and_scope() {
        let event = Event::Offline { runner: "a".into() };
        let line = Line {
            at: at(NOW),
            scope: "o".into(),
            event: &event,
        };
        assert_eq!(
            serde_json::to_value(&line).unwrap(),
            json!({"at": NOW, "scope": "o", "event": "offline", "runner": "a"})
        );
        let value = serde_json::to_value(Event::Recovered).unwrap();
        assert_eq!(value, json!({"event": "recovered"}));
    }

    #[test]
    fn many_events_make_one_summary_note() {
        let org = Scope::Org("o".into());
        let offline = |n: &str| Event::Offline { runner: n.into() };
        let few = notes(&org, &[offline("a")], at(NOW), 30);
        assert_eq!(few[0].title, "o: runner offline");
        assert_eq!(few[0].body, "a went offline");
        let many: Vec<Event> = ["a", "b", "c", "d", "e"].map(offline).into();
        let summary = notes(&org, &many, at(NOW), 30);
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].title, "o: 5 runner alerts");
        assert_eq!(
            summary[0].body,
            "a went offline\nb went offline\nc went offline\nand 2 more"
        );
    }

    #[test]
    fn the_service_is_given_the_scope_found_at_install() {
        let args = RunnersWatchArgs {
            org: None,
            interval: 60,
            max_wait: 45,
            install: true,
            uninstall: false,
        };
        let exe = Path::new("/x/gh-arriba");
        let repo = Scope::Repo(Repo::parse("o/r").unwrap());
        assert_eq!(
            watch_argv(exe, &args, &repo, false),
            [
                "/x/gh-arriba",
                "runners",
                "watch",
                "--interval",
                "60",
                "--max-wait",
                "45",
                "-R",
                "o/r"
            ]
        );
        let org = Scope::Org("snxd".into());
        let argv = watch_argv(exe, &args, &org, true);
        assert!(argv.ends_with(&["--org".into(), "snxd".into(), "--json".into()]));
    }

    fn ctx(server: &Server) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: None,
            json: false,
            yes: true,
        }
    }

    /// A repo whose runners answer `runners` (status and body), whose one queued run has a job
    /// asking for `labels`, with `remaining` requests left this hour.
    fn serve(runners: (usize, &str), labels: &str, remaining: u64) -> ServerGuard {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/runners")
            .match_query(Matcher::Any)
            .with_status(runners.0)
            .with_body(runners.1)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/runs")
            .match_query(Matcher::UrlEncoded("status".into(), "queued".into()))
            .with_body(r#"{"workflow_runs": [{"id": 2, "name": "CI"}]}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/runs")
            .match_query(Matcher::UrlEncoded("status".into(), "in_progress".into()))
            .with_body(r#"{"workflow_runs": []}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/runs/2/jobs")
            .match_query(Matcher::Any)
            .with_body(format!(
                r#"{{"jobs": [{{"id": 20, "run_id": 2, "name": "Test", "status": "queued",
                    "labels": {labels}, "created_at": "2026-01-01T00:00:00Z"}}]}}"#
            ))
            .create();
        server
            .mock("GET", "/rate_limit")
            .with_body(format!(
                r#"{{"resources": {{"core": {{"remaining": {remaining}}}}}}}"#
            ))
            .create();
        server
    }

    const IDLE_LINUX: (usize, &str) = (
        200,
        r#"{"runners": [{"id": 1, "name": "l", "status": "online", "busy": false,
            "labels": [{"name": "self-hosted"}, {"name": "Linux"}]}]}"#,
    );

    /// Whether each of `n` polls scanned the queue.
    fn scans(server: &ServerGuard, n: usize) -> (Vec<bool>, State) {
        let ctx = ctx(server);
        let scope = Scope::Repo(Repo::parse("o/r").unwrap());
        let (cache, mut state) = (Cache::default(), State::default());
        let scanned = (0..n)
            .map(|_| {
                poll(&ctx, &scope, &cache, &mut state)
                    .unwrap()
                    .queued
                    .is_some()
            })
            .collect();
        (scanned, state)
    }

    const EVERY_FIFTH: [bool; 6] = [true, false, false, false, false, true];

    #[test]
    fn an_idle_runner_beside_work_it_can_run_gets_a_scan_every_poll() {
        let server = serve(IDLE_LINUX, r#"["self-hosted", "linux"]"#, 5000);
        assert_eq!(scans(&server, 3).0, [true, true, true]);
    }

    #[test]
    fn an_idle_runner_beside_work_it_cannot_run_waits_for_every_fifth_poll() {
        let server = serve(IDLE_LINUX, r#"["self-hosted", "windows"]"#, 5000);
        assert_eq!(scans(&server, 6).0, EVERY_FIFTH);
    }

    #[test]
    fn busy_runners_get_a_scan_every_fifth_poll() {
        let busy = r#"{"runners": [{"id": 1, "name": "l", "status": "online", "busy": true}]}"#;
        let server = serve((200, busy), r#"["linux"]"#, 5000);
        assert_eq!(scans(&server, 6).0, EVERY_FIFTH);
    }

    #[test]
    fn without_access_to_the_runners_the_queue_is_scanned_every_fifth_poll() {
        let denied = (403, r#"{"message": "Must have admin rights"}"#);
        let server = serve(denied, r#"["gpu"]"#, 5000);
        let (scanned, state) = scans(&server, 6);
        assert_eq!(scanned, EVERY_FIFTH);
        assert!(state.no_access);
        assert_eq!(state.last_queue, BTreeSet::from([vec!["gpu".to_string()]]));
    }

    #[test]
    fn scans_pause_while_the_hour_runs_low() {
        let server = serve(IDLE_LINUX, r#"["linux"]"#, RESERVE - 1);
        let (scanned, state) = scans(&server, 2);
        assert_eq!(scanned, [false, false]);
        assert!(state.paused);
    }

    #[test]
    fn a_failed_scan_keeps_the_runners_seen_but_not_on_the_first_poll() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/runners")
            .match_query(Matcher::Any)
            .with_body(
                r#"{"runners": [{"id": 1, "name": "l", "status": "offline", "busy": false}]}"#,
            )
            .create();
        server
            .mock("GET", "/rate_limit")
            .with_status(502)
            .with_body(r#"{"message": "Server Error"}"#)
            .create();
        let (ctx, cache) = (ctx(&server), Cache::default());
        let scope = Scope::Repo(Repo::parse("o/r").unwrap());

        let mut first = State::default();
        assert!(poll(&ctx, &scope, &cache, &mut first).is_err());

        let mut later = State {
            polls: SCAN_EVERY,
            ..State::default()
        };
        let seen = poll(&ctx, &scope, &cache, &mut later).unwrap();
        assert!(seen.queued.is_none());
        assert_eq!(later.polls, SCAN_EVERY + 1);
        assert_eq!(
            step_at(&mut later, &seen),
            [Event::Offline { runner: "l".into() }]
        );
    }

    #[test]
    fn install_refuses_a_scope_that_does_not_exist() {
        let mut server = Server::new();
        server
            .mock("GET", "/orgs/nope")
            .with_status(404)
            .with_body(r#"{"message": "Not Found"}"#)
            .create();
        let runners = server
            .mock("GET", "/orgs/nope/actions/runners")
            .match_query(Matcher::Any)
            .expect(0)
            .create();
        let args = RunnersWatchArgs {
            org: Some("nope".into()),
            interval: 60,
            max_wait: 30,
            install: true,
            uninstall: false,
        };
        let err = run(&ctx(&server), &args).unwrap_err().to_string();
        assert_eq!(err, "GitHub API 404 on GET orgs/nope: Not Found");
        runners.assert();
    }
}

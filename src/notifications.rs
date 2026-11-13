use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::api::Repo;
use crate::api::notifications::{self, Thread};
use crate::cli::{NotifyArgs, NotifyCommand, NotifySendArgs, NotifyWatchArgs};
use crate::context::{self, Ctx};
use crate::focus;
use crate::notify::{self, Note};
use crate::startup::{self, Service};

mod agents;

/// The inbox watch, as it starts at login.
const SERVICE: Service = Service {
    label: "com.github.nmoinvaz.gh-arriba.notify-watch",
    unit: "gh-arriba-notify-watch.service",
    log: "gh-arriba-notify-watch.log",
    description: "gh arriba notify watch",
};

/// `ctx` connects to GitHub, so only the commands that need it build one. `yes` answers prompts
/// for those that do not.
pub fn run(ctx: impl FnOnce() -> Result<Ctx>, yes: bool, args: NotifyArgs) -> Result<()> {
    match args.command {
        // Building the context first fails without a token, before a watch that cannot poll is set up.
        Some(NotifyCommand::Watch(watch_args)) if watch_args.install => {
            let ctx = ctx()?;
            startup::install(&SERVICE, |exe| {
                watch_argv(exe, &watch_args, ctx.repo_flag.as_deref(), ctx.json)
            })?;
            agents::offer_add(yes);
            Ok(())
        }
        Some(NotifyCommand::Watch(watch_args)) if watch_args.uninstall => {
            if !startup::uninstall(&SERVICE)? {
                println!("No watch was set to start at login");
            }
            agents::offer_remove(yes);
            Ok(())
        }
        Some(NotifyCommand::Watch(watch_args)) => watch(&ctx()?, &watch_args),
        Some(NotifyCommand::Focus { target }) => focus::focus(&target),
        None => {
            let (message, cwd) = if args.send.hook {
                let input: HookInput = serde_json::from_reader(std::io::stdin())
                    .context("--hook reads the hook's JSON from stdin")?;
                (hook_message(&input, &args.send.title), input.cwd)
            } else {
                (args.send.message.clone().unwrap_or_default(), None)
            };
            let note = Note {
                command: focus::origin_command(),
                ..message_note(&args.send, message, &project_dir(cwd.as_deref()))
            };
            notify::send(&note);
            Ok(())
        }
    }
}

/// The command line the service runs: this executable and the watch options given now.
fn watch_argv(exe: &Path, args: &NotifyWatchArgs, repo: Option<&str>, json: bool) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        exe.to_string_lossy().into_owned(),
        "notify".into(),
        "watch".into(),
        "--interval".into(),
        args.interval.to_string(),
    ];
    for reason in &args.reason {
        argv.extend(["--reason".into(), reason.clone()]);
    }
    if args.participating {
        argv.push("--participating".into());
    }
    if let Some(repo) = repo {
        argv.extend(["-R".into(), repo.into()]);
    }
    if json {
        argv.push("--json".into());
    }
    argv
}

/// The JSON a Claude Code or Codex hook gets on stdin, as far as a notification needs it.
#[derive(Debug, Default, Deserialize)]
struct HookInput {
    cwd: Option<PathBuf>,
    hook_event_name: Option<String>,
    /// Claude Code's Notification says what it needs.
    message: Option<String>,
    /// A permission request names the tool it asks to use.
    tool_name: Option<String>,
    /// Codex's Stop carries the reply that ended the turn.
    last_assistant_message: Option<String>,
}

/// The agent's own message, or one made from the request or the turn it ended.
fn hook_message(input: &HookInput, title: &str) -> String {
    let given = |text: &Option<String>| {
        text.as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(String::from)
    };
    if let Some(message) = given(&input.message) {
        return message;
    }
    if input.hook_event_name.as_deref() == Some("PermissionRequest")
        && let Some(tool) = given(&input.tool_name)
    {
        return format!("{title} needs your permission to use {tool}");
    }
    given(&input.last_assistant_message).unwrap_or_else(|| format!("{title} needs you"))
}

/// The git root of `dir`, or of the working directory without one, or the directory itself
/// outside a repository.
fn project_dir(dir: Option<&Path>) -> PathBuf {
    let dir = dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    context::git_in(&dir, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or(dir)
}

/// The note for `message`, named and grouped by the project it came from.
fn message_note(args: &NotifySendArgs, message: String, project: &Path) -> Note {
    let name = project
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    let group = format!("gh-arriba:{}", project.display());
    Note {
        subtitle: args.subtitle.clone().or(name),
        group: Some(args.group.clone().unwrap_or(group)),
        sound: args.sound.clone(),
        ..Note::new(&args.title, message, None)
    }
}

/// More new threads than this in one poll make a single summary notification.
const MAX_NOTES: usize = 3;

/// What the watcher knows of the inbox between polls.
#[derive(Default)]
struct Inbox {
    /// Last-Modified of the previous poll, so an unchanged inbox costs a 304.
    since: Option<String>,
    /// Last update seen per thread, so a thread with new activity counts as new again.
    seen: HashMap<String, DateTime<Utc>>,
    polled: bool,
    /// Seconds GitHub asked to wait between polls.
    poll_interval: Option<u64>,
}

/// One line of output per new notification.
#[derive(Serialize)]
struct Line<'a> {
    reason: &'a str,
    repo: &'a str,
    kind: &'a str,
    title: &'a str,
    url: String,
    updated_at: DateTime<Utc>,
}

fn watch(ctx: &Ctx, args: &NotifyWatchArgs) -> Result<()> {
    let repo = ctx.repo_flag.as_deref().map(Repo::parse).transpose()?;
    let mut inbox = Inbox::default();
    loop {
        match poll_once(ctx, repo.as_ref(), args, &mut inbox) {
            Ok(threads) => announce(ctx, &threads)?,
            // A bad token or repo shows on the first poll; later failures are likely transient.
            Err(e) if !inbox.polled => return Err(e),
            Err(e) => eprintln!("warning: {e:#}"),
        }
        let secs = args.interval.max(inbox.poll_interval.unwrap_or(0));
        std::thread::sleep(Duration::from_secs(secs));
    }
}

/// Poll once and return the threads that are new or have new activity, oldest first. The first
/// poll only records what is already unread, unless --all.
fn poll_once(
    ctx: &Ctx,
    repo: Option<&Repo>,
    args: &NotifyWatchArgs,
    inbox: &mut Inbox,
) -> Result<Vec<Thread>> {
    let poll = notifications::poll(
        &ctx.client,
        repo,
        args.participating,
        inbox.since.as_deref(),
    )?;
    inbox.poll_interval = poll.poll_interval.or(inbox.poll_interval);
    if poll.last_modified.is_some() {
        inbox.since = poll.last_modified;
    }
    let baseline = !inbox.polled && !args.all;
    inbox.polled = true;
    let Some(threads) = poll.threads else {
        return Ok(Vec::new());
    };

    let mut seen = HashMap::new();
    let mut fresh = Vec::new();
    for thread in threads {
        let new = inbox.seen.get(&thread.id) != Some(&thread.updated_at);
        seen.insert(thread.id.clone(), thread.updated_at);
        if new && !baseline && wanted(args, &thread) {
            fresh.push(thread);
        }
    }
    inbox.seen = seen;
    fresh.reverse();
    Ok(fresh)
}

fn wanted(args: &NotifyWatchArgs, thread: &Thread) -> bool {
    args.reason.is_empty() || args.reason.iter().any(|r| r == &thread.reason)
}

/// review_requested reads as "review requested".
fn reason_label(reason: &str) -> String {
    reason.replace('_', " ")
}

/// Print a line per thread and show its notification, or one summary when there are many.
fn announce(ctx: &Ctx, threads: &[Thread]) -> Result<()> {
    for thread in threads {
        let url = thread.html_url();
        if ctx.json {
            let line = Line {
                reason: &thread.reason,
                repo: &thread.repository.full_name,
                kind: &thread.subject.kind,
                title: &thread.subject.title,
                url,
                updated_at: thread.updated_at,
            };
            println!("{}", serde_json::to_string(&line)?);
        } else {
            println!(
                "{:<17} {} {}  {url}",
                reason_label(&thread.reason),
                thread.repository.full_name,
                thread.subject.title
            );
        }
    }
    for note in notes(threads) {
        notify::send(&note);
    }
    Ok(())
}

fn notes(threads: &[Thread]) -> Vec<Note> {
    if threads.len() <= MAX_NOTES {
        return threads.iter().map(note).collect();
    }
    let titles: Vec<&str> = threads
        .iter()
        .rev()
        .take(MAX_NOTES)
        .map(|t| t.subject.title.as_str())
        .collect();
    let more = threads.len() - titles.len();
    let body = format!("{}\nand {more} more", titles.join("\n"));
    vec![Note::new(
        format!("{} new GitHub notifications", threads.len()),
        body,
        Some(inbox_url(&threads[0])),
    )]
}

fn note(thread: &Thread) -> Note {
    Note::new(
        format!(
            "{}: {}",
            thread.repository.full_name,
            reason_label(&thread.reason)
        ),
        thread.subject.title.clone(),
        Some(thread.html_url()),
    )
}

/// The notifications page on the thread's GitHub host.
fn inbox_url(thread: &Thread) -> String {
    let base = &thread.repository.html_url;
    let host_end = base
        .find("://")
        .and_then(|i| base[i + 3..].find('/').map(|j| i + 3 + j))
        .unwrap_or(base.len());
    format!("{}/notifications", &base[..host_end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use mockito::{Matcher, Mock, Server, ServerGuard};
    use serde_json::{Value, json};

    fn ctx(server: &Server) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: None,
            json: false,
            yes: false,
        }
    }

    fn args() -> NotifyWatchArgs {
        NotifyWatchArgs {
            interval: 60,
            reason: Vec::new(),
            participating: false,
            all: false,
            install: false,
            uninstall: false,
        }
    }

    #[test]
    fn the_service_runs_the_watch_with_the_options_given_at_install() {
        let exe = Path::new("/x/gh-arriba");
        let given = NotifyWatchArgs {
            interval: 90,
            reason: vec!["mention".into(), "review_requested".into()],
            participating: true,
            install: true,
            ..args()
        };
        assert_eq!(
            watch_argv(exe, &given, Some("o/r"), true),
            [
                "/x/gh-arriba",
                "notify",
                "watch",
                "--interval",
                "90",
                "--reason",
                "mention",
                "--reason",
                "review_requested",
                "--participating",
                "-R",
                "o/r",
                "--json",
            ]
        );
        assert_eq!(
            watch_argv(exe, &args(), None, false),
            ["/x/gh-arriba", "notify", "watch", "--interval", "60"]
        );
    }

    fn send_args(message: &str) -> NotifySendArgs {
        NotifySendArgs {
            message: Some(message.into()),
            hook: false,
            title: "gh arriba".into(),
            subtitle: None,
            group: None,
            sound: None,
        }
    }

    fn hook(json: Value) -> HookInput {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn hooks_say_what_the_agent_needs() {
        let claude = hook(json!({
            "session_id": "s", "cwd": "/src/zlib-ng", "hook_event_name": "Notification",
            "message": "Claude needs your permission to use Bash", "notification_type": "permission_prompt"
        }));
        assert_eq!(
            hook_message(&claude, "Claude Code"),
            "Claude needs your permission to use Bash"
        );
        assert_eq!(claude.cwd.as_deref(), Some(Path::new("/src/zlib-ng")));

        let approval = hook(json!({
            "hook_event_name": "PermissionRequest", "tool_name": "Bash",
            "tool_input": {"command": "rm -rf build"}, "turn_id": "t"
        }));
        assert_eq!(
            hook_message(&approval, "Codex"),
            "Codex needs your permission to use Bash"
        );

        let stop =
            |last: Value| hook(json!({"hook_event_name": "Stop", "last_assistant_message": last}));
        assert_eq!(
            hook_message(&stop(json!(" Tests pass.\n")), "Codex"),
            "Tests pass."
        );
        assert_eq!(hook_message(&stop(json!(null)), "Codex"), "Codex needs you");
        assert_eq!(hook_message(&stop(json!("  ")), "Codex"), "Codex needs you");
        assert_eq!(
            hook_message(&HookInput::default(), "Claude Code"),
            "Claude Code needs you"
        );
    }

    #[test]
    fn projects_outside_a_repository_are_their_directory() {
        let dir = std::env::temp_dir().join(format!("gh-arriba-project-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(project_dir(Some(&dir)), dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn message_notes_take_their_name_and_group_from_the_project() {
        let project = Path::new("/src/zlib-ng");
        assert_eq!(
            message_note(&send_args("tests done"), "tests done".into(), project),
            Note {
                subtitle: Some("zlib-ng".into()),
                group: Some("gh-arriba:/src/zlib-ng".into()),
                ..Note::new("gh arriba", "tests done", None)
            }
        );

        let custom = NotifySendArgs {
            title: "Claude Code".into(),
            subtitle: Some("CI".into()),
            group: Some("ci".into()),
            sound: Some("Submarine".into()),
            ..send_args("done")
        };
        assert_eq!(
            message_note(&custom, "done".into(), project),
            Note {
                subtitle: Some("CI".into()),
                group: Some("ci".into()),
                sound: Some("Submarine".into()),
                ..Note::new("Claude Code", "done", None)
            }
        );
    }

    fn thread(id: &str, reason: &str, updated: &str) -> Value {
        json!({
            "id": id, "reason": reason, "updated_at": updated,
            "subject": {
                "title": format!("Thread {id}"),
                "url": format!("https://api.github.com/repos/o/r/pulls/{id}"),
                "type": "PullRequest",
            },
            "repository": {"full_name": "o/r", "html_url": "https://github.com/o/r"},
        })
    }

    fn inbox(server: &mut ServerGuard, since: Option<&str>, threads: Option<Value>) -> Mock {
        let mock = server
            .mock("GET", "/notifications")
            .match_query(Matcher::Any)
            .match_header(
                "if-modified-since",
                since.map_or(Matcher::Missing, |s| Matcher::Exact(s.into())),
            )
            .with_header("X-Poll-Interval", "90");
        match threads {
            Some(t) => mock
                .with_header("Last-Modified", "later")
                .with_body(t.to_string()),
            None => mock.with_status(304),
        }
        .create()
    }

    fn ids(threads: &[Thread]) -> Vec<&str> {
        threads.iter().map(|t| t.id.as_str()).collect()
    }

    #[test]
    fn notifies_only_for_activity_after_the_first_poll() {
        let mut server = Server::new();
        let first = inbox(
            &mut server,
            None,
            Some(json!([thread("1", "mention", "2026-01-01T00:00:00Z")])),
        );
        let ctx = ctx(&server);
        let mut state = Inbox::default();
        assert!(
            poll_once(&ctx, None, &args(), &mut state)
                .unwrap()
                .is_empty()
        );
        assert_eq!(state.poll_interval, Some(90));
        first.assert();
        first.remove();

        let unchanged = inbox(&mut server, Some("later"), None);
        assert!(
            poll_once(&ctx, None, &args(), &mut state)
                .unwrap()
                .is_empty()
        );
        unchanged.assert();
        unchanged.remove();

        // Thread 1 has a new comment and 2 and 3 are new, newest first as GitHub sorts them.
        inbox(
            &mut server,
            Some("later"),
            Some(json!([
                thread("3", "review_requested", "2026-01-03T00:00:00Z"),
                thread("2", "ci_activity", "2026-01-02T12:00:00Z"),
                thread("1", "mention", "2026-01-02T00:00:00Z"),
            ])),
        );
        let fresh = poll_once(&ctx, None, &args(), &mut state).unwrap();
        assert_eq!(ids(&fresh), ["1", "2", "3"]);
        // Seen already, so the same page again is quiet.
        assert!(
            poll_once(&ctx, None, &args(), &mut state)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn all_notifies_for_unread_threads_at_start_filtered_by_reason() {
        let mut server = Server::new();
        inbox(
            &mut server,
            None,
            Some(json!([
                thread("3", "review_requested", "2026-01-03T00:00:00Z"),
                thread("2", "ci_activity", "2026-01-02T12:00:00Z"),
            ])),
        );
        let a = NotifyWatchArgs {
            reason: vec!["review_requested".into()],
            all: true,
            ..args()
        };
        let fresh = poll_once(&ctx(&server), None, &a, &mut Inbox::default()).unwrap();
        assert_eq!(ids(&fresh), ["3"]);
    }

    #[test]
    fn notes_one_per_thread_or_a_summary() {
        let threads: Vec<Thread> = (1..=4)
            .map(|i| {
                serde_json::from_value(thread(
                    &i.to_string(),
                    "review_requested",
                    "2026-01-01T00:00:00Z",
                ))
                .unwrap()
            })
            .collect();
        assert_eq!(
            notes(&threads[..1]),
            [Note::new(
                "o/r: review requested",
                "Thread 1",
                Some("https://github.com/o/r/pull/1".into())
            )]
        );
        assert_eq!(
            notes(&threads),
            [Note::new(
                "4 new GitHub notifications",
                "Thread 4\nThread 3\nThread 2\nand 1 more",
                Some("https://github.com/notifications".into())
            )]
        );
    }

    #[test]
    fn announce_sends_the_notes() {
        let server = Server::new();
        let thread: Thread =
            serde_json::from_value(thread("5", "mention", "2026-01-01T00:00:00Z")).unwrap();
        announce(&ctx(&server), &[thread]).unwrap();
        let sent = notify::sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].title, "o/r: mention");
    }
}

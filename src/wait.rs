//! What the watch commands share: polling until something arrives, the state a named watch keeps,
//! and the exit statuses that tell a caller how the wait ended.

use anyhow::{Context, Result, bail};
use chrono::TimeDelta;
use regex::Regex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::api::Repo;
use crate::context::Ctx;
use crate::output;
use crate::wake::Wake;

/// Exit status when nothing arrived before the timeout.
const EXIT_TIMED_OUT: i32 = 3;
/// Exit status when GitHub stayed out of reach.
const EXIT_UNREACHABLE: i32 = 4;
/// The longest pause between polls while GitHub is out of reach, unless --interval is longer.
const BACKOFF_CAP: Duration = Duration::from_secs(300);
/// How far back a named watch looks the first time it runs.
pub const LOOKBACK: TimeDelta = TimeDelta::hours(24);

/// Where a named watch keeps what it remembers between runs.
pub struct Store(pub PathBuf);

impl Store {
    /// The file for `key` on `slot` of `repo`, in the folder `kind` of this user's gh-arriba data
    /// directory.
    pub fn open(kind: &str, repo: &Repo, slot: &str, key: &str) -> Result<Self> {
        let dir = crate::notify::data_dir(std::env::consts::OS, |name| {
            std::env::var_os(name).map(PathBuf::from)
        })
        .context("No home directory to keep --state in")?;
        let name: String = format!("{}__{}__{slot}__{key}", repo.owner, repo.name)
            .chars()
            .map(|c| match c {
                c if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') => c,
                _ => '_',
            })
            .collect();
        Ok(Self(dir.join(kind).join(format!("{name}.json"))))
    }

    /// The stored state, or an empty one when the file is missing or unreadable.
    pub fn load<T: DeserializeOwned + Default>(&self) -> T {
        fs::read_to_string(&self.0)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save<T: Serialize>(&self, state: &T) -> Result<()> {
        let dir = self.0.parent().context("State file has no directory")?;
        fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
        // Written beside the file and renamed, so a crash never leaves half a state.
        let tmp = self.0.with_extension("tmp");
        fs::write(&tmp, serde_json::to_string(state)?)
            .with_context(|| format!("Failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &self.0).with_context(|| format!("Failed to write {}", self.0.display()))
    }
}

/// Which texts count.
#[derive(Default)]
pub struct Filter {
    pub matches: Option<Regex>,
    pub not_match: Option<Regex>,
    /// A `re:` line that names one of the paths given to --re, when any were.
    pub replies: Option<Regex>,
}

impl Filter {
    /// Compile the patterns given to --match and --not-match.
    pub fn new(matches: &Option<String>, not_match: &Option<String>) -> Result<Self> {
        let compile = |pattern: &Option<String>, flag: &str| {
            pattern
                .as_deref()
                .map(|p| Regex::new(p).with_context(|| format!("Invalid {flag} regex {p:?}")))
                .transpose()
        };
        Ok(Self {
            matches: compile(matches, "--match")?,
            not_match: compile(not_match, "--not-match")?,
            replies: None,
        })
    }

    /// Also require a `re:` line naming one of `paths`, the replies to those messages. The paths
    /// are matched as written, and a line may end in spaces or a carriage return.
    pub fn replying_to(mut self, paths: &[String]) -> Result<Self> {
        if !paths.is_empty() {
            // An empty path, as from an unset shell variable, would match only a blank re: line.
            if paths.iter().any(|p| p.trim().is_empty()) {
                bail!("--re needs a path");
            }
            let any = paths
                .iter()
                .map(|p| regex::escape(p.trim()))
                .collect::<Vec<_>>()
                .join("|");
            self.replies = Some(Regex::new(&format!(r"(?m)^re:[ \t]*(?:{any})[ \t]*\r?$"))?);
        }
        Ok(self)
    }

    pub fn wants(&self, body: &str) -> bool {
        self.matches.as_ref().is_none_or(|re| re.is_match(body))
            && self.replies.as_ref().is_none_or(|re| re.is_match(body))
            && !self.not_match.as_ref().is_some_and(|re| re.is_match(body))
    }
}

/// How long to poll and how often.
pub struct Wait {
    pub interval: Duration,
    pub timeout: Duration,
    /// How long GitHub may stay out of reach before the watch gives up.
    pub offline: Duration,
    /// Ends the pause after a quiet poll early, when given.
    pub wake: Option<Wake>,
}

/// How a watch ended.
#[derive(Debug, PartialEq)]
pub enum Outcome<T> {
    Found(Vec<T>),
    TimedOut,
    /// GitHub failed this many polls in a row, the last one with this error.
    Unreachable(u32, String),
}

/// The time a watch reads and sleeps on, so tests can pass hours without waiting.
pub trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&mut self, pause: Duration);
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, pause: Duration) {
        std::thread::sleep(pause);
    }
}

/// The pause after the `fails`-th failed poll in a row: --interval doubled each time, up to
/// BACKOFF_CAP, or --interval itself when that is longer.
fn backoff(interval: Duration, fails: u32) -> Duration {
    let cap = BACKOFF_CAP.max(interval);
    interval
        .checked_mul(1 << fails.min(16))
        .map_or(cap, |pause| pause.min(cap))
}

/// Poll `look` until it finds something, the timeout passes, or GitHub stays out of reach. An
/// error on the first poll is returned as it is, since a wrong repository or issue never
/// recovers. A later one starts an outage, polled with a growing pause, that ends the watch once
/// it has lasted `wait.offline`, since a network that drops for a moment comes back.
pub fn poll_until<T>(wait: &Wait, look: impl FnMut() -> Result<Vec<T>>) -> Result<Outcome<T>> {
    poll_with(wait, &mut SystemClock, look)
}

fn poll_with<T>(
    wait: &Wait,
    clock: &mut impl Clock,
    mut look: impl FnMut() -> Result<Vec<T>>,
) -> Result<Outcome<T>> {
    let deadline = clock.now() + wait.timeout;
    let mut fails = 0;
    // When the current outage began, and the last error it gave.
    let mut outage: Option<(Instant, String)> = None;
    let mut first = true;
    loop {
        let (pause, quiet) = match look() {
            Ok(found) if !found.is_empty() => return Ok(Outcome::Found(found)),
            Ok(_) => {
                fails = 0;
                outage = None;
                (wait.interval, true)
            }
            Err(e) if first => return Err(e),
            Err(e) => {
                fails += 1;
                let error = format!("{e:#}");
                let since = outage.as_ref().map_or_else(|| clock.now(), |(t, _)| *t);
                if outage.is_none() {
                    eprintln!(
                        "GitHub failed a poll, retrying for up to {}: {error}",
                        output::duration(wait.offline.as_secs() as i64)
                    );
                }
                if clock.now().duration_since(since) >= wait.offline {
                    return Ok(Outcome::Unreachable(fails, error));
                }
                outage = Some((since, error));
                (backoff(wait.interval, fails), false)
            }
        };
        first = false;
        if clock.now() + pause > deadline {
            // A watch still out of reach at its deadline cannot say that nothing arrived.
            return Ok(match outage {
                Some((_, error)) => Outcome::Unreachable(fails, error),
                None => Outcome::TimedOut,
            });
        }
        match &wait.wake {
            // An outage keeps its backoff, since an answer cannot bring GitHub back.
            Some(wake) if quiet => wake.pause(&mut *clock, pause),
            _ => clock.sleep(pause),
        }
    }
}

/// Sleep `pause`, ended early by `wake` when given, for loops that poll without `poll_until`.
pub fn rest(wake: Option<&Wake>, pause: Duration) {
    match wake {
        Some(wake) => wake.pause(&mut SystemClock, pause),
        None => std::thread::sleep(pause),
    }
}

/// Progress goes to stderr under --json so stdout carries only the final document.
pub fn progress(ctx: &Ctx, line: &str) {
    if ctx.json {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

/// Report how a watch ended, exiting with the status that tells a caller why.
pub fn finish<T: Serialize>(
    ctx: &Ctx,
    outcome: Outcome<T>,
    wait: &Wait,
    text: impl Fn(&T) -> String,
) -> Result<Vec<T>> {
    match outcome {
        Outcome::Found(found) => {
            if ctx.json {
                output::print_json(&found)?;
            } else {
                found.iter().for_each(|item| println!("{}", text(item)));
            }
            Ok(found)
        }
        Outcome::TimedOut => {
            progress(
                ctx,
                &format!(
                    "Nothing new in {}",
                    output::duration(wait.timeout.as_secs() as i64)
                ),
            );
            std::process::exit(EXIT_TIMED_OUT);
        }
        Outcome::Unreachable(fails, error) => {
            eprintln!("GitHub failed {fails} polls in a row, the last with: {error}");
            std::process::exit(EXIT_UNREACHABLE);
        }
    }
}

/// `s` quoted for a POSIX shell.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The filter flags as typed, each quoted for a shell, so a printed restart keeps them.
pub fn filter_flags(matches: &Option<String>, not_match: &Option<String>) -> String {
    let mut flags = String::new();
    if let Some(p) = matches {
        flags.push_str(&format!(" --match {}", quote(p)));
    }
    if let Some(p) = not_match {
        flags.push_str(&format!(" --not-match {}", quote(p)));
    }
    flags
}

/// The --wake flag as typed, quoted the same way, or nothing when it is empty.
pub fn wake_flag(value: &Option<String>) -> String {
    value
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .map(|v| format!(" --wake {}", quote(v)))
        .unwrap_or_default()
}

/// The --re flags as typed, quoted the same way.
pub fn reply_flags(paths: &[String]) -> String {
    paths
        .iter()
        .map(|p| format!(" --re {}", quote(p)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wake::Woke;
    use crate::wake::tests::Scripted;
    use anyhow::bail;

    fn no_wait() -> Wait {
        Wait {
            interval: Duration::ZERO,
            timeout: Duration::from_secs(60),
            offline: Duration::ZERO,
            wake: None,
        }
    }

    /// A clock that only moves when the watch sleeps, recording each pause.
    struct FakeClock {
        start: Instant,
        passed: Duration,
        pauses: Vec<Duration>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                start: Instant::now(),
                passed: Duration::ZERO,
                pauses: Vec::new(),
            }
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.start + self.passed
        }

        fn sleep(&mut self, pause: Duration) {
            self.passed += pause;
            self.pauses.push(pause);
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    /// Polls every 15 seconds for 110 minutes, out of reach for at most 15.
    fn watch_like() -> Wait {
        Wait {
            interval: secs(15),
            timeout: secs(110 * 60),
            offline: secs(15 * 60),
            wake: None,
        }
    }

    #[test]
    fn poll_until_finds_or_times_out() {
        let mut calls = 0;
        let found = poll_until(&no_wait(), || {
            calls += 1;
            Ok(if calls < 3 { vec![] } else { vec![calls] })
        });
        assert_eq!(found.unwrap(), Outcome::Found(vec![3]));

        let once = Wait {
            interval: secs(60),
            timeout: Duration::ZERO,
            offline: secs(60),
            wake: None,
        };
        let quiet = poll_until(&once, || Ok(Vec::<u8>::new()));
        assert_eq!(quiet.unwrap(), Outcome::TimedOut);
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let pauses: Vec<u64> = (1..=6).map(|n| backoff(secs(15), n).as_secs()).collect();
        assert_eq!(pauses, [30, 60, 120, 240, 300, 300]);
        // An interval longer than the cap is never shortened, and a huge count does not overflow.
        assert_eq!(backoff(secs(600), 3), secs(600));
        assert_eq!(backoff(secs(15), u32::MAX), BACKOFF_CAP);
    }

    thread_local! {
        /// The fake time, published by SharedClock so a `look` closure can read it.
        static NOW: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    }

    /// A FakeClock that also publishes its time to NOW.
    struct SharedClock(FakeClock);

    impl SharedClock {
        fn new() -> Self {
            let clock = Self(FakeClock::new());
            NOW.with(|now| now.set(Some(clock.0.now())));
            clock
        }
    }

    impl Clock for SharedClock {
        fn now(&self) -> Instant {
            self.0.now()
        }

        fn sleep(&mut self, pause: Duration) {
            self.0.sleep(pause);
            NOW.with(|now| now.set(Some(self.0.now())));
        }
    }

    /// The fake time since `start`, as SharedClock published it.
    fn since(start: Instant) -> Duration {
        NOW.with(|now| now.get().expect("a SharedClock").duration_since(start))
    }

    #[test]
    fn an_outage_shorter_than_offline_is_ridden_out() {
        // Down from the second poll until 14 minutes in, then up with something to report.
        let mut clock = SharedClock::new();
        let start = clock.now();
        let mut calls = 0;
        let outcome = poll_with(&watch_like(), &mut clock, || {
            calls += 1;
            match calls {
                1 => Ok(vec![]),
                _ if since(start) < secs(14 * 60) => bail!("dns error"),
                _ => Ok(vec!["message"]),
            }
        });
        assert_eq!(outcome.unwrap(), Outcome::Found(vec!["message"]));
        // The first pause is the interval, then each pause doubles up to the cap.
        assert_eq!(clock.0.pauses, [15, 30, 60, 120, 240, 300, 300].map(secs));
    }

    #[test]
    fn a_long_outage_ends_the_watch_after_offline() {
        let mut clock = FakeClock::new();
        let mut calls = 0;
        let outcome = poll_with(&watch_like(), &mut clock, || {
            calls += 1;
            if calls == 1 {
                Ok(Vec::<u8>::new())
            } else {
                bail!("dns error")
            }
        });
        // Polls fail at 15 s, 45 s, 105 s, 225 s, 465 s, 765 s and 1065 s, and the last is the
        // first one 15 minutes after the outage began.
        assert_eq!(
            outcome.unwrap(),
            Outcome::Unreachable(7, "dns error".into())
        );
        let outage: Duration = clock.pauses[1..].iter().sum();
        assert_eq!(outage, secs(1050));
    }

    #[test]
    fn a_good_poll_ends_the_outage() {
        // Two outages of 12.5 minutes each, with good polls between, so neither reaches 15.
        // Counted as one outage, they would end the watch at 990 s.
        let mut clock = SharedClock::new();
        let start = clock.now();
        let mut calls = 0;
        let outcome = poll_with(&watch_like(), &mut clock, || {
            calls += 1;
            let t = since(start);
            if calls == 1 || (t >= secs(600) && t < secs(960)) {
                Ok(vec![])
            } else if t < secs(1560) {
                bail!("dns error")
            } else {
                Ok(vec![calls])
            }
        });
        assert!(matches!(outcome.unwrap(), Outcome::Found(_)));
        assert_eq!(since(start), secs(1710));
    }

    #[test]
    fn out_of_reach_at_the_deadline_is_unreachable() {
        let wait = Wait {
            interval: secs(15),
            timeout: secs(5 * 60),
            offline: secs(15 * 60),
            wake: None,
        };
        let mut clock = FakeClock::new();
        let mut calls = 0;
        let outcome = poll_with(&wait, &mut clock, || {
            calls += 1;
            if calls == 1 {
                Ok(Vec::<u8>::new())
            } else {
                bail!("down")
            }
        });
        assert!(matches!(outcome.unwrap(), Outcome::Unreachable(_, e) if e == "down"));
    }

    #[test]
    fn offline_zero_gives_up_on_the_first_failed_poll_after_the_first() {
        let mut clock = FakeClock::new();
        let mut calls = 0;
        let outcome = poll_with(&no_wait(), &mut clock, || {
            calls += 1;
            if calls == 1 {
                Ok(Vec::<u8>::new())
            } else {
                bail!("down")
            }
        });
        assert_eq!(outcome.unwrap(), Outcome::Unreachable(1, "down".into()));
        assert_eq!(calls, 2);
    }

    #[test]
    fn a_wake_ends_the_pause_after_a_quiet_poll() {
        let (wake, calls) = Scripted::wake(vec![(secs(2), Woke::Early)]);
        let wait = Wait {
            wake: Some(wake),
            ..watch_like()
        };
        let mut clock = FakeClock::new();
        let mut polls = 0;
        let outcome = poll_with(&wait, &mut clock, || {
            polls += 1;
            Ok(if polls < 3 { vec![] } else { vec![polls] })
        });
        assert_eq!(outcome.unwrap(), Outcome::Found(vec![3]));
        // The answer after 2 s ends the first pause, and the second runs to the interval.
        assert_eq!(clock.pauses, [secs(2), secs(15)]);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn an_outage_backs_off_without_the_wake() {
        let (wake, calls) = Scripted::wake(vec![]);
        let wait = Wait {
            wake: Some(wake),
            ..watch_like()
        };
        let mut clock = SharedClock::new();
        let start = clock.now();
        let mut polls = 0;
        let outcome = poll_with(&wait, &mut clock, || {
            polls += 1;
            match polls {
                1 => Ok(vec![]),
                _ if since(start) < secs(100) => bail!("dns error"),
                _ => Ok(vec![polls]),
            }
        });
        assert_eq!(outcome.unwrap(), Outcome::Found(vec![4]));
        // Only the pause after the quiet poll waits on the wake.
        assert_eq!(clock.0.pauses, [15, 30, 60].map(secs));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn an_error_on_the_first_poll_is_final() {
        let mut calls = 0;
        let err = poll_until(&no_wait(), || -> Result<Vec<u8>> {
            calls += 1;
            bail!("GitHub API 404")
        });
        assert_eq!(err.unwrap_err().to_string(), "GitHub API 404");
        assert_eq!(calls, 1);
    }

    #[test]
    fn filter_rejects_a_bad_regex() {
        let err = Filter::new(&Some("(".into()), &None).err().unwrap();
        assert_eq!(err.to_string(), "Invalid --match regex \"(\"");
    }

    #[test]
    fn filter_wants_a_match_and_no_exclusion() {
        let filter = Filter::new(&Some("^to: mac".into()), &Some("skip".into())).unwrap();
        assert!(filter.wants("to: mac\nhello"));
        assert!(!filter.wants("to: win\nhello"));
        assert!(!filter.wants("to: mac\nskip this"));
        assert!(Filter::default().wants("anything"));
    }

    #[test]
    fn restart_hint_keeps_the_filters() {
        assert_eq!(
            filter_flags(&Some(r"^\S+ to (mac|all)".into()), &Some("it's".into())),
            r" --match '^\S+ to (mac|all)' --not-match 'it'\''s'"
        );
        assert_eq!(filter_flags(&None, &None), "");
        assert_eq!(
            reply_flags(&["chan/1.md".into(), "chan/2.md".into()]),
            " --re 'chan/1.md' --re 'chan/2.md'"
        );
        assert_eq!(reply_flags(&[]), "");
        assert_eq!(wake_flag(&Some("WAKE_URL".into())), " --wake 'WAKE_URL'");
        assert_eq!(wake_flag(&Some(" ".into())), "");
        assert_eq!(wake_flag(&None), "");
    }

    #[test]
    fn replying_to_wants_a_re_line_naming_one_of_the_paths() {
        let paths = ["chan/2026-11-10/163104Z-coordinator-cfbe.md".to_string()];
        let filter = Filter::default().replying_to(&paths).unwrap();
        let reply =
            "from: win\nto: coordinator\nre: chan/2026-11-10/163104Z-coordinator-cfbe.md\n\nok";
        assert!(filter.wants(reply));
        // Trailing spaces and a Windows line end still count.
        assert!(
            filter.wants("from: win\r\nre: chan/2026-11-10/163104Z-coordinator-cfbe.md \r\n\r\nok")
        );
        // Another message, a path that only starts the same, a dot that is not a dot, and the
        // path anywhere but a re: line do not.
        assert!(!filter.wants("re: chan/2026-11-10/000000Z-coordinator-0000.md\n"));
        assert!(!filter.wants("re: chan/2026-11-10/163104Z-coordinator-cfbe.md.old\n"));
        assert!(!filter.wants("re: chan/2026-11-10/163104Z-coordinator-cfbeXmd\n"));
        assert!(!filter.wants("see chan/2026-11-10/163104Z-coordinator-cfbe.md\n"));

        // Several paths are alternatives, and --match must still hold.
        let both = Filter::new(&Some("(?m)^from: win2$".into()), &None)
            .unwrap()
            .replying_to(&["a.md".into(), "b.md".into()])
            .unwrap();
        assert!(both.wants("from: win2\nre: b.md\n"));
        assert!(!both.wants("from: win\nre: b.md\n"));
        // No paths leave the filter as it was, and an empty or blank path is refused.
        assert!(
            Filter::default()
                .replying_to(&[])
                .unwrap()
                .wants("anything")
        );
        for empty in ["", "  "] {
            let err = Filter::default()
                .replying_to(&[empty.into()])
                .err()
                .unwrap();
            assert_eq!(err.to_string(), "--re needs a path");
        }
    }
}

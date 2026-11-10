//! What the watch commands share: polling until something arrives, the state a named watch keeps,
//! and the exit statuses that tell a caller how the wait ended.

use anyhow::{Context, Result};
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

/// Exit status when nothing arrived before the timeout.
const EXIT_TIMED_OUT: i32 = 3;
/// Exit status when GitHub stayed out of reach.
const EXIT_UNREACHABLE: i32 = 4;
/// Consecutive failed polls before the watch gives up.
const BLIND_AFTER: u32 = 5;
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
        })
    }

    pub fn wants(&self, body: &str) -> bool {
        self.matches.as_ref().is_none_or(|re| re.is_match(body))
            && !self.not_match.as_ref().is_some_and(|re| re.is_match(body))
    }
}

/// How long to poll and how often.
pub struct Wait {
    pub interval: Duration,
    pub timeout: Duration,
}

/// How a watch ended.
#[derive(Debug, PartialEq)]
pub enum Outcome<T> {
    Found(Vec<T>),
    TimedOut,
    /// GitHub failed this many polls in a row, the last one with this error.
    Unreachable(u32, String),
}

/// Poll `look` until it finds something, the timeout passes, or GitHub stays out of reach. An
/// error on the first poll is returned as it is, since a wrong repository or issue never
/// recovers. A later one is counted, since a network that drops for a moment does.
pub fn poll_until<T>(wait: &Wait, mut look: impl FnMut() -> Result<Vec<T>>) -> Result<Outcome<T>> {
    let deadline = Instant::now() + wait.timeout;
    let mut fails = 0;
    let mut first = true;
    loop {
        match look() {
            Ok(found) if !found.is_empty() => return Ok(Outcome::Found(found)),
            Ok(_) => fails = 0,
            Err(e) if first => return Err(e),
            Err(e) => {
                fails += 1;
                if fails >= BLIND_AFTER {
                    return Ok(Outcome::Unreachable(fails, format!("{e:#}")));
                }
            }
        }
        first = false;
        if Instant::now() + wait.interval > deadline {
            return Ok(Outcome::TimedOut);
        }
        std::thread::sleep(wait.interval);
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

/// The filter flags as typed, each quoted for a shell, so a printed restart keeps them.
pub fn filter_flags(matches: &Option<String>, not_match: &Option<String>) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    let mut flags = String::new();
    if let Some(p) = matches {
        flags.push_str(&format!(" --match {}", quote(p)));
    }
    if let Some(p) = not_match {
        flags.push_str(&format!(" --not-match {}", quote(p)));
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    fn no_wait() -> Wait {
        Wait {
            interval: Duration::ZERO,
            timeout: Duration::from_secs(60),
        }
    }

    #[test]
    fn poll_until_finds_times_out_or_goes_blind() {
        let mut calls = 0;
        let found = poll_until(&no_wait(), || {
            calls += 1;
            Ok(if calls < 3 { vec![] } else { vec![calls] })
        });
        assert_eq!(found.unwrap(), Outcome::Found(vec![3]));

        let once = Wait {
            interval: Duration::from_secs(60),
            timeout: Duration::ZERO,
        };
        let quiet = poll_until(&once, || Ok(Vec::<u8>::new()));
        assert_eq!(quiet.unwrap(), Outcome::TimedOut);

        // Five failures in a row end the watch, and a good poll between them resets the count.
        let mut calls = 0;
        let blind = poll_until(&no_wait(), || {
            calls += 1;
            match calls {
                1 | 4 => Ok(Vec::<u8>::new()),
                _ => bail!("down"),
            }
        });
        assert_eq!(blind.unwrap(), Outcome::Unreachable(5, "down".into()));
        assert_eq!(calls, 9);
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
    }
}

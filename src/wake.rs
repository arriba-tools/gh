//! Ending the pause between polls early. Between polls a watch given --wake holds a GET to a URL
//! open, and polls as soon as it answers. A long poll on a stream that a webhook fills thus wakes
//! the watch at once, while --interval still bounds the time between polls.

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client as Http;
use std::cell::Cell;
use std::time::Duration;

use crate::api::{Client, Repo};
use crate::output;
use crate::wait::Clock;

/// An answer sooner than this means the URL did not hold the request open.
const QUICK: Duration = Duration::from_secs(1);
/// Quick answers in a row after which the URL is taken not to hold requests open.
const QUICK_LIMIT: u32 = 3;

/// How a wait on the wake URL ended.
#[derive(Debug, PartialEq)]
pub enum Woke {
    /// The URL answered, so something may have happened.
    Early,
    /// The wait ran its full length without an answer.
    Late,
    /// The request failed with this error.
    Failed(String),
}

/// What a watch waits on between polls.
pub trait Waker {
    /// Wait up to `max` for an answer, with the time passing on `clock`.
    fn wait(&self, clock: &mut dyn Clock, max: Duration) -> Woke;
}

/// A URL that holds a GET open until something happens.
struct HttpWaker {
    http: Http,
    url: String,
    /// Scheme, host and port, named in errors in place of a URL that may carry a token.
    origin: String,
}

impl HttpWaker {
    fn new(url: &str) -> Result<Self> {
        let parsed = reqwest::Url::parse(url).context("--wake is not a valid URL")?;
        let origin = parsed.origin().ascii_serialization();
        let http = Http::builder().user_agent(concat!("gh-arriba/", env!("CARGO_PKG_VERSION")));
        // The test server is local, so skip any proxy the environment configures.
        #[cfg(test)]
        let http = http.no_proxy();
        Ok(Self {
            http: http.build().context("Failed to create HTTP client")?,
            url: url.to_string(),
            origin,
        })
    }

    fn failed(&self, e: reqwest::Error) -> Woke {
        if e.is_timeout() {
            return Woke::Late;
        }
        // The URL may carry a token, so the error names only its origin.
        let e = anyhow::Error::from(e.without_url());
        Woke::Failed(format!("{}: {e:#}", self.origin))
    }
}

impl Waker for HttpWaker {
    fn wait(&self, _clock: &mut dyn Clock, max: Duration) -> Woke {
        // The timeout covers the body too, so a server that sends its headers first still holds.
        let resp = match self.http.get(&self.url).timeout(max).send() {
            Ok(resp) => resp,
            Err(e) => return self.failed(e),
        };
        let status = resp.status();
        match resp.bytes() {
            Err(e) => self.failed(e),
            Ok(_) if status.is_success() => Woke::Early,
            Ok(_) => Woke::Failed(format!("{} answered {status}", self.origin)),
        }
    }
}

/// The wake a watch was given, with what it learned about the URL while waiting.
pub struct Wake {
    waker: Box<dyn Waker>,
    /// Quick answers in a row.
    quick: Cell<u32>,
    /// Set by the first failure, so its warning shows once.
    warned: Cell<bool>,
    /// Set after QUICK_LIMIT quick answers, from when the watch only polls.
    off: Cell<bool>,
}

impl Wake {
    pub fn new(waker: Box<dyn Waker>) -> Self {
        Self {
            waker,
            quick: Cell::new(0),
            warned: Cell::new(false),
            off: Cell::new(false),
        }
    }

    /// Pause for up to `max`, ending early once the URL answers. A failed request sleeps out the
    /// rest of `max`, so the watch polls no more often than it would without a wake.
    pub fn pause(&self, clock: &mut dyn Clock, max: Duration) {
        if self.off.get() {
            clock.sleep(max);
            return;
        }
        let start = clock.now();
        let woke = self.waker.wait(clock, max);
        let spent = clock.now().duration_since(start);
        match woke {
            Woke::Early if spent < QUICK => {
                let quick = self.quick.get() + 1;
                self.quick.set(quick);
                if quick >= QUICK_LIMIT {
                    self.off.set(true);
                    eprintln!(
                        "warning: --wake answered at once {quick} times in a row, so it does not \
                         hold the request open. Polling every {} instead",
                        output::duration(max.as_secs() as i64)
                    );
                }
            }
            Woke::Early | Woke::Late => self.quick.set(0),
            Woke::Failed(error) => {
                self.quick.set(0);
                if !self.warned.replace(true) {
                    eprintln!(
                        "warning: --wake failed, so polling every {} until it answers: {error}",
                        output::duration(max.as_secs() as i64)
                    );
                }
                clock.sleep(max.saturating_sub(spent));
            }
        }
    }
}

/// The wake `value` names for a watch of `repo`, None when there is none to use.
pub fn resolve(client: &Client, repo: &Repo, value: Option<&str>) -> Result<Option<Wake>> {
    url(client, repo, value)?
        .map(|url| Ok(Wake::new(Box::new(HttpWaker::new(&url)?))))
        .transpose()
}

/// The URL `value` names. A value starting with http:// or https:// is the URL, and any other is
/// the name of an Actions variable of `repo` that holds it. None when the value is empty, or the
/// variable is missing, unreadable or holds no URL, so a watch whose wake is not set up polls as
/// before.
fn url(client: &Client, repo: &Repo, value: Option<&str>) -> Result<Option<String>> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if is_url(value) {
        return Ok(Some(value.to_string()));
    }
    if !is_variable(value) {
        bail!("--wake takes an http(s) URL or the name of an Actions variable, not {value:?}");
    }
    let held = match client.get_opt(&format!("{}/actions/variables/{value}", repo.api())) {
        Ok(Some(var)) => var["value"].as_str().unwrap_or_default().trim().to_string(),
        Ok(None) => return Ok(off(&format!("{repo} has no Actions variable {value}"))),
        Err(e) => {
            return Ok(off(&format!(
                "reading Actions variable {value} failed: {e:#}"
            )));
        }
    };
    if !is_url(&held) {
        return Ok(off(&format!(
            "Actions variable {value} of {repo} holds no http(s) URL"
        )));
    }
    Ok(Some(held))
}

fn off(why: &str) -> Option<String> {
    eprintln!("warning: {why}, so --wake is off");
    None
}

fn is_url(value: &str) -> bool {
    (value.starts_with("https://") || value.starts_with("http://"))
        && reqwest::Url::parse(value).is_ok()
}

/// Whether `value` can name an Actions variable, which takes letters, digits and underscores and
/// does not start with a digit.
fn is_variable(value: &str) -> bool {
    !value.starts_with(|c: char| c.is_ascii_digit())
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::net::TcpListener;
    use std::rc::Rc;
    use std::time::Instant;

    /// A waker that answers from a script of how long each wait takes and how it ends, then
    /// waits out every later call. Counts its calls.
    pub(crate) struct Scripted {
        script: RefCell<VecDeque<(Duration, Woke)>>,
        calls: Rc<Cell<u32>>,
    }

    impl Scripted {
        /// A wake on `script`, and the count of its calls.
        pub(crate) fn wake(script: Vec<(Duration, Woke)>) -> (Wake, Rc<Cell<u32>>) {
            let calls = Rc::new(Cell::new(0));
            let waker = Self {
                script: RefCell::new(script.into()),
                calls: calls.clone(),
            };
            (Wake::new(Box::new(waker)), calls)
        }
    }

    impl Waker for Scripted {
        fn wait(&self, clock: &mut dyn Clock, max: Duration) -> Woke {
            self.calls.set(self.calls.get() + 1);
            let (took, woke) = self
                .script
                .borrow_mut()
                .pop_front()
                .unwrap_or((max, Woke::Late));
            clock.sleep(took.min(max));
            woke
        }
    }

    /// A clock that only moves when slept on.
    struct Ticks {
        start: Instant,
        passed: Duration,
    }

    impl Ticks {
        fn new() -> Self {
            Self {
                start: Instant::now(),
                passed: Duration::ZERO,
            }
        }
    }

    impl Clock for Ticks {
        fn now(&self) -> Instant {
            self.start + self.passed
        }

        fn sleep(&mut self, pause: Duration) {
            self.passed += pause;
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn ms(m: u64) -> Duration {
        Duration::from_millis(m)
    }

    #[test]
    fn an_answer_ends_the_pause() {
        let (wake, _) = Scripted::wake(vec![(secs(3), Woke::Early)]);
        let mut clock = Ticks::new();
        wake.pause(&mut clock, secs(15));
        assert_eq!(clock.passed, secs(3));
        // No answer waits the full pause.
        wake.pause(&mut clock, secs(15));
        assert_eq!(clock.passed, secs(18));
    }

    #[test]
    fn a_failure_sleeps_out_the_pause() {
        let (wake, calls) = Scripted::wake(vec![
            (secs(2), Woke::Failed("down".into())),
            (ms(10), Woke::Failed("down".into())),
        ]);
        let mut clock = Ticks::new();
        wake.pause(&mut clock, secs(15));
        wake.pause(&mut clock, secs(15));
        assert_eq!(clock.passed, secs(30));
        assert!(wake.warned.get());
        // A failure is tried again on the next pause, since the URL may come back.
        assert_eq!(calls.get(), 2);
        assert!(!wake.off.get());
    }

    #[test]
    fn quick_answers_turn_the_wake_off() {
        let (wake, calls) = Scripted::wake((0..3).map(|_| (ms(100), Woke::Early)).collect());
        let mut clock = Ticks::new();
        for _ in 0..3 {
            wake.pause(&mut clock, secs(15));
        }
        assert!(wake.off.get());
        assert_eq!(clock.passed, ms(300));
        // From then on the watch only polls.
        wake.pause(&mut clock, secs(15));
        assert_eq!(calls.get(), 3);
        assert_eq!(clock.passed, ms(15_300));
    }

    #[test]
    fn a_held_answer_resets_the_quick_count() {
        let quick = || (ms(100), Woke::Early);
        let (wake, _) = Scripted::wake(vec![
            quick(),
            quick(),
            (secs(5), Woke::Early),
            quick(),
            quick(),
            (secs(15), Woke::Late),
            quick(),
            quick(),
        ]);
        let mut clock = Ticks::new();
        for _ in 0..8 {
            wake.pause(&mut clock, secs(15));
        }
        assert!(!wake.off.get());
    }

    #[test]
    fn an_answer_wakes_without_sending_the_github_token() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/wait")
            .match_query(Matcher::UrlEncoded("_token".into(), "secret".into()))
            .match_header("authorization", Matcher::Missing)
            .with_body(r#"{"result":[["s",[["1-0",["k","v"]]]]]}"#)
            .create();
        let waker = HttpWaker::new(&format!("{}/wait?_token=secret", server.url())).unwrap();
        assert_eq!(waker.wait(&mut Ticks::new(), secs(5)), Woke::Early);
        mock.assert();
    }

    #[test]
    fn errors_name_the_origin_without_the_token() {
        let mut server = Server::new();
        server
            .mock("GET", "/wait")
            .match_query(Matcher::Any)
            .with_status(401)
            .create();
        let waker = HttpWaker::new(&format!("{}/wait?_token=secret", server.url())).unwrap();
        let Woke::Failed(error) = waker.wait(&mut Ticks::new(), secs(5)) else {
            panic!("a 401 is a failure");
        };
        assert_eq!(error, format!("{} answered 401 Unauthorized", server.url()));

        // A port nothing listens on refuses the connection.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let waker = HttpWaker::new(&format!("http://127.0.0.1:{port}/wait?_token=secret")).unwrap();
        let Woke::Failed(error) = waker.wait(&mut Ticks::new(), secs(5)) else {
            panic!("a refused connection is a failure");
        };
        assert!(error.starts_with(&format!("http://127.0.0.1:{port}: ")));
        assert!(!error.contains("secret"), "{error}");
    }

    #[test]
    fn a_request_held_past_the_pause_is_late() {
        // The kernel accepts the connection into the backlog, and nothing ever answers it.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let waker = HttpWaker::new(&format!("http://127.0.0.1:{port}/wait")).unwrap();
        assert_eq!(waker.wait(&mut Ticks::new(), ms(300)), Woke::Late);
    }

    #[test]
    fn url_takes_a_url_or_reads_an_actions_variable() {
        let mut server = Server::new();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        let read = |value: &str| url(&client, &repo, Some(value));

        assert_eq!(url(&client, &repo, None).unwrap(), None);
        // An empty value, as from an unset shell variable, leaves the wake off.
        assert_eq!(read("  ").unwrap(), None);
        assert_eq!(
            read("https://db.example/x?_token=t").unwrap().as_deref(),
            Some("https://db.example/x?_token=t")
        );

        let held = server
            .mock("GET", "/repos/o/r/actions/variables/WAKE_URL")
            .with_body(r#"{"name":"WAKE_URL","value":" https://db.example/y\n"}"#)
            .create();
        assert_eq!(
            read("WAKE_URL").unwrap().as_deref(),
            Some("https://db.example/y")
        );
        held.assert();

        // A missing variable, or one holding no URL, leaves the watch polling as before.
        server
            .mock("GET", "/repos/o/r/actions/variables/MISSING")
            .with_status(404)
            .with_body(r#"{"message":"Not Found"}"#)
            .create();
        assert_eq!(read("MISSING").unwrap(), None);
        server
            .mock("GET", "/repos/o/r/actions/variables/WORDS")
            .with_body(r#"{"name":"WORDS","value":"hello"}"#)
            .create();
        assert_eq!(read("WORDS").unwrap(), None);

        // A value that is neither is a mistake on the command line.
        for bad in ["not a name", "1ST", "ftp://db.example"] {
            assert_eq!(
                read(bad).unwrap_err().to_string(),
                format!(
                    "--wake takes an http(s) URL or the name of an Actions variable, not {bad:?}"
                )
            );
        }
    }
}

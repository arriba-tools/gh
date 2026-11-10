use anyhow::{Context, Result, bail};
use reqwest::Method;
use reqwest::blocking::{Client as Http, RequestBuilder, Response};
use reqwest::header::{CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, LINK};
use serde_json::{Value, json};
use std::process::Command;
use std::sync::OnceLock;

/// A conditional GET's response.
#[derive(Debug)]
pub struct Polled {
    /// None when nothing changed since the last poll.
    pub body: Option<Value>,
    /// Pass back as `since` on the next poll.
    pub last_modified: Option<String>,
    /// Seconds the server asks clients to wait between polls.
    pub poll_interval: Option<u64>,
}

/// REST and GraphQL client authenticated with the token gh uses.
pub struct Client {
    http: Http,
    token: String,
    api: String,
    graphql: String,
    /// Host that takes files for use in comments, None on GitHub Enterprise Server, which has none.
    uploads: Option<String>,
    viewer: OnceLock<String>,
}

impl Client {
    pub fn new() -> Result<Self> {
        let host = std::env::var("GH_HOST")
            .ok()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "github.com".to_string());
        let token = get_github_token(&host)?;
        // GitHub Enterprise Server serves the API under the host itself.
        let (api, graphql, uploads) = if host == "github.com" {
            (
                "https://api.github.com".to_string(),
                "https://api.github.com/graphql".to_string(),
                Some("https://uploads.github.com".to_string()),
            )
        } else {
            (
                format!("https://{host}/api/v3"),
                format!("https://{host}/api/graphql"),
                None,
            )
        };
        let http = Http::builder()
            .user_agent(concat!("gh-arriba/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("Failed to create HTTP client")?;
        Ok(Self {
            http,
            token,
            api,
            graphql,
            uploads,
            viewer: OnceLock::new(),
        })
    }

    /// A client for the API served at `base`, such as a local mock server in tests.
    #[cfg(test)]
    pub fn for_tests(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            // The test server is local, so skip any proxy the environment configures.
            http: Http::builder().no_proxy().build().unwrap(),
            token: "test-token".to_string(),
            api: base.to_string(),
            graphql: format!("{base}/graphql"),
            uploads: Some(base.to_string()),
            viewer: OnceLock::new(),
        }
    }

    /// Login of the authenticated user.
    pub fn viewer(&self) -> Result<&str> {
        if let Some(login) = self.viewer.get() {
            return Ok(login);
        }
        let data = self.graphql("query { viewer { login } }", json!({}))?;
        let login = data["viewer"]["login"]
            .as_str()
            .context("viewer login missing from response")?
            .to_string();
        Ok(self.viewer.get_or_init(|| login))
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("https://") || path.starts_with("http://") {
            path.to_string()
        } else {
            format!("{}/{}", self.api, path.trim_start_matches('/'))
        }
    }

    fn request(&self, method: Method, url: &str) -> RequestBuilder {
        self.request_as(method, url, "application/vnd.github+json")
    }

    /// A request that accepts `media_type`. reqwest appends repeated headers, so each request
    /// sets Accept exactly once.
    fn request_as(&self, method: Method, url: &str, media_type: &str) -> RequestBuilder {
        self.http
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", media_type)
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    fn send(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Response> {
        let url = self.url(path);
        let mut req = self.request(method.clone(), &url);
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req
            .send()
            .with_context(|| format!("{method} {path} failed"))?;
        check(resp, &method, path)
    }

    /// GET a path relative to the API root, or an absolute API URL.
    pub fn get(&self, path: &str) -> Result<Value> {
        parse_json(self.send(Method::GET, path, None)?)
    }

    /// GET that maps 404 to None.
    pub fn get_opt(&self, path: &str) -> Result<Option<Value>> {
        let url = self.url(path);
        let resp = self
            .request(Method::GET, &url)
            .send()
            .with_context(|| format!("GET {path} failed"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        parse_json(check(resp, &Method::GET, path)?).map(Some)
    }

    /// A conditional GET for polling: the body is None when the server answers 304 Not Modified
    /// to `since`, a Last-Modified value from an earlier poll.
    pub fn poll(&self, path: &str, since: Option<&str>) -> Result<Polled> {
        let url = self.url(path);
        let mut req = self.request(Method::GET, &url);
        if let Some(since) = since {
            req = req.header(IF_MODIFIED_SINCE, since);
        }
        let resp = req.send().with_context(|| format!("GET {path} failed"))?;
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let last_modified = header(LAST_MODIFIED.as_str());
        let poll_interval = header("x-poll-interval").and_then(|v| v.parse().ok());
        let body = if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            None
        } else {
            Some(parse_json(check(resp, &Method::GET, path)?)?)
        };
        Ok(Polled {
            body,
            last_modified,
            poll_interval,
        })
    }

    /// A conditional GET keyed on an entity tag: the body is None when the server answers 304 Not
    /// Modified to `etag`, the tag an earlier call returned. GitHub does not count a 304 against
    /// the rate limit, so an unchanged resource is free to poll.
    pub fn get_if_changed(
        &self,
        path: &str,
        etag: Option<&str>,
    ) -> Result<(Option<Value>, Option<String>)> {
        let url = self.url(path);
        let mut req = self.request(Method::GET, &url);
        if let Some(etag) = etag {
            req = req.header(IF_NONE_MATCH, etag);
        }
        let resp = req.send().with_context(|| format!("GET {path} failed"))?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok((None, etag.map(str::to_string)));
        }
        let resp = check(resp, &Method::GET, path)?;
        let tag = resp
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        Ok((Some(parse_json(resp)?), tag))
    }

    /// GET every page of a list endpoint that returns a JSON array.
    pub fn get_all(&self, path: &str) -> Result<Vec<Value>> {
        self.paginate(path, None)
    }

    /// GET every page of a list endpoint that wraps its array in `key`, as in `{"jobs": [...]}`.
    pub fn get_all_in(&self, path: &str, key: &str) -> Result<Vec<Value>> {
        self.paginate(path, Some(key))
    }

    fn paginate(&self, path: &str, key: Option<&str>) -> Result<Vec<Value>> {
        let mut next = Some(with_per_page(&self.url(path)));
        let mut items = Vec::new();
        while let Some(url) = next {
            let resp = self.send(Method::GET, &url, None)?;
            next = next_link(&resp);
            let page = parse_json(resp)?;
            let array = match key {
                Some(k) => page[k].clone(),
                None => page,
            };
            match array {
                Value::Array(a) => items.extend(a),
                other => bail!("Expected a JSON array from {path}, got {other}"),
            }
        }
        Ok(items)
    }

    /// GET a text body, such as a job log. Redirects are followed.
    pub fn get_text(&self, path: &str) -> Result<String> {
        let resp = self.send(Method::GET, path, None)?;
        resp.text()
            .with_context(|| format!("Failed to read body of {path}"))
    }

    /// GET a binary body, such as an artifact zip, streaming it into `dest`. Redirects are
    /// followed. Returns the bytes written.
    pub fn download(&self, path: &str, dest: &std::path::Path) -> Result<u64> {
        let mut resp = self.send(Method::GET, path, None)?;
        let mut file = std::fs::File::create(dest)
            .with_context(|| format!("Failed to create {}", dest.display()))?;
        std::io::copy(&mut resp, &mut file)
            .with_context(|| format!("Failed to download {path} to {}", dest.display()))
    }

    /// GET file contents from the contents API as raw text.
    pub fn get_raw(&self, path: &str) -> Result<String> {
        let url = self.url(path);
        let resp = self
            .request_as(Method::GET, &url, "application/vnd.github.raw+json")
            .send()
            .with_context(|| format!("GET {path} failed"))?;
        check(resp, &Method::GET, path)?
            .text()
            .with_context(|| format!("Failed to read body of {path}"))
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        parse_json(self.send(Method::POST, path, Some(body))?)
    }

    pub fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        parse_json(self.send(Method::PATCH, path, Some(body))?)
    }

    pub fn put(&self, path: &str, body: &Value) -> Result<Value> {
        parse_json(self.send(Method::PUT, path, Some(body))?)
    }

    pub fn delete(&self, path: &str) -> Result<Value> {
        parse_json(self.send(Method::DELETE, path, None)?)
    }

    /// POST raw bytes to a path on the uploads host, with `query` as its parameters.
    pub fn upload(&self, path: &str, query: &[(&str, &str)], body: Vec<u8>) -> Result<Value> {
        let Some(base) = &self.uploads else {
            bail!("Uploads are not supported on GitHub Enterprise Server");
        };
        let url = format!("{base}/{}", path.trim_start_matches('/'));
        let resp = self
            .request(Method::POST, &url)
            .query(query)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(body)
            .send()
            .with_context(|| format!("POST {path} failed"))?;
        parse_json(check(resp, &Method::POST, path)?)
    }

    /// Run a GraphQL query and return its `data`. Any entry in `errors` fails the call.
    pub fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let body = json!({ "query": query, "variables": variables });
        let resp = self
            .request(Method::POST, &self.graphql)
            .json(&body)
            .send()
            .context("GraphQL request failed")?;
        let mut value = parse_json(check(resp, &Method::POST, "graphql")?)?;
        if let Some(errors) = value["errors"].as_array().filter(|e| !e.is_empty()) {
            let messages: Vec<&str> = errors
                .iter()
                .filter_map(|e| e["message"].as_str())
                .collect();
            bail!("GraphQL error: {}", messages.join("; "));
        }
        Ok(value["data"].take())
    }
}

/// Fail with GitHub's own message on a non-2xx response.
fn check(resp: Response, method: &Method, path: &str) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let text = resp.text().unwrap_or_default();
    let parsed: Option<Value> = serde_json::from_str(&text).ok();
    let mut message = parsed
        .as_ref()
        .and_then(|v| v["message"].as_str())
        .map(str::to_string)
        .unwrap_or(text);
    // 422 responses carry the specific validation failures in `errors`.
    if let Some(errors) = parsed.as_ref().and_then(|v| v["errors"].as_array()) {
        let details: Vec<String> = errors
            .iter()
            .map(|e| match e {
                Value::String(s) => s.clone(),
                other => other["message"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| other.to_string()),
            })
            .collect();
        if !details.is_empty() {
            message = format!("{message} ({})", details.join("; "));
        }
    }
    bail!(
        "GitHub API {} on {method} {path}: {message}",
        status.as_u16()
    )
}

/// Parse a JSON body, mapping an empty body (such as 204) to Null.
fn parse_json(resp: Response) -> Result<Value> {
    let text = resp.text().context("Failed to read response body")?;
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).context("Failed to parse JSON response")
}

fn with_per_page(url: &str) -> String {
    if url.contains("per_page=") {
        url.to_string()
    } else if url.contains('?') {
        format!("{url}&per_page=100")
    } else {
        format!("{url}?per_page=100")
    }
}

/// Extract the rel="next" URL from a Link header.
fn next_link(resp: &Response) -> Option<String> {
    let header = resp.headers().get(LINK)?.to_str().ok()?;
    parse_next_link(header)
}

fn parse_next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
    })
}

/// Percent-encode a repository path or query value, keeping its slashes readable.
pub fn encode_path(s: &str) -> String {
    encode(s, true)
}

/// Percent-encode text for use as a single URL path segment, slashes included.
pub fn encode_segment(s: &str) -> String {
    encode(s, false)
}

fn encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'.' | b'_' | b'~')
            || (keep_slash && b == b'/')
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Whether a check run, job, step, or workflow run conclusion counts as failed. REST spells
/// conclusions in lowercase and GraphQL in uppercase, so either is accepted.
pub fn is_failed(conclusion: Option<&str>) -> bool {
    conclusion.is_some_and(|c| {
        ["failure", "cancelled", "timed_out", "startup_failure"]
            .iter()
            .any(|f| c.eq_ignore_ascii_case(f))
    })
}

pub fn get_github_token(host: &str) -> Result<String> {
    // Try gh auth token first.
    if let Ok(output) = Command::new("gh")
        .args(["auth", "token", "--hostname", host])
        .output()
        && output.status.success()
    {
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }

    // Fall back to environment variables.
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(token) = std::env::var(var)
            && !token.is_empty()
        {
            return Ok(token);
        }
    }

    bail!("No GitHub token found. Run `gh auth login` or set GH_TOKEN.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    #[test]
    fn encoding_keeps_or_escapes_slashes() {
        assert_eq!(encode_path("a b/c#d.rs"), "a%20b/c%23d.rs");
        assert_eq!(encode_path("nathan/dev/fix#2"), "nathan/dev/fix%232");
        assert_eq!(encode_segment("ui/ux"), "ui%2Fux");
        assert_eq!(encode_segment("build system"), "build%20system");
        assert_eq!(encode_segment("type: bug"), "type%3A%20bug");
        assert_eq!(encode_segment("naïve"), "na%C3%AFve");
    }

    #[test]
    fn failed_conclusions_in_either_case() {
        assert!(is_failed(Some("failure")));
        assert!(is_failed(Some("timed_out")));
        assert!(is_failed(Some("STARTUP_FAILURE")));
        assert!(is_failed(Some("CANCELLED")));
        assert!(!is_failed(Some("success")));
        assert!(!is_failed(Some("ACTION_REQUIRED")));
        assert!(!is_failed(None));
    }

    #[test]
    fn next_link_found_among_rels() {
        let header = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=5>; rel="last""#;
        assert_eq!(
            parse_next_link(header).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
    }

    #[test]
    fn next_link_absent_on_last_page() {
        let header = r#"<https://api.github.com/x?page=1>; rel="first", <https://api.github.com/x?page=4>; rel="prev""#;
        assert_eq!(parse_next_link(header), None);
    }

    #[test]
    fn per_page_appended_once() {
        assert_eq!(with_per_page("https://a/x"), "https://a/x?per_page=100");
        assert_eq!(
            with_per_page("https://a/x?state=open"),
            "https://a/x?state=open&per_page=100"
        );
        assert_eq!(
            with_per_page("https://a/x?per_page=5"),
            "https://a/x?per_page=5"
        );
    }

    #[test]
    fn get_sends_the_token_and_parses_json() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/repos/o/r")
            .match_header("authorization", "Bearer test-token")
            .match_header("x-github-api-version", "2022-11-28")
            .with_body(r#"{"default_branch": "main"}"#)
            .create();
        let client = Client::for_tests(&server.url());
        assert_eq!(client.get("repos/o/r").unwrap()["default_branch"], "main");
        mock.assert();
    }

    #[test]
    fn get_all_follows_next_links() {
        let mut server = Server::new();
        let next = format!("<{}/items?page=2>; rel=\"next\"", server.url());
        server
            .mock("GET", "/items")
            .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
            .with_header("link", &next)
            .with_body("[1, 2]")
            .create();
        server
            .mock("GET", "/items")
            .match_query(Matcher::UrlEncoded("page".into(), "2".into()))
            .with_body("[3]")
            .create();
        let client = Client::for_tests(&server.url());
        let items = client.get_all("items").unwrap();
        assert_eq!(items, vec![json!(1), json!(2), json!(3)]);
    }

    #[test]
    fn get_all_in_unwraps_the_named_array() {
        let mut server = Server::new();
        server
            .mock("GET", "/runs")
            .match_query(Matcher::Any)
            .with_body(r#"{"total_count": 1, "jobs": [{"id": 7}]}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let jobs = client.get_all_in("runs", "jobs").unwrap();
        assert_eq!(jobs[0]["id"], 7);
        assert!(client.get_all_in("runs", "missing").is_err());
    }

    #[test]
    fn get_if_changed_sends_the_tag_and_reads_304() {
        let mut server = Server::new();
        server
            .mock("GET", "/items")
            .match_header("if-none-match", Matcher::Missing)
            .with_header("ETag", "\"abc\"")
            .with_body("[1]")
            .create();
        server
            .mock("GET", "/items")
            .match_header("if-none-match", "\"abc\"")
            .with_status(304)
            .create();
        let client = Client::for_tests(&server.url());
        let (body, tag) = client.get_if_changed("items", None).unwrap();
        assert_eq!(body, Some(json!([1])));
        assert_eq!(tag.as_deref(), Some("\"abc\""));
        // An unchanged resource keeps the tag it was asked with.
        let (body, tag) = client.get_if_changed("items", tag.as_deref()).unwrap();
        assert_eq!(body, None);
        assert_eq!(tag.as_deref(), Some("\"abc\""));
    }

    #[test]
    fn get_opt_maps_404_to_none() {
        let mut server = Server::new();
        server
            .mock("GET", "/missing")
            .with_status(404)
            .with_body(r#"{"message": "Not Found"}"#)
            .create();
        let client = Client::for_tests(&server.url());
        assert!(client.get_opt("missing").unwrap().is_none());
        let err = client.get("missing").unwrap_err().to_string();
        assert_eq!(err, "GitHub API 404 on GET missing: Not Found");
    }

    #[test]
    fn errors_carry_validation_details() {
        let mut server = Server::new();
        server
            .mock("POST", "/repos/o/r/pulls/1/comments")
            .with_status(422)
            .with_body(
                r#"{"message": "Validation Failed",
                    "errors": ["line could not be resolved", {"message": "path is invalid"}]}"#,
            )
            .create();
        let client = Client::for_tests(&server.url());
        let err = client
            .post("repos/o/r/pulls/1/comments", &json!({"body": "x"}))
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "GitHub API 422 on POST repos/o/r/pulls/1/comments: Validation Failed \
             (line could not be resolved; path is invalid)"
        );
    }

    #[test]
    fn empty_bodies_read_as_null() {
        let mut server = Server::new();
        server
            .mock("DELETE", "/repos/o/r/actions/caches/1")
            .with_status(204)
            .create();
        server
            .mock("PUT", "/x")
            .match_body(Matcher::Json(json!({"a": 1})))
            .with_status(204)
            .create();
        server
            .mock("PATCH", "/x")
            .with_body(r#"{"ok": true}"#)
            .create();
        let client = Client::for_tests(&server.url());
        assert_eq!(
            client.delete("repos/o/r/actions/caches/1").unwrap(),
            Value::Null
        );
        assert_eq!(client.put("x", &json!({"a": 1})).unwrap(), Value::Null);
        assert_eq!(client.patch("x", &json!({})).unwrap()["ok"], true);
    }

    #[test]
    fn text_and_raw_bodies() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/jobs/1/logs")
            .with_body("line 1\nline 2\n")
            .create();
        server
            .mock("GET", "/repos/o/r/contents/a.yml")
            .match_header("accept", "application/vnd.github.raw+json")
            .with_body("on: push\n")
            .create();
        let client = Client::for_tests(&server.url());
        assert_eq!(
            client.get_text("repos/o/r/actions/jobs/1/logs").unwrap(),
            "line 1\nline 2\n"
        );
        assert_eq!(
            client.get_raw("repos/o/r/contents/a.yml").unwrap(),
            "on: push\n"
        );
    }

    #[test]
    fn graphql_returns_data_or_the_errors() {
        let mut server = Server::new();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({"query": "query { viewer { login } }"}),
            ))
            .with_body(r#"{"data": {"viewer": {"login": "me"}}}"#)
            .expect(1)
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"query": "bad"})))
            .with_body(r#"{"data": null, "errors": [{"message": "one"}, {"message": "two"}]}"#)
            .create();
        let client = Client::for_tests(&server.url());
        // The viewer is fetched once and remembered.
        assert_eq!(client.viewer().unwrap(), "me");
        assert_eq!(client.viewer().unwrap(), "me");
        let err = client.graphql("bad", json!({})).unwrap_err().to_string();
        assert_eq!(err, "GraphQL error: one; two");
    }
}

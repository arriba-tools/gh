//! Files uploaded for use in comments, which GitHub serves from `github.com/user-attachments`.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::{Client, Repo};

/// Numeric ID of `repo`, which an upload names as its target. Uploading needs write access, and
/// the endpoint answers 404 without it, so a missing push permission fails here instead.
pub fn target(client: &Client, repo: &Repo) -> Result<u64> {
    #[derive(Deserialize)]
    struct Repository {
        id: u64,
        permissions: Option<Permissions>,
    }
    #[derive(Deserialize)]
    struct Permissions {
        push: bool,
    }
    let r: Repository =
        serde_json::from_value(client.get(&repo.api())?).context("Failed to parse repository")?;
    // An app installation token reads no permissions, so the endpoint decides for it.
    if r.permissions.is_some_and(|p| !p.push) {
        bail!("Uploading to {repo} requires write access to it");
    }
    Ok(r.id)
}

/// Upload `bytes` as `name` against the repository `repository_id`, returning the asset URL.
/// An upload cannot be undone.
pub fn upload(
    client: &Client,
    repository_id: u64,
    name: &str,
    content_type: &str,
    bytes: Vec<u8>,
) -> Result<String> {
    #[derive(Deserialize)]
    struct Asset {
        url: Option<String>,
    }
    let id = repository_id.to_string();
    let query = [
        ("name", name),
        ("content_type", content_type),
        ("repository_id", id.as_str()),
    ];
    let resp = client
        .upload("user-attachments/assets", &query, bytes)
        .with_context(|| format!("Failed to upload {name}"))?;
    let asset: Asset = serde_json::from_value(resp).context("Failed to parse uploaded asset")?;
    asset.url.context("GitHub returned no asset URL")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use serde_json::json;

    fn repo() -> Repo {
        Repo::parse("o/r").unwrap()
    }

    #[test]
    fn target_reads_the_repository_id() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r")
            .with_body(json!({ "id": 7, "permissions": { "push": true } }).to_string())
            .create();
        let client = Client::for_tests(&server.url());
        assert_eq!(target(&client, &repo()).unwrap(), 7);
    }

    #[test]
    fn target_refuses_without_write_access() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r")
            .with_body(json!({ "id": 7, "permissions": { "push": false } }).to_string())
            .create();
        let client = Client::for_tests(&server.url());
        let err = target(&client, &repo()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Uploading to o/r requires write access to it"
        );
    }

    #[test]
    fn target_leaves_unknown_permissions_to_the_endpoint() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r")
            .with_body(json!({ "id": 7 }).to_string())
            .create();
        let client = Client::for_tests(&server.url());
        assert_eq!(target(&client, &repo()).unwrap(), 7);
    }

    #[test]
    fn upload_posts_the_bytes_and_returns_the_url() {
        let mut server = Server::new();
        let post = server
            .mock("POST", "/user-attachments/assets")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("name".into(), "a b.png".into()),
                Matcher::UrlEncoded("content_type".into(), "image/png".into()),
                Matcher::UrlEncoded("repository_id".into(), "7".into()),
            ]))
            .match_header("content-type", "application/octet-stream")
            .match_body(b"png!".to_vec())
            .with_body(r#"{"url": "https://github.com/user-attachments/assets/u1"}"#)
            .create();
        let client = Client::for_tests(&server.url());

        let url = upload(&client, 7, "a b.png", "image/png", b"png!".to_vec()).unwrap();

        assert_eq!(url, "https://github.com/user-attachments/assets/u1");
        post.assert();
    }

    #[test]
    fn upload_reports_the_failed_file() {
        let mut server = Server::new();
        server
            .mock("POST", "/user-attachments/assets")
            .match_query(Matcher::Any)
            .with_status(404)
            .with_body(r#"{"message": "Not Found"}"#)
            .create();
        let client = Client::for_tests(&server.url());

        let err = upload(&client, 7, "a.png", "image/png", b"x".to_vec()).unwrap_err();

        assert_eq!(err.to_string(), "Failed to upload a.png");
        assert!(format!("{err:#}").contains("404"), "{err:#}");
    }
}

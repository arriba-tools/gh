//! Upload images for use inline in comments and print the markdown that shows them.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs;
use std::path::Path;

use crate::api::attachments;
use crate::cli::UploadArgs;
use crate::context::Ctx;
use crate::output;

/// Largest image GitHub takes for use in a comment.
const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// Content type the endpoint expects for each image extension.
const CONTENT_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
];

/// A local image checked for upload.
#[derive(Debug, PartialEq)]
struct Image {
    /// The path as the user wrote it.
    path: String,
    /// File name the upload is stored under.
    name: String,
    content_type: &'static str,
    alt: String,
}

#[derive(Debug, Serialize)]
struct Uploaded {
    path: String,
    url: String,
    markdown: String,
}

pub fn run(ctx: &Ctx, args: UploadArgs) -> Result<()> {
    // An upload cannot be undone, so every file is checked before the first one is sent.
    let images = args
        .files
        .iter()
        .map(|a| Image::from_arg(a))
        .collect::<Result<Vec<_>>>()?;
    let id = attachments::target(&ctx.client, &ctx.repo()?)?;

    // What uploaded before a failure is still reported, since nothing else records its URL.
    let mut uploaded = Vec::new();
    let mut failure = None;
    for image in &images {
        match image.upload(ctx, id) {
            Ok(u) => {
                if !ctx.json {
                    println!("{}", u.markdown);
                }
                uploaded.push(u);
            }
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    if ctx.json {
        output::print_json(&uploaded)?;
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

impl Image {
    /// Check one `FILE[#ALT]` argument.
    fn from_arg(arg: &str) -> Result<Self> {
        let (path, alt) = split_file_arg(arg);
        if path.is_empty() {
            bail!("Pass a path to an image");
        }
        let meta = fs::metadata(path).with_context(|| format!("Cannot read {path}"))?;
        if meta.is_dir() {
            bail!("{path} is a directory");
        }
        // A named pipe passes metadata, then blocks the read forever.
        if !meta.is_file() {
            bail!("{path} is not a regular file");
        }
        // An empty file uploads, then renders broken.
        if meta.len() == 0 {
            bail!("{path} is empty");
        }
        if meta.len() > MAX_IMAGE_BYTES {
            bail!("{path} is over the 10 MiB limit for images");
        }

        let file = Path::new(path);
        let ext = file
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let Some(&(_, content_type)) = CONTENT_TYPES.iter().find(|(e, _)| *e == ext) else {
            let supported: Vec<&str> = CONTENT_TYPES.iter().map(|(e, _)| *e).collect();
            bail!(
                "{path} is not a supported image (supported: {})",
                supported.join(", ")
            );
        };
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .with_context(|| format!("{path} has no file name"))?;
        let alt = if alt.is_empty() {
            default_alt(&name)
        } else {
            alt.to_string()
        };
        Ok(Self {
            path: path.to_string(),
            name,
            content_type,
            alt,
        })
    }

    fn upload(&self, ctx: &Ctx, repository_id: u64) -> Result<Uploaded> {
        let bytes = fs::read(&self.path).with_context(|| format!("Cannot read {}", self.path))?;
        let url = attachments::upload(
            &ctx.client,
            repository_id,
            &self.name,
            self.content_type,
            bytes,
        )?;
        Ok(Uploaded {
            path: self.path.clone(),
            markdown: self.markdown(&url),
            url,
        })
    }

    fn markdown(&self, url: &str) -> String {
        format!("![{}]({url})", escape_alt(&self.alt))
    }
}

/// Split `FILE#ALT`. A path that exists wins over the delimiter, since `#` is legal in file
/// names, and the longest existing prefix wins among several hashes.
fn split_file_arg(arg: &str) -> (&str, &str) {
    if Path::new(arg).exists() {
        return (arg, "");
    }
    let mut end = arg.len();
    while let Some(i) = arg[..end].rfind('#').filter(|&i| i > 0) {
        if Path::new(&arg[..i]).exists() {
            return (&arg[..i], &arg[i + 1..]);
        }
        end = i;
    }
    // Nothing exists, so split at the last hash to name the path a reader would expect.
    match arg.rfind('#').filter(|&i| i > 0) {
        Some(i) => (&arg[..i], &arg[i + 1..]),
        None => (arg, ""),
    }
}

/// Alt text the web uploader gives a file, its name without the extension and with dots as spaces.
fn default_alt(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    stem.replace('.', " ")
}

/// Alt text that cannot close the image syntax early or span lines.
fn escape_alt(alt: &str) -> String {
    alt.replace('\\', r"\\")
        .replace('[', r"\[")
        .replace(']', r"\]")
        .replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use mockito::{Matcher, Server, ServerGuard};
    use serde_json::json;
    use std::path::PathBuf;

    /// A scratch directory, removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("gh-arriba-upload-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        /// Write `name` and return its path as a string.
        fn file(&self, name: &str, bytes: &[u8]) -> String {
            let path = self.0.join(name);
            fs::write(&path, bytes).unwrap();
            path.to_string_lossy().into_owned()
        }

        fn path(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn ctx(server: &ServerGuard, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: false,
        }
    }

    fn serve_repo(server: &mut ServerGuard) -> mockito::Mock {
        server
            .mock("GET", "/repos/o/r")
            .with_body(json!({ "id": 7, "permissions": { "push": true } }).to_string())
            .create()
    }

    #[test]
    fn split_prefers_paths_that_exist() {
        let dir = Dir::new("split");
        let hashed = dir.file("a#b.png", b"x");
        assert_eq!(split_file_arg(&hashed), (hashed.as_str(), ""));
        let arg = format!("{hashed}#Alt #1");
        assert_eq!(split_file_arg(&arg), (hashed.as_str(), "Alt #1"));
        // Nothing exists, so the last hash splits.
        let missing = dir.path("c#d.png");
        let arg = format!("{missing}#Alt");
        assert_eq!(split_file_arg(&arg), (missing.as_str(), "Alt"));
        assert_eq!(split_file_arg("#a.png"), ("#a.png", ""));
    }

    #[test]
    fn from_arg_reads_type_name_and_alt() {
        let dir = Dir::new("from-arg");
        let path = dir.file("login.error.PNG", b"x");

        let image = Image::from_arg(&path).unwrap();
        assert_eq!(image.name, "login.error.PNG");
        assert_eq!(image.content_type, "image/png");
        assert_eq!(image.alt, "login error");

        let image = Image::from_arg(&format!("{path}#The login error")).unwrap();
        assert_eq!(image.path, path);
        assert_eq!(image.alt, "The login error");
    }

    #[test]
    fn from_arg_refuses_what_cannot_upload() {
        let dir = Dir::new("refuse");
        let err = |arg: &str| Image::from_arg(arg).unwrap_err().to_string();

        let missing = dir.path("missing.png");
        assert_eq!(err(&missing), format!("Cannot read {missing}"));
        let sub = dir.path("sub.png");
        fs::create_dir(&sub).unwrap();
        assert_eq!(err(&sub), format!("{sub} is a directory"));
        let empty = dir.file("empty.png", b"");
        assert_eq!(err(&empty), format!("{empty} is empty"));
        let text = dir.file("notes.txt", b"x");
        assert_eq!(
            err(&text),
            format!("{text} is not a supported image (supported: png, jpg, jpeg, gif, webp, svg)")
        );
        // A sparse file is over the limit without writing 10 MiB.
        let big = dir.file("big.png", b"");
        fs::File::options()
            .write(true)
            .open(&big)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES + 1)
            .unwrap();
        assert_eq!(
            err(&big),
            format!("{big} is over the 10 MiB limit for images")
        );
        assert_eq!(err(""), "Pass a path to an image");
    }

    #[test]
    fn markdown_escapes_alt_text() {
        let image = Image {
            path: "a.png".into(),
            name: "a.png".into(),
            content_type: "image/png",
            alt: "a](https://evil) [b\\\nc".into(),
        };
        assert_eq!(
            image.markdown("https://u/1"),
            r"![a\](https://evil) \[b\\ c](https://u/1)"
        );
    }

    #[test]
    fn run_uploads_each_image_to_the_repository() {
        let dir = Dir::new("run");
        let first = dir.file("one.png", b"1");
        let second = dir.file("two.gif", b"2");
        let mut server = Server::new();
        let get = serve_repo(&mut server);
        let mut post = |name: &str, body: &[u8], url: &str| {
            server
                .mock("POST", "/user-attachments/assets")
                .match_query(Matcher::AllOf(vec![
                    Matcher::UrlEncoded("name".into(), name.into()),
                    Matcher::UrlEncoded("repository_id".into(), "7".into()),
                ]))
                .match_body(body.to_vec())
                .with_body(json!({ "url": url }).to_string())
                .create()
        };
        let one = post("one.png", b"1", "https://u/1");
        let two = post("two.gif", b"2", "https://u/2");

        run(
            &ctx(&server, false),
            UploadArgs {
                files: vec![first, format!("{second}#Second")],
            },
        )
        .unwrap();

        get.assert();
        one.assert();
        two.assert();
    }

    #[test]
    fn run_checks_every_file_before_uploading_any() {
        let dir = Dir::new("run-check");
        let good = dir.file("good.png", b"1");
        let bad = dir.file("bad.bmp", b"2");
        let mut server = Server::new();
        let get = server.mock("GET", Matcher::Any).expect(0).create();
        let post = server.mock("POST", Matcher::Any).expect(0).create();

        let err = run(
            &ctx(&server, true),
            UploadArgs {
                files: vec![good, bad.clone()],
            },
        )
        .unwrap_err();

        assert!(
            err.to_string().starts_with(&format!("{bad} is not")),
            "{err}"
        );
        get.assert();
        post.assert();
    }

    #[test]
    fn run_stops_at_the_first_failed_upload() {
        let dir = Dir::new("run-fail");
        let first = dir.file("one.png", b"1");
        let second = dir.file("two.png", b"2");
        let mut server = Server::new();
        serve_repo(&mut server);
        let post = server
            .mock("POST", "/user-attachments/assets")
            .match_query(Matcher::Any)
            .with_status(422)
            .with_body(r#"{"message": "Validation Failed"}"#)
            .expect(1)
            .create();

        let err = run(
            &ctx(&server, true),
            UploadArgs {
                files: vec![first, second],
            },
        )
        .unwrap_err();

        assert_eq!(err.to_string(), "Failed to upload one.png");
        post.assert();
    }
}

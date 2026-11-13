mod manifest;
mod project;
mod sync;

use anyhow::Result;

use crate::api::gists::GistClient;
use crate::cli::{ConfigArgs, ConfigCommand};
use crate::context::Ctx;
use crate::prompt;
use project::ProjectConfig;

/// Says where config moved, on stderr so a script reading stdout sees the same output.
const MOVED: &str = "note: `gh arriba config` moved to `gh roam`, which takes the same commands and \
                     gists, and will leave gh-arriba in a later release. Install it with \
                     `gh extension install arriba-tools/gh-roam`.";

pub fn run(ctx: &Ctx, args: ConfigArgs) -> Result<()> {
    eprintln!("{MOVED}");
    let config = ProjectConfig::new(args.name, args.root, args.private)?;
    let client = GistClient::new(&ctx.client);

    let gist_id = client.find_gist(&config.gist_description())?;

    match args.command {
        Some(ConfigCommand::Add { files, platform }) => {
            sync::do_add(&client, &config, &files, platform, gist_id.as_deref())?;
        }
        Some(ConfigCommand::Remove { files, platform }) => {
            sync::do_remove(&client, &config, &files, platform, gist_id.as_deref())?;
        }
        Some(ConfigCommand::Cleanup) => {
            sync::do_cleanup(&client, &config, gist_id.as_deref())?;
        }
        Some(ConfigCommand::Delete) => {
            let prompt = format!("Delete the {} gist?", config.gist_description());
            if prompt::confirm(&prompt, ctx.yes)? {
                sync::do_delete(&client, gist_id.as_deref())?;
            }
        }
        Some(ConfigCommand::Open) => {
            sync::do_open(gist_id.as_deref())?;
        }
        Some(ConfigCommand::Sync { files }) => {
            sync::do_sync(&client, &config, &files, gist_id.as_deref())?;
        }
        Some(ConfigCommand::Pull { files }) => {
            sync::do_pull(&client, &config, &files, gist_id.as_deref())?;
        }
        Some(ConfigCommand::Status { files }) => {
            sync::do_status(&client, &config, &files, gist_id.as_deref())?;
        }
        Some(ConfigCommand::Restore { files }) => {
            sync::do_restore(&client, &config, &files, gist_id.as_deref())?;
        }
        None => {
            sync::do_sync(&client, &config, &[], gist_id.as_deref())?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use mockito::{Matcher, Server, ServerGuard};
    use serde_json::json;
    use std::path::PathBuf;

    /// A scratch project root, removed on drop.
    struct Root(PathBuf);

    impl Root {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("arriba-config-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ctx(server: &ServerGuard) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json: true,
            yes: true,
        }
    }

    fn args(root: &Root, command: Option<ConfigCommand>) -> ConfigArgs {
        ConfigArgs {
            command,
            name: Some("proj".into()),
            root: Some(root.0.clone()),
            private: true,
        }
    }

    /// List the user's gists, with `proj config-sync` as g1 when `exists`.
    fn list_gists(server: &mut ServerGuard, exists: bool) {
        let mut gists = vec![json!({ "id": "other", "description": "other config-sync" })];
        if exists {
            gists.push(json!({ "id": "g1", "description": "proj config-sync" }));
        }
        server
            .mock("GET", "/gists")
            .match_query(Matcher::Any)
            .with_body(json!(gists).to_string())
            .create();
    }

    #[test]
    fn no_command_syncs_into_the_given_root() {
        let root = Root::new("default");
        let mut server = Server::new();
        list_gists(&mut server, true);
        let manifest = "name: proj\ndescription: d\nurl: u\nfiles:\n- path: a.txt\n  gist: proj_a.txt\n  updated_at: 2024-01-01T00:00:00Z\n";
        server
            .mock("GET", "/gists/g1")
            .with_body(
                json!({
                    "id": "g1",
                    "files": {
                        ".proj-config-sync.yaml": { "content": manifest },
                        "proj_a.txt": { "content": "from gist" }
                    }
                })
                .to_string(),
            )
            .create();

        run(&ctx(&server), args(&root, None)).unwrap();
        let pulled = std::fs::read_to_string(root.0.join("a.txt")).unwrap();
        assert_eq!(pulled, "from gist\n");
    }

    #[test]
    fn add_without_a_gist_creates_a_private_one() {
        let root = Root::new("add");
        std::fs::write(root.0.join("a.txt"), "a").unwrap();
        let mut server = Server::new();
        list_gists(&mut server, false);
        let create = server
            .mock("POST", "/gists")
            .match_body(Matcher::PartialJson(json!({
                "description": "proj config-sync",
                "public": false,
                "files": { "proj_a.txt": { "content": "a" } }
            })))
            .with_body(r#"{"id": "g2"}"#)
            .expect(1)
            .create();

        let command = ConfigCommand::Add {
            files: vec!["a.txt".into()],
            platform: false,
        };
        run(&ctx(&server), args(&root, Some(command))).unwrap();
        create.assert();
    }

    #[test]
    fn delete_with_yes_deletes_the_project_gist() {
        let root = Root::new("delete");
        let mut server = Server::new();
        list_gists(&mut server, true);
        let delete = server
            .mock("DELETE", "/gists/g1")
            .with_status(204)
            .expect(1)
            .create();
        run(&ctx(&server), args(&root, Some(ConfigCommand::Delete))).unwrap();
        delete.assert();
    }

    #[test]
    fn commands_needing_a_gist_fail_without_one() {
        let root = Root::new("missing");
        let mut server = Server::new();
        list_gists(&mut server, false);
        let ctx = ctx(&server);

        // Status reports the missing gist instead of failing.
        let status = ConfigCommand::Status { files: vec![] };
        run(&ctx, args(&root, Some(status))).unwrap();

        for command in [
            ConfigCommand::Cleanup,
            ConfigCommand::Open,
            ConfigCommand::Pull { files: vec![] },
            ConfigCommand::Restore { files: vec![] },
            ConfigCommand::Remove {
                files: vec!["a".into()],
                platform: false,
            },
            ConfigCommand::Sync { files: vec![] },
        ] {
            assert!(run(&ctx, args(&root, Some(command))).is_err());
        }
    }
}

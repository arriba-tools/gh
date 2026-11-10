mod api;
mod cli;
mod config;
mod context;
mod dependabot;
mod focus;
mod inbox;
mod issue;
mod log;
mod notifications;
mod notify;
mod output;
mod parallel;
mod pr;
mod prompt;
mod release;
mod run;
mod runners;
mod upload;
mod wait;
mod workflow;
mod worktree;

use anyhow::Result;
use clap::Parser;

use api::Client;
use cli::{Cli, Command};
use context::Ctx;

fn main() -> Result<()> {
    // Exit quietly when stdout closes early, as when piped into head, instead of panicking.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();

    // Built on first use, so commands that never reach GitHub need no token.
    let ctx = || -> Result<Ctx> {
        Ok(Ctx {
            client: Client::new()?,
            repo_flag: cli.repo.clone(),
            json: cli.json,
            yes: cli.yes,
        })
    };

    match cli.command {
        Command::Pr(cmd) => pr::run(&ctx()?, cmd),
        Command::Upload(args) => upload::run(&ctx()?, args),
        Command::Run(cmd) => run::run(&ctx()?, cmd),
        Command::Release(args) => release::run(&ctx()?, args),
        Command::Issue(cmd) => issue::run(&ctx()?, cmd),
        Command::Dependabot(cmd) => dependabot::run(&ctx()?, cmd),
        Command::Runners(args) => runners::run(&ctx()?, args),
        Command::Worktree(cmd) => worktree::run(&ctx()?, cmd),
        Command::Notify(args) => notifications::run(ctx, cli.yes, args),
        Command::Inbox(cmd) => inbox::run(&ctx()?, cmd),
        Command::Config(args) => config::run(&ctx()?, args),
    }
}

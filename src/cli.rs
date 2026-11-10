use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "gh-arriba",
    bin_name = "gh arriba",
    about = "GitHub workflows that gh doesn't ship",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Target repository as OWNER/REPO (default: current repo)
    #[arg(short = 'R', long, global = true, value_name = "OWNER/REPO")]
    pub repo: Option<String>,

    /// Print machine-readable JSON
    #[arg(long, global = true)]
    pub json: bool,

    /// Skip confirmation prompts
    #[arg(short, long, global = true)]
    pub yes: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Review threads, pending reviews, retries, and rebases for pull requests
    #[command(subcommand)]
    Pr(PrCommand),
    /// Upload images and print the markdown that shows them in a comment
    ///
    /// GitHub hosts each image against the repository, which needs write access to it. An upload
    /// cannot be undone.
    Upload(UploadArgs),
    /// Watch workflow runs and download their logs
    #[command(subcommand)]
    Run(RunCommand),
    /// Dispatch the repo's release workflow with a version bump
    Release(ReleaseArgs),
    /// Wait on an issue for new comments, or on a repo for labelled issues
    #[command(subcommand)]
    Issue(IssueCommand),
    /// Wait on a branch for commits that add or change files
    #[command(subcommand)]
    Repo(RepoCommand),
    /// Triage Dependabot pull requests and security alerts
    #[command(subcommand)]
    Dependabot(DependabotCommand),
    /// List self-hosted runners and what they are running
    Runners(RunnersArgs),
    /// List and prune git worktrees by the state of their pull requests
    #[command(subcommand)]
    Worktree(WorktreeCommand),
    /// Show a desktop notification, or raise them for your GitHub inbox
    #[command(alias = "notifications")]
    Notify(NotifyArgs),
    /// Tidy your GitHub notifications inbox
    #[command(subcommand)]
    Inbox(InboxCommand),
    /// Sync project config files to and from a gist
    Config(ConfigArgs),
}

#[derive(Args)]
pub struct UploadArgs {
    /// Images to upload, each with optional alt text after `#`, as in `login.png#Login error`
    #[arg(required = true, value_name = "FILE[#ALT]")]
    pub files: Vec<String>,
}

#[derive(Subcommand)]
pub enum PrCommand {
    /// List unresolved review threads from humans and bots
    Threads(ThreadsArgs),
    /// Reply to a review comment or thread
    Reply(ReplyArgs),
    /// Resolve or unresolve review threads
    Resolve(ResolveArgs),
    /// Show or edit one comment by URL or ID
    #[command(subcommand)]
    Comment(CommentCommand),
    /// Manage your pending review and its inline comments
    #[command(subcommand)]
    Review(ReviewCommand),
    /// Apply reviewers' suggestion blocks to the working tree
    Apply(ApplyArgs),
    /// Write the whole discussion as grep-friendly text
    Dump(DumpArgs),
    /// Summarize a PR's state and checks, failing when a check failed
    Checks(ChecksArgs),
    /// Rerun failed and cancelled checks on a PR, your PRs, or every open PR
    Retry(RetryArgs),
    /// Show failed checks with their failed steps and error lines
    Failures(FailuresArgs),
    /// Rebase a PR onto its base, signed locally or server-side
    Rebase(RebaseArgs),
    /// Suggest reviewers from top contributors and request one
    Reviewers(ReviewersArgs),
    /// List the repo's labels, and add or remove them on a PR
    Labels(LabelsArgs),
}

#[derive(Args)]
pub struct ThreadsArgs {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,

    /// Only Copilot, CodeRabbit, and other bot threads
    #[arg(long, conflicts_with = "humans")]
    pub bots: bool,

    /// Only human threads
    #[arg(long)]
    pub humans: bool,

    /// Only threads started by this reviewer, @me for yours
    #[arg(long, value_name = "LOGIN")]
    pub author: Option<String>,

    /// Include resolved threads
    #[arg(long)]
    pub resolved: bool,

    /// Include outdated threads
    #[arg(long)]
    pub outdated: bool,

    /// Only threads opened or updated since your last review
    #[arg(long)]
    pub since_last_review: bool,

    /// Include CodeRabbit findings that live in review bodies
    #[arg(long)]
    pub review_bodies: bool,
}

#[derive(Args)]
pub struct ReplyArgs {
    /// Comment database ID, comment URL, or thread ID (PRRT_...)
    pub target: String,

    /// Reply text, or - to read stdin (default: stdin)
    pub body: Option<String>,

    /// Resolve the thread after replying
    #[arg(long)]
    pub resolve: bool,
}

#[derive(Args)]
pub struct ResolveArgs {
    /// Thread IDs (PRRT_...), comment database IDs, or comment URLs
    #[arg(required = true)]
    pub targets: Vec<String>,

    /// Unresolve instead
    #[arg(long)]
    pub undo: bool,
}

#[derive(Subcommand)]
pub enum CommentCommand {
    /// Print a review comment, issue comment, or review body
    Show(CommentShowArgs),
    /// Replace the body of a comment or review you posted
    Edit(CommentEditArgs),
}

#[derive(Args)]
pub struct CommentShowArgs {
    /// Comment URL (#discussion_r..., #issuecomment-..., #pullrequestreview-...) or ID
    #[arg(value_name = "URL|ID")]
    pub target: String,
}

#[derive(Args)]
pub struct CommentEditArgs {
    /// Comment URL (#discussion_r..., #issuecomment-..., #pullrequestreview-...) or ID
    #[arg(value_name = "URL|ID")]
    pub target: String,

    /// New text, or - to read stdin (default: stdin)
    pub body: Option<String>,
}

#[derive(Subcommand)]
pub enum ReviewCommand {
    /// Print the diff line ranges that accept inline comments
    Lines(PrArg),
    /// Add an inline comment to your pending review, creating it if needed
    #[command(override_usage = "gh arriba pr review add [OPTIONS] [PR] <PATH:LINE[-END]> [BODY]")]
    Add(ReviewAddArgs),
    /// Print the comments in your pending review
    Show(PrArg),
    /// Submit your pending review as comment, approve, or request changes
    Submit(ReviewSubmitArgs),
    /// Delete your pending review and its comments
    Discard(PrArg),
}

#[derive(Args)]
pub struct PrArg {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,
}

#[derive(Args)]
pub struct ReviewAddArgs {
    /// [PR]: number, URL, or branch (default: PR for the current branch).
    /// PATH:LINE[-END]: file and line or range in the PR diff.
    /// [BODY]: comment text, or - to read stdin (default: stdin).
    #[arg(value_name = "ARGS", required = true, num_args = 1..=3, verbatim_doc_comment)]
    pub args: Vec<String>,

    /// Diff side the line numbers refer to
    #[arg(long, value_enum, default_value_t = Side::Right)]
    pub side: Side,
}

#[derive(Args)]
pub struct ReviewSubmitArgs {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,

    /// Review verdict
    #[arg(long, value_enum, default_value_t = ReviewEvent::Comment)]
    pub event: ReviewEvent,

    /// Summary body for the review
    #[arg(long)]
    pub body: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Side {
    Left,
    Right,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

#[derive(Args)]
pub struct ApplyArgs {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,

    /// Only apply suggestions from these threads (PRRT_... or comment ID)
    #[arg(long = "thread", value_name = "ID")]
    pub threads: Vec<String>,

    /// Print the changes without writing files
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct DumpArgs {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,

    /// Write to this file instead of stdout
    #[arg(short, long, value_name = "FILE")]
    pub out: Option<PathBuf>,
}

#[derive(Args)]
pub struct ChecksArgs {
    /// Numbers, URLs, owner/repo#N, or branches (default: PR for the current branch)
    #[arg(value_name = "PR")]
    pub prs: Vec<String>,

    /// Poll until no check is pending, printing each check as it completes
    #[arg(long)]
    pub wait: bool,

    /// Seconds between polls
    #[arg(long, value_name = "SECS", default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..))]
    pub interval: u64,

    /// Minutes to wait before giving up
    #[arg(long, value_name = "MINS", default_value_t = 30)]
    pub timeout: u64,

    /// Stop waiting as soon as a check fails
    #[arg(long, requires = "wait")]
    pub fail_fast: bool,

    /// Show a desktop notification per PR when the wait ends
    #[arg(long, requires = "wait")]
    pub notify: bool,

    /// First wait until the PR head is this commit, a hash or a local revision such as HEAD
    #[arg(long, value_name = "SHA")]
    pub sha: Option<String>,
}

#[derive(Args)]
pub struct RetryArgs {
    /// Number, URL, or owner/repo#N, or owner/repo for all your PRs (default: current branch)
    pub target: Option<String>,

    /// Retry every open PR you authored in the repo
    #[arg(long)]
    pub mine: bool,

    /// Retry every open PR in the repo, whoever authored it
    #[arg(long, conflicts_with = "mine")]
    pub all: bool,

    /// Print the run tree without rerunning
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct FailuresArgs {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,

    /// Only checks whose name contains this text, repeatable
    #[arg(long = "job", value_name = "PATTERN")]
    pub jobs: Vec<String>,

    /// Directory to save full logs in (default: temp dir)
    #[arg(short, long, value_name = "DIR")]
    pub out: Option<PathBuf>,

    /// Lines of log to show before each error
    #[arg(long, value_name = "N", default_value_t = crate::log::CONTEXT)]
    pub context: usize,

    /// Error blocks to show per job
    #[arg(long, value_name = "N", default_value_t = crate::log::MAX)]
    pub max: usize,

    /// Show the lines matching this regex instead of the error lines
    #[arg(long, value_name = "REGEX", value_parser = parse_regex)]
    pub grep: Option<regex::Regex>,

    /// Download logs again even when a saved copy exists
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Args)]
pub struct RebaseArgs {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,

    /// Rebase server-side, commits are unsigned
    #[arg(long, conflicts_with = "check")]
    pub remote: bool,

    /// Report conflicting files without rebasing
    #[arg(long)]
    pub check: bool,

    /// Don't comment "Rebased." on the PR
    #[arg(long)]
    pub no_comment: bool,
}

#[derive(Args)]
pub struct ReviewersArgs {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,

    /// Request a review from this user, repeatable
    #[arg(long, value_name = "LOGIN")]
    pub add: Vec<String>,

    /// Number of contributors to suggest
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
}

#[derive(Args)]
pub struct LabelsArgs {
    /// Number, URL, or branch (default: PR for the current branch, if any)
    pub pr: Option<String>,

    /// Add a label, matched loosely against the repo's labels, repeatable
    #[arg(long, value_name = "NAME")]
    pub add: Vec<String>,

    /// Remove a label, matched loosely against the repo's labels, repeatable
    #[arg(long, value_name = "NAME")]
    pub remove: Vec<String>,

    /// Rank labels by words shared with the PR's title, body, branch, and files
    #[arg(long, conflicts_with_all = ["add", "remove"])]
    pub suggest: bool,
}

#[derive(Subcommand)]
pub enum RunCommand {
    /// Download job logs to files for searching
    Logs(LogsArgs),
    /// Follow runs until their jobs finish, failing if any job fails
    Watch(WatchArgs),
    /// Dispatch a workflow and print the run it starts
    Dispatch(DispatchArgs),
    /// List, download, or prune the artifacts of runs
    Artifacts(ArtifactsArgs),
    /// Show Actions cache usage by ref and key against the repo limit, or prune caches
    Caches(CachesArgs),
    /// Show each job's pass rate, flaky re-runs, and run time over recent runs
    Stats(StatsArgs),
}

#[derive(Args)]
pub struct StatsArgs {
    /// Only runs of this workflow, by name or file
    #[arg(short, long, value_name = "NAME")]
    pub workflow: Option<String>,

    /// Branch whose runs to measure (default: the default branch)
    #[arg(long)]
    pub branch: Option<String>,

    /// Completed runs to measure, newest first
    #[arg(long, value_name = "N", default_value_t = 50, value_parser = clap::value_parser!(u64).range(1..=1000))]
    pub limit: u64,
}

#[derive(Args)]
pub struct LogsArgs {
    /// Run ID, job ID, or Actions URL
    #[arg(value_name = "RUN_OR_JOB")]
    pub target: String,

    /// Only jobs that failed
    #[arg(long)]
    pub failed: bool,

    /// Download directory (default: temp dir)
    #[arg(short, long, value_name = "DIR")]
    pub out: Option<PathBuf>,

    /// Only jobs whose name contains this text, repeatable
    #[arg(long = "job", value_name = "PATTERN")]
    pub jobs: Vec<String>,

    /// Print each job's error lines after saving its log
    #[arg(long)]
    pub errors: bool,

    /// Print the lines matching this regex instead of the error lines
    #[arg(long, value_name = "REGEX", value_parser = parse_regex, conflicts_with = "errors")]
    pub grep: Option<regex::Regex>,

    /// Lines of log to show before each error or match
    #[arg(long, value_name = "N", default_value_t = crate::log::CONTEXT)]
    pub context: usize,

    /// Error or match blocks to show per job
    #[arg(long, value_name = "N", default_value_t = crate::log::MAX)]
    pub max: usize,

    /// Download logs again even when a saved copy exists
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Args)]
pub struct WatchArgs {
    /// Run IDs, job IDs, or Actions URLs (default: every run for the local HEAD commit)
    #[arg(value_name = "RUN")]
    pub runs: Vec<String>,

    /// Watch every run for this commit
    #[arg(long, conflicts_with_all = ["runs", "branch"])]
    pub sha: Option<String>,

    /// Watch the runs for the newest commit with runs on this branch
    #[arg(long, conflicts_with = "runs")]
    pub branch: Option<String>,

    /// Only runs of this workflow, by name or file
    #[arg(short, long, value_name = "NAME", conflicts_with = "runs")]
    pub workflow: Option<String>,

    #[command(flatten)]
    pub wait: WaitArgs,
}

#[derive(Args)]
pub struct DispatchArgs {
    /// Workflow name, file name, or file name without extension
    pub workflow: String,

    /// Branch or tag to run the workflow on (default: the default branch)
    #[arg(long = "ref", value_name = "BRANCH")]
    pub git_ref: Option<String>,

    /// Set a workflow input, repeatable
    #[arg(short = 'f', long = "field", value_name = "KEY=VALUE")]
    pub fields: Vec<String>,

    /// Follow the run until it finishes
    #[arg(long)]
    pub watch: bool,

    #[command(flatten)]
    pub wait: WaitArgs,
}

#[derive(Args)]
pub struct ArtifactsArgs {
    /// Run IDs, job IDs, or Actions URLs (default: every run for the local HEAD commit)
    #[arg(value_name = "RUN")]
    pub runs: Vec<String>,

    /// Runs for this commit
    #[arg(long, conflicts_with_all = ["runs", "branch", "pr"])]
    pub sha: Option<String>,

    /// Runs for the newest commit with runs on this branch
    #[arg(long, conflicts_with_all = ["runs", "pr"])]
    pub branch: Option<String>,

    /// Runs for the head commit of this PR, as a number, URL, or branch
    #[arg(long, value_name = "PR", conflicts_with = "runs")]
    pub pr: Option<String>,

    /// Only runs of this workflow, by name or file
    #[arg(short, long, value_name = "NAME", conflicts_with = "runs")]
    pub workflow: Option<String>,

    /// The newest successful run with artifacts on --branch (default: the default branch)
    #[arg(long, conflicts_with_all = ["runs", "sha", "pr"])]
    pub success: bool,

    /// Only artifacts with this exact name, repeatable
    #[arg(short = 'n', long = "name", value_name = "NAME")]
    pub names: Vec<String>,

    /// Only artifacts whose name matches this glob of * and ?, repeatable
    #[arg(short = 'p', long = "pattern", value_name = "GLOB")]
    pub patterns: Vec<String>,

    /// Download and extract the artifacts, each into its own directory when there are several
    #[arg(long)]
    pub download: bool,

    /// Directory to extract into
    #[arg(
        short = 'D',
        long,
        value_name = "DIR",
        default_value = ".",
        requires = "download"
    )]
    pub dir: PathBuf,

    /// Delete old artifacts across the repo, or of the selected runs
    #[arg(long, conflicts_with_all = ["download", "success"])]
    pub prune: bool,

    /// Prune only artifacts older than this, as in 12h, 7d, or 2w
    #[arg(long, value_name = "AGE", default_value = "7d", value_parser = parse_age, requires = "prune")]
    pub older_than: chrono::TimeDelta,

    /// Newest artifacts of each name to keep, however old
    #[arg(long, value_name = "N", default_value_t = 1, requires = "prune")]
    pub keep_latest: usize,

    /// Print what --prune would delete without deleting it
    #[arg(long, requires = "prune")]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct CachesArgs {
    /// Only caches for this branch, PR number, or full ref
    #[arg(long = "ref", value_name = "REF")]
    pub git_ref: Option<String>,

    /// Only caches whose key starts with this
    #[arg(short, long, value_name = "PREFIX")]
    pub key: Option<String>,

    /// Delete the caches that the filters and the options below select
    #[arg(long)]
    pub prune: bool,

    /// Prune only caches not used for this long, as in 12h, 7d, or 2w
    #[arg(long, value_name = "AGE", value_parser = parse_age, requires = "prune")]
    pub unused_for: Option<chrono::TimeDelta>,

    /// Prune only caches created longer ago than this
    #[arg(long, value_name = "AGE", value_parser = parse_age, requires = "prune")]
    pub older_than: Option<chrono::TimeDelta>,

    /// Prune only caches of PRs that are closed or merged
    #[arg(long, requires = "prune")]
    pub closed: bool,

    /// Print what --prune would delete without deleting it
    #[arg(long, requires = "prune")]
    pub dry_run: bool,
}

/// Parse an age such as `30m`, `12h`, `7d`, or `2w`.
fn parse_age(s: &str) -> Result<chrono::TimeDelta, String> {
    let (number, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = number
        .parse()
        .map_err(|_| format!("expected a number and a unit such as 7d, got {s:?}"))?;
    match unit {
        "m" => Ok(chrono::TimeDelta::minutes(n)),
        "h" => Ok(chrono::TimeDelta::hours(n)),
        "d" => Ok(chrono::TimeDelta::days(n)),
        "w" => Ok(chrono::TimeDelta::weeks(n)),
        _ => Err(format!("expected a unit of m, h, d, or w, got {s:?}")),
    }
}

#[derive(Args)]
pub struct WaitArgs {
    /// Seconds between polls
    #[arg(long, value_name = "SECS", default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..))]
    pub interval: u64,

    /// Minutes to wait before giving up
    #[arg(long, value_name = "MINS", default_value_t = 60)]
    pub timeout: u64,

    /// Only wait for jobs whose name contains this, ignoring case
    #[arg(long, value_name = "PATTERN")]
    pub job: Option<String>,

    /// Show a desktop notification when the runs finish
    #[arg(long)]
    pub notify: bool,
}

#[derive(Args)]
pub struct ReleaseArgs {
    /// Version part to increment
    #[arg(long, value_enum, default_value_t = Bump::Patch)]
    pub bump: Bump,

    /// Exact version, overrides --bump
    #[arg(long, value_name = "X.Y.Z")]
    pub version: Option<String>,

    /// Set a workflow input, repeatable
    #[arg(short = 'f', long = "field", value_name = "KEY=VALUE")]
    pub fields: Vec<String>,

    /// Workflow to dispatch (default: the one named release)
    #[arg(short, long, value_name = "NAME")]
    pub workflow: Option<String>,

    /// Branch to run the workflow on (default: the default branch)
    #[arg(long = "ref", value_name = "BRANCH")]
    pub git_ref: Option<String>,

    /// Follow the run until it finishes
    #[arg(long)]
    pub watch: bool,

    /// Print the plan without dispatching
    #[arg(long, conflicts_with = "watch")]
    pub dry_run: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Bump {
    Patch,
    Minor,
    Major,
}

#[derive(Subcommand)]
pub enum IssueCommand {
    /// Wait for new comments on an issue, or for open issues with a label, then print them
    ///
    /// Exits 0 after printing what arrived, 3 when nothing arrived before the timeout, and 4 when
    /// GitHub stayed out of reach for --offline minutes. A pull request number works too.
    ///
    /// With --state, printing a comment does not acknowledge it. Restart with the --ack the
    /// command printed once the comments are handled, so a caller that stops first sees them
    /// again.
    Watch(IssueWatchArgs),
}

#[derive(Args)]
pub struct IssueWatchArgs {
    /// Issue or pull request to wait for comments on
    #[arg(value_name = "NUMBER", required_unless_present = "label")]
    pub number: Option<u64>,

    /// Wait for open issues with this label instead, repeated to require several
    #[arg(long, value_name = "LABEL", conflicts_with = "number")]
    pub label: Vec<String>,

    /// Skip issues that also have this label
    #[arg(long, value_name = "LABEL", requires = "label")]
    pub not_label: Vec<String>,

    /// Only comments whose body matches this regex
    #[arg(long = "match", value_name = "REGEX", conflicts_with = "label")]
    pub matches: Option<String>,

    /// Skip comments whose body matches this regex
    #[arg(long, value_name = "REGEX", conflicts_with = "label")]
    pub not_match: Option<String>,

    /// Remember progress under this name, so a restart picks up where the last one stopped
    #[arg(long, value_name = "KEY")]
    pub state: Option<String>,

    /// Acknowledge comments up to this ID before waiting
    #[arg(long, value_name = "ID", requires = "state", conflicts_with = "label")]
    pub ack: Option<u64>,

    /// Seconds between polls
    #[arg(long, value_name = "SECS", default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..))]
    pub interval: u64,

    /// Minutes to wait before giving up
    #[arg(long, value_name = "MINS", default_value_t = 110)]
    pub timeout: u64,

    /// Minutes GitHub may stay out of reach before the watch gives up, polled meanwhile with a
    /// pause that doubles from --interval up to 5 minutes
    #[arg(long, value_name = "MINS", default_value_t = 15)]
    pub offline: u64,
}

#[derive(Subcommand)]
pub enum RepoCommand {
    /// Wait for commits that add or change files on a branch, then print the files
    ///
    /// Exits 0 after printing the files, 3 when nothing arrived before the timeout, and 4 when
    /// GitHub stayed out of reach for --offline minutes.
    ///
    /// With --state, printing a file does not acknowledge it. Restart with the --ack the command
    /// printed once the files are handled, so a caller that stops first sees them again.
    Watch(RepoWatchArgs),
}

#[derive(Args)]
pub struct RepoWatchArgs {
    /// Branch to watch (default: the repo's default branch)
    #[arg(short, long)]
    pub branch: Option<String>,

    /// Only files under this path
    #[arg(long, value_name = "PATH")]
    pub path: Option<String>,

    /// Only files whose contents match this regex
    #[arg(long = "match", value_name = "REGEX")]
    pub matches: Option<String>,

    /// Skip files whose contents match this regex
    #[arg(long, value_name = "REGEX")]
    pub not_match: Option<String>,

    /// Only files with a `re:` line naming this path, the replies to that message, repeated to
    /// wait on replies to several
    #[arg(long = "re", value_name = "PATH")]
    pub re: Vec<String>,

    /// Remember progress under this name, so a restart picks up where the last one stopped
    #[arg(long, value_name = "KEY")]
    pub state: Option<String>,

    /// Acknowledge every commit up to this one before waiting
    #[arg(long, value_name = "SHA", requires = "state")]
    pub ack: Option<String>,

    /// Seconds between polls
    #[arg(long, value_name = "SECS", default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..))]
    pub interval: u64,

    /// Minutes to wait before giving up
    #[arg(long, value_name = "MINS", default_value_t = 110)]
    pub timeout: u64,

    /// Minutes GitHub may stay out of reach before the watch gives up, polled meanwhile with a
    /// pause that doubles from --interval up to 5 minutes
    #[arg(long, value_name = "MINS", default_value_t = 15)]
    pub offline: u64,
}

#[derive(Subcommand)]
pub enum DependabotCommand {
    /// Group open PRs into ready, needs rebase, and blocked, with lock-step sets
    List(DependabotListArgs),
    /// Approve and merge PRs, singly or as a lock-step group
    Merge(DependabotMergeArgs),
    /// Ask Dependabot to rebase PRs
    Rebase(DependabotRebaseArgs),
    /// List open security alerts with their fix PR
    Alerts(DependabotAlertsArgs),
    /// Dismiss alerts with a reason
    Dismiss(DependabotDismissArgs),
}

#[derive(Args)]
pub struct DependabotListArgs {
    /// Every repo in an organization
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,

    /// Only PRs that fix a security alert
    #[arg(long)]
    pub security: bool,
}

#[derive(Args)]
pub struct DependabotMergeArgs {
    /// PRs to merge (default: prompt for each ready PR)
    pub prs: Vec<u64>,

    /// Merge every ready PR without prompting
    #[arg(long, conflicts_with = "prs")]
    pub all_ready: bool,

    /// Enable auto-merge, and request a rebase where a PR conflicts
    #[arg(long)]
    pub auto: bool,

    /// Comment posted before approving
    #[arg(long, value_name = "TEXT")]
    pub comment: Option<String>,

    /// Merge method
    #[arg(long, value_enum, default_value_t = MergeMethod::Rebase)]
    pub method: MergeMethod,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Args)]
pub struct DependabotRebaseArgs {
    /// PRs to rebase (default: prompt)
    pub prs: Vec<u64>,

    /// Rebase every open Dependabot PR
    #[arg(long, conflicts_with = "prs")]
    pub all: bool,
}

#[derive(Args)]
pub struct DependabotAlertsArgs {
    /// Every repo in an organization
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

#[derive(Args)]
pub struct DependabotDismissArgs {
    /// Alert numbers
    #[arg(required = true)]
    pub alerts: Vec<u64>,

    /// Why the alert is being dismissed
    #[arg(long, value_enum)]
    pub reason: DismissReason,

    /// Dismissal comment
    #[arg(long, value_name = "TEXT")]
    pub comment: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum DismissReason {
    #[value(name = "fix_started")]
    FixStarted,
    #[value(name = "inaccurate")]
    Inaccurate,
    #[value(name = "no_bandwidth")]
    NoBandwidth,
    #[value(name = "not_used")]
    NotUsed,
    #[value(name = "tolerable_risk")]
    TolerableRisk,
}

#[derive(Args)]
pub struct RunnersArgs {
    /// Runners registered to an organization instead of the repo
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,

    /// List queued jobs even when no runner is busy, scanning every repo of an org
    #[arg(long)]
    pub queued: bool,

    /// Infer runners from recent jobs instead of the runners API, which needs admin access
    #[arg(long)]
    pub from_jobs: bool,

    /// Recent runs per repo to read when inferring runners from jobs
    #[arg(long, default_value_t = 30, value_name = "N")]
    pub runs: usize,

    /// Most recently pushed org repos to read when inferring runners from jobs
    #[arg(long, default_value_t = 10, value_name = "N")]
    pub repos: usize,
}

#[derive(Subcommand)]
pub enum WorktreeCommand {
    /// List worktrees with their PR, its state and checks, and uncommitted or unpushed work
    List,
    /// Remove worktrees of your PRs that merged or closed, and their local branches
    Prune(WorktreePruneArgs),
    /// Check one of your PRs out into a new worktree, or print the one it already has
    Add(WorktreeAddArgs),
}

#[derive(Args)]
pub struct WorktreeAddArgs {
    /// Number, URL, or owner/repo#N
    pub pr: String,

    /// Directory for the worktree (default: <repo>-pr-<N> beside the main worktree)
    pub dir: Option<PathBuf>,
}

#[derive(Args)]
pub struct WorktreePruneArgs {
    /// Print what would be removed without removing it
    #[arg(long)]
    pub dry_run: bool,

    /// Remove worktrees with uncommitted changes or unpushed commits too
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct NotifyArgs {
    #[command(subcommand)]
    pub command: Option<NotifyCommand>,

    #[command(flatten)]
    pub send: NotifySendArgs,
}

#[derive(Args)]
pub struct NotifySendArgs {
    /// Message to show
    #[arg(required_unless_present = "hook")]
    pub message: Option<String>,

    /// Read the message from the JSON a Claude Code or Codex hook gets on stdin
    #[arg(long, conflicts_with = "message")]
    pub hook: bool,

    /// Notification title
    #[arg(long, default_value = "gh arriba")]
    pub title: String,

    /// Line under the title (default: name of the git root or working directory)
    #[arg(long)]
    pub subtitle: Option<String>,

    /// A newer notification replaces an older one in its group (default: git root or working
    /// directory)
    #[arg(long)]
    pub group: Option<String>,

    /// Sound to play, such as Submarine on macOS
    #[arg(long)]
    pub sound: Option<String>,
}

#[derive(Subcommand)]
pub enum NotifyCommand {
    /// Poll your notifications and show a desktop notification for each new one, until stopped
    Watch(NotifyWatchArgs),
    /// Focus the window or tab a notification came from, run when it is clicked
    #[command(hide = true)]
    Focus {
        #[command(subcommand)]
        target: FocusTarget,
    },
}

/// Where a notification came from.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum FocusTarget {
    /// A Ghostty terminal, by its scripting ID
    Ghostty { id: String },
    /// A Terminal.app tab, by its TTY
    Terminal { tty: String },
    /// The VS Code window whose workspace folder holds this directory
    Vscode { dir: PathBuf },
    /// Any other app, by its bundle ID
    App { bundle: String },
}

#[derive(Subcommand)]
pub enum InboxCommand {
    /// Mark done the threads whose pull request or issue merged or closed, or whose repo is gone
    Clean(InboxCleanArgs),
}

#[derive(Args)]
pub struct InboxCleanArgs {
    /// List the threads that would be marked done without marking them
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct NotifyWatchArgs {
    /// Seconds between polls, never less than GitHub asks for
    #[arg(long, value_name = "SECS", default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    pub interval: u64,

    /// Only notifications sent for this reason, such as review_requested or mention, repeatable
    #[arg(long, value_name = "REASON")]
    pub reason: Vec<String>,

    /// Only threads you take part in or are mentioned in
    #[arg(long)]
    pub participating: bool,

    /// Also notify for what is already unread at start
    #[arg(long)]
    pub all: bool,

    /// Start this watch, with these options, now and at every login (macOS and Linux)
    #[arg(long, conflicts_with_all = ["all", "uninstall"])]
    pub install: bool,

    /// Stop the watch that starts at login, and stop starting it
    #[arg(long)]
    pub uninstall: bool,
}

#[derive(Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: Option<ConfigCommand>,

    /// Gist name prefix (default: basename of project root)
    #[arg(long, global = true)]
    pub name: Option<String>,

    /// Root directory for relative paths (default: git root or cwd)
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,

    /// Create the gist as secret instead of public
    #[arg(long, global = true)]
    pub private: bool,
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Sync files with the gist (default when no command given)
    Sync {
        /// Files to sync, relative to root
        files: Vec<String>,
    },
    /// Report what sync would do without making any changes
    Status {
        /// Files to check, relative to root (default: all tracked files)
        files: Vec<String>,
    },
    /// Overwrite local files with the gist version, ignoring mtime
    Restore {
        /// Files to restore, relative to root (default: all tracked files)
        files: Vec<String>,
    },
    /// Add files to the tracked set and push them
    Add {
        /// Mark these files as specific to the current platform
        #[arg(long)]
        platform: bool,

        /// Files to add, relative to root
        files: Vec<String>,
    },
    /// Remove files from the tracked set and delete from gist
    Remove {
        /// Only remove the variant specific to the current platform
        #[arg(long)]
        platform: bool,

        /// Files to remove, relative to root
        files: Vec<String>,
    },
    /// Remove gist files not listed in the manifest
    Cleanup,
    /// Delete the entire config gist
    Delete,
    /// Open the project's gist in a web browser
    Open,
}

/// Parse a `--grep` pattern, reporting a bad one as a usage error.
fn parse_regex(s: &str) -> Result<regex::Regex, String> {
    regex::Regex::new(s).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_take_a_number_and_unit() {
        assert_eq!(parse_age("30m"), Ok(chrono::TimeDelta::minutes(30)));
        assert_eq!(parse_age("12h"), Ok(chrono::TimeDelta::hours(12)));
        assert_eq!(parse_age("7d"), Ok(chrono::TimeDelta::days(7)));
        assert_eq!(parse_age("2w"), Ok(chrono::TimeDelta::weeks(2)));
        assert!(parse_age("7").is_err());
        assert!(parse_age("d").is_err());
        assert!(parse_age("7y").is_err());
    }

    #[test]
    fn retry_takes_mine_or_all_but_not_both() {
        let parse = |flags: &[&str]| {
            let argv = ["gh-arriba", "pr", "retry"].iter().chain(flags);
            Cli::try_parse_from(argv).map(|_| ()).map_err(|e| e.kind())
        };
        assert!(parse(&["--all"]).is_ok());
        assert!(parse(&["--mine"]).is_ok());
        assert_eq!(
            parse(&["--all", "--mine"]),
            Err(clap::error::ErrorKind::ArgumentConflict)
        );
    }

    fn notify(args: &[&str]) -> Result<NotifyArgs, clap::error::ErrorKind> {
        let argv = ["gh-arriba"].iter().chain(args);
        match Cli::try_parse_from(argv).map_err(|e| e.kind())?.command {
            Command::Notify(args) => Ok(args),
            _ => unreachable!(),
        }
    }

    #[test]
    fn notify_takes_a_message_or_a_subcommand() {
        let sent = notify(&["notify", "--title", "CI", "tests done"]).unwrap();
        assert!(sent.command.is_none());
        assert_eq!(sent.send.message.as_deref(), Some("tests done"));
        assert_eq!(sent.send.title, "CI");

        let watch = notify(&["notifications", "watch", "--all"]).unwrap();
        assert!(matches!(watch.command, Some(NotifyCommand::Watch(w)) if w.all));

        // A message that names a subcommand needs a separator.
        let literal = notify(&["notify", "--", "watch"]).unwrap();
        assert_eq!(literal.send.message.as_deref(), Some("watch"));

        use clap::error::ErrorKind;
        assert_eq!(
            notify(&["notify", "--title", "CI"]).err(),
            Some(ErrorKind::MissingRequiredArgument)
        );
        // A hook's message comes from stdin, so it takes no argument.
        let hooked = notify(&["notify", "--hook", "--title", "Codex"]).unwrap();
        assert!(hooked.send.hook && hooked.send.message.is_none());
        assert_eq!(
            notify(&["notify", "--hook", "done"]).err(),
            Some(ErrorKind::ArgumentConflict)
        );
        assert_eq!(
            notify(&["notify", "done", "watch"]).err(),
            Some(ErrorKind::ArgumentConflict)
        );
        // Repeating every unread thread at each login would bury the new ones.
        assert_eq!(
            notify(&["notify", "watch", "--install", "--all"]).err(),
            Some(ErrorKind::ArgumentConflict)
        );
        assert_eq!(
            notify(&["notify", "watch", "--install", "--uninstall"]).err(),
            Some(ErrorKind::ArgumentConflict)
        );
    }
}

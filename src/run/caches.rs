use anyhow::{Result, bail};
use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use super::watch;
use crate::api::Repo;
use crate::api::actions::{self, Cache};
use crate::api::pulls;
use crate::cli::CachesArgs;
use crate::context::Ctx;
use crate::{output, parallel, prompt};

/// GitHub's default cache limit per repository. Past it, the least recently used caches are evicted.
const LIMIT: u64 = 10 * 1024 * 1024 * 1024;

/// A `--ref` value as the full ref the cache API filters on: a full ref as given, a PR number as
/// its merge ref, or a branch name.
fn full_ref(s: &str) -> String {
    if s.starts_with("refs/") {
        s.to_string()
    } else if let Ok(n) = s.trim_start_matches('#').parse::<u64>() {
        format!("refs/pull/{n}/merge")
    } else {
        format!("refs/heads/{s}")
    }
}

/// The PR number of a `refs/pull/N/merge` ref.
fn pr_number(git_ref: &str) -> Option<u64> {
    git_ref
        .strip_prefix("refs/pull/")?
        .strip_suffix("/merge")?
        .parse()
        .ok()
}

/// A ref as people say it: `PR #N`, the branch name, or the ref itself.
fn short_ref(git_ref: &str) -> String {
    match pr_number(git_ref) {
        Some(n) => format!("PR #{n}"),
        None => git_ref
            .strip_prefix("refs/heads/")
            .unwrap_or(git_ref)
            .to_string(),
    }
}

/// A key without its trailing hash segments, so the caches one step saves group together, as
/// `Linux-cargo-9f86d081884c7d65` does under `Linux-cargo`.
fn key_family(key: &str) -> &str {
    let mut family = key;
    while let Some((head, last)) = family.rsplit_once(['-', '_'])
        && last.len() >= 8
        && last.chars().all(|c| c.is_ascii_hexdigit())
    {
        family = head;
    }
    family
}

/// The repo's caches, narrowed by `--ref` and `--key` on the server.
fn fetch(ctx: &Ctx, repo: &Repo, args: &CachesArgs) -> Result<Vec<Cache>> {
    let git_ref = args.git_ref.as_deref().map(full_ref);
    actions::caches(&ctx.client, repo, git_ref.as_deref(), args.key.as_deref())
}

/// Caches that share a ref or a key family.
#[derive(Debug, PartialEq, Serialize)]
struct Group {
    name: String,
    count: usize,
    size: u64,
    last_used: Option<DateTime<Utc>>,
}

/// Group caches by `name_of`, largest first.
fn group_by(caches: &[Cache], name_of: impl Fn(&Cache) -> String) -> Vec<Group> {
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for c in caches {
        let name = name_of(c);
        let g = groups.entry(name.clone()).or_insert(Group {
            name,
            count: 0,
            size: 0,
            last_used: None,
        });
        g.count += 1;
        g.size += c.size;
        g.last_used = g.last_used.max(c.last_accessed_at);
    }
    let mut groups: Vec<Group> = groups.into_values().collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.size));
    groups
}

#[derive(Serialize)]
struct Summary<'a> {
    repo: String,
    /// Size and count of every active cache in the repo, filtered or not.
    total_bytes: u64,
    total_count: u64,
    limit_bytes: u64,
    by_ref: Vec<Group>,
    by_key: Vec<Group>,
    caches: &'a [Cache],
}

pub fn run(ctx: &Ctx, args: CachesArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let caches = fetch(ctx, &repo, &args)?;
    if args.prune {
        return prune(ctx, &repo, &args, &caches);
    }
    let usage = actions::cache_usage(&ctx.client, &repo)?;
    let summary = Summary {
        repo: repo.to_string(),
        total_bytes: usage.active_caches_size_in_bytes,
        total_count: usage.active_caches_count,
        limit_bytes: LIMIT,
        by_ref: group_by(&caches, |c| short_ref(&c.git_ref)),
        by_key: group_by(&caches, |c| key_family(&c.key).to_string()),
        caches: &caches,
    };
    if ctx.json {
        return output::print_json(&summary);
    }
    let filtered = args.git_ref.is_some() || args.key.is_some();
    print!("{}", render(&summary, filtered, Utc::now()));
    Ok(())
}

fn render(s: &Summary, filtered: bool, now: DateTime<Utc>) -> String {
    let mut out = format!(
        "{}: {} use {} of the default {} limit ({}%)\n",
        s.repo,
        output::plural(s.total_count as usize, "cache"),
        output::size(s.total_bytes),
        output::size(s.limit_bytes),
        s.total_bytes * 100 / s.limit_bytes.max(1)
    );
    if filtered {
        let size: u64 = s.caches.iter().map(|c| c.size).sum();
        out.push_str(&format!(
            "{} match the filters, using {}\n",
            s.caches.len(),
            output::size(size)
        ));
    }
    for (title, groups) in [("By ref", &s.by_ref), ("By key", &s.by_key)] {
        if groups.is_empty() {
            continue;
        }
        let rows: Vec<Vec<String>> = groups
            .iter()
            .map(|g| {
                vec![
                    g.name.clone(),
                    output::plural(g.count, "cache"),
                    output::size(g.size),
                    g.last_used
                        .map(|t| format!("used {} ago", output::age(now - t)))
                        .unwrap_or_default(),
                ]
            })
            .collect();
        out.push_str(&format!("\n{title}:\n{}\n", output::table(&rows, "  ")));
    }
    out
}

/// Caches to delete: those unused for longer than `unused_for`, created longer ago than
/// `older_than`, and of a PR in `closed`, each only when given. Least recently used first.
fn prune_plan<'a>(
    caches: &'a [Cache],
    unused_for: Option<TimeDelta>,
    older_than: Option<TimeDelta>,
    closed: Option<&HashSet<u64>>,
    now: DateTime<Utc>,
) -> Vec<&'a Cache> {
    let past = |t: Option<DateTime<Utc>>, span: Option<TimeDelta>| {
        span.is_none_or(|span| t.is_some_and(|t| now - t > span))
    };
    let mut doomed: Vec<&Cache> = caches
        .iter()
        .filter(|c| past(c.last_accessed_at, unused_for) && past(c.created_at, older_than))
        .filter(|c| {
            closed.is_none_or(|prs| pr_number(&c.git_ref).is_some_and(|n| prs.contains(&n)))
        })
        .collect();
    doomed.sort_by_key(|c| c.last_accessed_at);
    doomed
}

/// The PRs with caches that are no longer open. A PR whose state can't be read is kept open.
fn closed_prs(ctx: &Ctx, repo: &Repo, caches: &[Cache]) -> HashSet<u64> {
    let prs: Vec<u64> = caches
        .iter()
        .filter_map(|c| pr_number(&c.git_ref))
        .collect::<BTreeSet<u64>>()
        .into_iter()
        .collect();
    let states = parallel::map(&prs, |n| pulls::pull_state(&ctx.client, repo, *n));
    prs.into_iter()
        .zip(states)
        .filter_map(|(n, state)| match state {
            Ok(s) => (s == "closed").then_some(n),
            Err(e) => {
                eprintln!("warning: kept the caches of PR #{n}, its state is unknown: {e:#}");
                None
            }
        })
        .collect()
}

#[derive(Serialize)]
struct Pruned<'a> {
    repo: String,
    freed_bytes: u64,
    dry_run: bool,
    caches: Vec<&'a Cache>,
    /// IDs that failed to delete.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failed: Vec<u64>,
}

/// Delete the caches the filters and pruning options select, after confirming.
fn prune(ctx: &Ctx, repo: &Repo, args: &CachesArgs, caches: &[Cache]) -> Result<()> {
    if args.git_ref.is_none()
        && args.key.is_none()
        && args.unused_for.is_none()
        && args.older_than.is_none()
        && !args.closed
    {
        bail!(
            "Pick the caches to prune with --unused-for, --older-than, --closed, --ref, or --key"
        );
    }
    let closed = if args.closed {
        Some(closed_prs(ctx, repo, caches))
    } else {
        None
    };
    let now = Utc::now();
    let plan = prune_plan(
        caches,
        args.unused_for,
        args.older_than,
        closed.as_ref(),
        now,
    );
    let mut report = Pruned {
        repo: repo.to_string(),
        freed_bytes: plan.iter().map(|c| c.size).sum(),
        dry_run: args.dry_run,
        caches: plan,
        failed: Vec::new(),
    };
    let say = |line: &str| watch::progress(ctx, line);
    if report.caches.is_empty() {
        say(&format!("{repo}: no caches to prune"));
        return if ctx.json {
            output::print_json(&report)
        } else {
            Ok(())
        };
    }
    let rows: Vec<Vec<String>> = report
        .caches
        .iter()
        .map(|c| {
            vec![
                c.key.clone(),
                short_ref(&c.git_ref),
                output::size(c.size),
                c.last_accessed_at
                    .map(|t| format!("used {} ago", output::age(now - t)))
                    .unwrap_or_default(),
            ]
        })
        .collect();
    say(&output::table(&rows, "  "));
    say(&format!(
        "{repo}: pruning {} frees {}",
        output::plural(report.caches.len(), "cache"),
        output::size(report.freed_bytes)
    ));

    if args.dry_run {
        return if ctx.json {
            output::print_json(&report)
        } else {
            Ok(())
        };
    }
    let question = format!("Delete {}?", output::plural(report.caches.len(), "cache"));
    if !prompt::confirm(&question, ctx.yes)? {
        say("Cancelled");
        return Ok(());
    }
    for c in &report.caches {
        if let Err(e) = actions::delete_cache(&ctx.client, repo, c.id) {
            eprintln!("failed to delete {} ({}): {e:#}", c.key, c.id);
            report.failed.push(c.id);
        }
    }
    report.freed_bytes = report
        .caches
        .iter()
        .filter(|c| !report.failed.contains(&c.id))
        .map(|c| c.size)
        .sum();
    if ctx.json {
        return output::print_json(&report);
    }
    println!(
        "Deleted {}, freeing {}",
        output::plural(report.caches.len() - report.failed.len(), "cache"),
        output::size(report.freed_bytes)
    );
    if !report.failed.is_empty() {
        bail!(
            "{} could not be deleted",
            output::plural(report.failed.len(), "cache")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use mockito::{Matcher, Server, ServerGuard};
    use serde_json::json;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn cache(id: u64, key: &str, git_ref: &str, size: u64, used: &str) -> Cache {
        Cache {
            id,
            key: key.to_string(),
            git_ref: git_ref.to_string(),
            size,
            created_at: Some(at("2026-10-01T00:00:00Z")),
            last_accessed_at: Some(at(used)),
        }
    }

    #[test]
    fn refs_in_both_directions() {
        assert_eq!(full_ref("main"), "refs/heads/main");
        assert_eq!(full_ref("feature/x"), "refs/heads/feature/x");
        assert_eq!(full_ref("42"), "refs/pull/42/merge");
        assert_eq!(full_ref("#42"), "refs/pull/42/merge");
        assert_eq!(full_ref("refs/tags/v1"), "refs/tags/v1");
        assert_eq!(short_ref("refs/pull/42/merge"), "PR #42");
        assert_eq!(short_ref("refs/heads/feature/x"), "feature/x");
        assert_eq!(short_ref("refs/tags/v1"), "refs/tags/v1");
        assert_eq!(pr_number("refs/pull/7/merge"), Some(7));
        assert_eq!(pr_number("refs/heads/7"), None);
    }

    #[test]
    fn key_families_drop_trailing_hashes() {
        assert_eq!(key_family("Linux-cargo-9f86d081884c7d65"), "Linux-cargo");
        assert_eq!(
            key_family("setup-python-Linux-3.12-pip-0123456789abcdef-fedcba98"),
            "setup-python-Linux-3.12-pip"
        );
        assert_eq!(key_family("node-modules-1700000000"), "node-modules");
        assert_eq!(key_family("ccache-gcc-13"), "ccache-gcc-13");
        assert_eq!(key_family("deadbeef00"), "deadbeef00");
    }

    #[test]
    fn groups_sum_and_sort_by_size() {
        let caches = vec![
            cache(
                1,
                "a-11111111",
                "refs/heads/main",
                100,
                "2026-10-20T00:00:00Z",
            ),
            cache(
                2,
                "a-22222222",
                "refs/pull/5/merge",
                300,
                "2026-10-21T00:00:00Z",
            ),
            cache(3, "b", "refs/heads/main", 50, "2026-10-22T00:00:00Z"),
        ];
        let by_ref = group_by(&caches, |c| short_ref(&c.git_ref));
        assert_eq!(
            by_ref
                .iter()
                .map(|g| (g.name.as_str(), g.count, g.size))
                .collect::<Vec<_>>(),
            vec![("PR #5", 1, 300), ("main", 2, 150)]
        );
        assert_eq!(by_ref[1].last_used, Some(at("2026-10-22T00:00:00Z")));
        let by_key = group_by(&caches, |c| key_family(&c.key).to_string());
        assert_eq!(
            by_key
                .iter()
                .map(|g| (g.name.as_str(), g.size))
                .collect::<Vec<_>>(),
            vec![("a", 400), ("b", 50)]
        );
    }

    #[test]
    fn renders_usage_and_groups() {
        let caches = vec![cache(
            1,
            "a-11111111",
            "refs/heads/main",
            1024 * 1024 * 1024,
            "2026-10-23T00:00:00Z",
        )];
        let summary = Summary {
            repo: "o/r".to_string(),
            total_bytes: 2 * 1024 * 1024 * 1024,
            total_count: 2,
            limit_bytes: LIMIT,
            by_ref: group_by(&caches, |c| short_ref(&c.git_ref)),
            by_key: group_by(&caches, |c| key_family(&c.key).to_string()),
            caches: &caches,
        };
        assert_eq!(
            render(&summary, true, at("2026-10-24T00:00:00Z")),
            "o/r: 2 caches use 2.0 GB of the default 10.0 GB limit (20%)\n\
             1 match the filters, using 1.0 GB\n\
             \n\
             By ref:\n  \
             main  1 cache  1.0 GB  used 1d ago\n\
             \n\
             By key:\n  \
             a  1 cache  1.0 GB  used 1d ago\n"
        );
    }

    #[test]
    fn prune_plan_needs_every_given_age() {
        let now = at("2026-10-24T00:00:00Z");
        let mut old_unused = cache(1, "a", "refs/heads/main", 10, "2026-10-01T00:00:00Z");
        old_unused.created_at = Some(at("2026-09-01T00:00:00Z"));
        let old_used = cache(2, "b", "refs/heads/main", 10, "2026-10-23T00:00:00Z");
        let mut new_unused = cache(3, "c", "refs/heads/main", 10, "2026-10-10T00:00:00Z");
        new_unused.created_at = Some(at("2026-10-10T00:00:00Z"));
        let all = vec![old_unused, old_used, new_unused];
        let ids = |plan: Vec<&Cache>| plan.iter().map(|c| c.id).collect::<Vec<_>>();
        assert_eq!(ids(prune_plan(&all, None, None, None, now)), vec![1, 3, 2]);
        assert_eq!(
            ids(prune_plan(&all, Some(TimeDelta::days(7)), None, None, now)),
            vec![1, 3]
        );
        assert_eq!(
            ids(prune_plan(
                &all,
                Some(TimeDelta::days(7)),
                Some(TimeDelta::days(20)),
                None,
                now
            )),
            vec![1]
        );
        assert_eq!(
            ids(prune_plan(&all, None, Some(TimeDelta::days(20)), None, now)),
            vec![1, 2]
        );
    }

    #[test]
    fn prune_plan_keeps_open_and_branch_caches_with_closed() {
        let now = at("2026-10-24T00:00:00Z");
        let all = vec![
            cache(1, "a", "refs/pull/5/merge", 10, "2026-10-20T00:00:00Z"),
            cache(2, "a", "refs/pull/6/merge", 10, "2026-10-20T00:00:00Z"),
            cache(3, "a", "refs/heads/main", 10, "2026-10-20T00:00:00Z"),
        ];
        let closed = HashSet::from([5]);
        let plan = prune_plan(&all, None, None, Some(&closed), now);
        assert_eq!(plan.iter().map(|c| c.id).collect::<Vec<_>>(), vec![1]);
    }

    fn ctx(server: &ServerGuard, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    fn args() -> CachesArgs {
        CachesArgs {
            git_ref: None,
            key: None,
            prune: false,
            unused_for: None,
            older_than: None,
            closed: false,
            dry_run: false,
        }
    }

    /// Caches of PRs 5 and 6 and of main, all last used a month ago.
    fn serve_caches(server: &mut ServerGuard) {
        let used = (Utc::now() - TimeDelta::days(30)).to_rfc3339();
        server
            .mock("GET", "/repos/o/r/actions/caches")
            .match_query(Matcher::Exact("per_page=100".into()))
            .with_body(
                json!({"actions_caches": [
                    {"id": 1, "key": "cargo-aaaaaaaa", "ref": "refs/pull/5/merge",
                     "size_in_bytes": 10, "last_accessed_at": used, "created_at": used},
                    {"id": 2, "key": "cargo-bbbbbbbb", "ref": "refs/pull/6/merge",
                     "size_in_bytes": 20, "last_accessed_at": used, "created_at": used},
                    {"id": 3, "key": "cargo-cccccccc", "ref": "refs/heads/main",
                     "size_in_bytes": 30, "last_accessed_at": used, "created_at": used},
                    {"id": 4, "key": "cargo-dddddddd", "ref": "refs/pull/7/merge",
                     "size_in_bytes": 40, "last_accessed_at": used, "created_at": used}
                ]})
                .to_string(),
            )
            .create();
    }

    #[test]
    fn summary_with_filters_sent_to_the_server() {
        let mut server = Server::new();
        let filtered = server
            .mock("GET", "/repos/o/r/actions/caches")
            .match_query(Matcher::Exact(
                "ref=refs%2Fpull%2F5%2Fmerge&key=cargo&per_page=100".into(),
            ))
            .with_body(r#"{"actions_caches": [{"id": 1, "ref": "refs/pull/5/merge"}]}"#)
            .expect(2)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/cache/usage")
            .with_body(r#"{"active_caches_size_in_bytes": 1024, "active_caches_count": 3}"#)
            .create();
        let mut a = args();
        a.git_ref = Some("5".to_string());
        a.key = Some("cargo".to_string());
        run(&ctx(&server, true), a).unwrap();
        let mut a = args();
        a.git_ref = Some("#5".to_string());
        a.key = Some("cargo".to_string());
        run(&ctx(&server, false), a).unwrap();
        filtered.assert();
    }

    #[test]
    fn prune_needs_a_selection() {
        let mut server = Server::new();
        serve_caches(&mut server);
        let mut a = args();
        a.prune = true;
        assert_eq!(
            run(&ctx(&server, true), a).unwrap_err().to_string(),
            "Pick the caches to prune with --unused-for, --older-than, --closed, --ref, or --key"
        );
    }

    #[test]
    fn prune_closed_deletes_only_closed_pr_caches() {
        let mut server = Server::new();
        serve_caches(&mut server);
        server
            .mock("GET", "/repos/o/r/pulls/5")
            .with_body(r#"{"state": "closed"}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/pulls/6")
            .with_body(r#"{"state": "open"}"#)
            .create();
        // A PR whose state can't be read keeps its caches.
        server
            .mock("GET", "/repos/o/r/pulls/7")
            .with_status(500)
            .create();
        let deleted = server
            .mock("DELETE", "/repos/o/r/actions/caches/1")
            .with_status(204)
            .create();
        let others = server
            .mock(
                "DELETE",
                Matcher::Regex("^/repos/o/r/actions/caches/[234]$".into()),
            )
            .expect(0)
            .create();

        // A dry run deletes nothing.
        let mut a = args();
        a.prune = true;
        a.closed = true;
        a.dry_run = true;
        run(&ctx(&server, true), a).unwrap();

        let mut a = args();
        a.prune = true;
        a.closed = true;
        run(&ctx(&server, false), a).unwrap();
        deleted.assert();
        others.assert();
    }

    #[test]
    fn prune_by_age_reports_failed_deletes() {
        let mut server = Server::new();
        serve_caches(&mut server);
        server
            .mock(
                "DELETE",
                Matcher::Regex("^/repos/o/r/actions/caches/[123]$".into()),
            )
            .with_status(204)
            .expect(3)
            .create();
        server
            .mock("DELETE", "/repos/o/r/actions/caches/4")
            .with_status(404)
            .create();
        let mut a = args();
        a.prune = true;
        a.unused_for = Some(TimeDelta::days(7));
        assert_eq!(
            run(&ctx(&server, false), a).unwrap_err().to_string(),
            "1 cache could not be deleted"
        );

        // Nothing is that old.
        let mut a = args();
        a.prune = true;
        a.older_than = Some(TimeDelta::days(365));
        run(&ctx(&server, true), a).unwrap();
    }
}

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::api::gists::GistClient;
use crate::config::manifest::{Manifest, ManifestFile, current_platform};
use crate::config::project::ProjectConfig;

/// Tolerance for comparing local mtime against the manifest timestamp.
/// Accounts for coarser filesystem mtime resolution (e.g. FAT's two-second
/// granularity) and minor clock drift between machines.
const MTIME_TOLERANCE_SECS: i64 = 1;

/// Scan files for secrets and bail if any are found.
fn check_secrets(config: &ProjectConfig, paths: &[String]) -> Result<()> {
    let patterns = secretscan::patterns::get_all_patterns();
    let mut all_findings = Vec::new();

    for path in paths {
        let full = config.root.join(path);
        let content = match fs::read_to_string(&full) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (line_num, line) in content.lines().enumerate() {
            for (name, regex) in patterns.iter() {
                if regex.is_match(line) {
                    all_findings.push(format!(
                        "  {}:{} [{}] {}",
                        path,
                        line_num + 1,
                        name,
                        line.trim()
                    ));
                }
            }
        }
    }

    if !all_findings.is_empty() {
        eprintln!("Potential secrets found:");
        for f in &all_findings {
            eprintln!("{f}");
        }
        anyhow::bail!(
            "Aborting — {} potential secret(s) detected. Review and remove them before pushing.",
            all_findings.len()
        );
    }
    Ok(())
}

/// Read the manifest from an existing gist.
fn read_manifest(
    client: &GistClient,
    gist_id: &str,
    config: &ProjectConfig,
) -> Result<Option<Manifest>> {
    let gist = client.get_gist(gist_id)?;
    let manifest_name = config.manifest_name();
    match gist.file_content(&manifest_name) {
        Some(yaml) => Ok(Manifest::from_yaml(yaml)),
        None => Ok(None),
    }
}

/// Check whether a gist is private (i.e. not public).
fn is_gist_private(client: &GistClient, gist_id: &str) -> Result<bool> {
    let gist = client.get_gist(gist_id)?;
    Ok(gist.is_private())
}

/// Print the gist URL, marking private gists.
fn print_gist_url(id: &str, private: bool) {
    let suffix = if private { " (private)" } else { "" };
    println!("Gist: https://gist.github.com/{id}{suffix}");
}

/// Read a local file's modification time as a UTC timestamp.
fn local_mtime(config: &ProjectConfig, path: &str) -> Option<DateTime<Utc>> {
    let full = config.root.join(path);
    let meta = fs::metadata(&full).ok()?;
    let mtime = meta.modified().ok()?;
    Some(DateTime::<Utc>::from(mtime))
}

/// Set a local file's modification time to the given UTC timestamp.
fn set_local_mtime(path: &Path, time: DateTime<Utc>) -> Result<()> {
    let system_time: std::time::SystemTime = time.into();
    let file = fs::File::options()
        .write(true)
        .open(path)
        .with_context(|| format!("Failed to open {} to set mtime", path.display()))?;
    file.set_modified(system_time)
        .with_context(|| format!("Failed to set mtime on {}", path.display()))?;
    Ok(())
}

/// Build the file map for creating a gist from the manifest's entries.
///
/// Keys each file by the entry's stored gist name so platform variants of the
/// same path stay distinct. Entries with no readable local file are skipped.
fn build_file_map(config: &ProjectConfig, manifest: &Manifest) -> HashMap<String, String> {
    let mut files = HashMap::new();
    files.insert(config.manifest_name(), manifest.to_yaml());
    for entry in &manifest.files {
        let full = config.root.join(&entry.path);
        if let Ok(content) = fs::read_to_string(&full) {
            files.insert(entry.gist.clone(), content);
        }
    }
    files
}

/// Remove gist files that are not in the expected set.
fn remove_stale_files(
    client: &GistClient,
    gist_id: &str,
    config: &ProjectConfig,
    manifest: &Manifest,
) -> Result<()> {
    let gist = client.get_gist(gist_id)?;
    let remote_files = gist.file_names();

    let mut expected: HashSet<String> = HashSet::new();
    expected.insert(config.manifest_name());
    for entry in &manifest.files {
        expected.insert(entry.gist.clone());
    }

    let mut deletions: HashMap<String, Option<String>> = HashMap::new();
    for name in &remote_files {
        if !expected.contains(name) {
            deletions.insert(name.clone(), None);
            println!("  removed stale file: {name}");
        }
    }

    if !deletions.is_empty() {
        client.update_files(gist_id, &deletions)?;
    }
    Ok(())
}

/// Resolve optional command-line file arguments into a focus set.
///
/// Files listed must already be tracked; adding new files is `gh arriba config add`.
/// Returns `None` when no files were given, meaning "all tracked files".
fn parse_focus(
    config: &ProjectConfig,
    manifest: &Manifest,
    arg_files: &[String],
) -> Result<Option<HashSet<String>>> {
    if arg_files.is_empty() {
        return Ok(None);
    }
    let tracked: HashSet<String> = manifest.files.iter().map(|f| f.path.clone()).collect();
    let mut set = HashSet::new();
    for f in arg_files {
        let norm = config.relative_path(f);
        if !tracked.contains(&norm) {
            anyhow::bail!("{norm} is not tracked. Use `gh arriba config add` to track new files.");
        }
        set.insert(norm);
    }
    Ok(Some(set))
}

/// The action sync would take for a single manifest entry.
enum Action {
    Push(DateTime<Utc>),
    Pull,
    InSync,
}

/// Classify each platform-relevant manifest entry into a sync action.
///
/// Skips entries for other platforms and, when `focus` is set, entries
/// outside the focus set. Returns the index into `manifest.files` alongside
/// the action so callers can mutate or report on the entry.
fn build_plan(
    config: &ProjectConfig,
    manifest: &Manifest,
    focus: &Option<HashSet<String>>,
) -> Vec<(usize, Action)> {
    let current = current_platform();
    let mut plan = Vec::new();
    for (idx, entry) in manifest.files.iter().enumerate() {
        let platform_match = entry.platform.is_none() || entry.platform.as_deref() == Some(current);
        if !platform_match {
            continue;
        }
        if let Some(focus) = focus
            && !focus.contains(&entry.path)
        {
            continue;
        }

        let action = match local_mtime(config, &entry.path) {
            None => Action::Pull,
            Some(local) => {
                let delta = (local - entry.updated_at).num_seconds();
                if delta > MTIME_TOLERANCE_SECS {
                    Action::Push(local)
                } else if delta < -MTIME_TOLERANCE_SECS {
                    Action::Pull
                } else {
                    Action::InSync
                }
            }
        };
        plan.push((idx, action));
    }
    plan
}

pub fn do_sync(
    client: &GistClient,
    config: &ProjectConfig,
    arg_files: &[String],
    gist_id: Option<&str>,
) -> Result<()> {
    // No gist yet: create one from the files supplied on the command line.
    let Some(id) = gist_id else {
        return create_new_gist(client, config, arg_files);
    };

    let mut manifest = read_manifest(client, id, config)?.context("No manifest found in gist")?;

    // Optionally narrow the sync to a specific subset.
    let focus = parse_focus(config, &manifest, arg_files)?;

    let plan = build_plan(config, &manifest, &focus);

    let mut push_indices: Vec<(usize, DateTime<Utc>)> = Vec::new();
    let mut pull_indices: Vec<usize> = Vec::new();
    let mut in_sync_count = 0usize;
    for (idx, action) in plan {
        match action {
            Action::Push(ts) => push_indices.push((idx, ts)),
            Action::Pull => pull_indices.push(idx),
            Action::InSync => in_sync_count += 1,
        }
    }

    // Pull first so a failed push doesn't leave local out of date.
    if !pull_indices.is_empty() {
        let gist = client.get_gist(id)?;
        for idx in &pull_indices {
            let entry = &manifest.files[*idx];
            let Some(content) = gist.file_content(&entry.gist) else {
                eprintln!("  warning: {} not found in gist", entry.gist);
                continue;
            };
            let target = config.root.join(&entry.path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create directory for {}", entry.path))?;
            }
            fs::write(&target, format!("{content}\n"))
                .with_context(|| format!("Failed to write {}", entry.path))?;
            set_local_mtime(&target, entry.updated_at)?;
            println!("  pulled {} -> {}", entry.gist, entry.path);
        }
    }

    if !push_indices.is_empty() {
        let push_paths: Vec<String> = push_indices
            .iter()
            .map(|(idx, _)| manifest.files[*idx].path.clone())
            .collect();
        check_secrets(config, &push_paths)?;

        for (idx, ts) in &push_indices {
            manifest.files[*idx].updated_at = *ts;
        }

        let mut updates: HashMap<String, Option<String>> = HashMap::new();
        updates.insert(config.manifest_name(), Some(manifest.to_yaml()));
        for (idx, _) in &push_indices {
            let entry = &manifest.files[*idx];
            let full = config.root.join(&entry.path);
            let content = fs::read_to_string(&full)
                .with_context(|| format!("Failed to read {}", entry.path))?;
            updates.insert(entry.gist.clone(), Some(content));
            println!("  pushed {}", entry.path);
        }
        client.update_files(id, &updates)?;
    }

    println!(
        "Summary: {} pushed, {} pulled, {} already in sync",
        push_indices.len(),
        pull_indices.len(),
        in_sync_count
    );
    print_gist_url(id, is_gist_private(client, id)?);

    Ok(())
}

/// Create a new gist from the files specified on the command line.
fn create_new_gist(
    client: &GistClient,
    config: &ProjectConfig,
    arg_files: &[String],
) -> Result<()> {
    if arg_files.is_empty() {
        anyhow::bail!("No files specified and no manifest found.");
    }

    let normalized: Vec<String> = arg_files.iter().map(|p| config.relative_path(p)).collect();
    let entries: Vec<ManifestFile> = normalized
        .iter()
        .map(|p| {
            let ts = local_mtime(config, p).unwrap_or_else(Utc::now);
            Manifest::entry(config, p, None, ts)
        })
        .collect();

    let manifest = Manifest::new(config, &entries);
    let local_files: Vec<String> = normalized
        .iter()
        .filter(|p| config.root.join(p).is_file())
        .cloned()
        .collect();

    if local_files.is_empty() {
        println!("No tracked config files found and no remote gist exists.");
        return Ok(());
    }

    check_secrets(config, &local_files)?;
    let files = build_file_map(config, &manifest);
    println!("Creating gist: {}", config.gist_description());
    let id = client.create_gist(&config.gist_description(), config.public, &files)?;
    println!(
        "Pushed {} file(s): {}",
        local_files.len(),
        local_files.join(" ")
    );
    print_gist_url(&id, !config.public);
    Ok(())
}

/// Report what `sync` would do, without touching local files or the gist.
pub fn do_status(
    client: &GistClient,
    config: &ProjectConfig,
    arg_files: &[String],
    gist_id: Option<&str>,
) -> Result<()> {
    let Some(id) = gist_id else {
        println!(
            "No gist exists for this project. `gh arriba config sync FILE...` would create one."
        );
        return Ok(());
    };

    let manifest = read_manifest(client, id, config)?.context("No manifest found in gist")?;
    let focus = parse_focus(config, &manifest, arg_files)?;
    let plan = build_plan(config, &manifest, &focus);

    let mut push = 0usize;
    let mut pull = 0usize;
    let mut in_sync = 0usize;
    for (idx, action) in &plan {
        let entry = &manifest.files[*idx];
        match action {
            Action::Push(_) => {
                println!("  push  {} (local newer)", entry.path);
                push += 1;
            }
            Action::Pull => {
                let reason = if local_mtime(config, &entry.path).is_none() {
                    "missing locally"
                } else {
                    "gist newer"
                };
                println!("  pull  {} ({reason})", entry.path);
                pull += 1;
            }
            Action::InSync => in_sync += 1,
        }
    }

    // Files present in the gist but absent from the manifest. `sync` leaves
    // these alone; `gh arriba config cleanup` removes them.
    let gist = client.get_gist(id)?;
    let mut expected: HashSet<String> = HashSet::new();
    expected.insert(config.manifest_name());
    for entry in &manifest.files {
        expected.insert(entry.gist.clone());
    }
    let mut stale = 0usize;
    for name in gist.file_names() {
        if !expected.contains(&name) {
            println!("  stale {name} (not in manifest, run `gh arriba config cleanup`)");
            stale += 1;
        }
    }

    if push == 0 && pull == 0 && stale == 0 {
        println!("Everything is in sync ({in_sync} file(s)).");
    } else {
        println!("Summary: {push} to push, {pull} to pull, {in_sync} in sync, {stale} stale");
    }
    print_gist_url(id, is_gist_private(client, id)?);

    Ok(())
}

pub fn do_restore(
    client: &GistClient,
    config: &ProjectConfig,
    arg_files: &[String],
    gist_id: Option<&str>,
) -> Result<()> {
    let id = gist_id.context("No gist found for this project")?;
    let manifest = read_manifest(client, id, config)?.context("No manifest found in gist")?;

    let focus = parse_focus(config, &manifest, arg_files)?;

    let current = current_platform();
    let targets: Vec<&ManifestFile> = manifest
        .files
        .iter()
        .filter(|entry| {
            let platform_match =
                entry.platform.is_none() || entry.platform.as_deref() == Some(current);
            let focus_match = focus.as_ref().is_none_or(|f| f.contains(&entry.path));
            platform_match && focus_match
        })
        .collect();

    if targets.is_empty() {
        println!("No files to restore.");
        return Ok(());
    }

    let gist = client.get_gist(id)?;
    let mut restored = 0usize;
    let mut in_sync = 0usize;
    for entry in &targets {
        // Skip when local mtime already matches the manifest timestamp.
        if let Some(local) = local_mtime(config, &entry.path)
            && (local - entry.updated_at).num_seconds().abs() <= MTIME_TOLERANCE_SECS
        {
            in_sync += 1;
            continue;
        }

        let Some(content) = gist.file_content(&entry.gist) else {
            eprintln!("  warning: {} not found in gist", entry.gist);
            continue;
        };
        let target = config.root.join(&entry.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory for {}", entry.path))?;
        }
        fs::write(&target, format!("{content}\n"))
            .with_context(|| format!("Failed to write {}", entry.path))?;
        set_local_mtime(&target, entry.updated_at)?;
        println!("  restored {} -> {}", entry.gist, entry.path);
        restored += 1;
    }

    println!("Summary: {restored} restored, {in_sync} already in sync");
    print_gist_url(id, is_gist_private(client, id)?);

    Ok(())
}

pub fn do_add(
    client: &GistClient,
    config: &ProjectConfig,
    files: &[String],
    platform_specific: bool,
    gist_id: Option<&str>,
) -> Result<()> {
    if files.is_empty() {
        anyhow::bail!("Usage: gh arriba config add FILE...");
    }

    let platform = if platform_specific {
        Some(current_platform())
    } else {
        None
    };

    // Start with existing manifest entries, then merge new ones.
    let mut entries: Vec<ManifestFile> = if let Some(id) = gist_id {
        read_manifest(client, id, config)?
            .map(|m| m.files)
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // An entry is identified by (path, platform): the same path may carry one
    // entry per platform. Newly created entries accumulate in `added`.
    let platform_str = platform.map(|s| s.to_string());
    let mut added: Vec<ManifestFile> = Vec::new();
    for f in files {
        let normalized = config.relative_path(f);

        // Already tracked for this exact platform scope, nothing to do.
        if entries
            .iter()
            .chain(added.iter())
            .any(|e| e.path == normalized && e.platform == platform_str)
        {
            println!("  skipped {normalized} (already tracked)");
            continue;
        }
        // A platform-agnostic entry and a platform-specific one for the same
        // path would both match on sync. Reject the mix instead. Variants for
        // different platforms never match together, so they may coexist.
        if entries
            .iter()
            .chain(added.iter())
            .any(|e| e.path == normalized && (e.platform.is_none() || platform_str.is_none()))
        {
            anyhow::bail!(
                "{normalized} is already tracked with a different platform scope. \
                 Run `gh arriba config remove {normalized}` first to change it."
            );
        }

        let ts = local_mtime(config, &normalized).unwrap_or_else(Utc::now);
        added.push(Manifest::entry(config, &normalized, platform, ts));
    }

    if added.is_empty() {
        return Ok(());
    }

    entries.extend(added.iter().cloned());
    let manifest = Manifest::new(config, &entries);
    let added_paths: Vec<String> = added.iter().map(|e| e.path.clone()).collect();
    check_secrets(config, &added_paths)?;

    if let Some(id) = gist_id {
        let mut updates: HashMap<String, Option<String>> = HashMap::new();
        updates.insert(config.manifest_name(), Some(manifest.to_yaml()));
        for entry in &added {
            let full = config.root.join(&entry.path);
            let suffix = entry
                .platform
                .as_deref()
                .map(|p| format!(" ({p})"))
                .unwrap_or_default();
            if full.is_file() {
                let content = fs::read_to_string(&full)
                    .with_context(|| format!("Failed to read {}", entry.path))?;
                updates.insert(entry.gist.clone(), Some(content));
                println!("  added {}{suffix}", entry.path);
            } else {
                println!("  skipped {} (not found locally)", entry.path);
            }
        }
        client.update_files(id, &updates)?;
        print_gist_url(id, is_gist_private(client, id)?);
    } else {
        let file_map = build_file_map(config, &manifest);
        println!("Creating gist: {}", config.gist_description());
        let id = client.create_gist(&config.gist_description(), config.public, &file_map)?;
        for entry in &added {
            let suffix = entry
                .platform
                .as_deref()
                .map(|p| format!(" ({p})"))
                .unwrap_or_default();
            println!("  added {}{suffix}", entry.path);
        }
        print_gist_url(&id, !config.public);
    }

    Ok(())
}

pub fn do_remove(
    client: &GistClient,
    config: &ProjectConfig,
    files: &[String],
    platform_specific: bool,
    gist_id: Option<&str>,
) -> Result<()> {
    if files.is_empty() {
        anyhow::bail!("Usage: gh arriba config remove FILE...");
    }

    let id = gist_id.context("No gist found")?;

    let gist = client.get_gist(id)?;
    let manifest = gist
        .file_content(&config.manifest_name())
        .and_then(Manifest::from_yaml)
        .context("No manifest found in gist")?;
    let stored: HashSet<String> = gist.file_names().into_iter().collect();

    let remove_set: HashSet<String> = files.iter().map(|f| config.relative_path(f)).collect();

    // Without --platform, removing by path drops every variant of that path.
    // With it, only the variant scoped to the current platform is removed.
    // Either way, delete each matched entry's stored gist file if the gist has one.
    let current = current_platform();
    let (removed, remaining): (Vec<ManifestFile>, Vec<ManifestFile>) =
        manifest.files.into_iter().partition(|e| {
            remove_set.contains(&e.path)
                && (!platform_specific || e.platform.as_deref() == Some(current))
        });

    if removed.is_empty() {
        if platform_specific {
            anyhow::bail!("None of the given files are tracked for {current}.");
        }
        anyhow::bail!("None of the given files are tracked.");
    }

    let mut updates: HashMap<String, Option<String>> = HashMap::new();
    for entry in &removed {
        // GitHub rejects deleting a file the gist lacks with 422.
        if stored.contains(&entry.gist) {
            updates.insert(entry.gist.clone(), None);
        }
        let suffix = entry
            .platform
            .as_deref()
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        println!("  removed {}{suffix}", entry.path);
    }
    let new_manifest = Manifest::new(config, &remaining);
    updates.insert(config.manifest_name(), Some(new_manifest.to_yaml()));

    client.update_files(id, &updates)?;
    print_gist_url(id, gist.is_private());

    Ok(())
}

pub fn do_cleanup(
    client: &GistClient,
    config: &ProjectConfig,
    gist_id: Option<&str>,
) -> Result<()> {
    let id = gist_id.context("No gist found")?;
    let manifest = read_manifest(client, id, config)?.context("No manifest found in gist")?;
    remove_stale_files(client, id, config, &manifest)?;
    print_gist_url(id, is_gist_private(client, id)?);
    Ok(())
}

pub fn do_delete(client: &GistClient, gist_id: Option<&str>) -> Result<()> {
    let id = gist_id.context("No gist found")?;
    client.delete_gist(id)?;
    println!("Deleted gist: https://gist.github.com/{id}");
    Ok(())
}

pub fn do_open(gist_id: Option<&str>) -> Result<()> {
    let id = gist_id.context("No gist found")?;
    let url = format!("https://gist.github.com/{id}");
    println!("Opening {url}");

    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };

    std::process::Command::new(program)
        .arg(&url)
        .status()
        .with_context(|| format!("Failed to launch {program}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use mockito::{Mock, Server, ServerGuard};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    const OTHER_OS: &str = "plan9-test";

    /// A scratch project root, removed on drop.
    struct Root(PathBuf);

    impl Root {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("arriba-sync-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn config(&self) -> ProjectConfig {
            ProjectConfig {
                root: self.0.clone(),
                name: "proj".to_string(),
                public: true,
            }
        }

        /// Write a file and, when given, pin its mtime.
        fn write(&self, path: &str, content: &str, mtime: Option<DateTime<Utc>>) {
            let full = self.0.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, content).unwrap();
            if let Some(t) = mtime {
                set_local_mtime(&full, t).unwrap();
            }
        }

        fn read(&self, path: &str) -> Option<String> {
            fs::read_to_string(self.0.join(path)).ok()
        }

        fn mtime(&self, path: &str) -> Option<DateTime<Utc>> {
            local_mtime(&self.config(), path)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn ts(offset: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_700_000_000 + offset, 0).unwrap()
    }

    fn entry(c: &ProjectConfig, path: &str, platform: Option<&str>, at: i64) -> ManifestFile {
        Manifest::entry(c, path, platform, ts(at))
    }

    /// A gist response holding the manifest and the given files.
    fn gist_json(c: &ProjectConfig, entries: &[ManifestFile], files: &[(&str, &str)]) -> String {
        let mut map = serde_json::Map::new();
        map.insert(
            c.manifest_name(),
            json!({ "content": Manifest::new(c, entries).to_yaml() }),
        );
        for (name, content) in files {
            map.insert(name.to_string(), json!({ "content": content }));
        }
        json!({ "id": "g1", "public": false, "files": map }).to_string()
    }

    fn serve_gist(server: &mut ServerGuard, body: String) -> Mock {
        server.mock("GET", "/gists/g1").with_body(body).create()
    }

    /// Mock a write and record each request body it receives.
    fn capture(
        server: &mut ServerGuard,
        method: &str,
        path: &str,
    ) -> (Mock, Arc<Mutex<Vec<Value>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mock = server
            .mock(method, path)
            .match_request(move |req| {
                let body = req.utf8_lossy_body().unwrap();
                sink.lock()
                    .unwrap()
                    .push(serde_json::from_str(&body).unwrap_or(Value::Null));
                true
            })
            .with_body(r#"{"id": "new"}"#)
            .create();
        (mock, seen)
    }

    /// The `files` map of the last captured body.
    fn sent_files(seen: &Arc<Mutex<Vec<Value>>>) -> BTreeMap<String, Value> {
        let bodies = seen.lock().unwrap();
        let last = bodies.last().expect("no request captured");
        serde_json::from_value(last["files"].clone()).unwrap()
    }

    fn sent_manifest(c: &ProjectConfig, files: &BTreeMap<String, Value>) -> Manifest {
        let yaml = files[&c.manifest_name()]["content"].as_str().unwrap();
        Manifest::from_yaml(yaml).unwrap()
    }

    fn plan_labels(
        c: &ProjectConfig,
        m: &Manifest,
        focus: &Option<HashSet<String>>,
    ) -> Vec<String> {
        build_plan(c, m, focus)
            .into_iter()
            .map(|(idx, action)| {
                let label = match action {
                    Action::Push(_) => "push",
                    Action::Pull => "pull",
                    Action::InSync => "in-sync",
                };
                format!("{} {label}", m.files[idx].path)
            })
            .collect()
    }

    #[test]
    fn plan_compares_local_mtime_with_manifest_within_tolerance() {
        let root = Root::new("plan");
        let c = root.config();
        root.write("newer", "x", Some(ts(10)));
        root.write("older", "x", Some(ts(-10)));
        root.write("ahead-1s", "x", Some(ts(1)));
        root.write("behind-1s", "x", Some(ts(-1)));
        root.write("elsewhere", "x", Some(ts(10)));
        root.write("mine", "x", Some(ts(10)));
        let m = Manifest::new(
            &c,
            &[
                entry(&c, "newer", None, 0),
                entry(&c, "older", None, 0),
                entry(&c, "ahead-1s", None, 0),
                entry(&c, "behind-1s", None, 0),
                entry(&c, "missing", None, 0),
                entry(&c, "elsewhere", Some(OTHER_OS), 0),
                entry(&c, "mine", Some(current_platform()), 0),
            ],
        );
        assert_eq!(
            plan_labels(&c, &m, &None),
            vec![
                "newer push",
                "older pull",
                "ahead-1s in-sync",
                "behind-1s in-sync",
                "missing pull",
                "mine push",
            ]
        );

        let focus = Some(HashSet::from(["older".to_string()]));
        assert_eq!(plan_labels(&c, &m, &focus), vec!["older pull"]);
    }

    #[test]
    fn focus_accepts_tracked_paths_in_any_spelling() {
        let root = Root::new("focus");
        let c = root.config();
        let m = Manifest::new(&c, &[entry(&c, ".vscode/settings.json", None, 0)]);

        assert!(parse_focus(&c, &m, &[]).unwrap().is_none());

        let absolute = format!("{}/.vscode/settings.json", c.root.display());
        let focus = parse_focus(&c, &m, &["./.vscode/settings.json".into(), absolute])
            .unwrap()
            .unwrap();
        assert_eq!(focus, HashSet::from([".vscode/settings.json".to_string()]));

        let err = parse_focus(&c, &m, &["other.json".into()]).unwrap_err();
        assert!(
            err.to_string().contains("other.json is not tracked"),
            "{err}"
        );
    }

    #[test]
    fn sync_pulls_pushes_and_leaves_the_rest_alone() {
        let root = Root::new("sync");
        let c = root.config();
        root.write("push.txt", "local push", Some(ts(100)));
        root.write("pull.txt", "stale local", Some(ts(-100)));
        root.write("same.txt", "local same", Some(ts(0)));
        let entries = [
            entry(&c, "push.txt", None, 0),
            entry(&c, "pull.txt", None, 0),
            entry(&c, "same.txt", None, 0),
            entry(&c, ".vscode/settings.json", None, 0),
            entry(&c, "other.txt", Some(OTHER_OS), 0),
        ];
        let mut server = Server::new();
        serve_gist(
            &mut server,
            gist_json(
                &c,
                &entries,
                &[
                    ("proj_push.txt", "remote push"),
                    ("proj_pull.txt", "remote pull"),
                    ("proj_same.txt", "remote same"),
                    ("proj_.vscode_settings.json", "{}"),
                    ("proj_other[plan9-test].txt", "other"),
                ],
            ),
        );
        let (patch, seen) = capture(&mut server, "PATCH", "/gists/g1");
        let patch = patch.expect(1);
        let client = Client::for_tests(&server.url());

        do_sync(&GistClient::new(&client), &c, &[], Some("g1")).unwrap();
        patch.assert();

        // Pulled files get the gist content and the manifest's timestamp.
        assert_eq!(root.read("pull.txt").as_deref(), Some("remote pull\n"));
        assert_eq!(root.mtime("pull.txt"), Some(ts(0)));
        assert_eq!(root.read(".vscode/settings.json").as_deref(), Some("{}\n"));
        assert_eq!(root.mtime(".vscode/settings.json"), Some(ts(0)));
        // In-sync and other-platform files are untouched.
        assert_eq!(root.read("same.txt").as_deref(), Some("local same"));
        assert_eq!(root.read("other.txt"), None);

        // Only the newer file and the manifest are uploaded.
        let files = sent_files(&seen);
        let names: Vec<&str> = files.keys().map(String::as_str).collect();
        assert_eq!(names, vec![".proj-config-sync.yaml", "proj_push.txt"]);
        assert_eq!(files["proj_push.txt"]["content"], "local push");
        let manifest = sent_manifest(&c, &files);
        let stamps: Vec<(String, DateTime<Utc>)> = manifest
            .files
            .iter()
            .map(|f| (f.path.clone(), f.updated_at))
            .collect();
        assert_eq!(
            stamps,
            vec![
                ("push.txt".to_string(), ts(100)),
                ("pull.txt".to_string(), ts(0)),
                ("same.txt".to_string(), ts(0)),
                (".vscode/settings.json".to_string(), ts(0)),
                ("other.txt".to_string(), ts(0)),
            ]
        );
        assert_eq!(manifest.files[4].platform.as_deref(), Some(OTHER_OS));
    }

    #[test]
    fn sync_sends_nothing_when_nothing_is_newer_locally() {
        let root = Root::new("sync-quiet");
        let c = root.config();
        root.write("a.txt", "a", Some(ts(0)));
        let entries = [entry(&c, "a.txt", None, 0), entry(&c, "gone.txt", None, 0)];
        let mut server = Server::new();
        // gone.txt is in the manifest but not the gist: warn and carry on.
        serve_gist(&mut server, gist_json(&c, &entries, &[("proj_a.txt", "a")]));
        let patch = server.mock("PATCH", "/gists/g1").expect(0).create();
        let client = Client::for_tests(&server.url());

        do_sync(&GistClient::new(&client), &c, &[], Some("g1")).unwrap();
        patch.assert();
        assert_eq!(root.read("gone.txt"), None);
    }

    #[test]
    fn sync_refuses_to_push_secrets() {
        let root = Root::new("sync-secret");
        let c = root.config();
        root.write("env", "KEY=AKIAQWERTYUIOPASDFGH\n", Some(ts(100)));
        let entries = [entry(&c, "env", None, 0)];
        let mut server = Server::new();
        serve_gist(&mut server, gist_json(&c, &entries, &[("proj_env", "")]));
        let patch = server.mock("PATCH", "/gists/g1").expect(0).create();
        let client = Client::for_tests(&server.url());

        let err = do_sync(&GistClient::new(&client), &c, &[], Some("g1")).unwrap_err();
        assert!(err.to_string().contains("potential secret"), "{err}");
        patch.assert();
    }

    #[test]
    fn sync_needs_a_manifest_and_tracked_focus_files() {
        let root = Root::new("sync-errors");
        let c = root.config();
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/gists/g1")
            .with_body(r#"{"id": "g1", "files": {"unrelated.txt": {"content": "x"}}}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let gists = GistClient::new(&client);
        let err = do_sync(&gists, &c, &[], Some("g1")).unwrap_err();
        assert_eq!(err.to_string(), "No manifest found in gist");
        mock.remove();

        serve_gist(&mut server, gist_json(&c, &[entry(&c, "a", None, 0)], &[]));
        let err = do_sync(&gists, &c, &["b".into()], Some("g1")).unwrap_err();
        assert!(err.to_string().contains("b is not tracked"), "{err}");
    }

    #[test]
    fn first_sync_creates_a_gist_from_the_named_files() {
        let root = Root::new("create");
        let c = root.config();
        root.write(".vscode/settings.json", "{\"a\": 1}", Some(ts(5)));
        let mut server = Server::new();
        let (post, seen) = capture(&mut server, "POST", "/gists");
        let post = post.expect(1);
        let client = Client::for_tests(&server.url());

        let arg = format!("{}/.vscode/settings.json", c.root.display());
        do_sync(&GistClient::new(&client), &c, &[arg], None).unwrap();
        post.assert();

        let body = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(body["description"], "proj config-sync");
        assert_eq!(body["public"], true);
        let files = sent_files(&seen);
        assert_eq!(files["proj_.vscode_settings.json"]["content"], "{\"a\": 1}");
        let manifest = sent_manifest(&c, &files);
        assert_eq!(manifest.name, "proj");
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].path, ".vscode/settings.json");
        assert_eq!(manifest.files[0].gist, "proj_.vscode_settings.json");
        assert_eq!(manifest.files[0].updated_at, ts(5));
    }

    #[test]
    fn first_sync_without_local_files_creates_nothing() {
        let root = Root::new("create-empty");
        let c = root.config();
        let mut server = Server::new();
        let post = server.mock("POST", "/gists").expect(0).create();
        let client = Client::for_tests(&server.url());
        let gists = GistClient::new(&client);

        let err = do_sync(&gists, &c, &[], None).unwrap_err();
        assert_eq!(err.to_string(), "No files specified and no manifest found.");
        do_sync(&gists, &c, &["missing.txt".into()], None).unwrap();
        post.assert();
    }

    #[test]
    fn status_changes_nothing() {
        let root = Root::new("status");
        let c = root.config();
        root.write("newer.txt", "local", Some(ts(100)));
        root.write("older.txt", "local", Some(ts(-100)));
        let entries = [
            entry(&c, "newer.txt", None, 0),
            entry(&c, "older.txt", None, 0),
            entry(&c, "missing.txt", None, 0),
        ];
        let mut server = Server::new();
        serve_gist(
            &mut server,
            gist_json(
                &c,
                &entries,
                &[("proj_older.txt", "remote"), ("stale", "x")],
            ),
        );
        let patch = server.mock("PATCH", "/gists/g1").expect(0).create();
        let client = Client::for_tests(&server.url());
        let gists = GistClient::new(&client);

        do_status(&gists, &c, &[], Some("g1")).unwrap();
        do_status(&gists, &c, &["older.txt".into()], Some("g1")).unwrap();
        assert!(do_status(&gists, &c, &["nope".into()], Some("g1")).is_err());
        patch.assert();
        assert_eq!(root.read("older.txt").as_deref(), Some("local"));
        assert_eq!(root.read("missing.txt"), None);

        // Without a gist there is nothing to fetch.
        let get = server.mock("GET", "/gists/none").expect(0).create();
        do_status(&gists, &c, &[], None).unwrap();
        get.assert();
    }

    #[test]
    fn restore_overwrites_files_that_differ_from_the_manifest_time() {
        let root = Root::new("restore");
        let c = root.config();
        root.write("newer.txt", "local edits", Some(ts(100)));
        root.write("same.txt", "local same", Some(ts(0)));
        let entries = [
            entry(&c, "newer.txt", None, 0),
            entry(&c, "same.txt", None, 0),
            entry(&c, "dir/missing.txt", None, 0),
            entry(&c, "nogist.txt", None, 0),
            entry(&c, "other.txt", Some(OTHER_OS), 0),
        ];
        let mut server = Server::new();
        serve_gist(
            &mut server,
            gist_json(
                &c,
                &entries,
                &[
                    ("proj_newer.txt", "remote newer"),
                    ("proj_same.txt", "remote same"),
                    ("proj_dir_missing.txt", "remote missing"),
                    ("proj_other[plan9-test].txt", "other"),
                ],
            ),
        );
        let client = Client::for_tests(&server.url());
        let gists = GistClient::new(&client);

        do_restore(&gists, &c, &[], Some("g1")).unwrap();
        // Unlike sync, restore overwrites local changes.
        assert_eq!(root.read("newer.txt").as_deref(), Some("remote newer\n"));
        assert_eq!(root.mtime("newer.txt"), Some(ts(0)));
        assert_eq!(
            root.read("dir/missing.txt").as_deref(),
            Some("remote missing\n")
        );
        assert_eq!(root.read("same.txt").as_deref(), Some("local same"));
        assert_eq!(root.read("nogist.txt"), None);
        assert_eq!(root.read("other.txt"), None);
    }

    #[test]
    fn restore_limits_itself_to_the_focus_files() {
        let root = Root::new("restore-focus");
        let c = root.config();
        let entries = [entry(&c, "a", None, 0), entry(&c, "b", None, 0)];
        let mut server = Server::new();
        serve_gist(
            &mut server,
            gist_json(&c, &entries, &[("proj_a", "A"), ("proj_b", "B")]),
        );
        let client = Client::for_tests(&server.url());
        let gists = GistClient::new(&client);

        do_restore(&gists, &c, &["./b".into()], Some("g1")).unwrap();
        assert_eq!(root.read("a"), None);
        assert_eq!(root.read("b").as_deref(), Some("B\n"));

        let err = do_restore(&gists, &c, &[], None).unwrap_err();
        assert_eq!(err.to_string(), "No gist found for this project");
    }

    #[test]
    fn restore_with_only_other_platform_files_does_nothing() {
        let root = Root::new("restore-none");
        let c = root.config();
        let entries = [entry(&c, "a", Some(OTHER_OS), 0)];
        let mut server = Server::new();
        serve_gist(
            &mut server,
            gist_json(&c, &entries, &[("proj_a[plan9-test]", "A")]),
        );
        let client = Client::for_tests(&server.url());
        do_restore(&GistClient::new(&client), &c, &[], Some("g1")).unwrap();
        assert_eq!(root.read("a"), None);
    }

    #[test]
    fn add_appends_entries_and_uploads_their_content() {
        let root = Root::new("add");
        let c = root.config();
        root.write("new.txt", "new", Some(ts(7)));
        root.write("old.txt", "old", Some(ts(0)));
        let entries = [entry(&c, "old.txt", None, 0)];
        let mut server = Server::new();
        serve_gist(
            &mut server,
            gist_json(&c, &entries, &[("proj_old.txt", "old")]),
        );
        let (patch, seen) = capture(&mut server, "PATCH", "/gists/g1");
        let patch = patch.expect(1);
        let client = Client::for_tests(&server.url());

        let args = ["old.txt".into(), "./new.txt".into(), "later.txt".into()];
        do_add(&GistClient::new(&client), &c, &args, false, Some("g1")).unwrap();
        patch.assert();

        let files = sent_files(&seen);
        let names: Vec<&str> = files.keys().map(String::as_str).collect();
        // later.txt is tracked but has no content to upload yet.
        assert_eq!(names, vec![".proj-config-sync.yaml", "proj_new.txt"]);
        assert_eq!(files["proj_new.txt"]["content"], "new");
        let manifest = sent_manifest(&c, &files);
        let paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["old.txt", "new.txt", "later.txt"]);
        assert_eq!(manifest.files[1].updated_at, ts(7));
    }

    #[test]
    fn add_with_platform_tags_the_gist_name() {
        let root = Root::new("add-platform");
        let c = root.config();
        root.write("cfg.yaml", "x: 1", Some(ts(0)));
        let entries = [entry(&c, "cfg.yaml", Some(OTHER_OS), 0)];
        let mut server = Server::new();
        serve_gist(&mut server, gist_json(&c, &entries, &[]));
        let (_patch, seen) = capture(&mut server, "PATCH", "/gists/g1");
        let client = Client::for_tests(&server.url());

        do_add(
            &GistClient::new(&client),
            &c,
            &["cfg.yaml".into()],
            true,
            Some("g1"),
        )
        .unwrap();

        let files = sent_files(&seen);
        let tagged = format!("proj_cfg[{}].yaml", current_platform());
        assert_eq!(files[&tagged]["content"], "x: 1");
        let manifest = sent_manifest(&c, &files);
        let variants: Vec<(Option<&str>, &str)> = manifest
            .files
            .iter()
            .map(|f| (f.platform.as_deref(), f.gist.as_str()))
            .collect();
        assert_eq!(
            variants,
            vec![
                (Some(OTHER_OS), "proj_cfg[plan9-test].yaml"),
                (Some(current_platform()), tagged.as_str()),
            ]
        );
    }

    #[test]
    fn add_skips_tracked_files_and_rejects_mixed_platform_scopes() {
        let root = Root::new("add-skip");
        let c = root.config();
        root.write("a", "a", None);
        let entries = [entry(&c, "a", None, 0)];
        let mut server = Server::new();
        serve_gist(&mut server, gist_json(&c, &entries, &[("proj_a", "a")]));
        let patch = server.mock("PATCH", "/gists/g1").expect(0).create();
        let client = Client::for_tests(&server.url());
        let gists = GistClient::new(&client);

        do_add(&gists, &c, &["a".into()], false, Some("g1")).unwrap();
        let err = do_add(&gists, &c, &["a".into()], true, Some("g1")).unwrap_err();
        assert!(
            err.to_string().contains("different platform scope"),
            "{err}"
        );
        let err = do_add(&gists, &c, &[], false, Some("g1")).unwrap_err();
        assert!(err.to_string().starts_with("Usage:"), "{err}");
        patch.assert();
    }

    #[test]
    fn add_without_a_gist_creates_one() {
        let root = Root::new("add-create");
        let c = ProjectConfig {
            public: false,
            ..root.config()
        };
        root.write("a.txt", "a", Some(ts(3)));
        let mut server = Server::new();
        let (post, seen) = capture(&mut server, "POST", "/gists");
        let post = post.expect(1);
        let client = Client::for_tests(&server.url());

        do_add(
            &GistClient::new(&client),
            &c,
            &["a.txt".into()],
            false,
            None,
        )
        .unwrap();
        post.assert();
        let body = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(body["public"], false);
        let files = sent_files(&seen);
        assert_eq!(files["proj_a.txt"]["content"], "a");
        assert_eq!(sent_manifest(&c, &files).files[0].updated_at, ts(3));
    }

    #[test]
    fn add_refuses_secrets() {
        let root = Root::new("add-secret");
        let c = root.config();
        root.write("token", "ghp_abcdefghijklmnopqrstuvwxyz0123456789AB", None);
        let mut server = Server::new();
        let post = server.mock("POST", "/gists").expect(0).create();
        let client = Client::for_tests(&server.url());
        let err = do_add(
            &GistClient::new(&client),
            &c,
            &["token".into()],
            false,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("potential secret"), "{err}");
        post.assert();
    }

    fn platform_variants(c: &ProjectConfig) -> Vec<ManifestFile> {
        vec![
            entry(c, "cfg.yaml", Some(current_platform()), 0),
            entry(c, "cfg.yaml", Some(OTHER_OS), 0),
            entry(c, "keep.txt", None, 0),
        ]
    }

    /// Each entry's stored gist file, in the form `gist_json` takes.
    fn stored(entries: &[ManifestFile]) -> Vec<(&str, &str)> {
        entries.iter().map(|e| (e.gist.as_str(), "x")).collect()
    }

    #[test]
    fn remove_by_path_drops_every_platform_variant() {
        let root = Root::new("remove");
        let c = root.config();
        let entries = platform_variants(&c);
        let mut server = Server::new();
        serve_gist(&mut server, gist_json(&c, &entries, &stored(&entries)));
        let (_patch, seen) = capture(&mut server, "PATCH", "/gists/g1");
        let client = Client::for_tests(&server.url());

        do_remove(
            &GistClient::new(&client),
            &c,
            &["cfg.yaml".into()],
            false,
            Some("g1"),
        )
        .unwrap();

        let files = sent_files(&seen);
        assert_eq!(
            files[&format!("proj_cfg[{}].yaml", current_platform())],
            Value::Null
        );
        assert_eq!(files["proj_cfg[plan9-test].yaml"], Value::Null);
        let manifest = sent_manifest(&c, &files);
        let paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["keep.txt"]);
    }

    #[test]
    fn remove_with_platform_drops_only_this_platform() {
        let root = Root::new("remove-platform");
        let c = root.config();
        let entries = platform_variants(&c);
        let mut server = Server::new();
        serve_gist(&mut server, gist_json(&c, &entries, &stored(&entries)));
        let (_patch, seen) = capture(&mut server, "PATCH", "/gists/g1");
        let client = Client::for_tests(&server.url());

        do_remove(
            &GistClient::new(&client),
            &c,
            &["cfg.yaml".into()],
            true,
            Some("g1"),
        )
        .unwrap();

        let files = sent_files(&seen);
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[&format!("proj_cfg[{}].yaml", current_platform())],
            Value::Null
        );
        let manifest = sent_manifest(&c, &files);
        let left: Vec<Option<&str>> = manifest
            .files
            .iter()
            .map(|f| f.platform.as_deref())
            .collect();
        assert_eq!(left, vec![Some(OTHER_OS), None]);
    }

    #[test]
    fn remove_skips_deleting_files_the_gist_lacks() {
        let root = Root::new("remove-unstored");
        let c = root.config();
        let entries = vec![
            entry(&c, "gone.txt", None, 0),
            entry(&c, "keep.txt", None, 0),
        ];
        let mut server = Server::new();
        serve_gist(&mut server, gist_json(&c, &entries, &stored(&entries[1..])));
        let (_patch, seen) = capture(&mut server, "PATCH", "/gists/g1");
        let client = Client::for_tests(&server.url());

        do_remove(
            &GistClient::new(&client),
            &c,
            &["gone.txt".into()],
            false,
            Some("g1"),
        )
        .unwrap();

        // gone.txt has no stored file, so only the manifest is written.
        let files = sent_files(&seen);
        assert_eq!(files.keys().collect::<Vec<_>>(), vec![&c.manifest_name()]);
        let manifest = sent_manifest(&c, &files);
        let paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["keep.txt"]);
    }

    #[test]
    fn remove_fails_for_untracked_files() {
        let root = Root::new("remove-errors");
        let c = root.config();
        let mut server = Server::new();
        serve_gist(
            &mut server,
            gist_json(&c, &[entry(&c, "keep.txt", None, 0)], &[]),
        );
        let patch = server.mock("PATCH", "/gists/g1").expect(0).create();
        let client = Client::for_tests(&server.url());
        let gists = GistClient::new(&client);

        let err = do_remove(&gists, &c, &["x".into()], false, Some("g1")).unwrap_err();
        assert_eq!(err.to_string(), "None of the given files are tracked.");
        // keep.txt is tracked, but not for this platform specifically.
        let err = do_remove(&gists, &c, &["keep.txt".into()], true, Some("g1")).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "None of the given files are tracked for {}.",
                current_platform()
            )
        );
        assert_eq!(
            do_remove(&gists, &c, &["x".into()], false, None)
                .unwrap_err()
                .to_string(),
            "No gist found"
        );
        assert!(do_remove(&gists, &c, &[], false, Some("g1")).is_err());
        patch.assert();
    }

    #[test]
    fn cleanup_deletes_only_files_missing_from_the_manifest() {
        let root = Root::new("cleanup");
        let c = root.config();
        let entries = [entry(&c, "a", None, 0)];
        let mut server = Server::new();
        serve_gist(
            &mut server,
            gist_json(
                &c,
                &entries,
                &[("proj_a", "a"), ("old", "x"), ("proj_b", "b")],
            ),
        );
        let (patch, seen) = capture(&mut server, "PATCH", "/gists/g1");
        let patch = patch.expect(1);
        let client = Client::for_tests(&server.url());

        do_cleanup(&GistClient::new(&client), &c, Some("g1")).unwrap();
        patch.assert();
        let files = sent_files(&seen);
        assert_eq!(
            files,
            BTreeMap::from([
                ("old".to_string(), Value::Null),
                ("proj_b".to_string(), Value::Null)
            ])
        );
    }

    #[test]
    fn cleanup_with_nothing_stale_sends_nothing() {
        let root = Root::new("cleanup-none");
        let c = root.config();
        let entries = [entry(&c, "a", None, 0)];
        let mut server = Server::new();
        serve_gist(&mut server, gist_json(&c, &entries, &[("proj_a", "a")]));
        let patch = server.mock("PATCH", "/gists/g1").expect(0).create();
        let client = Client::for_tests(&server.url());
        let gists = GistClient::new(&client);

        do_cleanup(&gists, &c, Some("g1")).unwrap();
        patch.assert();
        assert_eq!(
            do_cleanup(&gists, &c, None).unwrap_err().to_string(),
            "No gist found"
        );
    }

    #[test]
    fn delete_removes_the_gist() {
        let mut server = Server::new();
        let mock = server
            .mock("DELETE", "/gists/g1")
            .with_status(204)
            .expect(1)
            .create();
        let client = Client::for_tests(&server.url());
        let gists = GistClient::new(&client);
        do_delete(&gists, Some("g1")).unwrap();
        mock.assert();
        assert_eq!(
            do_delete(&gists, None).unwrap_err().to_string(),
            "No gist found"
        );
        assert_eq!(do_open(None).unwrap_err().to_string(), "No gist found");
    }
}

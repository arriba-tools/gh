use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::project::ProjectConfig;

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub url: String,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub gist: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default = "epoch")]
    pub updated_at: DateTime<Utc>,
}

fn epoch() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).unwrap()
}

impl Manifest {
    pub fn new(config: &ProjectConfig, files: &[ManifestFile]) -> Self {
        Self {
            name: config.name.clone(),
            description: "Configuration files synced by config-sync".to_string(),
            repo: config.repo_url(),
            url: env!("CARGO_PKG_REPOSITORY").to_string(),
            files: files.to_vec(),
        }
    }

    /// Create a manifest entry for a path with an optional platform and timestamp.
    pub fn entry(
        config: &ProjectConfig,
        path: &str,
        platform: Option<&str>,
        updated_at: DateTime<Utc>,
    ) -> ManifestFile {
        ManifestFile {
            path: path.to_string(),
            gist: config.gist_filename(path, platform),
            platform: platform.map(|s| s.to_string()),
            updated_at,
        }
    }

    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(self).unwrap_or_default()
    }

    pub fn from_yaml(yaml: &str) -> Option<Self> {
        serde_yaml::from_str(yaml).ok()
    }
}

/// Returns the current platform name: "macos", "linux", or "windows".
pub fn current_platform() -> &'static str {
    std::env::consts::OS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn config() -> ProjectConfig {
        ProjectConfig {
            // Not a git checkout, so the manifest carries no repo URL.
            root: std::env::temp_dir(),
            name: "proj".to_string(),
            public: true,
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn entry_names_the_gist_file_and_keeps_the_platform() {
        let c = config();
        let e = Manifest::entry(&c, ".vscode/tasks.json", Some("macos"), at(5));
        assert_eq!(e.path, ".vscode/tasks.json");
        assert_eq!(e.gist, "proj_.vscode_tasks[macos].json");
        assert_eq!(e.platform.as_deref(), Some("macos"));
        assert_eq!(e.updated_at, at(5));
    }

    #[test]
    fn yaml_round_trips_entries() {
        let c = config();
        let files = [
            Manifest::entry(&c, "a.txt", None, at(1_700_000_000)),
            Manifest::entry(&c, "b.yaml", Some("linux"), at(1_700_000_100)),
        ];
        let manifest = Manifest {
            repo: Some("git@github.com:o/r.git".into()),
            ..Manifest::new(&c, &files)
        };
        let yaml = manifest.to_yaml();
        let back = Manifest::from_yaml(&yaml).unwrap();
        assert_eq!(back.name, "proj");
        assert_eq!(back.repo.as_deref(), Some("git@github.com:o/r.git"));
        assert_eq!(back.files.len(), 2);
        assert_eq!(back.files[0].gist, "proj_a.txt");
        assert_eq!(back.files[0].platform, None);
        assert_eq!(back.files[0].updated_at, at(1_700_000_000));
        assert_eq!(back.files[1].gist, "proj_b[linux].yaml");
        assert_eq!(back.files[1].platform.as_deref(), Some("linux"));
        assert_eq!(back.files[1].updated_at, at(1_700_000_100));
    }

    #[test]
    fn yaml_omits_absent_optional_fields() {
        let c = ProjectConfig {
            root: PathBuf::from("/nonexistent/arriba"),
            ..config()
        };
        let yaml = Manifest::new(&c, &[Manifest::entry(&c, "a", None, at(0))]).to_yaml();
        assert!(!yaml.contains("repo:"), "{yaml}");
        assert!(!yaml.contains("platform:"), "{yaml}");
    }

    #[test]
    fn older_manifests_without_timestamps_default_to_the_epoch() {
        let yaml = "name: proj\ndescription: d\nurl: u\nfiles:\n- path: a\n  gist: proj_a\n";
        let m = Manifest::from_yaml(yaml).unwrap();
        assert_eq!(m.repo, None);
        assert_eq!(m.files[0].updated_at, at(0));
    }

    #[test]
    fn invalid_yaml_is_none() {
        assert!(Manifest::from_yaml("files: [").is_none());
        assert!(Manifest::from_yaml("name: proj").is_none());
    }
}

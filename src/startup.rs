//! Starts a watch at login: a LaunchAgent on macOS, a systemd user service on Linux.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::context;
use crate::output::xml_escape;

/// A watch that can start at login, by the names launchd, systemd, and its log know it by.
pub struct Service {
    /// The LaunchAgent's label on macOS.
    pub label: &'static str,
    /// The user unit's file name on Linux.
    pub unit: &'static str,
    /// The log's file name under ~/Library/Logs on macOS.
    pub log: &'static str,
    /// The unit's Description on Linux.
    pub description: &'static str,
}

/// Only macOS and Linux have a service manager this registers with.
fn ensure_supported() -> Result<()> {
    match std::env::consts::OS {
        "macos" | "linux" => Ok(()),
        os => bail!("Starting the watch at login is not supported on {os} yet"),
    }
}

/// Register `service` to run the command line `argv` makes from this executable at every login,
/// and start it now.
pub fn install(service: &Service, argv: impl FnOnce(&Path) -> Vec<String>) -> Result<()> {
    ensure_supported()?;
    let exe = std::env::current_exe().context("Failed to find the gh-arriba executable")?;
    let argv = argv(&exe);
    // Services start with a bare PATH, so the current one lets the watch find gh and the notifier.
    let path = std::env::var("PATH").unwrap_or_default();
    let home = home()?;
    match std::env::consts::OS {
        "macos" => {
            let plist = launch_agent(&home, service);
            let log = home.join("Library/Logs").join(service.log);
            write(&plist, &plist_xml(service.label, &argv, &path, &log))?;
            let domain = gui_domain()?;
            // Unloading first makes a second install pick up its new options.
            let _ = launchctl(&["bootout", &format!("{domain}/{}", service.label)]);
            bootstrap(&domain, &plist)?;
            println!("Started the watch, and it starts again at every login");
            println!("  Agent: {}", plist.display());
            println!("  Log:   {}", log.display());
        }
        "linux" => {
            let unit = systemd_unit(&home, service);
            write(&unit, &unit_file(service.description, &argv, &path))?;
            systemctl(&["daemon-reload"])?;
            systemctl(&["enable", service.unit])?;
            // Restart rather than start, so a running watch picks up new options.
            systemctl(&["restart", service.unit])?;
            println!("Started the watch, and it starts again at every login");
            println!("  Unit: {}", unit.display());
            println!("  Log:  journalctl --user -u {}", service.unit);
        }
        _ => unreachable!("ensure_supported checked the OS"),
    }
    Ok(())
}

/// Stop `service` and remove its registration. False when there was none.
pub fn uninstall(service: &Service) -> Result<bool> {
    ensure_supported()?;
    let home = home()?;
    let file = match std::env::consts::OS {
        "macos" => {
            // Failing to unload only means it was not loaded.
            let domain = gui_domain()?;
            let _ = launchctl(&["bootout", &format!("{domain}/{}", service.label)]);
            launch_agent(&home, service)
        }
        "linux" => {
            let _ = systemctl(&["disable", "--now", service.unit]);
            systemd_unit(&home, service)
        }
        _ => unreachable!("ensure_supported checked the OS"),
    };
    if !file.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&file).with_context(|| format!("Failed to remove {}", file.display()))?;
    if std::env::consts::OS == "linux" {
        systemctl(&["daemon-reload"])?;
    }
    println!("Stopped the watch, and removed {}", file.display());
    Ok(true)
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("Failed to write {}", path.display()))
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not valid UTF-8", path.display()))
}

fn launch_agent(home: &Path, service: &Service) -> PathBuf {
    home.join(format!("Library/LaunchAgents/{}.plist", service.label))
}

/// The launchd domain of the logged-in user's session, where agents can show notifications.
fn gui_domain() -> Result<String> {
    Ok(format!("gui/{}", context::run("id", &["-u"], None)?))
}

fn launchctl(args: &[&str]) -> Result<String> {
    context::run("launchctl", args, None)
}

/// Load the agent, retrying while launchd finishes unloading an earlier copy.
fn bootstrap(domain: &str, plist: &Path) -> Result<()> {
    let plist = path_str(plist)?;
    let mut tries = 0;
    loop {
        match launchctl(&["bootstrap", domain, plist]) {
            Ok(_) => return Ok(()),
            Err(_) if tries < 10 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e),
        }
    }
}

/// A LaunchAgent labelled `label` that runs `argv` at login with `path`, restarting it a minute
/// after it exits, as when the first poll runs before the network is up.
fn plist_xml(label: &str, argv: &[String], path: &str, log: &Path) -> String {
    let args: String = argv
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml_escape(a)))
        .collect();
    let label = xml_escape(label);
    let path = xml_escape(path);
    let log = xml_escape(&log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>60</integer>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

fn systemd_unit(home: &Path, service: &Service) -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    config.join("systemd/user").join(service.unit)
}

fn systemctl(args: &[&str]) -> Result<String> {
    let args: Vec<&str> = ["--user"].into_iter().chain(args.iter().copied()).collect();
    context::run("systemctl", &args, None)
}

/// A user service described as `description` that runs `argv` with `path` from login, restarting
/// it a minute after it exits. The journal keeps its output.
fn unit_file(description: &str, argv: &[String], path: &str) -> String {
    let exec: Vec<String> = argv.iter().map(|a| exec_word(a)).collect();
    format!(
        "[Unit]
Description={description}

[Service]
ExecStart={}
Environment={}
Restart=always
RestartSec=60

[Install]
WantedBy=default.target
",
        exec.join(" "),
        quoted(&format!("PATH={}", path.replace('%', "%%")))
    )
}

/// One word of an ExecStart line. systemd expands % specifiers and $ variables there, so both are
/// doubled, and a word with spaces or quotes is quoted.
fn exec_word(word: &str) -> String {
    let word = word.replace('%', "%%").replace('$', "$$");
    if word.contains(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ';')) {
        quoted(&word)
    } else {
        word
    }
}

fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LABEL: &str = "com.github.nmoinvaz.gh-arriba.notify-watch";
    const UNIT: &str = "gh-arriba-notify-watch.service";

    #[test]
    fn launch_agents_escape_their_strings() {
        let argv = vec!["/Apps/A&B/gh-arriba".to_string(), "<watch>".to_string()];
        let xml = plist_xml(
            LABEL,
            &argv,
            "/opt/homebrew/bin:/usr/bin",
            Path::new("/u/l.log"),
        );
        assert!(xml.contains("    <string>/Apps/A&amp;B/gh-arriba</string>\n"));
        assert!(xml.contains("    <string>&lt;watch&gt;</string>\n  </array>"));
        assert!(xml.contains("<string>/opt/homebrew/bin:/usr/bin</string>"));
        assert!(xml.contains(&format!("<key>Label</key>\n  <string>{LABEL}</string>")));
        assert_eq!(xml.matches("<string>/u/l.log</string>").count(), 2);
    }

    #[test]
    fn each_service_has_its_own_files() {
        let service = Service {
            label: "com.example.watch",
            unit: "example-watch.service",
            log: "example-watch.log",
            description: "example watch",
        };
        assert_eq!(
            launch_agent(Path::new("/u"), &service),
            Path::new("/u/Library/LaunchAgents/com.example.watch.plist")
        );
        let unit = unit_file(service.description, &["/x/w".to_string()], "/usr/bin");
        assert!(unit.starts_with("[Unit]\nDescription=example watch\n"));
    }

    #[test]
    fn units_quote_and_escape_their_words() {
        assert_eq!(exec_word("--interval"), "--interval");
        assert_eq!(exec_word("/My Apps/gh-arriba"), r#""/My Apps/gh-arriba""#);
        assert_eq!(exec_word("100%"), "100%%");
        assert_eq!(exec_word("$HOME"), "$$HOME");
        assert_eq!(exec_word(r#"say "hi""#), r#""say \"hi\"""#);

        let argv = vec!["/x/gh arriba".to_string(), "notify".to_string()];
        let unit = unit_file("gh arriba notify watch", &argv, "/usr/bin:/a%b");
        assert!(unit.contains("\nExecStart=\"/x/gh arriba\" notify\n"));
        assert!(unit.contains("\nEnvironment=\"PATH=/usr/bin:/a%%b\"\n"));
        assert!(unit.contains("\nRestart=always\nRestartSec=60\n"));
        assert!(unit.ends_with("[Install]\nWantedBy=default.target\n"));
    }

    /// systemd checks the unit itself wherever it is installed, as on the Linux CI runners.
    #[test]
    fn units_pass_systemd_analyze_verify() {
        use std::process::Command;
        if Command::new("systemd-analyze")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = std::env::temp_dir().join(format!("gh-arriba-unit-{}", std::process::id()));
        // A path with a space exercises the quoting.
        let exe = dir.join("My Apps/gh-arriba");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::copy("/bin/sleep", &exe).unwrap();
        let unit = dir.join(UNIT);
        let argv = [exe.to_string_lossy().into_owned(), "100".to_string()];
        std::fs::write(
            &unit,
            unit_file("gh arriba notify watch", &argv, "/usr/bin:/a%b"),
        )
        .unwrap();

        let out = Command::new("systemd-analyze")
            .args(["verify", unit.to_str().unwrap()])
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

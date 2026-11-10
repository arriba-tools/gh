//! Native desktop notifications through the platform's own notifier, so long waits can end with a
//! banner instead of a terminal someone has to keep checking.

mod icon;

pub(crate) use icon::data_dir;

use std::path::{Path, PathBuf};

use crate::output::xml_escape;
use icon::Icon;

/// One desktop notification.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Note {
    pub title: String,
    pub body: String,
    /// Opened on click where the notifier supports it, otherwise appended to the body.
    pub url: Option<String>,
    /// Shown under the title where the notifier supports it.
    pub subtitle: Option<String>,
    /// Replaces the last note of the same group where the notifier supports it.
    pub group: Option<String>,
    /// Played with the note where the notifier supports it.
    pub sound: Option<String>,
    /// Program and arguments run on click where the notifier supports it.
    pub command: Option<Vec<String>>,
}

impl Note {
    pub fn new(title: impl Into<String>, body: impl Into<String>, url: Option<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            url,
            ..Self::default()
        }
    }

    /// The body with the URL on its own line, for notifiers that can't open links.
    fn body_with_url(&self) -> String {
        match &self.url {
            Some(url) if self.body.is_empty() => url.clone(),
            Some(url) => format!("{}\n{url}", self.body),
            None => self.body.clone(),
        }
    }
}

/// A notifier command: program, arguments, and environment variables.
#[derive(Debug, PartialEq)]
struct Invocation {
    program: String,
    args: Vec<String>,
    env: Vec<(&'static str, String)>,
}

/// Shows a toast through the WinRT API. The toast's XML comes from the environment so it needs no
/// quoting.
const WINDOWS_TOAST: &str = "\
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null
$xml = [Windows.Data.Xml.Dom.XmlDocument]::new()
$xml.LoadXml($env:ARRIBA_NOTE_XML)
$toast = [Windows.UI.Notifications.ToastNotification]::new($xml)
$app = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe'
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($app).Show($toast)";

/// The toast as the XML the WinRT API takes, with `icon` in place of PowerShell's logo. The text
/// comes first, as the schema's own examples order it.
fn toast_xml(note: &Note, icon: Option<&Path>) -> String {
    let image = icon
        .map(|png| {
            let src = xml_escape(&file_uri(png));
            format!(r#"<image placement="appLogoOverride" src="{src}"/>"#)
        })
        .unwrap_or_default();
    format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text>{image}</binding></visual></toast>",
        xml_escape(&note.title),
        xml_escape(&note.body_with_url())
    )
}

/// `path` as a file URI, which is how a toast names a local image.
fn file_uri(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let mut uri = String::from("file:///");
    for byte in path.trim_start_matches('/').bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                uri.push(byte as char);
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

/// `args` as one sh command line. terminal-notifier runs -execute through sh, so each argument is
/// single-quoted.
fn shell_line(args: &[String]) -> String {
    let quoted: Vec<String> = args
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', r"'\''")))
        .collect();
    quoted.join(" ")
}

/// The command that shows `note` on `os` with `icon`, preferring a notifier `has` reports as
/// installed.
fn invocation(
    os: &str,
    note: &Note,
    has: impl Fn(&str) -> bool,
    icon: &Icon,
) -> Option<Invocation> {
    match os {
        "macos" if has("terminal-notifier") => {
            let mut args = vec![
                "-title".into(),
                note.title.clone(),
                "-message".into(),
                note.body.clone(),
                "-group".into(),
                note.group.clone().unwrap_or_else(|| "gh-arriba".into()),
            ];
            if let Some(subtitle) = &note.subtitle {
                args.extend(["-subtitle".into(), subtitle.clone()]);
            }
            if let Some(sound) = &note.sound {
                args.extend(["-sound".into(), sound.clone()]);
            }
            if let Some(url) = &note.url {
                args.extend(["-open".into(), url.clone()]);
            }
            if let Some(command) = &note.command {
                args.extend(["-execute".into(), shell_line(command)]);
            }
            // The copy that carries the icon is terminal-notifier itself, taking the same options.
            let program = match &icon.app {
                Some(app) => app.to_string_lossy().into_owned(),
                None => "terminal-notifier".into(),
            };
            Some(Invocation {
                program,
                args,
                env: Vec::new(),
            })
        }
        // Passing the text as script arguments avoids escaping it into AppleScript.
        "macos" => Some(Invocation {
            program: "osascript".into(),
            args: vec![
                "-e".into(),
                "on run argv".into(),
                "-e".into(),
                "display notification (item 2 of argv) with title (item 1 of argv)".into(),
                "-e".into(),
                "end run".into(),
                note.title.clone(),
                note.body_with_url(),
            ],
            env: Vec::new(),
        }),
        "windows" => Some(Invocation {
            program: "powershell.exe".into(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                WINDOWS_TOAST.into(),
            ],
            env: vec![("ARRIBA_NOTE_XML", toast_xml(note, icon.png.as_deref()))],
        }),
        _ if has("notify-send") => {
            let mut args = vec!["--app-name=gh arriba".to_string()];
            if let Some(png) = &icon.png {
                args.push(format!("--icon={}", png.display()));
            }
            // A title or body starting with a dash would otherwise parse as an option.
            args.extend(["--".into(), note.title.clone(), note.body_with_url()]);
            Some(Invocation {
                program: "notify-send".into(),
                args,
                env: Vec::new(),
            })
        }
        _ => None,
    }
}

/// Where `program` is on the PATH.
#[cfg_attr(test, allow(dead_code))]
fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let file = dir.join(program);
        if file.is_file() {
            return Some(file);
        }
        let exe = file.with_extension("exe");
        (cfg!(windows) && exe.is_file()).then_some(exe)
    })
}

/// Whether `program` is on the PATH.
#[cfg_attr(test, allow(dead_code))]
fn on_path(program: &str) -> bool {
    find_on_path(program).is_some()
}

#[cfg(not(test))]
fn show(note: &Note) -> bool {
    let os = std::env::consts::OS;
    let icon = icon::prepare(os);
    if run(invocation(os, note, on_path, &icon)) {
        return true;
    }
    // A copy not yet, or not, allowed to notify leaves the note to the notifier that already is.
    icon.app.is_some() && run(invocation(os, note, on_path, &Icon::default()))
}

#[cfg(not(test))]
fn run(inv: Option<Invocation>) -> bool {
    use std::process::{Command, Stdio};
    let Some(inv) = inv else {
        return false;
    };
    Command::new(&inv.program)
        .args(&inv.args)
        .envs(inv.env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
thread_local! {
    static SENT: std::cell::RefCell<Vec<Note>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Tests record notes instead of showing them.
#[cfg(test)]
fn show(note: &Note) -> bool {
    SENT.with(|s| s.borrow_mut().push(note.clone()));
    true
}

/// The notes this test thread has sent, clearing them.
#[cfg(test)]
pub fn sent() -> Vec<Note> {
    SENT.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

/// Show `note`, or when no notifier works, ring the terminal bell and print it to stderr.
pub fn send(note: &Note) {
    if !show(note) {
        eprintln!(
            "\x07No desktop notifier found (install notify-send or terminal-notifier): {}: {}",
            note.title,
            note.body_with_url()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(url: Option<&str>) -> Note {
        Note::new("CI passed", "Fix inflate", url.map(String::from))
    }

    fn png() -> Icon {
        Icon {
            png: Some("/u/share/gh-arriba/icon.png".into()),
            app: None,
        }
    }

    #[test]
    fn macos_prefers_terminal_notifier_with_a_click_url() {
        let has = |p: &str| p == "terminal-notifier";
        let inv = invocation("macos", &note(Some("https://x/1")), has, &Icon::default()).unwrap();
        assert_eq!(inv.program, "terminal-notifier");
        assert!(inv.args.ends_with(&["-open".into(), "https://x/1".into()]));
    }

    #[test]
    fn macos_runs_the_copy_that_carries_the_icon() {
        let exe = "/u/Library/Application Support/gh-arriba/gh-arriba.app/Contents/MacOS/terminal-notifier";
        let copy = Icon {
            app: Some(exe.into()),
            png: None,
        };
        let inv = invocation("macos", &note(None), |p| p == "terminal-notifier", &copy).unwrap();
        assert_eq!(inv.program, exe);
        assert_eq!(inv.args[..2], ["-title", "CI passed"]);
    }

    #[test]
    fn terminal_notifier_takes_the_group_subtitle_and_sound() {
        let has = |p: &str| p == "terminal-notifier";
        let inv = invocation("macos", &note(None), has, &Icon::default()).unwrap();
        assert!(inv.args.ends_with(&["-group".into(), "gh-arriba".into()]));

        let styled = Note {
            subtitle: Some("zlib-ng".into()),
            group: Some("gh-arriba:/src/zlib-ng".into()),
            sound: Some("Submarine".into()),
            ..note(None)
        };
        let inv = invocation("macos", &styled, has, &Icon::default()).unwrap();
        assert_eq!(
            inv.args[4..],
            [
                "-group",
                "gh-arriba:/src/zlib-ng",
                "-subtitle",
                "zlib-ng",
                "-sound",
                "Submarine"
            ]
        );
    }

    #[test]
    fn terminal_notifier_runs_the_click_command_through_sh() {
        let clicked = Note {
            command: Some(vec!["/Apps/gh arriba".into(), "it's".into()]),
            ..note(None)
        };
        let has = |p: &str| p == "terminal-notifier";
        let inv = invocation("macos", &clicked, has, &Icon::default()).unwrap();
        assert!(
            inv.args
                .ends_with(&["-execute".into(), r"'/Apps/gh arriba' 'it'\''s'".into()])
        );
    }

    #[test]
    fn macos_falls_back_to_osascript_with_text_as_arguments() {
        let inv = invocation("macos", &note(Some("https://x/1")), |_| false, &png()).unwrap();
        assert_eq!(inv.program, "osascript");
        assert_eq!(
            inv.args[inv.args.len() - 2..],
            [
                "CI passed".to_string(),
                "Fix inflate\nhttps://x/1".to_string()
            ]
        );
    }

    #[test]
    fn linux_uses_notify_send_when_installed() {
        let has = |p: &str| p == "notify-send";
        let inv = invocation("linux", &note(None), has, &Icon::default()).unwrap();
        assert_eq!(inv.program, "notify-send");
        assert_eq!(inv.args[1..], ["--", "CI passed", "Fix inflate"]);
        let inv = invocation("linux", &note(None), has, &png()).unwrap();
        assert_eq!(inv.args[1..3], ["--icon=/u/share/gh-arriba/icon.png", "--"]);
        assert_eq!(invocation("linux", &note(None), |_| false, &png()), None);
    }

    #[test]
    fn windows_passes_the_toast_through_the_environment() {
        let inv = invocation(
            "windows",
            &note(Some("https://x/1")),
            |_| false,
            &Icon::default(),
        )
        .unwrap();
        assert_eq!(inv.program, "powershell.exe");
        assert!(!inv.args.iter().any(|a| a.contains("Fix inflate")));
        assert_eq!(
            inv.env,
            [(
                "ARRIBA_NOTE_XML",
                "<toast><visual><binding template=\"ToastGeneric\">\
                 <text>CI passed</text><text>Fix inflate\nhttps://x/1</text>\
                 </binding></visual></toast>"
                    .to_string()
            )]
        );
    }

    #[test]
    fn toasts_escape_their_text_and_name_the_icon_by_file_uri() {
        let icon = Path::new(r"C:\Users\Nathan Moin\AppData\Local\gh-arriba\icon.png");
        let note = Note::new("<b> & \"c\"", "a < b", None);
        assert_eq!(
            toast_xml(&note, Some(icon)),
            "<toast><visual><binding template=\"ToastGeneric\">\
             <text>&lt;b&gt; &amp; &quot;c&quot;</text><text>a &lt; b</text>\
             <image placement=\"appLogoOverride\" \
             src=\"file:///C:/Users/Nathan%20Moin/AppData/Local/gh-arriba/icon.png\"/>\
             </binding></visual></toast>"
        );
        assert_eq!(
            file_uri(Path::new("/tmp/é#.png")),
            "file:///tmp/%C3%A9%23.png"
        );
    }

    #[test]
    fn records_notes_in_tests() {
        send(&note(None));
        assert_eq!(sent(), [note(None)]);
        assert!(sent().is_empty());
    }
}

//! The GitHub mark on the notifications. A macOS notification's icon is its sender's, so there it
//! takes a copy of terminal-notifier.app that carries the mark, with its own permission prompt and
//! entry in System Settings > Notifications. notify-send and the Windows toast take the image.

#![cfg_attr(test, allow(dead_code))]

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::context;

/// assets/icon.svg rendered at 512 pixels.
const PNG: &[u8] = include_bytes!("../../assets/icon.png");

/// An identifier of its own is what gives the copy an icon of its own.
const BUNDLE_ID: &str = "com.github.nmoinvaz.gh-arriba.notify";
/// Named on the notification as its sender.
const APP_NAME: &str = "gh arriba";
const APP: &str = "gh-arriba.app";
const EXECUTABLE: &str = "Contents/MacOS/terminal-notifier";
/// Records what the copy was made from, so one made from an older terminal-notifier is remade.
const STAMP: &str = "Contents/Resources/gh-arriba-stamp";
/// Bumped when the copy changes in any other way, so copies made by older releases are remade.
const RECIPE: u32 = 1;

/// Where the icon is for the notifier that runs.
#[derive(Debug, Default, PartialEq)]
pub struct Icon {
    /// The executable of the terminal-notifier copy that carries it, on macOS.
    pub app: Option<PathBuf>,
    /// The image itself, elsewhere.
    pub png: Option<PathBuf>,
}

/// Put the icon in place for `os`, leaving out what cannot be.
pub fn prepare(os: &str) -> Icon {
    let env = |name: &str| std::env::var_os(name).map(PathBuf::from);
    let Some(dir) = data_dir(os, env) else {
        return Icon::default();
    };
    match os {
        "macos" => Icon {
            app: app_executable(&dir),
            png: None,
        },
        _ => Icon {
            png: png_file(&dir),
            app: None,
        },
    }
}

/// This user's gh-arriba data directory on `os`, from the variables `env` reads.
pub(crate) fn data_dir(os: &str, env: impl Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    let env = |name: &str| env(name).filter(|p| !p.as_os_str().is_empty());
    let base = match os {
        "macos" => env("HOME")?.join("Library/Application Support"),
        "windows" => env("LOCALAPPDATA")?,
        _ => env("XDG_DATA_HOME").or_else(|| Some(env("HOME")?.join(".local/share")))?,
    };
    Some(base.join("gh-arriba"))
}

/// The PNG in `dir`, written when missing or different.
fn png_file(dir: &Path) -> Option<PathBuf> {
    let file = dir.join("icon.png");
    if !std::fs::read(&file).is_ok_and(|bytes| bytes == PNG) {
        std::fs::create_dir_all(dir).ok()?;
        std::fs::write(&file, PNG).ok()?;
    }
    Some(file)
}

/// The executable of the terminal-notifier copy in `dir`, made from the installed one when there
/// is none, or when the one there was made from an older terminal-notifier or recipe.
fn app_executable(dir: &Path) -> Option<PathBuf> {
    let source = source_app()?;
    let app = dir.join(APP);
    let stamp = stamp(&source)?;
    let current = std::fs::read_to_string(app.join(STAMP)).is_ok_and(|s| s == stamp);
    (current || make_app(&source, &app, &stamp).is_ok()).then(|| app.join(EXECUTABLE))
}

/// The terminal-notifier.app that the terminal-notifier on PATH runs, through the symlink or the
/// script Homebrew installs in its place.
fn source_app() -> Option<PathBuf> {
    let real = std::fs::canonicalize(super::find_on_path("terminal-notifier")?).ok()?;
    app_of(&real).or_else(|| wrapped_app(&std::fs::read_to_string(&real).ok()?))
}

/// The .app that `exe` is the executable of.
fn app_of(exe: &Path) -> Option<PathBuf> {
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension()? == "app").then(|| app.to_path_buf())
}

/// The .app whose executable the shell script `script` runs.
fn wrapped_app(script: &str) -> Option<PathBuf> {
    let end = script.find(".app/Contents/MacOS/")? + ".app".len();
    let head = &script[..end];
    let start = head
        .rfind(['"', '\''])
        .or_else(|| head.rfind(char::is_whitespace))
        .map_or(0, |i| i + 1);
    Some(PathBuf::from(&head[start..]))
}

/// What a copy is made from: the recipe, and the source's executable by path, size, and
/// modification time.
fn stamp(source: &Path) -> Option<String> {
    let exe = source.join(EXECUTABLE);
    let meta = std::fs::metadata(&exe).ok()?;
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(format!(
        "{RECIPE}\n{}\n{}\n{}\n",
        exe.display(),
        meta.len(),
        modified.as_secs()
    ))
}

/// Make `app` from `source`, carrying the icon and `stamp` and signed as its own identity. It is
/// built beside `app` and moved into place once signed, so a failure leaves no half-made copy.
fn make_app(source: &Path, app: &Path, stamp: &str) -> Result<()> {
    let new = app.with_extension("app.new");
    let _ = std::fs::remove_dir_all(&new);
    if let Some(dir) = app.parent() {
        std::fs::create_dir_all(dir)?;
    }
    context::run("ditto", &[utf8(source)?, utf8(&new)?], None)?;
    let resources = new.join("Contents/Resources");
    std::fs::write(resources.join("icon.icns"), icns(PNG))?;
    let _ = std::fs::remove_file(resources.join("Terminal.icns"));
    std::fs::write(new.join(STAMP), stamp)?;
    let plist = new.join("Contents/Info.plist");
    for (key, value) in [
        ("CFBundleIconFile", "icon"),
        ("CFBundleName", APP_NAME),
        ("CFBundleIdentifier", BUNDLE_ID),
    ] {
        let args = ["-replace", key, "-string", value, utf8(&plist)?];
        context::run("plutil", &args, None)?;
    }
    context::run("codesign", &["--force", "--sign", "-", utf8(&new)?], None)?;
    let _ = std::fs::remove_dir_all(app);
    std::fs::rename(&new, app)?;
    Ok(())
}

fn utf8(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not valid UTF-8", path.display()))
}

/// An icns holding `png` as its 512 pixel image, which macOS scales to the sizes it shows.
fn icns(png: &[u8]) -> Vec<u8> {
    let element = (8 + png.len()) as u32;
    let mut out = Vec::with_capacity(16 + png.len());
    out.extend_from_slice(b"icns");
    out.extend_from_slice(&(8 + element).to_be_bytes());
    out.extend_from_slice(b"ic09");
    out.extend_from_slice(&element.to_be_bytes());
    out.extend_from_slice(png);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dirs_follow_each_platform() {
        let env = |name: &str| match name {
            "HOME" => Some(PathBuf::from("/u")),
            "LOCALAPPDATA" => Some(PathBuf::from(r"C:\u\AppData\Local")),
            _ => None,
        };
        assert_eq!(
            data_dir("macos", env),
            Some("/u/Library/Application Support/gh-arriba".into())
        );
        assert_eq!(
            data_dir("windows", env),
            Some(Path::new(r"C:\u\AppData\Local").join("gh-arriba"))
        );
        assert_eq!(
            data_dir("linux", env),
            Some("/u/.local/share/gh-arriba".into())
        );

        let xdg = |name: &str| match name {
            "XDG_DATA_HOME" => Some(PathBuf::from("/x")),
            _ => env(name),
        };
        assert_eq!(data_dir("linux", xdg), Some("/x/gh-arriba".into()));
        let empty_xdg = |name: &str| match name {
            "XDG_DATA_HOME" => Some(PathBuf::new()),
            _ => env(name),
        };
        assert_eq!(
            data_dir("linux", empty_xdg),
            Some("/u/.local/share/gh-arriba".into())
        );
        assert_eq!(data_dir("macos", |_| None), None);
    }

    #[test]
    fn finds_the_app_behind_the_binary_or_homebrews_script() {
        let exe = Path::new("/Applications/terminal-notifier.app/Contents/MacOS/terminal-notifier");
        assert_eq!(
            app_of(exe),
            Some("/Applications/terminal-notifier.app".into())
        );
        assert_eq!(app_of(Path::new("/usr/local/bin/terminal-notifier")), None);

        let script = "#!/bin/bash\nexec \"/opt/homebrew/Cellar/terminal-notifier/3.1.0/terminal-notifier.app/Contents/MacOS/terminal-notifier\" \"$@\"\n";
        assert_eq!(
            wrapped_app(script),
            Some("/opt/homebrew/Cellar/terminal-notifier/3.1.0/terminal-notifier.app".into())
        );
        assert_eq!(
            wrapped_app("exec '/Apps/My Tools/t.app/Contents/MacOS/t' \"$@\""),
            Some("/Apps/My Tools/t.app".into())
        );
        assert_eq!(
            wrapped_app("exec /x/t.app/Contents/MacOS/t"),
            Some("/x/t.app".into())
        );
        assert_eq!(wrapped_app("#!/bin/sh\necho hi\n"), None);
    }

    #[test]
    fn the_icns_wraps_the_png_with_its_lengths() {
        let out = icns(b"PNG!");
        assert_eq!(&out[..4], b"icns");
        assert_eq!(u32::from_be_bytes(out[4..8].try_into().unwrap()), 20);
        assert_eq!(&out[8..12], b"ic09");
        assert_eq!(u32::from_be_bytes(out[12..16].try_into().unwrap()), 12);
        assert_eq!(&out[16..], b"PNG!");
    }

    /// ic09 holds a 512 pixel PNG, so the embedded image must be one.
    #[test]
    fn the_embedded_icon_is_a_512_pixel_png() {
        assert_eq!(&PNG[..8], b"\x89PNG\r\n\x1a\n");
        let dim = |at: usize| u32::from_be_bytes(PNG[at..at + 4].try_into().unwrap());
        assert_eq!((dim(16), dim(20)), (512, 512));
    }
}

//! The file-chooser portal: protocol, and which backend the desktop uses.
//!
//! When Firefox asks "where should I save this?", it does not open a dialog
//! itself — it asks `xdg-desktop-portal`, which hands the request to whichever
//! *backend* is configured. Implementing that backend is what lets Fileman be
//! the dialog every application uses.
//!
//! Deliberately free of GTK. This module is compiled into the daemon as well
//! as the application, and the daemon's whole reason for being small is that
//! it never links a toolkit: it sits idle on D-Bus until a request arrives and
//! only then starts a process that can draw. See `src/bin/fileman-portal.rs`.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// Which of the three portal methods is being served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// Pick one or more existing files, or a folder.
    Open,
    /// Pick a name and folder to write to.
    Save,
    /// Pick a folder to write several given names into.
    SaveFiles,
}

/// One entry of the application's file-type filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rule {
    /// A shell glob, as the portal's type 0.
    Glob(String),
    /// A MIME type, as the portal's type 1.
    Mime(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filter {
    pub name: String,
    pub rules: Vec<Rule>,
}

/// Everything the dialog needs, as passed from the daemon to the UI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Request {
    pub kind_or_default: Option<Kind>,
    /// The application asking, for the window title. Empty when unknown.
    pub app_id: String,
    pub title: String,
    /// The asking window, as an exported handle — `wayland:…` or `x11:…`.
    ///
    /// Setting the dialog as that window's child is what makes a compositor
    /// treat it as a dialog: Hyprland floats a child toplevel and tiles a
    /// plain one, which is the whole difference between this feeling like a
    /// file dialog and feeling like another window of the file manager.
    pub parent_window: Option<String>,
    /// Label for the accept button, e.g. "Upload".
    pub accept_label: Option<String>,
    pub multiple: bool,
    /// Choose a folder rather than files.
    pub directory: bool,
    pub current_folder: Option<PathBuf>,
    /// Pre-filled name, for Save.
    pub current_name: Option<String>,
    /// Names to write, for SaveFiles.
    pub files: Vec<String>,
    pub filters: Vec<Filter>,
    pub current_filter: Option<usize>,
}

impl Request {
    pub fn kind(&self) -> Kind {
        self.kind_or_default.unwrap_or(Kind::Open)
    }
}

/// What the dialog decided.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Response {
    pub uris: Vec<String>,
    /// Index into [`Request::filters`] of the filter in force when accepted.
    pub current_filter: Option<usize>,
}

impl Response {
    pub fn cancelled() -> Self {
        Self::default()
    }

    pub fn is_cancelled(&self) -> bool {
        self.uris.is_empty()
    }
}

/// Whether a path matches a filter, for the dialog's own filtering.
///
/// Globs are matched against the file name only, which is what applications
/// mean by `*.png`, and case-insensitively, because a photo named `IMG.JPG`
/// is a JPEG whatever the shell thinks.
pub fn matches(filter: &Filter, path: &Path, content_type: &str) -> bool {
    filter.rules.iter().any(|rule| match rule {
        Rule::Mime(wanted) => mime_matches(content_type, wanted),
        Rule::Glob(pattern) => path
            .file_name()
            .map(|name| glob_matches(&pattern.to_lowercase(), &name.to_string_lossy().to_lowercase()))
            .unwrap_or(false),
    })
}

/// `image/*` has to match `image/png`; anything else is an exact comparison.
fn mime_matches(content_type: &str, wanted: &str) -> bool {
    match wanted.strip_suffix("/*") {
        Some(family) => content_type
            .split_once('/')
            .is_some_and(|(actual, _)| actual.eq_ignore_ascii_case(family)),
        None => content_type.eq_ignore_ascii_case(wanted),
    }
}

/// `*` and `?` globbing, which is all the portal's filters use.
///
/// Written out rather than pulled in: the patterns are short, and a glob
/// crate would be a dependency carried by the daemon as well.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let (pattern, name): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    // Classic two-pointer glob with backtracking on the last `*`.
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut matched) = (None, 0usize);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            matched = n;
            p += 1;
        } else if let Some(star) = star {
            p = star + 1;
            matched += 1;
            n = matched;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

// ── which backend the desktop uses ──────────────────────────────────────────

/// The D-Bus name this backend answers on.
pub const DBUS_NAME: &str = "org.freedesktop.impl.portal.desktop.fileman";

/// The name used in portal configuration files, which is the `.portal` file's
/// basename.
pub const PORTAL_NAME: &str = "fileman";

const FILE_CHOOSER: &str = "org.freedesktop.impl.portal.FileChooser";

/// The user's own portal preferences, which override the distribution's.
///
/// This one file is the whole answer to "how do we switch backends on every
/// distro": `xdg-desktop-portal` 1.18 and later read it before any of the
/// system-wide `*-portals.conf` files, whoever shipped those and whatever
/// desktop is running. Nothing needs root and nothing in `/usr` is touched.
pub fn user_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("xdg-desktop-portal/portals.conf")
}

/// Whether Fileman is the configured file chooser for this user.
pub fn is_enabled() -> bool {
    std::fs::read_to_string(user_config_path())
        .map(|text| chooser_backend(&text).as_deref() == Some(PORTAL_NAME))
        .unwrap_or(false)
}

/// Reads the configured FileChooser backend out of a portals.conf.
fn chooser_backend(text: &str) -> Option<String> {
    let mut in_preferred = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_preferred = line == "[preferred]";
            continue;
        }
        if !in_preferred {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim() == FILE_CHOOSER
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// Rewrites a portals.conf so the FileChooser line says `backend`, or is
/// removed when `backend` is `None`.
///
/// Every other line is preserved exactly. The file is shared with whatever
/// else the user has configured — a screenshot backend, a notification one —
/// and rewriting it wholesale would quietly undo their choices.
fn with_chooser(text: &str, backend: Option<&str>) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_preferred = false;
    let mut written = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // Leaving the section: this is the last chance to add the line.
            if in_preferred && !written && let Some(backend) = backend {
                out.push(format!("{FILE_CHOOSER}={backend}"));
                written = true;
            }
            in_preferred = trimmed == "[preferred]";
            out.push(line.to_string());
            continue;
        }
        let is_ours = in_preferred
            && trimmed.split_once('=').is_some_and(|(key, _)| key.trim() == FILE_CHOOSER);
        if is_ours {
            if let Some(backend) = backend {
                out.push(format!("{FILE_CHOOSER}={backend}"));
                written = true;
            }
            continue;
        }
        out.push(line.to_string());
    }

    if let Some(backend) = backend
        && !written
    {
        if !in_preferred {
            if !out.is_empty() && !out.last().is_some_and(|l| l.trim().is_empty()) {
                out.push(String::new());
            }
            out.push("[preferred]".to_string());
        }
        out.push(format!("{FILE_CHOOSER}={backend}"));
    }

    let mut text = out.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Makes Fileman the file chooser, or hands the job back.
///
/// Returns the path that was written, so the caller can say where to look.
pub fn set_enabled(enabled: bool) -> std::io::Result<PathBuf> {
    let path = user_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = with_chooser(&existing, enabled.then_some(PORTAL_NAME));

    // Written and renamed, so a portal reading the file mid-write never sees
    // half of it.
    let temp = path.with_extension("conf.tmp");
    {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(updated.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_the_way_applications_expect() {
        assert!(glob_matches("*.png", "photo.png"));
        assert!(!glob_matches("*.png", "photo.jpg"));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("img_??.jpg", "img_01.jpg"));
        assert!(!glob_matches("img_??.jpg", "img_1.jpg"));
        assert!(glob_matches("*.tar.*", "backup.tar.gz"));
        // Backtracking: the first `*` must give ground for the rest to match.
        assert!(glob_matches("*a*b", "xxaxxb"));
        assert!(!glob_matches("*a*b", "xxaxx"));
    }

    #[test]
    fn filters_match_on_name_and_mime_ignoring_case() {
        let images = Filter {
            name: "Images".into(),
            rules: vec![Rule::Glob("*.png".into()), Rule::Mime("image/*".into())],
        };
        assert!(matches(&images, Path::new("/a/photo.PNG"), "image/png"));
        assert!(matches(&images, Path::new("/a/scan.tiff"), "image/tiff"), "mime wildcard");
        assert!(!matches(&images, Path::new("/a/notes.txt"), "text/plain"));

        let exact = Filter { name: "PDF".into(), rules: vec![Rule::Mime("application/pdf".into())] };
        assert!(matches(&exact, Path::new("/a/x"), "application/pdf"));
        assert!(!matches(&exact, Path::new("/a/x"), "application/postscript"));
    }

    #[test]
    fn enabling_adds_the_line_to_an_empty_configuration() {
        let written = with_chooser("", Some("fileman"));
        assert_eq!(written, "[preferred]\norg.freedesktop.impl.portal.FileChooser=fileman\n");
        assert_eq!(chooser_backend(&written).as_deref(), Some("fileman"));
    }

    /// The file is shared with the user's other portal choices; switching the
    /// file chooser must not disturb any of them.
    #[test]
    fn other_preferences_survive_being_switched() {
        let original = "[preferred]\ndefault=hyprland;gtk\norg.freedesktop.impl.portal.Screenshot=hyprland\n";
        let enabled = with_chooser(original, Some("fileman"));
        assert!(enabled.contains("default=hyprland;gtk"));
        assert!(enabled.contains("Screenshot=hyprland"));
        assert_eq!(chooser_backend(&enabled).as_deref(), Some("fileman"));

        // And switching back leaves exactly what was there before.
        let disabled = with_chooser(&enabled, None);
        assert_eq!(disabled, original);
        assert_eq!(chooser_backend(&disabled), None);
    }

    #[test]
    fn an_existing_chooser_line_is_replaced_not_duplicated() {
        let original = "[preferred]\norg.freedesktop.impl.portal.FileChooser=gtk\n";
        let switched = with_chooser(original, Some("fileman"));
        assert_eq!(switched.matches("FileChooser").count(), 1);
        assert_eq!(chooser_backend(&switched).as_deref(), Some("fileman"));
    }

    /// Sections other than `[preferred]` must be left alone, including a
    /// FileChooser key that happens to live in one of them.
    #[test]
    fn only_the_preferred_section_is_touched() {
        let original = "[background]\norg.freedesktop.impl.portal.FileChooser=something\n";
        let switched = with_chooser(original, Some("fileman"));
        assert!(switched.contains("[background]\norg.freedesktop.impl.portal.FileChooser=something"));
        assert!(switched.contains("[preferred]"));
        assert_eq!(chooser_backend(&switched).as_deref(), Some("fileman"));
    }

    #[test]
    fn a_response_with_no_uris_is_a_cancellation() {
        assert!(Response::cancelled().is_cancelled());
        let picked = Response { uris: vec!["file:///tmp/a".into()], current_filter: None };
        assert!(!picked.is_cancelled());
    }
}

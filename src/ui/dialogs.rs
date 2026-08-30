//! Modal dialogs, written as async functions.
//!
//! Every one of these returns a future that resolves to the user's choice, so
//! call sites read as straight-line code (`if confirm(...).await { ... }`)
//! rather than as a web of response callbacks.

use std::path::Path;

use adw::prelude::*;
use gtk::glib;

use crate::{
    archive::CompressFormat,
    drives::Volume,
    fs::ops::ConflictChoice,
};

/// Asks a yes/no question. `destructive` styles the accept button in red.
pub async fn confirm(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    accept_label: &str,
    destructive: bool,
) -> bool {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("accept", accept_label);
    dialog.set_response_appearance(
        "accept",
        if destructive {
            adw::ResponseAppearance::Destructive
        } else {
            adw::ResponseAppearance::Suggested
        },
    );
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    dialog.choose_future(Some(parent)).await == "accept"
}

/// Shows an error. Fire-and-forget: there is nothing to decide.
pub fn show_error(parent: &impl IsA<gtk::Widget>, heading: &str, body: &str) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("ok", "OK");
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("ok");
    dialog.present(Some(parent));
}

/// Prompts for a single line of text, returning `None` if cancelled.
///
/// `select_stem` selects only the part before the extension, which is what you
/// want when renaming `photo.jpg` and almost never want otherwise.
pub async fn prompt_text(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: Option<&str>,
    initial: &str,
    accept_label: &str,
    select_stem: bool,
) -> Option<String> {
    let entry = gtk::Entry::builder()
        .text(initial)
        .activates_default(true)
        .margin_top(8)
        .build();

    if select_stem {
        let stem_len = Path::new(initial)
            .file_stem()
            .map(|s| s.to_string_lossy().chars().count() as i32)
            .unwrap_or(-1);
        entry.select_region(0, stem_len);
    } else {
        entry.select_region(0, -1);
    }

    let dialog = adw::AlertDialog::new(Some(heading), body);
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("accept", accept_label);
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("accept"));
    dialog.set_close_response("cancel");

    // An empty name can never be valid, so keep the accept button off until
    // there is something to accept.
    let dialog_for_entry = dialog.clone();
    entry.connect_changed(move |entry| {
        dialog_for_entry.set_response_enabled("accept", !entry.text().trim().is_empty());
    });
    dialog.set_response_enabled("accept", !initial.trim().is_empty());

    // The entry must have focus for the pre-selection to be visible.
    let entry_for_focus = entry.clone();
    glib::idle_add_local_once(move || {
        entry_for_focus.grab_focus_without_selecting();
    });

    let response = dialog.choose_future(Some(parent)).await;
    (response == "accept").then(|| entry.text().trim().to_string())
}

/// Asks what to do about an existing destination during a copy or move.
pub async fn resolve_conflict(
    parent: &impl IsA<gtk::Widget>,
    source: &Path,
    dest: &Path,
    apply_to_all_possible: bool,
) -> ConflictChoice {
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

    let body = format!(
        "“{name}” already exists in {}.\n\nReplacing it overwrites the existing file permanently.",
        dest.parent().map(|p| p.display().to_string()).unwrap_or_default()
    );

    let dialog = adw::AlertDialog::new(Some("A file with that name already exists"), Some(&body));

    // Show both files' details so the choice is informed rather than a guess.
    let details = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(8)
        .build();
    details.append(&file_summary_row("Existing file", dest));
    details.append(&file_summary_row("New file", source));

    let apply_all = gtk::CheckButton::builder()
        .label("Apply this to all remaining conflicts")
        .margin_top(10)
        .build();
    if apply_to_all_possible {
        details.append(&apply_all);
    }
    dialog.set_extra_child(Some(&details));

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("skip", "Skip");
    dialog.add_response("rename", "Keep Both");
    dialog.add_response("replace", "Replace");
    dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");

    let response = dialog.choose_future(Some(parent)).await;
    let all = apply_to_all_possible && apply_all.is_active();

    match (response.as_str(), all) {
        ("skip", false) => ConflictChoice::Skip,
        ("skip", true) => ConflictChoice::SkipAll,
        ("rename", false) => ConflictChoice::Rename,
        ("rename", true) => ConflictChoice::RenameAll,
        ("replace", false) => ConflictChoice::Replace,
        ("replace", true) => ConflictChoice::ReplaceAll,
        _ => ConflictChoice::Cancel,
    }
}

fn file_summary_row(caption: &str, path: &Path) -> gtk::Box {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    row.append(
        &gtk::Label::builder()
            .label(caption)
            .xalign(0.0)
            .css_classes(["caption", "heading"])
            .build(),
    );

    let detail = match std::fs::metadata(path) {
        Ok(md) => {
            let size = if md.is_dir() {
                "folder".to_string()
            } else {
                humansize::format_size(md.len(), humansize::DECIMAL)
            };
            let modified = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|d| chrono::Local.timestamp_opt(d.as_secs() as i64, 0).single())
                .map(|dt| dt.format("%d %b %Y %H:%M").to_string())
                .unwrap_or_default();
            format!("{size} · {modified}")
        }
        Err(_) => "unavailable".to_string(),
    };

    row.append(
        &gtk::Label::builder()
            .label(&detail)
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    row
}

/// Confirms a shred, spelling out that it is unrecoverable and — when the
/// storage makes overwriting unreliable — that the guarantee does not hold.
pub async fn confirm_shred(
    parent: &impl IsA<gtk::Widget>,
    paths: &[std::path::PathBuf],
    passes: u32,
    caveat: Option<(String, String)>,
) -> bool {
    let what = if paths.len() == 1 {
        format!(
            "“{}”",
            paths[0].file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        )
    } else {
        format!("{} items", paths.len())
    };

    let body = format!(
        "{what} will be overwritten {passes} time{} and then deleted.\n\n\
         This cannot be undone, and the files will not go to the Trash.",
        if passes == 1 { "" } else { "s" }
    );

    let dialog = adw::AlertDialog::new(Some("Shred permanently?"), Some(&body));

    if let Some((fstype, reason)) = caveat {
        // Being quiet about this would be the wrong call: the user is choosing
        // shredding specifically for a guarantee that this storage cannot give.
        let banner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(10)
            .css_classes(["card", "shred-warning"])
            .build();
        banner.append(
            &gtk::Label::builder()
                .label(format!("Overwriting may not be effective on {fstype}"))
                .xalign(0.0)
                .wrap(true)
                .margin_start(12)
                .margin_end(12)
                .margin_top(10)
                .css_classes(["heading"])
                .build(),
        );
        banner.append(
            &gtk::Label::builder()
                .label(format!(
                    "This is a {reason}.\n\nThe files will still be deleted, but treat the \
                     overwrite as best-effort. Full-disk encryption is the reliable answer here."
                ))
                .xalign(0.0)
                .wrap(true)
                .margin_start(12)
                .margin_end(12)
                .margin_bottom(10)
                .css_classes(["caption"])
                .build(),
        );
        dialog.set_extra_child(Some(&banner));
    }

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("shred", "Shred");
    dialog.set_response_appearance("shred", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    dialog.choose_future(Some(parent)).await == "shred"
}

/// What the user chose to do about an NTFS volume Windows left dirty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NtfsChoice {
    ReadOnly,
    Repair,
    Force,
    Cancel,
}

/// Offers the three real ways out of a refused NTFS mount.
///
/// Ordered by risk: read-only is always safe and is the default; repair clears
/// the dirty flag; force discards a hibernated Windows session.
pub async fn ntfs_recovery(
    parent: &impl IsA<gtk::Widget>,
    volume: &Volume,
    error: &str,
    hibernated: bool,
    ntfsfix_available: bool,
) -> NtfsChoice {
    let cause = if hibernated {
        "Windows is hibernated or was shut down with Fast Startup enabled, so it still \
         considers this volume in use."
    } else {
        "Windows did not unmount this volume cleanly, so it is marked as needing a check."
    };

    let body = format!(
        "“{}” could not be mounted for writing.\n\n{cause}\n\nWhat would you like to do?",
        volume.label
    );

    let dialog = adw::AlertDialog::new(Some("This NTFS volume is not clean"), Some(&body));

    let details = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(8)
        .build();

    details.append(&option_explainer(
        "Open read-only",
        "Always safe. You can browse and copy files off the volume, but not change anything.",
    ));
    if ntfsfix_available {
        details.append(&option_explainer(
            "Repair and mount",
            "Clears the dirty flag with ntfsfix and asks Windows to check the volume on its \
             next boot. Requires administrator access.",
        ));
    }
    if hibernated {
        details.append(&option_explainer(
            "Force read-write",
            "Deletes the Windows hibernation file. Your files are untouched, but the suspended \
             Windows session cannot be resumed afterwards.",
        ));
    }

    // Surface the underlying driver message rather than hiding it — it is often
    // the only clue when none of the three options work.
    let expander = gtk::Expander::builder().label("Technical details").margin_top(6).build();
    expander.set_child(Some(
        &gtk::Label::builder()
            .label(error)
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .css_classes(["caption", "dim-label", "monospace"])
            .build(),
    ));
    details.append(&expander);

    if !ntfsfix_available {
        details.append(
            &gtk::Label::builder()
                .label("Install the ntfsprogs package to enable repair.")
                .xalign(0.0)
                .wrap(true)
                .margin_top(4)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
    }

    dialog.set_extra_child(Some(&details));

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("readonly", "Open Read-Only");
    if ntfsfix_available {
        dialog.add_response("repair", "Repair and Mount");
    }
    if hibernated {
        dialog.add_response("force", "Force Read-Write");
        dialog.set_response_appearance("force", adw::ResponseAppearance::Destructive);
    }
    dialog.set_response_appearance("readonly", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("readonly"));
    dialog.set_close_response("cancel");

    match dialog.choose_future(Some(parent)).await.as_str() {
        "readonly" => NtfsChoice::ReadOnly,
        "repair" => NtfsChoice::Repair,
        "force" => NtfsChoice::Force,
        _ => NtfsChoice::Cancel,
    }
}

fn option_explainer(title: &str, body: &str) -> gtk::Box {
    let boxed = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    boxed.append(
        &gtk::Label::builder().label(title).xalign(0.0).css_classes(["heading", "caption"]).build(),
    );
    boxed.append(
        &gtk::Label::builder()
            .label(body)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    boxed
}

/// Prompts for an archive passphrase.
pub async fn ask_archive_password(
    parent: &impl IsA<gtk::Widget>,
    archive_name: &str,
) -> Option<String> {
    let entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .activates_default(true)
        .margin_top(8)
        .build();

    let dialog = adw::AlertDialog::new(
        Some("This archive is encrypted"),
        Some(&format!("Enter the password for “{archive_name}”.")),
    );
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("accept", "Extract");
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("accept"));
    dialog.set_close_response("cancel");

    let entry_for_focus = entry.clone();
    glib::idle_add_local_once(move || {
        entry_for_focus.grab_focus();
    });

    let response = dialog.choose_future(Some(parent)).await;
    (response == "accept").then(|| entry.text().to_string()).filter(|t| !t.is_empty())
}

/// Asks for an archive name and format when compressing a selection.
pub async fn ask_compress(
    parent: &impl IsA<gtk::Widget>,
    default_stem: &str,
) -> Option<(String, CompressFormat)> {
    let entry = gtk::Entry::builder().text(default_stem).activates_default(true).build();

    let formats = gtk::DropDown::from_strings(
        &CompressFormat::ALL.iter().map(|f| f.label()).collect::<Vec<_>>(),
    );

    let grid = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(8)
        .build();
    grid.append(&entry);
    grid.append(&formats);

    let dialog = adw::AlertDialog::new(Some("Compress"), Some("Choose a name and a format."));
    dialog.set_extra_child(Some(&grid));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("accept", "Create");
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("accept"));
    dialog.set_close_response("cancel");

    let response = dialog.choose_future(Some(parent)).await;
    if response != "accept" {
        return None;
    }

    let stem = entry.text().trim().to_string();
    if stem.is_empty() {
        return None;
    }
    let format = CompressFormat::ALL[formats.selected() as usize];
    Some((stem, format))
}

/// Summarises a finished job's failures without dumping a hundred lines.
pub fn show_job_errors(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    errors: &[(std::path::PathBuf, String)],
) {
    if errors.is_empty() {
        return;
    }

    const SHOWN: usize = 8;
    let mut body = String::new();
    for (path, error) in errors.iter().take(SHOWN) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        body.push_str(&format!("• {name}: {error}\n"));
    }
    if errors.len() > SHOWN {
        body.push_str(&format!("\n…and {} more.", errors.len() - SHOWN));
    }

    show_error(parent, heading, body.trim_end());
}

use chrono::TimeZone;

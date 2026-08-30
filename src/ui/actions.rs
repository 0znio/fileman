//! Window actions: everything reachable from a menu, a shortcut, or the
//! context menu, plus the operation handlers they invoke.
//!
//! Actions are registered on the window's `win.` group so they can be named
//! from `GMenu` models and given application-level accelerators in one place.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::{
    archive,
    config::{SortKey, Theme, ViewMode},
    drives::{MountError, Volume},
    fs::{FileEntry, ops, shred, trash},
    ui::{
        dialogs::{self, NtfsChoice},
        window::{Clip, Location, Window},
    },
};

impl Window {
    pub(crate) fn install_actions(self: &Rc<Self>) {

        // ── navigation ────────────────────────────────────────────────────
        self.simple("go-back", |this| this.go_back());
        self.simple("go-forward", |this| this.go_forward());
        self.simple("go-up", |this| this.go_up());
        self.simple("go-home", |this| {
            if let Some(home) = dirs::home_dir() {
                this.navigate_to(Location::Directory(home), true);
            }
        });
        self.simple("reload", |this| {
            this.reload();
            this.refresh_drives();
        });
        self.simple("edit-path", |this| this.pathbar.start_editing());
        self.simple("focus-search", |this| {
            this.search_bar.set_search_mode(true);
            this.search_entry.grab_focus();
        });

        // ── selection ─────────────────────────────────────────────────────
        self.simple("select-all", |this| this.view.select_all());
        self.simple("select-none", |this| this.view.select_none());
        self.simple("invert-selection", |this| this.view.invert_selection());

        // ── files ─────────────────────────────────────────────────────────
        self.simple("open", |this| {
            for entry in this.selected_entries() {
                this.activate_item(&entry);
            }
        });
        self.simple("open-with", |this| this.open_with());
        self.simple("new-folder", |this| this.create_new(true));
        self.simple("new-file", |this| this.create_new(false));
        self.simple("rename", |this| this.rename_selected());
        self.simple("copy", |this| this.copy_to_clipboard(false));
        self.simple("cut", |this| this.copy_to_clipboard(true));
        self.simple("paste", |this| this.paste());
        self.simple("trash", |this| this.delete_selected(false));
        self.simple("delete-permanently", |this| this.delete_selected(true));
        self.simple("shred", |this| this.shred_selected());
        self.simple("properties", |this| {
            let entries = this.selected_entries();
            if entries.is_empty() {
                // No selection means "tell me about this folder".
                if let Some(dir) = this.current_dir()
                    && let Some(entry) = current_dir_entry(&dir)
                {
                    crate::ui::properties::present(&this.widget(), &[entry]);
                }
                return;
            }
            crate::ui::properties::present(&this.widget(), &entries);
        });
        self.simple("copy-path", |this| this.copy_paths_as_text());
        // Also an action, not just a key controller, so the menu key works
        // everywhere and the menu itself is reachable for testing.
        self.simple("context-menu", |this| this.show_context_menu(8.0, 8.0));
        self.simple("open-terminal", |this| this.open_terminal());
        self.simple("add-favourite", |this| this.toggle_favourite());

        // ── archives ──────────────────────────────────────────────────────
        self.simple("extract-here", |this| this.extract_selected(false));
        self.simple("extract-to", |this| this.extract_selected(true));
        self.simple("compress", |this| this.compress_selected());

        // ── trash ─────────────────────────────────────────────────────────
        self.simple("restore", |this| this.restore_selected());
        self.simple("empty-trash", |this| this.empty_trash());

        // ── view ──────────────────────────────────────────────────────────
        self.simple("toggle-view", |this| {
            let next = match this.config.borrow().view_mode {
                ViewMode::Grid => ViewMode::List,
                ViewMode::List => ViewMode::Grid,
            };
            this.set_view_mode(next);
        });
        self.simple("zoom-in", |this| this.zoom(1));
        self.simple("zoom-out", |this| this.zoom(-1));
        self.simple("zoom-reset", |this| {
            this.config.borrow_mut().icon_size = crate::config::DEFAULT_ICON_SIZE;
            crate::ui::thumbs::clear();
            this.view.refresh_items();
            this.schedule_save();
        });
        self.simple("toggle-sidebar", |this| {
            let showing = this.split.shows_sidebar();
            this.split.set_show_sidebar(!showing);
            this.config.borrow_mut().sidebar_visible = !showing;
            this.schedule_save();
        });

        // Stateful toggles so the menu shows a checkmark.
        self.toggle("toggle-hidden", self.config.borrow().show_hidden, |this, on| {
            this.config.borrow_mut().show_hidden = on;
            this.view.refilter();
            this.update_status();
            this.schedule_save();
        });
        self.toggle("toggle-dirs-first", self.config.borrow().dirs_first, |this, on| {
            this.config.borrow_mut().dirs_first = on;
            this.view.apply_sort_from_config();
            this.schedule_save();
        });
        self.toggle("toggle-thumbnails", self.config.borrow().show_thumbnails, |this, on| {
            this.config.borrow_mut().show_thumbnails = on;
            crate::ui::thumbs::clear();
            this.view.refresh_items();
            this.schedule_save();
        });
        self.toggle(
            "sort-descending",
            self.config.borrow().sort_descending,
            |this, on| {
                this.config.borrow_mut().sort_descending = on;
                this.view.apply_sort_from_config();
                this.schedule_save();
            },
        );

        // ── radio-style actions ───────────────────────────────────────────
        let sort_state = match self.config.borrow().sort_key {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Modified => "modified",
            SortKey::Kind => "kind",
        };
        self.radio("sort-by", sort_state, |this, value| {
            let key = match value {
                "size" => SortKey::Size,
                "modified" => SortKey::Modified,
                "kind" => SortKey::Kind,
                _ => SortKey::Name,
            };
            this.config.borrow_mut().sort_key = key;
            this.view.apply_sort_from_config();
            this.schedule_save();
        });

        let theme_state = match self.config.borrow().theme {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        };
        self.radio("theme", theme_state, |this, value| {
            let theme = match value {
                "light" => Theme::Light,
                "dark" => Theme::Dark,
                _ => Theme::System,
            };
            this.config.borrow_mut().theme = theme;
            this.apply_theme();
            this.schedule_save();
        });

        // ── app ───────────────────────────────────────────────────────────
        self.simple("about", |this| this.show_about());
        self.simple("shortcuts", |this| this.show_shortcuts());
        self.simple("preferences", |this| this.show_preferences());
        self.simple("new-window", |this| {
            let start = this.current_dir().unwrap_or_else(|| PathBuf::from("/"));
            let window = Window::new(&this.app, this.config.borrow().clone(), start);
            window.present();
            // The new window is owned by the application, not by us; leaking the
            // Rc here is deliberate and matches the window's lifetime.
            std::mem::forget(window);
        });
        self.simple("close", |this| this.gtk_window().close());

        self.install_accelerators();
    }

    /// Registers an action in the window's own `win.` action map.
    ///
    /// `GtkApplicationWindow` already *is* the "win" action group; inserting a
    /// second group under that name shadows it in a way the application's
    /// accelerator lookup does not follow, which silently breaks every
    /// keyboard shortcut while leaving the menu items working.
    fn simple(self: &Rc<Self>, name: &str, callback: impl Fn(Rc<Window>) + 'static) {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(self);
        action.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                callback(this);
            }
        });
        self.window.add_action(&action);
    }

    fn toggle(
        self: &Rc<Self>,
        name: &str,
        initial: bool,
        callback: impl Fn(Rc<Window>, bool) + 'static,
    ) {
        let action = gio::SimpleAction::new_stateful(name, None, &initial.to_variant());
        let weak = Rc::downgrade(self);
        action.connect_activate(move |action, _| {
            let current = action.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
            let next = !current;
            action.set_state(&next.to_variant());
            if let Some(this) = weak.upgrade() {
                callback(this, next);
            }
        });
        self.window.add_action(&action);
    }

    fn radio(
        self: &Rc<Self>,
        name: &str,
        initial: &str,
        callback: impl Fn(Rc<Window>, &str) + 'static,
    ) {
        let action = gio::SimpleAction::new_stateful(
            name,
            Some(glib::VariantTy::STRING),
            &initial.to_variant(),
        );
        let weak = Rc::downgrade(self);
        action.connect_activate(move |action, parameter| {
            let Some(value) = parameter.and_then(|p| p.get::<String>()) else { return };
            action.set_state(&value.to_variant());
            if let Some(this) = weak.upgrade() {
                callback(this, &value);
            }
        });
        self.window.add_action(&action);
    }

    fn install_accelerators(&self) {
        // Accelerators are application-scoped in GTK4, but every one of these
        // targets a `win.` action, so they apply to whichever window has focus.
        for (action, keys) in [
            ("win.go-back", &["<Alt>Left", "<Alt>bracketleft"][..]),
            ("win.go-forward", &["<Alt>Right", "<Alt>bracketright"]),
            ("win.go-up", &["<Alt>Up"]),
            ("win.go-home", &["<Alt>Home"]),
            ("win.reload", &["F5", "<Ctrl>r"]),
            ("win.edit-path", &["<Ctrl>l"]),
            ("win.focus-search", &["<Ctrl>f"]),
            ("win.select-all", &["<Ctrl>a"]),
            ("win.select-none", &["<Ctrl><Shift>a"]),
            ("win.invert-selection", &["<Ctrl>i"]),
            ("win.open", &["Return", "<Ctrl>o"]),
            ("win.new-folder", &["<Ctrl><Shift>n"]),
            ("win.new-file", &["<Ctrl><Shift>t"]),
            ("win.rename", &["F2"]),
            ("win.copy", &["<Ctrl>c"]),
            ("win.cut", &["<Ctrl>x"]),
            ("win.paste", &["<Ctrl>v"]),
            ("win.trash", &["Delete", "BackSpace"]),
            ("win.delete-permanently", &["<Shift>Delete"]),
            ("win.shred", &["<Ctrl><Shift>Delete"]),
            ("win.properties", &["<Alt>Return"]),
            ("win.copy-path", &["<Ctrl><Shift>c"]),
            ("win.open-terminal", &["<Ctrl><Alt>t"]),
            ("win.add-favourite", &["<Ctrl>d"]),
            ("win.extract-here", &["<Ctrl>e"]),
            ("win.compress", &["<Ctrl><Shift>e"]),
            ("win.toggle-view", &["<Ctrl><Shift>v"]),
            ("win.toggle-hidden", &["<Ctrl>h"]),
            ("win.toggle-sidebar", &["F9"]),
            ("win.zoom-in", &["<Ctrl>plus", "<Ctrl>equal", "<Ctrl>KP_Add"]),
            ("win.zoom-out", &["<Ctrl>minus", "<Ctrl>KP_Subtract"]),
            ("win.zoom-reset", &["<Ctrl>0"]),
            ("win.new-window", &["<Ctrl>n"]),
            ("win.close", &["<Ctrl>w"]),
            ("win.shortcuts", &["<Ctrl>question"]),
            ("win.context-menu", &["Menu", "<Shift>F10"]),
        ] {
            self.app.set_accels_for_action(action, keys);
        }
    }

    // ── selection helpers ──────────────────────────────────────────────────

    pub(crate) fn selected_entries(&self) -> Vec<FileEntry> {
        self.view.selected().iter().map(|o| o.entry()).collect()
    }

    fn selected_or_toast(&self, what: &str) -> Vec<FileEntry> {
        let entries = self.selected_entries();
        if entries.is_empty() {
            self.toast(&format!("Select something to {what}"));
        }
        entries
    }

    // ── file operations ────────────────────────────────────────────────────

    fn create_new(self: &Rc<Self>, folder: bool) {
        let Some(dir) = self.current_dir() else {
            self.toast("Cannot create items here");
            return;
        };

        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let (heading, default, label) = if folder {
                ("New Folder", "Untitled Folder", "Create")
            } else {
                ("New File", "Untitled Document", "Create")
            };

            let Some(name) =
                dialogs::prompt_text(&this.widget(), heading, None, default, label, !folder).await
            else {
                return;
            };

            let result = if folder {
                ops::create_directory(&dir, &name)
            } else {
                ops::create_file(&dir, &name)
            };

            match result {
                Ok(path) => {
                    this.reload();
                    // Select the new item so it can be renamed immediately.
                    let weak = Rc::downgrade(&this);
                    glib::timeout_add_local_once(std::time::Duration::from_millis(200), move || {
                        if let Some(this) = weak.upgrade() {
                            this.view.select_paths(&[path]);
                        }
                    });
                }
                Err(err) => dialogs::show_error(&this.widget(), "Could not create item", &err),
            }
        });
    }

    fn rename_selected(self: &Rc<Self>) {
        let entries = self.selected_or_toast("rename");
        let Some(entry) = entries.first().cloned() else { return };
        if entries.len() > 1 {
            self.toast("Renaming several items at once isn't supported yet");
            return;
        }
        if self.in_trash() {
            self.toast("Restore this item before renaming it");
            return;
        }

        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let Some(name) = dialogs::prompt_text(
                &this.widget(),
                "Rename",
                None,
                &entry.display_name,
                "Rename",
                !entry.is_dir,
            )
            .await
            else {
                return;
            };
            if name == entry.display_name {
                return;
            }

            match ops::rename_in_place(&entry.path, &name) {
                Ok(path) => {
                    this.reload();
                    let weak = Rc::downgrade(&this);
                    glib::timeout_add_local_once(std::time::Duration::from_millis(200), move || {
                        if let Some(this) = weak.upgrade() {
                            this.view.select_paths(&[path]);
                        }
                    });
                }
                Err(err) => dialogs::show_error(&this.widget(), "Could not rename", &err),
            }
        });
    }

    fn copy_to_clipboard(self: &Rc<Self>, is_cut: bool) {
        let paths = self.view.selected_paths();
        if paths.is_empty() {
            self.toast(if is_cut { "Select something to cut" } else { "Select something to copy" });
            return;
        }

        *self.clipboard.borrow_mut() = Some(Clip { paths: paths.clone(), is_cut });

        // Publish to the system clipboard too, in both the portable file-list
        // form and the GNOME text form that carries the copy/cut distinction.
        let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
        let file_list = gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value());

        let verb = if is_cut { "cut" } else { "copy" };
        let uris: Vec<String> = paths.iter().map(|p| gio::File::for_path(p).uri().to_string()).collect();
        let gnome_payload = format!("{verb}\n{}", uris.join("\n"));
        let gnome_provider = gdk::ContentProvider::for_bytes(
            "x-special/gnome-copied-files",
            &glib::Bytes::from(gnome_payload.as_bytes()),
        );

        let provider = gdk::ContentProvider::new_union(&[gnome_provider, file_list]);
        WidgetExt::display(&self.window).clipboard().set_content(Some(&provider)).ok();

        self.toast(&format!(
            "{} {} item{}",
            if is_cut { "Cut" } else { "Copied" },
            paths.len(),
            if paths.len() == 1 { "" } else { "s" }
        ));
    }

    fn copy_paths_as_text(&self) {
        let paths = self.view.selected_paths();
        if paths.is_empty() {
            self.toast("Select something first");
            return;
        }
        let text = paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join("\n");
        WidgetExt::display(&self.window).clipboard().set_text(&text);
        self.toast("Path copied");
    }

    fn paste(self: &Rc<Self>) {
        let Some(dest) = self.current_dir() else {
            self.toast("Cannot paste here");
            return;
        };

        // Take the clip out of the RefCell *before* the body runs. Reading it
        // in the `if let` scrutinee keeps the borrow guard alive for the whole
        // body, so clearing it after a cut panicked with "already borrowed".
        let clip = self.clipboard.borrow().clone();
        if let Some(clip) = clip {
            self.start_transfer(clip.paths.clone(), dest, clip.is_cut);
            if clip.is_cut {
                *self.clipboard.borrow_mut() = None;
            }
            return;
        }

        let clipboard = WidgetExt::display(&self.window).clipboard();
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let Ok(value) = clipboard.read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT).await
            else {
                this.toast("Nothing to paste");
                return;
            };
            let Ok(list) = value.get::<gdk::FileList>() else {
                this.toast("Nothing to paste");
                return;
            };
            let paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
            if paths.is_empty() {
                this.toast("Nothing to paste");
                return;
            }
            this.start_transfer(paths, dest, false);
        });
    }

    /// Handles a drag-and-drop: same filesystem defaults to move, across
    /// filesystems to copy, matching what users expect from every desktop.
    pub(crate) fn drop_files(self: &Rc<Self>, paths: Vec<PathBuf>, destination: PathBuf) {
        let already_there = paths
            .iter()
            .all(|p| p.parent().map(|parent| parent == destination).unwrap_or(false));
        if already_there {
            return;
        }

        let same_device = paths
            .first()
            .and_then(|p| same_filesystem(p, &destination))
            .unwrap_or(false);
        self.start_transfer(paths, destination, same_device);
    }

    pub(crate) fn start_transfer(
        self: &Rc<Self>,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        is_move: bool,
    ) {
        let job = ops::start_transfer(sources, dest, is_move);
        let title = format!("{} files", job.kind.verb());
        let this = Rc::clone(self);

        self.jobs.run(job, self.widget(), title, move |outcome| {
            if !outcome.errors.is_empty() {
                dialogs::show_job_errors(&this.widget(), "Some items could not be transferred", &outcome.errors);
            } else if !outcome.cancelled && outcome.bytes_done > 0 {
                this.toast(&format!(
                    "{} {} in {} item{}",
                    if is_move { "Moved" } else { "Copied" },
                    humansize::format_size(outcome.bytes_done, humansize::DECIMAL),
                    outcome.items_done,
                    if outcome.items_done == 1 { "" } else { "s" }
                ));
            }
            this.reload();
            let created = outcome.created.clone();
            if !created.is_empty() {
                let weak = Rc::downgrade(&this);
                glib::timeout_add_local_once(std::time::Duration::from_millis(200), move || {
                    if let Some(this) = weak.upgrade() {
                        this.view.select_paths(&created);
                    }
                });
            }
        });
    }

    fn delete_selected(self: &Rc<Self>, permanent: bool) {
        let entries = self.selected_or_toast("delete");
        if entries.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();

        // Inside the trash, "delete" can only mean permanent.
        let permanent = permanent || self.in_trash();
        let this = Rc::clone(self);

        glib::spawn_future_local(async move {
            if permanent {
                let what = describe(&paths);
                let confirmed = dialogs::confirm(
                    &this.widget(),
                    "Delete permanently?",
                    &format!("{what} will be deleted immediately. This cannot be undone."),
                    "Delete",
                    true,
                )
                .await;
                if !confirmed {
                    return;
                }
            } else if this.config.borrow().confirm_trash {
                let confirmed = dialogs::confirm(
                    &this.widget(),
                    "Move to Trash?",
                    &format!("{} will be moved to the Trash.", describe(&paths)),
                    "Move to Trash",
                    false,
                )
                .await;
                if !confirmed {
                    return;
                }
            }

            let job = ops::start_delete(paths.clone(), permanent);
            let title = job.kind.verb().to_string();
            let after = Rc::clone(&this);

            this.jobs.run(job, this.widget(), title, move |outcome| {
                if !outcome.errors.is_empty() {
                    dialogs::show_job_errors(&after.widget(), "Some items could not be deleted", &outcome.errors);
                } else if !permanent && !outcome.cancelled {
                    let count = outcome.items_done;
                    // Trashing is reversible, so offer the reversal right there
                    // rather than making the user go and find the Trash.
                    let undo_target = Rc::downgrade(&after);
                    let trashed = paths.clone();
                    after.toast_with_action(
                        &format!("Moved {count} item{} to Trash", if count == 1 { "" } else { "s" }),
                        "Undo",
                        move || {
                            let Some(this) = undo_target.upgrade() else { return };
                            this.restore_from_trash(&trashed);
                        },
                    );
                }
                after.reload();
                after.refresh_trash_count();
            });
        });
    }

    fn shred_selected(self: &Rc<Self>) {
        let entries = self.selected_or_toast("shred");
        if entries.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
        let passes = self.config.borrow().shred_passes;
        let caveat = paths.first().and_then(|p| shred::filesystem_caveat(p));

        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            if !dialogs::confirm_shred(&this.widget(), &paths, passes, caveat).await {
                return;
            }

            let job = shred::start_shred(paths, passes);
            let after = Rc::clone(&this);
            this.jobs.run(job, this.widget(), "Shredding".to_string(), move |outcome| {
                if !outcome.errors.is_empty() {
                    dialogs::show_job_errors(&after.widget(), "Some items could not be shredded", &outcome.errors);
                } else if !outcome.cancelled {
                    after.toast(&format!("Shredded {} item{}", outcome.items_done, if outcome.items_done == 1 { "" } else { "s" }));
                }
                after.reload();
            });
        });
    }

    // ── archives ───────────────────────────────────────────────────────────

    fn extract_selected(self: &Rc<Self>, ask_destination: bool) {
        let entries = self.selected_or_toast("extract");
        let archives: Vec<PathBuf> = entries
            .iter()
            .filter(|e| !e.is_dir && archive::is_archive(&e.path))
            .map(|e| e.path.clone())
            .collect();

        if archives.is_empty() {
            if !entries.is_empty() {
                self.toast("The selection has no archives in it");
            }
            return;
        }

        let Some(current) = self.current_dir() else {
            self.toast("Cannot extract here");
            return;
        };

        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let into = if ask_destination {
                match this.choose_folder("Extract to").await {
                    Some(dir) => dir,
                    None => return,
                }
            } else {
                current
            };

            for path in archives {
                this.extract_one(path, into.clone()).await;
            }
        });
    }

    /// Runs a job on the progress strip and resolves with its outcome.
    ///
    /// `JobMonitor::run` is callback-shaped, which is fine for fire-and-forget
    /// work but awkward when the caller has to react and then carry on — an
    /// encrypted archive that needs a passphrase and a second attempt. Bridging
    /// it to a future keeps those flows as one linear `async fn`.
    async fn run_job(self: &Rc<Self>, job: ops::JobHandle, title: String) -> ops::JobOutcome {
        let (tx, rx) = async_channel::bounded(1);
        self.jobs.run(job, self.widget(), title, move |outcome| {
            // The channel has room for exactly this one message and the job
            // sends exactly one outcome, so this never blocks the main loop.
            let _ = tx.send_blocking(outcome);
        });
        rx.recv().await.unwrap_or_default()
    }

    async fn extract_one(self: &Rc<Self>, path: PathBuf, into: PathBuf) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut password: Option<String> = None;

        let outcome = loop {
            let job = archive::start_extract(path.clone(), into.clone(), password.clone());
            let outcome = self.run_job(job, format!("Extracting {name}")).await;

            // An archive only admits to being encrypted once libarchive reaches
            // its first entry, so the prompt comes after the job rather than
            // before it. Nothing has been written by then: the run stops on that
            // first entry and its staging directory is removed.
            let needs_password = password.is_none()
                && outcome.errors.iter().any(|(_, e)| e == archive::PASSWORD_REQUIRED);
            if !needs_password {
                break outcome;
            }
            match dialogs::ask_archive_password(&self.widget(), &name).await {
                Some(entered) => password = Some(entered),
                None => return,
            }
        };

        if let Some((_, error)) = outcome.errors.first() {
            dialogs::show_error(&self.widget(), &format!("Could not extract “{name}”"), error);
        } else if !outcome.cancelled {
            self.toast(&format!(
                "Extracted {} item{}",
                outcome.items_done,
                if outcome.items_done == 1 { "" } else { "s" }
            ));
        }

        self.reload();
        let created = outcome.created.clone();
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
            if let Some(this) = weak.upgrade() {
                this.view.select_paths(&created);
            }
        });
    }

    fn compress_selected(self: &Rc<Self>) {
        let entries = self.selected_or_toast("compress");
        if entries.is_empty() {
            return;
        }
        if !archive::can_compress() {
            dialogs::show_error(
                &self.widget(),
                "Compression is unavailable",
                "Install the libarchive package, which provides bsdtar.",
            );
            return;
        }

        let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
        let default_stem = if paths.len() == 1 {
            entries[0].path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "archive".into())
        } else {
            self.current_dir()
                .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "archive".into())
        };
        let Some(dir) = self.current_dir() else { return };

        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let Some((stem, format)) = dialogs::ask_compress(&this.widget(), &default_stem).await
            else {
                return;
            };

            let dest = ops::unique_destination(&dir.join(format!("{stem}{}", format.suffix())));
            let job = archive::start_compress(paths, dest.clone(), format);
            let after = Rc::clone(&this);

            this.jobs.run(job, this.widget(), "Compressing".to_string(), move |outcome| {
                if let Some((_, error)) = outcome.errors.first() {
                    dialogs::show_error(&after.widget(), "Could not create the archive", error);
                } else if !outcome.cancelled {
                    after.toast(&format!(
                        "Created {}",
                        dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                    ));
                }
                after.reload();
            });
        });
    }

    // ── trash ──────────────────────────────────────────────────────────────

    fn restore_selected(self: &Rc<Self>) {
        if !self.in_trash() {
            self.toast("Restore only works inside the Trash");
            return;
        }
        let paths = self.view.selected_paths();
        if paths.is_empty() {
            self.toast("Select items to restore");
            return;
        }

        let items = trash::list_trash();
        let mut restored = 0usize;
        let mut errors = Vec::new();

        for item in items.iter().filter(|i| paths.contains(&i.file_path)) {
            match trash::restore(item) {
                Ok(_) => restored += 1,
                Err(err) => errors.push((item.file_path.clone(), err)),
            }
        }

        if !errors.is_empty() {
            dialogs::show_job_errors(&self.widget(), "Some items could not be restored", &errors);
        } else {
            self.toast(&format!("Restored {restored} item{}", if restored == 1 { "" } else { "s" }));
        }
        self.reload();
        self.refresh_trash_count();
    }

    /// Puts items back where they came from, matching on their original paths.
    ///
    /// Used by the Undo button on the "moved to Trash" toast; the trash names
    /// are generated, so the original path is the only reliable key.
    pub(crate) fn restore_from_trash(self: &Rc<Self>, original_paths: &[PathBuf]) {
        let items = trash::list_trash();
        let mut restored = 0usize;
        let mut errors = Vec::new();

        for item in items.iter().filter(|i| original_paths.contains(&i.original_path)) {
            match trash::restore(item) {
                Ok(_) => restored += 1,
                Err(err) => errors.push((item.original_path.clone(), err)),
            }
        }

        if !errors.is_empty() {
            dialogs::show_job_errors(&self.widget(), "Some items could not be restored", &errors);
        } else if restored == 0 {
            self.toast("Those items are no longer in the Trash");
        } else {
            self.toast(&format!("Restored {restored} item{}", if restored == 1 { "" } else { "s" }));
        }
        self.reload();
        self.refresh_trash_count();
    }

    fn empty_trash(self: &Rc<Self>) {
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let count = trash::trash_count();
            if count == 0 {
                this.toast("The Trash is already empty");
                return;
            }

            let confirmed = dialogs::confirm(
                &this.widget(),
                "Empty the Trash?",
                &format!(
                    "All {count} item{} will be permanently deleted. This cannot be undone.",
                    if count == 1 { "" } else { "s" }
                ),
                "Empty Trash",
                true,
            )
            .await;
            if !confirmed {
                return;
            }

            let (removed, errors) = run_off_thread(trash::empty_trash).await;
            if !errors.is_empty() {
                dialogs::show_error(&this.widget(), "Some items could not be deleted", &errors.join("\n"));
            } else {
                this.toast(&format!("Deleted {removed} item{}", if removed == 1 { "" } else { "s" }));
            }
            this.reload();
            this.refresh_trash_count();
        });
    }

    // ── drives ─────────────────────────────────────────────────────────────

    /// Mounts a volume, then navigates into it.
    ///
    /// NTFS failures are routed to the recovery dialog rather than shown as a
    /// dead-end error, because they are almost always fixable.
    pub(crate) fn mount_volume(self: &Rc<Self>, volume: Volume) {
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            this.toast(&format!("Mounting “{}”…", volume.label));

            let result = run_off_thread({
                let volume = volume.clone();
                move || crate::drives::mount(&volume)
            })
            .await;

            match result {
                Ok(path) => this.after_mount(&volume, path),
                Err(MountError::AlreadyMounted(path)) => this.after_mount(&volume, path),
                Err(MountError::NtfsUnclean { message, hibernated }) => {
                    this.recover_ntfs(volume, message, hibernated).await;
                }
                Err(err) => {
                    dialogs::show_error(
                        &this.widget(),
                        &format!("Could not mount “{}”", volume.label),
                        &err.message(),
                    );
                }
            }
        });
    }

    async fn recover_ntfs(self: &Rc<Self>, volume: Volume, message: String, hibernated: bool) {
        let choice = dialogs::ntfs_recovery(
            &self.widget(),
            &volume,
            &message,
            hibernated,
            crate::drives::ntfsfix_available(),
        )
        .await;

        let result = match choice {
            NtfsChoice::Cancel => return,
            NtfsChoice::ReadOnly => {
                run_off_thread({
                    let volume = volume.clone();
                    move || crate::drives::mount_read_only(&volume)
                })
                .await
            }
            NtfsChoice::Repair => {
                self.toast("Repairing the volume…");
                run_off_thread({
                    let volume = volume.clone();
                    move || crate::drives::repair_and_mount(&volume)
                })
                .await
            }
            NtfsChoice::Force => {
                let confirmed = dialogs::confirm(
                    &self.widget(),
                    "Discard the hibernated Windows session?",
                    "Your files are not affected, but Windows will not be able to resume the \
                     suspended session after this. Windows will boot fresh instead.",
                    "Discard and Mount",
                    true,
                )
                .await;
                if !confirmed {
                    return;
                }
                run_off_thread({
                    let volume = volume.clone();
                    move || crate::drives::force_mount(&volume)
                })
                .await
            }
        };

        match result {
            Ok(path) => {
                if choice == NtfsChoice::ReadOnly {
                    self.toast(&format!("“{}” mounted read-only", volume.label));
                }
                self.after_mount(&volume, path);
            }
            Err(err) => {
                dialogs::show_error(
                    &self.widget(),
                    &format!("Could not mount “{}”", volume.label),
                    &err.message(),
                );
            }
        }
    }

    fn after_mount(self: &Rc<Self>, volume: &Volume, path: PathBuf) {
        self.refresh_drives();
        self.toast(&format!("Mounted “{}”", volume.label));
        self.navigate_to(Location::Directory(path), true);
    }

    pub(crate) fn unmount_volume(self: &Rc<Self>, volume: Volume, eject: bool) {
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            // Navigate out first: unmounting the directory being viewed would
            // otherwise leave the window pointed at a path that no longer exists.
            if let Some(mount_point) = &volume.mount_point
                && this.current_dir().is_some_and(|d| d.starts_with(mount_point))
            {
                let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
                this.history.borrow_mut().forget(mount_point);
                this.navigate_to(Location::Directory(home), false);
            }

            let label = volume.label.clone();
            let result = run_off_thread({
                let volume = volume.clone();
                move || {
                    let first = if eject {
                        crate::drives::eject(&volume)
                    } else {
                        crate::drives::unmount(&volume)
                    };

                    // A volume mounted by the NTFS force path is outside
                    // UDisks2's control, so its Unmount can refuse; fall back to
                    // a privileged umount of the mount point itself.
                    match (first, &volume.mount_point) {
                        (Ok(()), _) => Ok(()),
                        (Err(err), Some(mount_point)) => {
                            crate::drives::unmount_privileged(mount_point).map_err(|second| {
                                if second.trim().is_empty() { err } else { second }
                            })
                        }
                        (Err(err), None) => Err(err),
                    }
                }
            })
            .await;

            match result {
                Ok(()) => {
                    this.toast(&if eject {
                        format!("“{label}” is safe to remove")
                    } else {
                        format!("Unmounted “{label}”")
                    });
                }
                Err(err) => {
                    // A busy volume is the common case and deserves a useful
                    // message rather than the raw D-Bus text.
                    let hint = if err.to_lowercase().contains("busy") {
                        "\n\nSomething is still using it — close any programs with files open there."
                    } else {
                        ""
                    };
                    dialogs::show_error(
                        &this.widget(),
                        &format!("Could not unmount “{label}”"),
                        &format!("{err}{hint}"),
                    );
                }
            }
            this.refresh_drives();
        });
    }

    // ── misc ───────────────────────────────────────────────────────────────

    fn toggle_favourite(self: &Rc<Self>) {
        let target = self
            .selected_entries()
            .into_iter()
            .find(|e| e.is_dir)
            .map(|e| e.path)
            .or_else(|| self.current_dir());

        let Some(path) = target else {
            self.toast("Select a folder to add to Favourites");
            return;
        };

        let added = !self.config.borrow().is_favourite(&path);
        if !self.config.borrow_mut().toggle_favourite(&path) {
            self.toast("Only folders can be added to Favourites");
            return;
        }

        self.sidebar.set_favourites(self.config.borrow().favourites.clone());
        self.schedule_save();

        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.toast(&if added {
            format!("Added “{name}” to Favourites")
        } else {
            format!("Removed “{name}” from Favourites")
        });
    }

    fn open_terminal(&self) {
        let Some(dir) = self.current_dir() else {
            self.toast("Cannot open a terminal here");
            return;
        };

        // Honour the user's choice first, then try what's actually installed.
        let mut candidates: Vec<String> = Vec::new();
        if let Ok(preferred) = std::env::var("TERMINAL") {
            candidates.push(preferred);
        }
        candidates.extend(
            ["ghostty", "kitty", "alacritty", "wezterm", "foot", "gnome-terminal", "konsole", "xfce4-terminal", "xterm"]
                .iter()
                .map(|s| s.to_string()),
        );

        for candidate in candidates {
            if which(&candidate).is_none() {
                continue;
            }
            let spawned = std::process::Command::new(&candidate)
                .current_dir(&dir)
                .spawn();
            if spawned.is_ok() {
                return;
            }
        }
        self.toast("No terminal emulator found");
    }

    fn open_with(self: &Rc<Self>) {
        let entries = self.selected_or_toast("open");
        let Some(entry) = entries.first().cloned() else { return };

        let apps = gio::AppInfo::recommended_for_type(&entry.content_type);
        let all = gio::AppInfo::all_for_type(&entry.content_type);
        // Recommended apps come first, then anything else that claims the type.
        let mut ordered = apps.clone();
        for app in all {
            if !ordered.iter().any(|a| a.id() == app.id()) {
                ordered.push(app);
            }
        }

        if ordered.is_empty() {
            self.toast("No applications are registered for this file type");
            return;
        }

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["boxed-list"])
            .build();

        for app in &ordered {
            let row = adw::ActionRow::builder().title(app.display_name().as_str()).build();
            if let Some(icon) = app.icon() {
                row.add_prefix(&gtk::Image::from_gicon(&icon));
            }
            list.append(&row);
        }

        let dialog = adw::AlertDialog::new(
            Some("Open With"),
            Some(&format!("Choose an application for “{}”.", entry.display_name)),
        );
        let scroller = gtk::ScrolledWindow::builder()
            .height_request(280)
            .propagate_natural_height(true)
            .child(&list)
            .margin_top(8)
            .build();
        dialog.set_extra_child(Some(&scroller));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("open", "Open");
        dialog.set_response_appearance("open", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("open"));
        dialog.set_close_response("cancel");

        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let response = dialog.choose_future(Some(&this.widget())).await;
            if response != "open" {
                return;
            }
            let Some(index) = list.selected_row().map(|r| r.index()) else { return };
            let Some(app) = ordered.get(index as usize) else { return };

            let file = gio::File::for_path(&entry.path);
            if let Err(err) = app.launch(&[file], None::<&gio::AppLaunchContext>) {
                dialogs::show_error(&this.widget(), "Could not open the file", err.message());
            }
        });
    }

    async fn choose_folder(&self, title: &str) -> Option<PathBuf> {
        let dialog = gtk::FileDialog::builder().title(title).modal(true).build();
        if let Some(current) = self.current_dir() {
            dialog.set_initial_folder(Some(&gio::File::for_path(&current)));
        }
        dialog.select_folder_future(Some(&self.window)).await.ok()?.path()
    }

    fn show_about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("Fileman")
            .application_icon("system-file-manager")
            .version(env!("CARGO_PKG_VERSION"))
            .developer_name("Built with Rust, GTK 4 and libadwaita")
            .comments(
                "A fast file manager with archive extraction, secure deletion, \
                 and NTFS drive mounting that copes with Windows Fast Startup.",
            )
            .license_type(gtk::License::MitX11)
            .build();
        about.present(Some(&self.window));
    }

    fn show_shortcuts(&self) {
        let page = adw::PreferencesPage::new();

        for (title, rows) in SHORTCUT_GROUPS {
            let group = adw::PreferencesGroup::builder().title(*title).build();
            for (keys, description) in *rows {
                let row = adw::ActionRow::builder().title(*description).build();
                let label = gtk::Label::builder()
                    .label(*keys)
                    .css_classes(["dim-label", "monospace"])
                    .valign(gtk::Align::Center)
                    .build();
                row.add_suffix(&label);
                group.add(&row);
            }
            page.add(&group);
        }

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&page));

        adw::Dialog::builder()
            .title("Keyboard Shortcuts")
            .content_width(520)
            .content_height(640)
            .child(&toolbar)
            .build()
            .present(Some(&self.window));
    }

    fn show_preferences(self: &Rc<Self>) {
        let page = adw::PreferencesPage::new();

        let behaviour = adw::PreferencesGroup::builder().title("Behaviour").build();

        let confirm_row = adw::SwitchRow::builder()
            .title("Confirm before moving to Trash")
            .active(self.config.borrow().confirm_trash)
            .build();
        let weak = Rc::downgrade(self);
        confirm_row.connect_active_notify(move |row| {
            let Some(this) = weak.upgrade() else { return };
            this.config.borrow_mut().confirm_trash = row.is_active();
            this.schedule_save();
        });
        behaviour.add(&confirm_row);
        page.add(&behaviour);

        let shredding = adw::PreferencesGroup::builder()
            .title("Shredding")
            .description(
                "How many times a file's contents are overwritten before it is deleted. \
                 More passes take proportionally longer and, on modern storage, rarely add \
                 real protection — full-disk encryption does.",
            )
            .build();

        let passes = adw::SpinRow::with_range(1.0, 35.0, 1.0);
        passes.set_title("Overwrite passes");
        passes.set_value(self.config.borrow().shred_passes as f64);
        let weak = Rc::downgrade(self);
        passes.connect_value_notify(move |row| {
            let Some(this) = weak.upgrade() else { return };
            this.config.borrow_mut().shred_passes = row.value() as u32;
            this.schedule_save();
        });
        shredding.add(&passes);
        page.add(&shredding);

        let performance = adw::PreferencesGroup::builder()
            .title("Performance")
            .description(
                "Threads used for compressing, extracting and shredding. Automatic keeps \
                 about half the machine's cores free so a long job doesn't slow down \
                 everything else you are doing.",
            )
            .build();

        let threads = adw::SpinRow::with_range(0.0, 64.0, 1.0);
        threads.set_title("Worker threads");
        threads.set_subtitle("0 for automatic");
        threads.set_value(self.config.borrow().worker_threads as f64);
        let weak = Rc::downgrade(self);
        threads.connect_value_notify(move |row| {
            let Some(this) = weak.upgrade() else { return };
            let value = row.value() as u32;
            this.config.borrow_mut().worker_threads = value;
            // Applied immediately rather than at the next launch, so the effect
            // of changing it can actually be observed.
            crate::fs::parallel::set_override(value);
            this.schedule_save();
        });
        performance.add(&threads);
        page.add(&performance);

        let thumbs_group = adw::PreferencesGroup::builder().title("Thumbnails").build();
        let max_mb = adw::SpinRow::with_range(1.0, 512.0, 1.0);
        max_mb.set_title("Skip images larger than");
        max_mb.set_subtitle("Megabytes");
        max_mb.set_value((self.config.borrow().thumbnail_max_bytes / (1024 * 1024)) as f64);
        let weak = Rc::downgrade(self);
        max_mb.connect_value_notify(move |row| {
            let Some(this) = weak.upgrade() else { return };
            this.config.borrow_mut().thumbnail_max_bytes = (row.value() as u64) * 1024 * 1024;
            crate::ui::thumbs::clear();
            this.view.refresh_items();
            this.schedule_save();
        });
        thumbs_group.add(&max_mb);
        page.add(&thumbs_group);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&page));

        adw::Dialog::builder()
            .title("Preferences")
            .content_width(520)
            .content_height(520)
            .child(&toolbar)
            .build()
            .present(Some(&self.window));
    }

    /// Builds and pops up the right-click menu for the current selection.
    pub(crate) fn show_context_menu(self: &Rc<Self>, x: f64, y: f64) {
        let selection = self.selected_entries();
        let menu = &self.context_menu;
        menu.begin();

        if self.in_trash() {
            if !selection.is_empty() {
                menu.item("Restore", "win.restore", None);
                menu.item("Delete Permanently", "win.delete-permanently", Some("Shift+Delete"));
                menu.section();
            }
            menu.item("Empty Trash", "win.empty-trash", None);
        } else if selection.is_empty() {
            menu.item("New Folder", "win.new-folder", Some("Ctrl+Shift+N"));
            menu.item("New File", "win.new-file", Some("Ctrl+Shift+T"));
            menu.section();
            menu.item("Paste", "win.paste", Some("Ctrl+V"));
            menu.item("Select All", "win.select-all", Some("Ctrl+A"));
            menu.section();
            menu.item("Open Terminal Here", "win.open-terminal", Some("Ctrl+Alt+T"));
            menu.item("Add to Favourites", "win.add-favourite", Some("Ctrl+D"));
            menu.section();
            menu.item("Properties", "win.properties", Some("Alt+Return"));
        } else {
            let single = selection.len() == 1;
            let has_dir = selection.iter().any(|e| e.is_dir);
            let has_archive = selection.iter().any(|e| !e.is_dir && archive::is_archive(&e.path));

            menu.item("Open", "win.open", Some("Return"));
            if single && !selection[0].is_dir {
                menu.item("Open With…", "win.open-with", None);
            }

            menu.section();
            menu.item("Cut", "win.cut", Some("Ctrl+X"));
            menu.item("Copy", "win.copy", Some("Ctrl+C"));
            menu.item("Copy Path", "win.copy-path", Some("Ctrl+Shift+C"));
            if single {
                menu.item("Rename…", "win.rename", Some("F2"));
            }

            menu.section();
            if has_archive {
                menu.item("Extract Here", "win.extract-here", Some("Ctrl+E"));
                menu.item("Extract To…", "win.extract-to", None);
            }
            menu.item("Compress…", "win.compress", Some("Ctrl+Shift+E"));

            menu.section();
            menu.item("Move to Trash", "win.trash", Some("Delete"));
            menu.item("Delete Permanently", "win.delete-permanently", Some("Shift+Delete"));
            menu.item("Shred…", "win.shred", Some("Ctrl+Shift+Delete"));

            menu.section();
            if has_dir {
                menu.item("Add to Favourites", "win.add-favourite", Some("Ctrl+D"));
            }
            menu.item("Properties", "win.properties", Some("Alt+Return"));
        }

        if menu.is_empty() {
            return;
        }
        menu.show_at(x, y);

        if crate::trace::enabled() {
            let menu = Rc::clone(menu);
            glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
                let (natural, allocated) = menu.measured();
                crate::trace::log(&format!(
                    "context menu: wants {natural}px, allocated {allocated}px"
                ));
            });
        }
    }
}

// ── free functions ─────────────────────────────────────────────────────────

/// Runs a blocking closure on a worker thread and awaits its result.
async fn run_off_thread<T, F>(work: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = async_channel::bounded(1);
    std::thread::Builder::new()
        .name("fileman-worker".into())
        .spawn(move || {
            let _ = tx.send_blocking(work());
        })
        .expect("spawn worker thread");

    rx.recv().await.expect("worker thread panicked")
}

/// Whether two paths live on the same filesystem, which decides whether a drop
/// defaults to move or copy.
fn same_filesystem(a: &Path, b: &Path) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    let a_dev = std::fs::metadata(a).ok()?.dev();
    let b_dev = std::fs::metadata(b).ok()?.dev();
    Some(a_dev == b_dev)
}

fn describe(paths: &[PathBuf]) -> String {
    if paths.len() == 1 {
        format!(
            "“{}”",
            paths[0].file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        )
    } else {
        format!("{} items", paths.len())
    }
}

fn which(binary: &str) -> Option<PathBuf> {
    if binary.contains('/') {
        let path = PathBuf::from(binary);
        return path.is_file().then_some(path);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|dir| dir.join(binary)).find(|c| c.is_file())
    })
}

/// Builds a `FileEntry` for a directory itself, for "properties of this folder".
fn current_dir_entry(dir: &Path) -> Option<FileEntry> {
    let md = std::fs::metadata(dir).ok()?;
    use std::os::unix::fs::PermissionsExt;
    let display_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.to_string_lossy().into_owned());
    Some(FileEntry {
        path: dir.to_path_buf(),
        name: dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into()),
        display_name: display_name.clone(),
        search_key: display_name.to_lowercase(),
        is_dir: true,
        is_symlink: false,
        symlink_target: None,
        is_hidden: false,
        size: 0,
        modified: md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64),
        content_type: "inode/directory".to_string(),
        can_read: true,
        can_write: !md.permissions().readonly(),
        can_execute: true,
        mode: md.permissions().mode(),
    })
}

type ShortcutRows = &'static [(&'static str, &'static str)];

const SHORTCUT_GROUPS: &[(&str, ShortcutRows)] = &[
    (
        "Navigation",
        &[
            ("Alt+←", "Back"),
            ("Alt+→", "Forward"),
            ("Alt+↑", "Open parent folder"),
            ("Alt+Home", "Home folder"),
            ("Ctrl+L", "Edit the location as text"),
            ("F5", "Reload"),
        ],
    ),
    (
        "Files",
        &[
            ("Return", "Open"),
            ("F2", "Rename"),
            ("Ctrl+C / Ctrl+X / Ctrl+V", "Copy, cut, paste"),
            ("Ctrl+Shift+C", "Copy the full path"),
            ("Delete", "Move to Trash"),
            ("Shift+Delete", "Delete permanently"),
            ("Ctrl+Shift+Delete", "Shred"),
            ("Ctrl+Shift+N", "New folder"),
            ("Alt+Return", "Properties"),
        ],
    ),
    (
        "Archives",
        &[("Ctrl+E", "Extract here"), ("Ctrl+Shift+E", "Compress selection")],
    ),
    (
        "View",
        &[
            ("Ctrl+F", "Filter by name"),
            ("Ctrl+H", "Show hidden files"),
            ("Ctrl+Shift+V", "Switch grid and list"),
            ("Ctrl++ / Ctrl+−", "Icon size"),
            ("Ctrl+0", "Reset icon size"),
            ("F9", "Toggle the sidebar"),
        ],
    ),
    (
        "Selection",
        &[
            ("Ctrl+A", "Select all"),
            ("Ctrl+Shift+A", "Select none"),
            ("Ctrl+I", "Invert selection"),
        ],
    ),
    (
        "Window",
        &[("Ctrl+N", "New window"), ("Ctrl+W", "Close window"), ("Ctrl+?", "This list")],
    ),
];

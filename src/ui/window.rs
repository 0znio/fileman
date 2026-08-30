//! The main window: layout, navigation, scanning and live updates.
//!
//! Operation handling (copy, extract, mount, shred…) lives in
//! [`crate::ui::actions`], which extends this same type — the split is purely
//! to keep each file readable.

use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::{
    config::{Config, Theme, ViewMode},
    drives::Volume,
    fs::{FileEntry, scan},
    history::History,
    ui::{
        file_view::FileView, pathbar::PathBar, progress::JobMonitor, sidebar::Sidebar,
    },
};

/// Where the view is pointed. Trash is not a normal directory: its contents are
/// assembled from the freedesktop trash metadata, and restore only works there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Directory(PathBuf),
    Trash,
}

impl Location {
    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Location::Directory(p) => Some(p),
            Location::Trash => None,
        }
    }
}

/// A pending copy or cut.
#[derive(Debug, Clone)]
pub struct Clip {
    pub paths: Vec<PathBuf>,
    pub is_cut: bool,
}

pub struct Window {
    pub(crate) window: adw::ApplicationWindow,
    pub(crate) app: adw::Application,
    pub(crate) config: Rc<RefCell<Config>>,

    pub(crate) view: Rc<FileView>,
    pub(crate) sidebar: Rc<Sidebar>,
    pub(crate) pathbar: Rc<PathBar>,
    pub(crate) jobs: Rc<JobMonitor>,
    pub(crate) toasts: adw::ToastOverlay,
    pub(crate) split: adw::OverlaySplitView,

    pub(crate) search_bar: gtk::SearchBar,
    pub(crate) search_entry: gtk::SearchEntry,
    pub(crate) status_label: gtk::Label,
    pub(crate) capacity_label: gtk::Label,
    pub(crate) banner: adw::Banner,

    pub(crate) back_button: gtk::Button,
    pub(crate) forward_button: gtk::Button,
    pub(crate) up_button: gtk::Button,

    pub(crate) history: RefCell<History>,
    pub(crate) location: RefCell<Location>,
    /// Entries currently loaded, kept so actions can work on the selection
    /// without re-querying the filesystem.
    pub(crate) entries: RefCell<Vec<FileEntry>>,
    pub(crate) volumes: RefCell<Vec<Volume>>,
    pub(crate) clipboard: RefCell<Option<Clip>>,

    /// Bumped on every navigation so a slow scan can detect it was superseded.
    pub(crate) scan_generation: Cell<u64>,
    pub(crate) scan_cancellable: RefCell<gio::Cancellable>,
    pub(crate) dir_monitor: RefCell<Option<gio::FileMonitor>>,
    /// Debounce timer for filesystem-change-triggered reloads.
    pub(crate) reload_timer: RefCell<Option<glib::SourceId>>,
    pub(crate) save_timer: RefCell<Option<glib::SourceId>>,

    /// The running recursive search, if any. Dropping it cancels the walk.
    pub(crate) search_job: RefCell<Option<crate::fs::search::SearchHandle>>,
    /// Bumped per search so a slow walk can tell it has been superseded.
    pub(crate) search_generation: Cell<u64>,
    /// What the banner's button does right now.
    pub(crate) banner_action: RefCell<BannerAction>,
    /// Set while `navigate_to` clears search mode, so the resulting signal
    /// doesn't trigger a second load of the folder we are already loading.
    pub(crate) navigating: Cell<bool>,

    /// One long-lived context menu, rather than one built per right-click.
    ///
    /// A popover that unparents itself when it closes is detached from the
    /// widget tree *before* GTK dispatches the item you clicked, so the
    /// `win.` action lookup finds nothing and the click silently does nothing.
    pub(crate) context_menu: Rc<crate::ui::menu::ContextMenu>,
}

/// The banner is shared between unrelated states, so its button needs to know
/// which one is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerAction {
    None,
    EmptyTrash,
    StopSearch,
}

impl Window {
    pub fn new(app: &adw::Application, config: Config, start: PathBuf) -> Rc<Self> {
        let config = Rc::new(RefCell::new(config));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Fileman")
            .default_width(config.borrow().window_width)
            .default_height(config.borrow().window_height)
            .build();
        if config.borrow().window_maximized {
            window.maximize();
        }

        let view = FileView::new(Rc::clone(&config));
        let sidebar = Sidebar::new();
        let pathbar = PathBar::new();
        let jobs = JobMonitor::new();

        // ── header ────────────────────────────────────────────────────────
        let back_button = nav_button("go-previous-symbolic", "Back (Alt+Left)", "win.go-back");
        let forward_button = nav_button("go-next-symbolic", "Forward (Alt+Right)", "win.go-forward");
        let up_button = nav_button("go-up-symbolic", "Up (Alt+Up)", "win.go-up");

        let nav_group = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["linked"])
            .build();
        nav_group.append(&back_button);
        nav_group.append(&forward_button);

        let sidebar_toggle = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("Toggle sidebar (F9)")
            .active(config.borrow().sidebar_visible)
            .build();

        let search_button = gtk::ToggleButton::builder()
            .icon_name("system-search-symbolic")
            .tooltip_text("Search this folder (Ctrl+F)")
            .build();

        let view_button = gtk::Button::builder()
            .icon_name(view_icon(config.borrow().view_mode))
            .tooltip_text("Switch between grid and list (Ctrl+Shift+V)")
            .action_name("win.toggle-view")
            .build();

        let zoom_button = gtk::MenuButton::builder()
            .icon_name("zoom-in-symbolic")
            .tooltip_text("Icon size")
            .popover(&build_zoom_popover(&config))
            .build();

        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Main menu (F10)")
            .menu_model(&build_main_menu())
            .primary(true)
            .build();

        let header = adw::HeaderBar::builder()
            .title_widget(pathbar.widget())
            .build();
        header.pack_start(&sidebar_toggle);
        header.pack_start(&nav_group);
        header.pack_start(&up_button);
        header.pack_end(&menu_button);
        header.pack_end(&zoom_button);
        header.pack_end(&view_button);
        header.pack_end(&search_button);

        // ── search ────────────────────────────────────────────────────────
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Filter by name…")
            .hexpand(true)
            .build();
        let search_bar = gtk::SearchBar::builder()
            .child(&search_entry)
            .key_capture_widget(&window)
            .build();
        search_bar
            .bind_property("search-mode-enabled", &search_button, "active")
            .bidirectional()
            .sync_create()
            .build();

        // ── status bar ────────────────────────────────────────────────────
        let status_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label"])
            .build();
        let capacity_label = gtk::Label::builder()
            .css_classes(["caption", "dim-label", "status-capacity"])
            .build();

        let status_bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_start(14)
            .margin_end(14)
            .margin_top(5)
            .margin_bottom(5)
            .css_classes(["status-bar"])
            .build();
        status_bar.append(&status_label);
        status_bar.append(&capacity_label);

        let banner = adw::Banner::builder().revealed(false).build();

        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        content.append(&search_bar);
        content.append(&banner);
        content.append(view.widget());
        content.append(jobs.widget());
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&status_bar);

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&content));

        // Both halves become floating panes; the window background is the gap.
        sidebar.widget().add_css_class("pane");
        sidebar.widget().add_css_class("pane-sidebar");
        content.add_css_class("pane");
        content.add_css_class("pane-content");

        let split = adw::OverlaySplitView::builder()
            .sidebar(sidebar.widget())
            .content(&toasts)
            // A sidebar's useful width is set by its contents — the longest
            // place name, the capacity ring and the eject button — not by how
            // wide the monitor is. The fraction only decides where inside this
            // narrow band it lands, so the sidebar stays essentially constant
            // from a compact window up to fullscreen instead of swelling to
            // 400px and stealing the space the files need.
            .min_sidebar_width(220.0)
            .max_sidebar_width(264.0)
            .sidebar_width_fraction(0.16)
            .show_sidebar(config.borrow().sidebar_visible)
            .build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&split));
        window.set_content(Some(&toolbar));

        let context_menu = crate::ui::menu::ContextMenu::new(&window);

        sidebar_toggle
            .bind_property("active", &split, "show-sidebar")
            .bidirectional()
            .sync_create()
            .build();

        let this = Rc::new(Self {
            window,
            app: app.clone(),
            config,
            view,
            sidebar,
            pathbar,
            jobs,
            toasts,
            split,
            search_bar,
            search_entry,
            status_label,
            capacity_label,
            banner,
            back_button,
            forward_button,
            up_button,
            history: RefCell::new(History::new(start.clone())),
            location: RefCell::new(Location::Directory(start.clone())),
            entries: RefCell::new(Vec::new()),
            volumes: RefCell::new(Vec::new()),
            clipboard: RefCell::new(None),
            scan_generation: Cell::new(0),
            scan_cancellable: RefCell::new(gio::Cancellable::new()),
            dir_monitor: RefCell::new(None),
            reload_timer: RefCell::new(None),
            save_timer: RefCell::new(None),
            search_job: RefCell::new(None),
            search_generation: Cell::new(0),
            banner_action: RefCell::new(BannerAction::None),
            navigating: Cell::new(false),
            context_menu,
        });

        this.apply_theme();
        this.install_actions();
        this.wire_widgets();
        this.refresh_drives();
        this.watch_drive_changes();
        this.refresh_trash_count();
        this.sidebar.set_favourites(this.config.borrow().favourites.clone());
        this.navigate_to(Location::Directory(start), false);

        this
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn gtk_window(&self) -> &adw::ApplicationWindow {
        &self.window
    }

    pub(crate) fn widget(&self) -> gtk::Widget {
        self.window.clone().upcast()
    }

    // ── wiring ─────────────────────────────────────────────────────────────

    fn wire_widgets(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.pathbar.connect_navigate(move |path| {
            let Some(this) = weak.upgrade() else { return };
            this.open_path(path);
        });

        let weak = Rc::downgrade(self);
        self.view.connect_activate(move |obj| {
            let Some(this) = weak.upgrade() else { return };
            this.activate_item(&obj.entry());
        });

        let weak = Rc::downgrade(self);
        self.view.connect_selection_changed(move || {
            if let Some(this) = weak.upgrade() {
                this.update_status();
            }
        });

        let weak = Rc::downgrade(self);
        self.view.connect_context_menu(move |(x, y)| {
            if let Some(this) = weak.upgrade() {
                this.show_context_menu(x, y);
            }
        });

        let weak = Rc::downgrade(self);
        self.view.connect_drop(move |(paths, onto)| {
            let Some(this) = weak.upgrade() else { return };
            let destination = onto.or_else(|| this.current_dir());
            let Some(destination) = destination else { return };
            this.drop_files(paths, destination);
        });

        let weak = Rc::downgrade(self);
        self.sidebar.connect_navigate(move |path| {
            if let Some(this) = weak.upgrade() {
                this.open_path(path);
            }
        });

        let weak = Rc::downgrade(self);
        self.sidebar.connect_open_trash(move || {
            if let Some(this) = weak.upgrade() {
                this.navigate_to(Location::Trash, true);
            }
        });

        let weak = Rc::downgrade(self);
        self.sidebar.connect_mount(move |volume| {
            if let Some(this) = weak.upgrade() {
                this.mount_volume(volume);
            }
        });

        let weak = Rc::downgrade(self);
        self.sidebar.connect_unmount(move |volume| {
            if let Some(this) = weak.upgrade() {
                this.unmount_volume(volume, false);
            }
        });

        let weak = Rc::downgrade(self);
        self.sidebar.connect_eject(move |volume| {
            if let Some(this) = weak.upgrade() {
                this.unmount_volume(volume, true);
            }
        });

        let weak = Rc::downgrade(self);
        self.sidebar.connect_favourite_removed(move |path| {
            let Some(this) = weak.upgrade() else { return };
            this.config.borrow_mut().toggle_favourite(&path);
            this.sidebar.set_favourites(this.config.borrow().favourites.clone());
            this.schedule_save();
        });

        let weak = Rc::downgrade(self);
        self.sidebar.connect_favourite_added(move |path| {
            let Some(this) = weak.upgrade() else { return };
            this.config.borrow_mut().toggle_favourite(&path);
            this.sidebar.set_favourites(this.config.borrow().favourites.clone());
            this.schedule_save();
        });

        let weak = Rc::downgrade(self);
        self.sidebar.connect_drop(move |(paths, destination)| {
            if let Some(this) = weak.upgrade() {
                this.drop_files(paths, destination);
            }
        });

        let weak = Rc::downgrade(self);
        self.search_entry.connect_search_changed(move |entry| {
            let Some(this) = weak.upgrade() else { return };
            this.update_search(entry.text().as_str());
        });

        // Leaving search should return focus to the files, not strand it in a
        // hidden entry.
        let weak = Rc::downgrade(self);
        self.search_bar.connect_notify_local(Some("search-mode-enabled"), move |bar, _| {
            let Some(this) = weak.upgrade() else { return };
            if !bar.is_search_mode() {
                this.search_entry.set_text("");
                this.view.set_search("");
                this.view.focus_first();
                this.update_status();
            }
        });

        // Ctrl+scroll zooms, matching every other file manager.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |controller, _, dy| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            if !controller
                .current_event_state()
                .contains(gtk::gdk::ModifierType::CONTROL_MASK)
            {
                return glib::Propagation::Proceed;
            }
            this.zoom(if dy < 0.0 { 1 } else { -1 });
            glib::Propagation::Stop
        });
        self.view.widget().add_controller(scroll);

        // Mouse back/forward buttons.
        let buttons = gtk::GestureClick::new();
        buttons.set_button(0);
        let weak = Rc::downgrade(self);
        buttons.connect_pressed(move |gesture, _, _, _| {
            let Some(this) = weak.upgrade() else { return };
            match gesture.current_button() {
                8 => this.go_back(),
                9 => this.go_forward(),
                _ => return,
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        self.window.add_controller(buttons);

        let weak = Rc::downgrade(self);
        self.banner.connect_button_clicked(move |_| {
            let Some(this) = weak.upgrade() else { return };
            let action = *this.banner_action.borrow();
            match action {
                BannerAction::EmptyTrash => {
                    WidgetExt::activate_action(&this.window, "win.empty-trash", None).ok();
                }
                BannerAction::StopSearch => this.stop_search(),
                BannerAction::None => {}
            }
        });

        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |window| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            // Persist geometry on the way out; the debounced save may not have
            // fired yet for a resize that happened moments ago.
            {
                let mut cfg = this.config.borrow_mut();
                cfg.window_maximized = window.is_maximized();
                if !cfg.window_maximized {
                    cfg.window_width = window.default_width();
                    cfg.window_height = window.default_height();
                }
                cfg.sidebar_visible = this.split.shows_sidebar();
                let _ = cfg.save();
            }
            glib::Propagation::Proceed
        });
    }

    // ── navigation ─────────────────────────────────────────────────────────

    pub(crate) fn current_dir(&self) -> Option<PathBuf> {
        match &*self.location.borrow() {
            Location::Directory(p) => Some(p.clone()),
            Location::Trash => None,
        }
    }

    pub(crate) fn in_trash(&self) -> bool {
        matches!(&*self.location.borrow(), Location::Trash)
    }

    /// Navigates to a path, or opens it if it turns out to be a file.
    pub(crate) fn open_path(self: &Rc<Self>, path: PathBuf) {
        if path.is_dir() {
            self.navigate_to(Location::Directory(path), true);
            return;
        }
        if path.is_file() {
            // Typing a file's path should reveal it, not silently do nothing.
            if let Some(parent) = path.parent() {
                self.navigate_to(Location::Directory(parent.to_path_buf()), true);
                let target = path.clone();
                let weak = Rc::downgrade(self);
                glib::timeout_add_local_once(std::time::Duration::from_millis(120), move || {
                    if let Some(this) = weak.upgrade() {
                        this.view.select_paths(&[target]);
                    }
                });
            }
            return;
        }
        self.toast(&format!("“{}” does not exist", path.display()));
    }

    pub(crate) fn navigate_to(self: &Rc<Self>, location: Location, push_history: bool) {
        if push_history && let Some(path) = location.as_path() {
            self.history.borrow_mut().push(path.to_path_buf());
        }

        *self.location.borrow_mut() = location.clone();

        // Cancel any scan still running for the previous directory.
        self.scan_cancellable.borrow().cancel();
        *self.scan_cancellable.borrow_mut() = gio::Cancellable::new();
        let generation = self.scan_generation.get() + 1;
        self.scan_generation.set(generation);

        self.view.clear();
        self.entries.borrow_mut().clear();
        // A walk rooted at the folder we are leaving is no longer wanted.
        self.search_generation.set(self.search_generation.get() + 1);
        *self.search_job.borrow_mut() = None;

        self.navigating.set(true);
        self.search_bar.set_search_mode(false);
        self.search_entry.set_text("");
        self.view.set_search("");
        self.navigating.set(false);

        match &location {
            Location::Directory(path) => {
                self.window.set_title(Some(&display_title(path)));
                self.pathbar.set_path(path);
                self.sidebar.set_current(path);
                self.set_banner(BannerAction::None, "", None);
                self.install_monitor(path);
                self.load_directory(path.clone(), generation);
            }
            Location::Trash => {
                self.window.set_title(Some("Trash"));
                self.sidebar.set_current(Path::new("<trash>"));
                *self.dir_monitor.borrow_mut() = None;
                self.load_trash(generation);
            }
        }

        self.update_nav_buttons();
    }

    fn load_directory(self: &Rc<Self>, path: PathBuf, generation: u64) {
        let weak = Rc::downgrade(self);
        let cancellable = self.scan_cancellable.borrow().clone();

        glib::spawn_future_local(async move {
            let _span = crate::trace::Span::new(format!("scan {}", path.display()));
            let result = {
                let weak = weak.clone();
                let path = path.clone();
                scan::scan_dir(&path.clone(), &cancellable, move |batch| {
                    let Some(this) = weak.upgrade() else { return };
                    if this.scan_generation.get() != generation {
                        return;
                    }
                    this.entries.borrow_mut().extend(batch.iter().cloned());
                    this.view.append_batch(batch);
                })
                .await
            };

            let Some(this) = weak.upgrade() else { return };
            if this.scan_generation.get() != generation {
                return;
            }

            if let Err(err) = result {
                this.set_banner(
                    BannerAction::None,
                    &format!("Could not read this folder: {}", err.message()),
                    None,
                );
            }

            this.update_status();
            this.update_capacity(&path);
            this.view.focus_first();
        });
    }

    fn load_trash(self: &Rc<Self>, generation: u64) {
        let weak = Rc::downgrade(self);

        glib::spawn_future_local(async move {
            // Reading trash metadata is a directory walk plus a parse per item;
            // do it off the main thread so a large trash doesn't stall the UI.
            let (tx, rx) = async_channel::bounded(1);
            std::thread::Builder::new()
                .name("fileman-trash-list".into())
                .spawn(move || {
                    let _ = tx.send_blocking(crate::fs::trash::list_trash());
                })
                .ok();

            let Ok(items) = rx.recv().await else { return };
            let Some(this) = weak.upgrade() else { return };
            if this.scan_generation.get() != generation {
                return;
            }

            let entries: Vec<FileEntry> = items
                .iter()
                .filter_map(trash_entry)
                .collect();

            *this.entries.borrow_mut() = entries.clone();
            this.view.append_batch(entries);

            this.pathbar.set_virtual("Trash", "user-trash-symbolic");
            let (title, button) = if items.is_empty() {
                ("Trash is empty", None)
            } else {
                (
                    "Items in the Trash keep their original location — restore puts them back",
                    Some("Empty Trash"),
                )
            };
            this.set_banner(BannerAction::EmptyTrash, title, button);
            this.update_status();
        });
    }

    /// Watches the open directory so external changes show up without a manual
    /// refresh.
    fn install_monitor(self: &Rc<Self>, path: &Path) {
        let file = gio::File::for_path(path);
        let Ok(monitor) =
            file.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        else {
            *self.dir_monitor.borrow_mut() = None;
            return;
        };

        let weak = Rc::downgrade(self);
        monitor.connect_changed(move |_, _, _, _| {
            let Some(this) = weak.upgrade() else { return };
            this.schedule_reload();
        });

        *self.dir_monitor.borrow_mut() = Some(monitor);
    }

    /// Coalesces a burst of filesystem events into one reload.
    ///
    /// Extracting an archive can emit thousands of events; without this the
    /// window would re-scan for every file created.
    fn schedule_reload(self: &Rc<Self>) {
        if let Some(existing) = self.reload_timer.borrow_mut().take() {
            existing.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
            let Some(this) = weak.upgrade() else { return };
            *this.reload_timer.borrow_mut() = None;
            this.reload();
        });
        *self.reload_timer.borrow_mut() = Some(id);
    }

    pub(crate) fn reload(self: &Rc<Self>) {
        // A reload triggered after a delete may find the directory itself gone.
        if let Some(current) = self.current_dir()
            && !current.is_dir()
        {
            self.recover_from_missing_directory(&current);
            return;
        }

        let selected = self.view.selected_paths();
        let location = self.location.borrow().clone();
        self.navigate_to(location, false);

        // Restore the selection once the new listing has had a chance to load.
        if !selected.is_empty() {
            let weak = Rc::downgrade(self);
            glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                if let Some(this) = weak.upgrade() {
                    this.view.select_paths(&selected);
                }
            });
        }
    }

    pub(crate) fn go_back(self: &Rc<Self>) {
        let Some(path) = self.history.borrow_mut().go_back() else { return };
        self.navigate_to(Location::Directory(path), false);
    }

    pub(crate) fn go_forward(self: &Rc<Self>) {
        let Some(path) = self.history.borrow_mut().go_forward() else { return };
        self.navigate_to(Location::Directory(path), false);
    }

    pub(crate) fn go_up(self: &Rc<Self>) {
        let Some(current) = self.current_dir() else { return };
        let Some(parent) = current.parent() else { return };
        self.navigate_to(Location::Directory(parent.to_path_buf()), true);
    }

    fn update_nav_buttons(&self) {
        let history = self.history.borrow();
        self.back_button.set_sensitive(history.can_go_back());
        self.forward_button.set_sensitive(history.can_go_forward());

        let parent = self.current_dir().and_then(|p| p.parent().map(|p| p.to_path_buf()));
        self.up_button.set_sensitive(parent.is_some());

        // Back and Up land in the same place whenever you just came from the
        // parent, which makes them look redundant. Naming the destination is
        // what actually distinguishes them.
        self.up_button.set_tooltip_text(Some(&match &parent {
            Some(p) => format!("Up to “{}”  (Alt+Up)", display_title(p)),
            None => "Up  (Alt+Up)".to_string(),
        }));
        self.back_button.set_tooltip_text(Some(&if history.can_go_back() {
            format!("Back to “{}”  (Alt+Left)", display_title(history.previous()))
        } else {
            "Back  (Alt+Left)".to_string()
        }));
        self.forward_button.set_tooltip_text(Some(&if history.can_go_forward() {
            format!("Forward to “{}”  (Alt+Right)", display_title(history.next()))
        } else {
            "Forward  (Alt+Right)".to_string()
        }));
    }

    /// Opens a file with its default application, or enters a directory.
    pub(crate) fn activate_item(self: &Rc<Self>, entry: &FileEntry) {
        if entry.is_dir {
            self.navigate_to(Location::Directory(entry.path.clone()), true);
            return;
        }
        if self.in_trash() {
            self.toast("Restore this item before opening it");
            return;
        }
        self.launch(&entry.path);
    }

    pub(crate) fn launch(self: &Rc<Self>, path: &Path) {
        let file = gio::File::for_path(path);
        let launcher = gtk::FileLauncher::new(Some(&file));
        let weak = Rc::downgrade(self);

        glib::spawn_future_local(async move {
            let Some(this) = weak.upgrade() else { return };
            if let Err(err) = launcher.launch_future(Some(&this.window)).await {
                // A dismissed "choose an application" prompt reports as
                // cancelled; that isn't an error worth shouting about.
                if !err.matches(gio::IOErrorEnum::Cancelled) {
                    this.toast(&format!("Could not open: {}", err.message()));
                }
            }
        });
    }

    // ── status ─────────────────────────────────────────────────────────────

    pub(crate) fn update_status(&self) {
        let selected = self.view.selected();
        let (visible, bytes, folders) = self.view.visible_stats();

        let text = if selected.is_empty() {
            let files = visible.saturating_sub(folders);
            let mut parts = Vec::new();
            if folders > 0 {
                parts.push(format!("{folders} folder{}", plural(folders as u64)));
            }
            if files > 0 || folders == 0 {
                parts.push(format!("{files} file{}", plural(files as u64)));
            }
            if bytes > 0 {
                parts.push(humansize::format_size(bytes, humansize::DECIMAL));
            }
            parts.join(", ")
        } else {
            let selected_bytes: u64 =
                selected.iter().map(|o| o.entry()).filter(|e| !e.is_dir).map(|e| e.size).sum();
            if selected.len() == 1 {
                let entry = selected[0].entry();
                if entry.is_dir {
                    format!("“{}” selected (folder)", entry.display_name)
                } else {
                    format!(
                        "“{}” selected ({})",
                        entry.display_name,
                        humansize::format_size(entry.size, humansize::DECIMAL)
                    )
                }
            } else {
                format!(
                    "{} items selected ({})",
                    selected.len(),
                    humansize::format_size(selected_bytes, humansize::DECIMAL)
                )
            }
        };

        self.status_label.set_text(&text);
    }

    fn update_capacity(&self, path: &Path) {
        let label = self.capacity_label.clone();
        let path = path.to_path_buf();
        glib::spawn_future_local(async move {
            match scan::filesystem_usage(&path).await {
                Some((free, total)) => label.set_text(&format!(
                    "{} free of {}",
                    humansize::format_size(free, humansize::DECIMAL),
                    humansize::format_size(total, humansize::DECIMAL)
                )),
                None => label.set_text(""),
            }
        });
    }

    /// Applies a query: filter the current folder immediately, then search the
    /// whole subtree in the background.
    ///
    /// The local filter is instant and covers the common case; the recursive
    /// walk is what makes searching from Home actually find anything.
    pub(crate) fn update_search(self: &Rc<Self>, query: &str) {
        self.view.set_search(query);
        self.update_status();
        self.stop_search();

        let query = query.trim().to_string();
        // One or two characters under a home directory matches thousands of
        // files and helps nobody; wait until the query means something.
        if query.chars().count() < 2 || self.in_trash() {
            self.set_banner(BannerAction::None, "", None);
            return;
        }
        let Some(root) = self.current_dir() else { return };

        let generation = self.search_generation.get() + 1;
        self.search_generation.set(generation);

        let skip_hidden = !self.config.borrow().show_hidden;
        let handle = crate::fs::search::start(root.clone(), query.clone(), skip_hidden);
        let results = handle.results.clone();
        *self.search_job.borrow_mut() = Some(handle);

        self.set_banner(
            BannerAction::StopSearch,
            &format!("Searching for “{query}” in {}…", display_title(&root)),
            Some("Stop"),
        );

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let mut found = 0usize;
            while let Ok(event) = results.recv().await {
                let Some(this) = weak.upgrade() else { return };
                if this.search_generation.get() != generation {
                    return;
                }
                match event {
                    crate::fs::search::SearchEvent::Matches(batch) => {
                        found += batch.len();
                        this.view.append_batch(batch);
                        this.update_status();
                    }
                    crate::fs::search::SearchEvent::Finished { total, truncated } => {
                        let message = match (total, truncated) {
                            (0, _) => format!("No matches for “{query}” below this folder"),
                            (n, true) => format!("Showing the first {n} matches — narrow the search to see fewer"),
                            (n, false) => format!("{n} match{} below this folder", if n == 1 { "" } else { "es" }),
                        };
                        this.set_banner(BannerAction::None, &message, None);
                        this.update_status();
                        return;
                    }
                }
                let _ = found;
            }
        });
    }

    /// Cancels any running recursive search.
    pub(crate) fn stop_search(self: &Rc<Self>) {
        // Bump the generation so late batches from the old walk are ignored.
        self.search_generation.set(self.search_generation.get() + 1);
        // Dropping the handle sets its cancel flag.
        *self.search_job.borrow_mut() = None;
        if *self.banner_action.borrow() == BannerAction::StopSearch {
            self.set_banner(BannerAction::None, "", None);
        }
    }

    fn set_banner(&self, action: BannerAction, title: &str, button: Option<&str>) {
        *self.banner_action.borrow_mut() = action;
        self.banner.set_button_label(button);
        if title.is_empty() {
            self.banner.set_revealed(false);
            return;
        }
        self.banner.set_title(title);
        self.banner.set_revealed(true);
    }

    pub(crate) fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    /// A toast with an action button, e.g. "Undo" after a trash operation.
    pub(crate) fn toast_with_action(
        &self,
        message: &str,
        button: &str,
        callback: impl Fn() + 'static,
    ) {
        let toast = adw::Toast::new(message);
        toast.set_button_label(Some(button));
        toast.connect_button_clicked(move |_| callback());
        self.toasts.add_toast(toast);
    }

    // ── appearance ─────────────────────────────────────────────────────────

    pub(crate) fn apply_theme(&self) {
        let scheme = match self.config.borrow().theme {
            Theme::System => adw::ColorScheme::Default,
            Theme::Light => adw::ColorScheme::ForceLight,
            Theme::Dark => adw::ColorScheme::ForceDark,
        };
        adw::StyleManager::default().set_color_scheme(scheme);
    }

    pub(crate) fn zoom(self: &Rc<Self>, delta: i32) {
        let changed = self.config.borrow_mut().zoom(delta);
        if !changed {
            return;
        }
        // Cached thumbnails are keyed by size, so old entries are dead weight.
        crate::ui::thumbs::clear();
        self.view.refresh_items();
        self.schedule_save();
        self.toast(&format!("Icon size: {}px", self.config.borrow().icon_size));
    }

    pub(crate) fn set_view_mode(self: &Rc<Self>, mode: ViewMode) {
        self.config.borrow_mut().view_mode = mode;
        self.view.set_view_mode(mode);
        self.schedule_save();
    }

    /// Writes the config a moment after the last change, so rapid adjustments
    /// (a zoom held down, a window drag) cost one write rather than dozens.
    pub(crate) fn schedule_save(self: &Rc<Self>) {
        if let Some(existing) = self.save_timer.borrow_mut().take() {
            existing.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
            let Some(this) = weak.upgrade() else { return };
            *this.save_timer.borrow_mut() = None;
            if let Err(err) = this.config.borrow().save() {
                eprintln!("fileman: could not save settings: {err}");
            }
        });
        *self.save_timer.borrow_mut() = Some(id);
    }

    // ── drives ─────────────────────────────────────────────────────────────

    /// Reloads the volume list on a worker thread and hands it to the sidebar.
    pub(crate) fn refresh_drives(self: &Rc<Self>) {
        let (tx, rx) = async_channel::bounded(1);
        std::thread::Builder::new()
            .name("fileman-drives".into())
            .spawn(move || {
                let _ = tx.send_blocking(crate::drives::list_volumes());
            })
            .ok();

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(result) = rx.recv().await else { return };
            let Some(this) = weak.upgrade() else { return };
            match result {
                Ok(volumes) => {
                    *this.volumes.borrow_mut() = volumes.clone();
                    this.sidebar.set_volumes(volumes);
                    if let Some(current) = this.current_dir() {
                        this.sidebar.set_current(&current);
                    }
                }
                Err(err) => eprintln!("fileman: could not list drives: {err}"),
            }
        });
    }

    /// Keeps the sidebar in step with drives being plugged in, unplugged,
    /// mounted or unmounted — including by other applications.
    fn watch_drive_changes(self: &Rc<Self>) {
        let changes = crate::drives::subscribe_changes();
        let weak = Rc::downgrade(self);

        glib::spawn_future_local(async move {
            while changes.recv().await.is_ok() {
                let Some(this) = weak.upgrade() else { return };
                this.refresh_drives();

                // A drive vanishing underneath the view leaves it pointed at a
                // path that no longer resolves; fall back to somewhere real.
                if let Some(current) = this.current_dir()
                    && !current.is_dir()
                {
                    this.recover_from_missing_directory(&current);
                }
            }
        });
    }

    /// Navigates away from a directory that has disappeared.
    pub(crate) fn recover_from_missing_directory(self: &Rc<Self>, gone: &Path) {
        self.history.borrow_mut().forget(gone);
        let fallback = self.history.borrow().current().to_path_buf();
        self.toast(&format!("“{}” is no longer available", gone.display()));
        self.navigate_to(Location::Directory(fallback), false);
    }

    pub(crate) fn refresh_trash_count(self: &Rc<Self>) {
        let (tx, rx) = async_channel::bounded(1);
        std::thread::Builder::new()
            .name("fileman-trash-count".into())
            .spawn(move || {
                let _ = tx.send_blocking(crate::fs::trash::trash_count());
            })
            .ok();

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(count) = rx.recv().await else { return };
            if let Some(this) = weak.upgrade() {
                this.sidebar.set_trash_count(count);
            }
        });
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn nav_button(icon: &str, tooltip: &str, action: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .action_name(action)
        .build()
}

pub(crate) fn view_icon(mode: ViewMode) -> &'static str {
    match mode {
        ViewMode::Grid => "view-list-symbolic",
        ViewMode::List => "view-grid-symbolic",
    }
}

fn display_title(path: &Path) -> String {
    if Some(path) == dirs::home_dir().as_deref() {
        return "Home".to_string();
    }
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Builds a listable entry for a trashed item, showing the name it had
/// originally rather than the mangled name inside `Trash/files`.
fn trash_entry(item: &crate::fs::trash::TrashItem) -> Option<FileEntry> {
    let md = std::fs::symlink_metadata(&item.file_path).ok()?;
    let display_name = item
        .original_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| item.name.clone());

    let content_type = if item.is_dir {
        "inode/directory".to_string()
    } else {
        gio::functions::content_type_guess(Some(Path::new(&display_name)), None).0.to_string()
    };

    Some(FileEntry {
        path: item.file_path.clone(),
        name: item.name.clone(),
        search_key: display_name.to_lowercase(),
        display_name,
        is_dir: item.is_dir,
        is_symlink: md.file_type().is_symlink(),
        symlink_target: None,
        is_hidden: false,
        size: item.size,
        modified: md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64),
        content_type,
        can_read: true,
        can_write: true,
        can_execute: false,
        mode: 0,
    })
}

fn build_zoom_popover(config: &Rc<RefCell<Config>>) -> gtk::Popover {
    let boxed = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(10)
        .margin_end(10)
        .build();

    boxed.append(&gtk::Label::builder().label("Icon size").css_classes(["heading"]).build());

    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .build();
    buttons.append(
        &gtk::Button::builder()
            .icon_name("zoom-out-symbolic")
            .action_name("win.zoom-out")
            .tooltip_text("Smaller (Ctrl+−)")
            .build(),
    );
    buttons.append(
        &gtk::Button::builder()
            .label("Reset")
            .action_name("win.zoom-reset")
            .hexpand(true)
            .build(),
    );
    buttons.append(
        &gtk::Button::builder()
            .icon_name("zoom-in-symbolic")
            .action_name("win.zoom-in")
            .tooltip_text("Larger (Ctrl++)")
            .build(),
    );
    boxed.append(&buttons);

    let current = gtk::Label::builder()
        .label(format!("{}px", config.borrow().icon_size))
        .css_classes(["dim-label", "caption"])
        .build();
    boxed.append(&current);

    gtk::Popover::builder().child(&boxed).build()
}

fn build_main_menu() -> gio::Menu {
    let menu = gio::Menu::new();

    let files = gio::Menu::new();
    files.append(Some("New Folder"), Some("win.new-folder"));
    files.append(Some("New File"), Some("win.new-file"));
    files.append(Some("Open Terminal Here"), Some("win.open-terminal"));
    menu.append_section(None, &files);

    let view = gio::Menu::new();
    view.append(Some("Show Hidden Files"), Some("win.toggle-hidden"));
    view.append(Some("Folders Before Files"), Some("win.toggle-dirs-first"));
    view.append(Some("Show Thumbnails"), Some("win.toggle-thumbnails"));
    menu.append_section(Some("View"), &view);

    let sort = gio::Menu::new();
    sort.append(Some("Name"), Some("win.sort-by::name"));
    sort.append(Some("Size"), Some("win.sort-by::size"));
    sort.append(Some("Modified"), Some("win.sort-by::modified"));
    sort.append(Some("Type"), Some("win.sort-by::kind"));
    sort.append(Some("Descending"), Some("win.sort-descending"));
    menu.append_submenu(Some("Sort By"), &sort);

    let theme = gio::Menu::new();
    theme.append(Some("Follow System"), Some("win.theme::system"));
    theme.append(Some("Light"), Some("win.theme::light"));
    theme.append(Some("Dark"), Some("win.theme::dark"));
    menu.append_submenu(Some("Appearance"), &theme);

    let app = gio::Menu::new();
    app.append(Some("Preferences"), Some("win.preferences"));
    app.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
    app.append(Some("About Fileman"), Some("win.about"));
    menu.append_section(None, &app);

    menu
}

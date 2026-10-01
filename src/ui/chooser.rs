//! The dialog other applications get when they ask for a file.
//!
//! Started by the portal daemon as `fileman --file-chooser`, with the request
//! on standard input and the answer printed to standard output. It is a
//! separate, short-lived process on purpose: the daemon that waits all session
//! stays tiny, and the toolkit is paid for only while a dialog is on screen.
//!
//! It reuses the file view, so the places, sorting, icon size and thumbnails
//! are the ones the user already set up in Fileman — which is the point of
//! choosing Fileman as the dialog in the first place.

use std::{
    cell::{Cell, RefCell},
    io::Read,
    path::{Path, PathBuf},
    rc::Rc,
};

use adw::prelude::*;
use gtk::{gdk, gio, glib, prelude::Cast};

use crate::{
    config::{Config, ViewMode},
    fs::scan,
    portal::{self, Kind, Request, Response},
    ui::{file_object::FileObject, file_view::FileView, pathbar::PathBar},
};

/// Reads the request, shows the dialog, prints the answer.
pub fn run_portal_dialog() -> glib::ExitCode {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return glib::ExitCode::FAILURE;
    }
    let request: Request = match serde_json::from_str(&input) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("fileman: the file chooser request could not be read: {error}");
            return glib::ExitCode::FAILURE;
        }
    };

    // Its own application id and NON_UNIQUE: handing this to the running
    // Fileman would raise that window and leave the asking application waiting
    // forever for an answer nobody is going to give.
    let app = adw::Application::builder()
        .application_id("dev.fileman.Files.FileChooser")
        .flags(gio::ApplicationFlags::NON_UNIQUE | gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    let answer: Rc<RefCell<Response>> = Rc::new(RefCell::new(Response::default()));
    // The dialog has to outlive the callback that builds it. GTK owns the
    // widgets, so the window stays on screen either way — but every handler
    // holds a weak reference back to this struct, and once it is dropped they
    // all quietly do nothing: the sidebar stops navigating, Save stops saving,
    // and the listing never arrives because the scan's callback returns early.
    // It looks like a dialog that merely does not work.
    let held: Rc<RefCell<Option<Rc<Chooser>>>> = Rc::new(RefCell::new(None));
    {
        let (request, answer) = (request.clone(), Rc::clone(&answer));
        let held = Rc::clone(&held);
        app.connect_command_line(move |app, _| {
            // The accent colour and spacing the user picked, so the dialog
            // matches the rest of Fileman rather than looking borrowed.
            let accent = Config::load().accent_color;
            crate::app::load_stylesheet(&accent);
            let chooser = Chooser::new(app, request.clone(), Rc::clone(&answer));
            chooser.present();
            *held.borrow_mut() = Some(chooser);
            glib::ExitCode::SUCCESS
        });
    }
    app.run_with_args::<&str>(&[]);

    // Printed after the loop ends, so the daemon reads one complete answer.
    match serde_json::to_string(&*answer.borrow()) {
        Ok(json) => {
            println!("{json}");
            glib::ExitCode::SUCCESS
        }
        Err(_) => glib::ExitCode::FAILURE,
    }
}

struct Chooser {
    window: adw::ApplicationWindow,
    request: Request,
    answer: Rc<RefCell<Response>>,
    view: Rc<FileView>,
    pathbar: Rc<PathBar>,
    name: gtk::Entry,
    accept: gtk::Button,
    filters: gtk::DropDown,
    sidebar: Rc<crate::ui::sidebar::Sidebar>,
    config: Rc<RefCell<Config>>,
    current: RefCell<PathBuf>,
    generation: Cell<u64>,
}

impl Chooser {
    fn new(app: &adw::Application, request: Request, answer: Rc<RefCell<Response>>) -> Rc<Self> {
        let config = Rc::new(RefCell::new(Config::load()));
        // A chooser is a list of names to pick from, not a gallery.
        config.borrow_mut().view_mode = ViewMode::List;

        let view = FileView::new(Rc::clone(&config));
        // The dialog is far narrower than the main window, and the fixed-width
        // columns would leave nothing for the file names.
        view.set_compact_columns(true);
        let pathbar = PathBar::new();

        let accept_label = request.accept_label.clone().unwrap_or_else(|| {
            match request.kind() {
                Kind::Open => "Open",
                _ => "Save",
            }
            .to_string()
        });
        let accept = gtk::Button::builder()
            .label(accept_label.replace('_', ""))
            .css_classes(["suggested-action"])
            .sensitive(false)
            .build();
        let cancel = gtk::Button::with_label("Cancel");

        let title = if request.title.is_empty() {
            match request.kind() {
                Kind::Open => "Open File".to_string(),
                _ => "Save File".to_string(),
            }
        } else {
            request.title.clone()
        };
        let hidden = gtk::ToggleButton::builder()
            .icon_name("view-conceal-symbolic")
            .tooltip_text("Show hidden files (Ctrl+H)")
            .active(config.borrow().show_hidden)
            .build();
        let view_mode = gtk::Button::builder()
            .icon_name("view-grid-symbolic")
            .tooltip_text("Switch between list and grid (Ctrl+Shift+V)")
            .build();

        let header = adw::HeaderBar::builder().show_end_title_buttons(false).build();
        header.set_title_widget(Some(&adw::WindowTitle::new(&title, &asking(&request))));
        header.pack_start(&cancel);
        header.pack_start(&hidden);
        header.pack_start(&view_mode);
        header.pack_end(&accept);

        // The main window's sidebar, as it is. Rebuilding it here is what made
        // the dialog look like a different program: oversized rows, no section
        // headings, no capacity rings, and padding that drifted apart the
        // moment either side changed.
        let sidebar = crate::ui::sidebar::Sidebar::new();
        sidebar.set_chooser_mode(true);
        sidebar.set_favourites(config.borrow().favourites.clone());
        let places_pane = sidebar.widget().clone();
        places_pane.set_width_request(220);

        let name = gtk::Entry::builder()
            .placeholder_text("File name")
            .hexpand(true)
            .activates_default(true)
            .text(request.current_name.clone().unwrap_or_default())
            .build();
        let name_row = gtk::Box::builder()
            .spacing(10)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(12)
            .margin_end(12)
            .build();
        name_row.append(&gtk::Label::new(Some("Save as")));
        name_row.append(&name);
        name_row.set_visible(request.kind() == Kind::Save);

        let labels: Vec<String> = std::iter::once("All files".to_string())
            .chain(request.filters.iter().map(|f| f.name.clone()))
            .collect();
        let filters = gtk::DropDown::from_strings(&labels.iter().map(String::as_str).collect::<Vec<_>>());
        filters.set_valign(gtk::Align::Center);
        // The application's own preferred filter, when it named one.
        filters.set_selected(request.current_filter.map_or(0, |index| index as u32 + 1));
        filters.set_visible(!request.filters.is_empty());

        let footer = gtk::Box::builder()
            .spacing(10)
            .margin_top(6)
            .margin_bottom(10)
            .margin_start(12)
            .margin_end(12)
            .build();
        footer.append(&gtk::Label::builder().label("").hexpand(true).build());
        footer.append(&filters);
        footer.set_visible(!request.filters.is_empty());

        let browser = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        browser.append(pathbar.widget());
        browser.append(view.widget());

        let split = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).build();
        split.append(&places_pane);
        split.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        split.append(&browser);

        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        content.append(&split);
        content.append(&name_row);
        content.append(&footer);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(940)
            .default_height(640)
            .title(&title)
            .content(&toolbar)
            .build();

        let this = Rc::new(Self {
            window,
            request,
            answer,
            view,
            pathbar,
            name,
            accept,
            filters,
            sidebar: Rc::clone(&sidebar),
            config,
            current: RefCell::new(PathBuf::new()),
            generation: Cell::new(0),
        });

        this.wire(&cancel, &hidden, &view_mode);
        this
    }

    fn wire(self: &Rc<Self>, cancel: &gtk::Button, hidden: &gtk::ToggleButton, view_mode: &gtk::Button) {
        let weak = Rc::downgrade(self);
        cancel.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.window.close();
            }
        });

        let weak = Rc::downgrade(self);
        self.accept.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.accept();
            }
        });

        // Sidebar: a folder is opened, a drive is mounted first.
        let weak = Rc::downgrade(self);
        self.sidebar.connect_navigate(move |path| {
            if let Some(this) = weak.upgrade() {
                this.navigate(path);
            }
        });
        let weak = Rc::downgrade(self);
        self.sidebar.connect_mount(move |volume| {
            if let Some(this) = weak.upgrade() {
                this.open_volume(volume);
            }
        });
        let weak = Rc::downgrade(self);
        self.sidebar.connect_open_server(move |server| {
            let Some(this) = weak.upgrade() else { return };
            // Already-mounted shares have a path; connecting a new one is the
            // main window's job.
            if let Some(share) = crate::fs::remote::mounted()
                .into_iter()
                .find(|m| m.uri == server.uri())
                .and_then(|m| m.path)
            {
                this.navigate(share);
            }
        });
        let weak = Rc::downgrade(self);
        self.sidebar.connect_open_cloud(move |account| {
            let Some(this) = weak.upgrade() else { return };
            if account.mounted {
                this.navigate(account.mount_point);
            }
        });

        // Hidden files, as Ctrl+H does everywhere else.
        let weak = Rc::downgrade(self);
        hidden.connect_toggled(move |toggle| {
            let Some(this) = weak.upgrade() else { return };
            this.config.borrow_mut().show_hidden = toggle.is_active();
            let current = this.current.borrow().clone();
            this.navigate(current);
        });

        let weak = Rc::downgrade(self);
        view_mode.connect_clicked(move |button| {
            let Some(this) = weak.upgrade() else { return };
            let next = match this.config.borrow().view_mode {
                ViewMode::List => ViewMode::Grid,
                ViewMode::Grid => ViewMode::List,
            };
            this.config.borrow_mut().view_mode = next;
            this.view.set_view_mode(next);
            button.set_icon_name(match next {
                ViewMode::List => "view-grid-symbolic",
                ViewMode::Grid => "view-list-symbolic",
            });
        });

        // Ctrl+scroll zooms the icons, matching the main window.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |controller, _, dy| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            if !controller.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) {
                return glib::Propagation::Proceed;
            }
            if this.config.borrow_mut().zoom(if dy < 0.0 { 1 } else { -1 }) {
                crate::ui::thumbs::clear();
                this.view.refresh_items();
            }
            glib::Propagation::Stop
        });
        self.view.widget().add_controller(scroll);

        let weak = Rc::downgrade(self);
        self.pathbar.connect_navigate(move |path| {
            let Some(this) = weak.upgrade() else { return };
            if path.is_dir() {
                this.navigate(path);
            }
        });

        // Opening an item: into a folder, or straight out with a file.
        let weak = Rc::downgrade(self);
        self.view.connect_activate(move |object| {
            let Some(this) = weak.upgrade() else { return };
            let path = object.path();
            if path.is_dir() {
                this.navigate(path);
            } else if this.request.kind() == Kind::Open && !this.request.directory {
                this.accept();
            } else {
                this.name.set_text(&object.display_name());
            }
        });

        let weak = Rc::downgrade(self);
        self.view.connect_selection_changed(move || {
            let Some(this) = weak.upgrade() else { return };
            // Saving under a typed name keeps the selection in step, so
            // clicking a file offers to replace it.
            if this.request.kind() == Kind::Save
                && let Some(object) = this.view.selected().first()
                && !object.path().is_dir()
            {
                this.name.set_text(&object.display_name());
            }
            this.update_accept();
        });

        let weak = Rc::downgrade(self);
        self.name.connect_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.update_accept();
            }
        });

        let weak = Rc::downgrade(self);
        self.filters.connect_selected_notify(move |_| {
            let Some(this) = weak.upgrade() else { return };
            let current = this.current.borrow().clone();
            this.navigate(current);
        });

        // Escape cancels, as it does in every other dialog on the desktop.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        let hidden_button = hidden.clone();
        let view_button = view_mode.clone();
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            match key {
                gdk::Key::Escape => this.window.close(),
                gdk::Key::h | gdk::Key::H if ctrl => {
                    hidden_button.set_active(!hidden_button.is_active());
                }
                gdk::Key::v | gdk::Key::V if ctrl && shift => view_button.emit_clicked(),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        self.window.add_controller(keys);
    }

    fn present(self: &Rc<Self>) {
        let start = self
            .request
            .current_folder
            .clone()
            .filter(|path| path.is_dir())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        self.navigate(start);
        self.window.present();
        self.attach_to_parent();
        self.add_drives();
        if self.request.kind() == Kind::Save {
            self.name.grab_focus();
            // Select the stem, so typing replaces the name but keeps `.png`.
            let text = self.name.text().to_string();
            let stem = text.rfind('.').filter(|at| *at > 0).unwrap_or(text.len());
            self.name.select_region(0, stem as i32);
        }
    }

    /// Makes the dialog a child of the window that asked for it.
    ///
    /// Without this the compositor sees an unrelated toplevel and tiles it
    /// like any other window; with it, Hyprland and the rest float it the way
    /// they float every other file dialog — no user configuration either way.
    /// The handle is exported by the asking application through xdg-foreign
    /// and handed to us by the portal.
    fn attach_to_parent(&self) {
        let Some(handle) = self.request.parent_window.as_deref() else { return };
        // X11 parents are given as `x11:<xid>`; this only handles Wayland,
        // which is the case Hyprland and every other wlroots compositor is.
        let Some(exported) = handle.strip_prefix("wayland:") else { return };
        let Some(surface) = self.window.surface() else { return };
        if let Ok(toplevel) = surface.downcast::<gdk4_wayland::WaylandToplevel>() {
            toplevel.set_transient_for_exported(exported);
        }
    }

    /// Fills in the drives once UDisks2 answers.
    ///
    /// A D-Bus round trip, so it happens after the window is up rather than
    /// delaying it. The sidebar draws them itself — capacity rings included —
    /// which is the point of using it rather than a copy.
    fn add_drives(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let volumes = crate::ui::actions::run_off_thread(crate::drives::list_volumes).await;
            let Some(this) = weak.upgrade() else { return };
            if let Ok(volumes) = volumes {
                this.sidebar.set_volumes(volumes);
            }

            // Cloud drives read rclone's configuration, which is a subprocess
            // of its own, so it follows the drives rather than holding them up.
            let accounts = crate::ui::actions::run_off_thread(crate::fs::cloud::accounts).await;
            if let Some(this) = weak.upgrade() {
                this.sidebar.set_cloud(accounts);
            }
        });
    }

    /// Mounts a drive the user picked, then opens it.
    fn open_volume(self: &Rc<Self>, volume: crate::drives::Volume) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let label = volume.label.clone();
            let mounted = crate::ui::actions::run_off_thread({
                let volume = volume.clone();
                move || crate::drives::mount(&volume, false)
            })
            .await;
            let Some(this) = weak.upgrade() else { return };

            match mounted {
                Ok(mounted) => this.navigate(mounted.path),
                Err(crate::drives::MountError::AlreadyMounted(path)) => this.navigate(path),
                Err(error) => {
                    let dialog = adw::AlertDialog::new(
                        Some(&format!("Could not open “{label}”")),
                        Some(&error.message()),
                    );
                    dialog.add_response("close", "Close");
                    dialog.present(Some(&this.window));
                }
            }
        });
    }

    fn navigate(self: &Rc<Self>, path: PathBuf) {
        *self.current.borrow_mut() = path.clone();
        self.pathbar.set_path(&path);
        self.sidebar.set_current(&path);
        self.view.clear();
        self.update_accept();

        let generation = self.generation.get() + 1;
        self.generation.set(generation);

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let cancellable = gio::Cancellable::new();
            let batches: Rc<RefCell<Vec<crate::fs::FileEntry>>> = Rc::new(RefCell::new(Vec::new()));
            let collected = Rc::clone(&batches);
            let _ = scan::scan_dir(&path, &cancellable, move |batch| {
                collected.borrow_mut().extend(batch);
            })
            .await;

            let Some(this) = weak.upgrade() else { return };
            if this.generation.get() != generation {
                return;
            }
            let show_hidden = this.config.borrow().show_hidden;
            let keep: Vec<crate::fs::FileEntry> = batches
                .borrow()
                .iter()
                .filter(|entry| show_hidden || !entry.is_hidden)
                .filter(|entry| this.passes_filter(entry))
                .cloned()
                .collect();
            this.view.append_batch(keep);
            this.update_accept();
        });
    }

    /// Whether an entry survives the application's chosen filter.
    ///
    /// Folders always do: filtering them out would make it impossible to
    /// reach a PNG that happens to live one directory down.
    fn passes_filter(&self, entry: &crate::fs::FileEntry) -> bool {
        if entry.is_dir {
            return true;
        }
        if self.request.directory {
            return false;
        }
        match self.active_filter() {
            None => true,
            Some(index) => self
                .request
                .filters
                .get(index)
                .is_some_and(|filter| portal::matches(filter, &entry.path, &entry.content_type)),
        }
    }

    /// Index into the request's filters, or `None` for "All files".
    fn active_filter(&self) -> Option<usize> {
        match self.filters.selected() {
            0 => None,
            selected => Some(selected as usize - 1),
        }
    }

    fn update_accept(&self) {
        let ready = match self.request.kind() {
            Kind::Save => !self.name.text().trim().is_empty(),
            // A folder request can always accept the folder being shown.
            _ if self.request.directory => true,
            Kind::Open => self.view.selected().iter().any(|object| !object.path().is_dir()),
            Kind::SaveFiles => true,
        };
        self.accept.set_sensitive(ready);
    }

    fn accept(self: &Rc<Self>) {
        let folder = self.current.borrow().clone();
        let selected: Vec<FileObject> = self.view.selected();

        let paths: Vec<PathBuf> = match self.request.kind() {
            Kind::Save => {
                let name = self.name.text().trim().to_string();
                if name.is_empty() {
                    return;
                }
                vec![folder.join(name)]
            }
            Kind::SaveFiles => {
                let target = selected
                    .first()
                    .map(|object| object.path())
                    .filter(|path| path.is_dir())
                    .unwrap_or(folder);
                self.request.files.iter().map(|name| target.join(name)).collect()
            }
            Kind::Open if self.request.directory => {
                let chosen: Vec<PathBuf> =
                    selected.iter().map(|o| o.path()).filter(|p| p.is_dir()).collect();
                if chosen.is_empty() { vec![folder] } else { chosen }
            }
            Kind::Open => selected.iter().map(|object| object.path()).collect(),
        };

        if paths.is_empty() {
            return;
        }

        // Overwriting is the one irreversible thing a save dialog can do.
        if self.request.kind() == Kind::Save && paths[0].exists() {
            let this = Rc::clone(self);
            let paths = paths.clone();
            glib::spawn_future_local(async move {
                if confirm_replace(&this.window, &paths[0]).await {
                    this.finish(paths);
                }
            });
            return;
        }
        self.finish(paths);
    }

    fn finish(self: &Rc<Self>, paths: Vec<PathBuf>) {
        *self.answer.borrow_mut() = Response {
            uris: paths.iter().map(|path| gio::File::for_path(path).uri().to_string()).collect(),
            current_filter: self.active_filter(),
        };
        self.window.close();
    }
}

/// Who is asking, for the subtitle — "Zen Browser wants a file" is far more
/// use than a bare "Open File" when several dialogs are open.
fn asking(request: &Request) -> String {
    if request.app_id.is_empty() {
        return String::new();
    }
    let desktop_id = format!("{}.desktop", request.app_id);
    let name = gio::AppInfo::all()
        .into_iter()
        .find(|info| info.id().as_deref() == Some(desktop_id.as_str()))
        .map(|info| info.display_name().to_string())
        .unwrap_or_else(|| request.app_id.clone());
    format!("for {name}")
}

async fn confirm_replace(parent: &impl IsA<gtk::Widget>, path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let dialog = adw::AlertDialog::new(
        Some(&format!("Replace “{name}”?")),
        Some("A file with that name already exists. Replacing it overwrites its contents."),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("replace", "Replace");
    dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.choose_future(Some(parent)).await == "replace"
}

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
    {
        let (request, answer) = (request.clone(), Rc::clone(&answer));
        app.connect_command_line(move |app, _| {
            // The accent colour and spacing the user picked, so the dialog
            // matches the rest of Fileman rather than looking borrowed.
            let accent = Config::load().accent_color;
            crate::app::load_stylesheet(&accent);
            let chooser = Chooser::new(app, request.clone(), Rc::clone(&answer));
            chooser.present();
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
        let header = adw::HeaderBar::builder().show_end_title_buttons(false).build();
        header.set_title_widget(Some(&adw::WindowTitle::new(&title, &asking(&request))));
        header.pack_start(&cancel);
        header.pack_end(&accept);

        // Places. Built here rather than reusing the main sidebar, which would
        // query UDisks2 and rclone just to draw a dialog — but it still has to
        // show the drives and shares the user actually keeps things on, or the
        // dialog cannot reach half their files.
        let shortcuts = places();
        let places = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar"])
            .build();
        for place in &shortcuts {
            let row = adw::ActionRow::builder().title(&place.label).activatable(true).build();
            row.add_prefix(&gtk::Image::from_icon_name(&place.icon));
            row.set_tooltip_text(Some(&place.path.to_string_lossy()));
            places.append(&row);
        }
        {
            // A heading wherever the kind of place changes, so "Devices" and
            // "Network" read as groups rather than one undifferentiated list.
            let sections: Vec<&'static str> = shortcuts.iter().map(|p| p.section).collect();
            places.set_header_func(move |row, before| {
                let index = row.index() as usize;
                let Some(current) = sections.get(index) else { return };
                let previous = before.map(|b| b.index() as usize).and_then(|i| sections.get(i));
                if previous == Some(current) {
                    row.set_header(None::<&gtk::Widget>);
                    return;
                }
                row.set_header(Some(
                    &gtk::Label::builder()
                        .label(*current)
                        .xalign(0.0)
                        .margin_start(14)
                        .margin_top(if before.is_none() { 6 } else { 12 })
                        .margin_bottom(2)
                        .css_classes(["heading", "dim-label"])
                        .build(),
                ));
            });
        }
        let places_pane = gtk::ScrolledWindow::builder()
            .child(&places)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .width_request(200)
            .build();

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
            config,
            current: RefCell::new(PathBuf::new()),
            generation: Cell::new(0),
        });

        this.wire(&cancel, places, shortcuts);
        this
    }

    fn wire(self: &Rc<Self>, cancel: &gtk::Button, places: gtk::ListBox, shortcuts: Vec<Place>) {
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

        let weak = Rc::downgrade(self);
        places.connect_row_activated(move |_, row| {
            let Some(this) = weak.upgrade() else { return };
            if let Some(place) = shortcuts.get(row.index() as usize) {
                this.navigate(place.path.clone());
            }
        });

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
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            if key == gdk::Key::Escape {
                this.window.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
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

    fn navigate(self: &Rc<Self>, path: PathBuf) {
        *self.current.borrow_mut() = path.clone();
        self.pathbar.set_path(&path);
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

/// One entry in the dialog's sidebar.
struct Place {
    section: &'static str,
    label: String,
    path: PathBuf,
    icon: String,
}

/// Everywhere the user might want to put a file, cheaply.
///
/// Mounted volumes come from GIO's volume monitor, which answers from the
/// mounts the session already knows about — no UDisks2 round trip, and it
/// covers a USB disk, a Windows partition and a gvfs network share alike.
/// rclone's cloud drives are FUSE mounts the monitor does not report, so those
/// are read straight from the kernel's mount table.
fn places() -> Vec<Place> {
    let mut places: Vec<Place> = scan::xdg_places()
        .into_iter()
        .map(|(label, path, icon)| Place {
            section: "Places",
            label,
            path,
            icon: icon.to_string(),
        })
        .collect();

    for favourite in Config::load().favourites {
        if favourite.is_dir() {
            places.push(Place {
                section: "Favourites",
                label: name_of(&favourite),
                path: favourite,
                icon: "starred-symbolic".into(),
            });
        }
    }

    let cloud_root = crate::fs::cloud::mount_root();
    for mount in gio::VolumeMonitor::get().mounts() {
        let root = mount.root();
        let Some(path) = root.path() else { continue };
        // The running system's own root is not a "place" to save into, and
        // listing it alongside a USB stick is just noise.
        if path == Path::new("/") || path.starts_with("/boot") {
            continue;
        }
        let network = root
            .uri_scheme()
            .map(|scheme| {
                matches!(
                    scheme.as_str(),
                    "smb" | "sftp" | "ssh" | "ftp" | "ftps" | "dav" | "davs" | "nfs" | "afp"
                )
            })
            .unwrap_or(false);
        places.push(Place {
            section: if network { "Network" } else { "Devices" },
            label: mount.name().to_string(),
            path,
            icon: if network { "folder-remote-symbolic" } else { "drive-removable-media-symbolic" }
                .to_string(),
        });
    }

    for path in fuse_mounts_under(&cloud_root) {
        places.push(Place {
            section: "Cloud",
            label: name_of(&path),
            path,
            icon: "folder-remote-symbolic".into(),
        });
    }

    // Grouped, but each group in the order it was built: XDG order is
    // deliberate, and drives read best in the order they were mounted.
    let order = |section: &str| match section {
        "Places" => 0,
        "Favourites" => 1,
        "Devices" => 2,
        "Network" => 3,
        _ => 4,
    };
    places.sort_by_key(|place| order(place.section));
    places.retain(|place| place.path.is_dir());
    places
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Mount points directly under `root`, read from the kernel's mount table.
fn fuse_mounts_under(root: &Path) -> Vec<PathBuf> {
    let Ok(table) = std::fs::read_to_string("/proc/self/mountinfo") else { return Vec::new() };
    table
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|point| PathBuf::from(point.replace("\\040", " ")))
        .filter(|point| point.parent() == Some(root))
        .collect()
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

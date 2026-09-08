//! The places sidebar: XDG folders, user favourites, trash, and drives.
//!
//! Drives come from [`crate::drives`], so an unmounted Windows partition or a
//! freshly plugged USB disk appears here the same way it does in Nautilus —
//! clicking it mounts it first and then navigates.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

use gtk::{gdk, glib, pango, prelude::*};

use crate::drives::{Volume, VolumeCategory};

type Callback<T> = RefCell<Option<Rc<dyn Fn(T)>>>;

/// Invokes a stored callback without holding its `RefCell` borrow.
///
/// Calling through `slot.borrow().as_ref()` keeps the borrow alive for the
/// whole call, so any handler that re-entered the same slot — or replaced the
/// callback — aborted the process. Cloning the `Rc` out first makes re-entry
/// harmless.
fn emit<T>(slot: &Callback<T>, arg: T) {
    let callback = slot.borrow().clone();
    if let Some(callback) = callback {
        callback(arg);
    }
}

/// The same, for callbacks that take no argument.
fn emit_unit(slot: &RefCell<Option<Rc<dyn Fn()>>>) {
    let callback = slot.borrow().clone();
    if let Some(callback) = callback {
        callback();
    }
}

/// What a row points at, so a click can be dispatched without a lookup table.
#[derive(Clone)]
enum Target {
    Path(PathBuf),
    Trash,
    Recent,
    /// Mounted network shares, which gvfs bridges into the filesystem.
    Network,
    Volume(Box<Volume>),
}

pub struct Sidebar {
    root: gtk::ScrolledWindow,
    container: gtk::Box,
    current: RefCell<PathBuf>,
    /// Every row built in the last rebuild, with what it points at, so the
    /// current location can be re-highlighted without another rebuild.
    rows: RefCell<Vec<(gtk::ListBoxRow, Target)>>,
    volumes: RefCell<Vec<Volume>>,
    favourites: RefCell<Vec<PathBuf>>,
    trash_count: RefCell<usize>,

    on_navigate: Callback<PathBuf>,
    on_open_trash: RefCell<Option<Rc<dyn Fn()>>>,
    on_open_recent: RefCell<Option<Rc<dyn Fn()>>>,
    on_mount: Callback<Volume>,
    on_unmount: Callback<Volume>,
    on_eject: Callback<Volume>,
    on_favourite_added: Callback<PathBuf>,
    on_favourite_removed: Callback<PathBuf>,
    /// Files dropped onto a folder row: (sources, destination).
    on_drop: Callback<(Vec<PathBuf>, PathBuf)>,
}

impl Sidebar {
    pub fn new() -> Rc<Self> {
        let container = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .margin_bottom(12)
            .build();

        let root = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&container)
            .build();

        Rc::new(Self {
            root,
            container,
            current: RefCell::new(PathBuf::new()),
            rows: RefCell::new(Vec::new()),
            volumes: RefCell::new(Vec::new()),
            favourites: RefCell::new(Vec::new()),
            trash_count: RefCell::new(0),
            on_navigate: RefCell::new(None),
            on_open_trash: RefCell::new(None),
            on_open_recent: RefCell::new(None),
            on_mount: RefCell::new(None),
            on_unmount: RefCell::new(None),
            on_eject: RefCell::new(None),
            on_favourite_added: RefCell::new(None),
            on_favourite_removed: RefCell::new(None),
            on_drop: RefCell::new(None),
        })
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.root
    }

    pub fn connect_navigate(&self, f: impl Fn(PathBuf) + 'static) {
        *self.on_navigate.borrow_mut() = Some(Rc::new(f));
    }
    pub fn connect_open_trash(&self, f: impl Fn() + 'static) {
        *self.on_open_trash.borrow_mut() = Some(Rc::new(f));
    }
    pub fn connect_open_recent(&self, f: impl Fn() + 'static) {
        *self.on_open_recent.borrow_mut() = Some(Rc::new(f));
    }
    pub fn connect_mount(&self, f: impl Fn(Volume) + 'static) {
        *self.on_mount.borrow_mut() = Some(Rc::new(f));
    }
    pub fn connect_unmount(&self, f: impl Fn(Volume) + 'static) {
        *self.on_unmount.borrow_mut() = Some(Rc::new(f));
    }
    pub fn connect_eject(&self, f: impl Fn(Volume) + 'static) {
        *self.on_eject.borrow_mut() = Some(Rc::new(f));
    }
    pub fn connect_favourite_added(&self, f: impl Fn(PathBuf) + 'static) {
        *self.on_favourite_added.borrow_mut() = Some(Rc::new(f));
    }
    pub fn connect_favourite_removed(&self, f: impl Fn(PathBuf) + 'static) {
        *self.on_favourite_removed.borrow_mut() = Some(Rc::new(f));
    }
    pub fn connect_drop(&self, f: impl Fn((Vec<PathBuf>, PathBuf)) + 'static) {
        *self.on_drop.borrow_mut() = Some(Rc::new(f));
    }

    pub fn set_volumes(self: &Rc<Self>, volumes: Vec<Volume>) {
        *self.volumes.borrow_mut() = volumes;
        self.rebuild();
    }

    pub fn set_favourites(self: &Rc<Self>, favourites: Vec<PathBuf>) {
        *self.favourites.borrow_mut() = favourites;
        self.rebuild();
    }

    pub fn set_trash_count(self: &Rc<Self>, count: usize) {
        if *self.trash_count.borrow() == count {
            return;
        }
        *self.trash_count.borrow_mut() = count;
        self.rebuild();
    }

    /// Highlights the row matching `path`, if any.
    pub fn set_current(&self, path: &Path) {
        *self.current.borrow_mut() = path.to_path_buf();
        for (row, target) in self.rows.borrow().iter() {
            let active = match target {
                Target::Path(p) => p == path,
                Target::Volume(v) => v.mount_point.as_deref() == Some(path),
                // Network resolves to a real directory, so it highlights like
                // any other path; Trash and Recent have none and are selected
                // by the window instead.
                Target::Network => crate::fs::recent::network_root().as_deref() == Some(path),
                Target::Trash | Target::Recent => false,
            };
            let Some(list) = row.parent().and_downcast::<gtk::ListBox>() else { continue };
            if active {
                list.select_row(Some(row));
            } else if list.selected_row().as_ref() == Some(row) {
                list.select_row(None::<&gtk::ListBoxRow>);
            }
        }
    }

    fn navigate(&self, path: PathBuf) {
        emit(&self.on_navigate, path);
    }

    fn rebuild(self: &Rc<Self>) {
        while let Some(child) = self.container.first_child() {
            self.container.remove(&child);
        }
        self.rows.borrow_mut().clear();

        self.build_places();
        self.build_favourites();
        self.build_system();
        self.build_devices();

        let current = self.current.borrow().clone();
        self.set_current(&current);
    }

    /// The folders that live in the user's home.
    ///
    /// Split from the virtual locations below because they are a different kind
    /// of thing: real directories you own, as against views the system
    /// assembles. Running the two together in one unlabelled list was the
    /// sidebar's least legible part.
    fn build_places(self: &Rc<Self>) {
        let list = self.new_section(Some("Places"));

        for (label, path, icon) in crate::fs::scan::xdg_places() {
            let row = self.make_row(&label, icon, None, Target::Path(path.clone()));
            self.attach_folder_drop(&row, &path);
            list.append(&row);
        }
    }

    /// Locations the system assembles rather than ones that sit on disk.
    fn build_system(self: &Rc<Self>) {
        let list = self.new_section(Some("System"));

        let recent = self.make_row(
            "Recent",
            "document-open-recent-symbolic",
            None,
            Target::Recent,
        );
        list.append(&recent);

        let network = self.make_row("Network", "network-workgroup-symbolic", None, Target::Network);
        // Say up front when there is nothing behind it, rather than opening an
        // empty folder and leaving the user to guess why.
        match crate::fs::recent::network_root() {
            Some(root) if !crate::fs::recent::network_is_empty(&root) => {
                network.set_tooltip_text(Some("Network shares mounted in this session"));
            }
            Some(_) => network
                .set_tooltip_text(Some("No network shares are mounted yet")),
            None => {
                network.set_sensitive(false);
                network.set_tooltip_text(Some(
                    "Needs gvfs, which provides network browsing for the desktop",
                ));
            }
        }
        list.append(&network);

        let count = *self.trash_count.borrow();
        let badge = (count > 0).then(|| count.to_string());
        let trash = self.make_row("Trash", "user-trash-symbolic", badge.as_deref(), Target::Trash);
        list.append(&trash);

        let computer = self.make_row(
            "Other Locations",
            "drive-multidisk-symbolic",
            None,
            Target::Path(PathBuf::from("/")),
        );
        list.append(&computer);
    }

    fn build_favourites(self: &Rc<Self>) {
        let favourites = self.favourites.borrow().clone();
        if favourites.is_empty() {
            return;
        }

        let list = self.new_section(Some("Favourites"));
        for path in favourites {
            let label = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());

            let row = self.make_row(&label, "starred-symbolic", None, Target::Path(path.clone()));
            row.set_tooltip_text(Some(&path.to_string_lossy()));

            // A missing favourite (deleted folder, unmounted drive) is dimmed
            // rather than hidden, so the user can see why it stopped working.
            if !path.is_dir() {
                row.set_sensitive(false);
                row.set_tooltip_text(Some(&format!("{} (unavailable)", path.display())));
            }

            self.attach_favourite_menu(&row, &path);
            self.attach_folder_drop(&row, &path);
            list.append(&row);
        }
    }

    fn build_devices(self: &Rc<Self>) {
        let volumes = self.volumes.borrow().clone();
        if volumes.is_empty() {
            return;
        }

        let mut current_section: Option<VolumeCategory> = None;
        let mut list: Option<gtk::ListBox> = None;

        for volume in volumes {
            if current_section != Some(volume.category) {
                current_section = Some(volume.category);
                list = Some(self.new_section(Some(volume.category.section_title())));
            }
            let Some(list) = list.as_ref() else { continue };

            let row = self.make_volume_row(&volume);
            list.append(&row);
        }
    }

    /// Adds a titled section and returns its list box.
    fn new_section(self: &Rc<Self>, title: Option<&str>) -> gtk::ListBox {
        if let Some(title) = title {
            let label = gtk::Label::builder()
                .label(title)
                .xalign(0.0)
                .margin_start(16)
                .margin_top(14)
                .margin_bottom(2)
                .css_classes(["sidebar-heading"])
                .build();
            self.container.append(&label);
        }

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar"])
            .build();

        let weak = Rc::downgrade(self);
        list.connect_row_activated(move |_, row| {
            let Some(this) = weak.upgrade() else { return };
            this.activate_row(row);
        });

        self.container.append(&list);
        list
    }

    fn activate_row(self: &Rc<Self>, row: &gtk::ListBoxRow) {
        let target = self
            .rows
            .borrow()
            .iter()
            .find(|(candidate, _)| candidate == row)
            .map(|(_, target)| target.clone());

        match target {
            Some(Target::Path(path)) => self.navigate(path),
            Some(Target::Trash) => {
                emit_unit(&self.on_open_trash);
            }
            Some(Target::Recent) => {
                emit_unit(&self.on_open_recent);
            }
            // The row is insensitive without gvfs, so a missing root here only
            // happens if the bridge stopped between building the row and the
            // click; doing nothing is the right response either way.
            Some(Target::Network) => {
                if let Some(root) = crate::fs::recent::network_root() {
                    self.navigate(root);
                }
            }
            Some(Target::Volume(volume)) => match &volume.mount_point {
                // Already mounted: just go there.
                Some(mount_point) => self.navigate(mount_point.clone()),
                // Not mounted: the window mounts it and navigates on success.
                None => {
                    emit(&self.on_mount, *volume);
                }
            },
            None => {}
        }
    }

    fn make_row(
        self: &Rc<Self>,
        label: &str,
        icon: &str,
        badge: Option<&str>,
        target: Target,
    ) -> gtk::ListBoxRow {
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_start(6)
            .margin_end(6)
            .build();

        content.append(&gtk::Image::from_icon_name(icon));
        content.append(
            &gtk::Label::builder()
                .label(label)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(pango::EllipsizeMode::Middle)
                .build(),
        );

        if let Some(text) = badge {
            content.append(
                &gtk::Label::builder()
                    .label(text)
                    .css_classes(["dim-label", "caption", "numeric"])
                    .build(),
            );
        }

        let row = gtk::ListBoxRow::builder().child(&content).build();
        self.rows.borrow_mut().push((row.clone(), target));
        row
    }

    fn make_volume_row(self: &Rc<Self>, volume: &Volume) -> gtk::ListBoxRow {
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_start(6)
            .margin_end(6)
            .build();

        // The capacity indicator is a ring drawn around the device glyph rather
        // than a bar under the label: it costs no extra row height, and it puts
        // the reading where the eye already goes.
        let fraction = Rc::new(std::cell::Cell::new(-1.0f64));

        let glyph = gtk::Image::from_icon_name(volume.icon_name());
        glyph.set_pixel_size(16);
        glyph.set_halign(gtk::Align::Center);
        glyph.set_valign(gtk::Align::Center);

        let ring = gtk::DrawingArea::builder()
            .content_width(30)
            .content_height(30)
            .valign(gtk::Align::Center)
            .build();

        let drawn = Rc::clone(&fraction);
        ring.set_draw_func(move |_, cr, width, height| {
            let value = drawn.get();
            // An unmounted volume has no usage to report, so it shows the glyph
            // alone rather than an empty ring implying "0% full".
            if value < 0.0 {
                return;
            }
            let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
            let radius = cx.min(cy) - 1.5;
            const START: f64 = -std::f64::consts::FRAC_PI_2;

            cr.set_line_width(2.0);
            cr.set_line_cap(gtk::cairo::LineCap::Round);

            cr.set_source_rgba(0.5, 0.55, 0.6, 0.30);
            cr.arc(cx, cy, radius, 0.0, std::f64::consts::PI * 2.0);
            let _ = cr.stroke();

            let (r, g, b) = capacity_color(value);
            cr.set_source_rgb(r, g, b);
            cr.arc(cx, cy, radius, START, START + value.clamp(0.0, 1.0) * std::f64::consts::PI * 2.0);
            let _ = cr.stroke();
        });

        let indicator = gtk::Overlay::new();
        indicator.set_child(Some(&ring));
        indicator.add_overlay(&glyph);
        content.append(&indicator);

        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        text.append(
            &gtk::Label::builder()
                .label(&volume.label)
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::Middle)
                .build(),
        );

        let subtitle = if volume.is_encrypted {
            format!("{}  encrypted", volume.size_label())
        } else if volume.read_only {
            // Read-only is worth calling out: it is the state an NTFS volume
            // lands in after the recovery dialog's safe option.
            format!("{}  {}  read-only", volume.size_label(), volume.fstype)
        } else {
            format!("{}  {}", volume.size_label(), volume.fstype)
        };
        // No ellipsizing: an ellipsized label reports a one-character minimum,
        // which lets the row shrink and hides exactly the number the row exists
        // to show. Let it ask for the width it needs instead.
        let detail = gtk::Label::builder()
            .label(&subtitle)
            .xalign(0.0)
            .css_classes(["device-detail"])
            .build();
        text.append(&detail);

        if let Some(mount_point) = volume.mount_point.clone() {
            let detail_for_usage = detail.clone();
            let ring_for_usage = ring.clone();
            let row_tooltip = volume.label.clone();
            let indicator_for_usage = indicator.clone();

            glib::spawn_future_local(async move {
                let Some((free, total)) = crate::fs::scan::filesystem_usage(&mount_point).await
                else {
                    return;
                };
                let used = total.saturating_sub(free);
                fraction.set(used as f64 / total as f64);
                ring_for_usage.queue_draw();

                // The sidebar is narrow, so the label carries the number that
                // prompts action and the ring carries the ratio.
                detail_for_usage.set_label(&format!(
                    "{} free",
                    humansize::format_size(free, humansize::DECIMAL),
                ));
                indicator_for_usage.set_tooltip_text(Some(&format!(
                    "{row_tooltip}: {} of {} used",
                    humansize::format_size(used, humansize::DECIMAL),
                    humansize::format_size(total, humansize::DECIMAL),
                )));
            });
        }

        content.append(&text);

        if volume.is_mounted() {
            // Always unmount, never eject.
            //
            // This button used to eject whenever the drive could be powered
            // off, which is true of every USB disk — so the one obvious control
            // next to a plugged-in SSD cut power to it and the drive
            // disappeared from the system entirely until it was physically
            // replugged. Unmounting is the reversible, everyday action and is
            // what a single click should do; ejecting is a deliberate "I am
            // about to unplug this" and lives in the right-click menu, named.
            let button = gtk::Button::builder()
                .icon_name("media-playback-stop-symbolic")
                .css_classes(["flat", "circular"])
                .valign(gtk::Align::Center)
                .tooltip_text("Unmount")
                .build();

            let weak = Rc::downgrade(self);
            let volume_for_button = volume.clone();
            button.connect_clicked(move |_| {
                let Some(this) = weak.upgrade() else { return };
                emit(&this.on_unmount, volume_for_button.clone());
            });
            content.append(&button);
        }

        let row = gtk::ListBoxRow::builder().child(&content).build();
        let mut tooltip = if volume.drive_name.is_empty() {
            volume.device.to_string_lossy().into_owned()
        } else {
            format!("{} — {}", volume.drive_name, volume.device.display())
        };
        if !volume.uuid.is_empty() {
            // The UUID is what makes a drive identifiable across replugs, so it
            // belongs where someone debugging a mount can find it.
            tooltip.push_str(&format!("\nUUID: {}", volume.uuid));
        }
        row.set_tooltip_text(Some(&tooltip));

        self.attach_volume_menu(&row, volume);
        if let Some(mount_point) = &volume.mount_point {
            self.attach_folder_drop(&row, mount_point);
        }

        self.rows.borrow_mut().push((row.clone(), Target::Volume(Box::new(volume.clone()))));
        row
    }

    /// Right-click menu for a favourite: remove it, or open it in a new window.
    fn attach_favourite_menu(self: &Rc<Self>, row: &gtk::ListBoxRow, path: &Path) {
        let menu = gio::Menu::new();
        menu.append(Some("Remove from Favourites"), Some("sidebar.unfavourite"));

        let group = gio::SimpleActionGroup::new();
        let action = gio::SimpleAction::new("unfavourite", None);
        let weak = Rc::downgrade(self);
        let path = path.to_path_buf();
        action.connect_activate(move |_, _| {
            let Some(this) = weak.upgrade() else { return };
            emit(&this.on_favourite_removed, path.clone());
        });
        group.add_action(&action);
        row.insert_action_group("sidebar", Some(&group));

        attach_menu_gesture(row, menu);
    }

    /// Right-click menu for a drive: mount, unmount, eject, and — for NTFS —
    /// the read-only escape hatch.
    fn attach_volume_menu(self: &Rc<Self>, row: &gtk::ListBoxRow, volume: &Volume) {
        let menu = gio::Menu::new();
        if volume.is_mounted() {
            menu.append(Some("Open"), Some("volume.open"));
            menu.append(Some("Unmount"), Some("volume.unmount"));
            if volume.ejectable || volume.can_power_off {
                // Named for what it does: this powers the drive down, and it
                // will not reappear until it is unplugged and plugged back in.
                menu.append(Some("Eject — safe to unplug"), Some("volume.eject"));
            }
        } else {
            menu.append(Some("Mount"), Some("volume.mount"));
        }

        let group = gio::SimpleActionGroup::new();
        for (name, handler) in [
            ("open", 0u8),
            ("mount", 1),
            ("unmount", 2),
            ("eject", 3),
        ] {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            let volume = volume.clone();
            action.connect_activate(move |_, _| {
                let Some(this) = weak.upgrade() else { return };
                match handler {
                    0 => {
                        if let Some(mount_point) = &volume.mount_point {
                            this.navigate(mount_point.clone());
                        }
                    }
                    1 => {
                        emit(&this.on_mount, volume.clone());
                    }
                    2 => {
                        emit(&this.on_unmount, volume.clone());
                    }
                    _ => {
                        emit(&this.on_eject, volume.clone());
                    }
                }
            });
            group.add_action(&action);
        }
        row.insert_action_group("volume", Some(&group));

        attach_menu_gesture(row, menu);
    }

    /// Lets files be dropped onto a sidebar row to copy them there.
    fn attach_folder_drop(self: &Rc<Self>, row: &gtk::ListBoxRow, destination: &Path) {
        let target = gtk::DropTarget::new(
            gdk::FileList::static_type(),
            gdk::DragAction::COPY | gdk::DragAction::MOVE,
        );

        let weak = Rc::downgrade(self);
        let destination = destination.to_path_buf();
        target.connect_drop(move |_, value, _, _| {
            let Some(this) = weak.upgrade() else { return false };
            let Ok(list) = value.get::<gdk::FileList>() else { return false };
            let paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
            if paths.is_empty() {
                return false;
            }
            emit(&this.on_drop, (paths, destination.clone()));
            true
        });

        // Highlight the row while a drag hovers it, so the drop target is
        // obvious among a dozen similar rows.
        let row_for_enter = row.clone();
        target.connect_enter(move |_, _, _| {
            row_for_enter.add_css_class("drop-target");
            gdk::DragAction::COPY
        });
        let row_for_leave = row.clone();
        target.connect_leave(move |_| {
            row_for_leave.remove_css_class("drop-target");
        });

        row.add_controller(target);
    }
}

/// Colour stops for the capacity ring, as `(fraction, r, g, b)`.
///
/// Interpolating directly from green to red in RGB passes through a muddy
/// brown, so the ramp goes via yellow and orange and only ever blends between
/// neighbouring stops. The spacing is deliberately uneven: a drive stays green
/// through normal use and the warning colours are compressed into the last
/// fifth, where the reading actually calls for action.
const CAPACITY_STOPS: &[(f64, f64, f64, f64)] = &[
    (0.00, 0.26, 0.72, 0.44), // green
    (0.60, 0.55, 0.75, 0.28), // yellow-green
    (0.80, 0.92, 0.74, 0.20), // yellow
    (0.92, 0.94, 0.52, 0.16), // orange
    (1.00, 0.88, 0.25, 0.20), // red
];

/// Colour for a drive that is `fraction` full, from green through yellow to red.
fn capacity_color(fraction: f64) -> (f64, f64, f64) {
    let fraction = fraction.clamp(0.0, 1.0);

    let mut previous = CAPACITY_STOPS[0];
    for &stop in &CAPACITY_STOPS[1..] {
        let (edge, r, g, b) = stop;
        if fraction > edge {
            previous = stop;
            continue;
        }
        let (start, pr, pg, pb) = previous;
        let span = edge - start;
        // Stops are distinct, but guard anyway rather than divide by zero.
        let t = if span > f64::EPSILON { (fraction - start) / span } else { 0.0 };
        return (pr + (r - pr) * t, pg + (g - pg) * t, pb + (b - pb) * t);
    }

    let (_, r, g, b) = CAPACITY_STOPS[CAPACITY_STOPS.len() - 1];
    (r, g, b)
}

/// Pops up a context menu for a row, creating the popover on demand.
///
/// A popover parented to a row must be explicitly unparented — GTK warns loudly
/// if the row is finalised while one is still attached, which happens on every
/// sidebar rebuild if the popover is created up front and kept.
fn attach_menu_gesture(row: &gtk::ListBoxRow, menu: gio::Menu) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_SECONDARY);

    let row_for_gesture = row.clone();
    gesture.connect_pressed(move |gesture, _, x, y| {
        gesture.set_state(gtk::EventSequenceState::Claimed);

        let popover = gtk::PopoverMenu::from_model_full(&menu, gtk::PopoverMenuFlags::NESTED);
        popover.set_parent(&row_for_gesture);
        popover.set_has_arrow(false);
        popover.set_position(gtk::PositionType::Bottom);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        // Unparent on the next main-loop pass, not inside `closed`: GTK closes
        // the popover before dispatching the clicked item, so detaching here
        // would break the row's action group lookup and the item would do
        // nothing. Deferring still avoids leaving a child on a finalised row.
        popover.connect_closed(|popover| {
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.popup();
    });

    row.add_controller(gesture);
}

// `gio` is used through `gtk::gio`; alias it so the module reads naturally.
use gtk::gio;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_ramps_from_green_through_yellow_to_red() {
        let (r, g, _) = capacity_color(0.0);
        assert!(g > r * 2.0, "an empty drive should read green, got {:?}", capacity_color(0.0));

        let (r, g, b) = capacity_color(0.85);
        assert!(r > 0.7 && g > 0.5 && b < 0.35, "a tight drive should read yellow, got {r},{g},{b}");

        let (r, g, _) = capacity_color(1.0);
        assert!(r > g * 2.0, "a full drive should read red, got {:?}", capacity_color(1.0));
    }

    #[test]
    fn capacity_colour_is_continuous_and_clamped() {
        // No abrupt jumps: neighbouring samples stay close, so the ring
        // animates smoothly rather than snapping between bands.
        let mut previous = capacity_color(0.0);
        let mut step = 1;
        while step <= 100 {
            let current = capacity_color(step as f64 / 100.0);
            for (a, b) in [(previous.0, current.0), (previous.1, current.1), (previous.2, current.2)] {
                assert!((a - b).abs() < 0.1, "jump at {step}%: {a} -> {b}");
            }
            previous = current;
            step += 1;
        }

        assert_eq!(capacity_color(-5.0), capacity_color(0.0));
        assert_eq!(capacity_color(9.9), capacity_color(1.0));
    }
}

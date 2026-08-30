//! The location bar: breadcrumbs that turn into an editable path on click.
//!
//! Two presentations of one location, swapped in a `GtkStack`. Breadcrumbs are
//! for navigating with the mouse; the entry is for typing, pasting, or copying
//! the literal path — which is why clicking the bar's empty space (or Ctrl+L)
//! reveals it pre-selected.

use std::{
    cell::RefCell,
    path::{Component, Path, PathBuf},
    rc::Rc,
};

use gtk::{gdk, glib, prelude::*};

/// A slot holding an optional callback, matching the pattern used by the other
/// UI modules.
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

/// Longest crumb rendered in full before it is middle-ellipsized.
const MAX_CRUMB_CHARS: usize = 24;

pub struct PathBar {
    stack: gtk::Stack,
    crumb_box: gtk::Box,
    crumb_scroller: gtk::ScrolledWindow,
    entry: gtk::Entry,
    current: RefCell<PathBuf>,
    /// Invoked when the user picks a crumb or commits the entry.
    on_navigate: Callback<PathBuf>,
    /// Guards the entry's `changed` handler while we rewrite its text, so
    /// inline completion doesn't recurse.
    updating: std::cell::Cell<bool>,
}

impl PathBar {
    pub fn new() -> Rc<Self> {
        let crumb_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .css_classes(["linked", "path-crumbs"])
            .valign(gtk::Align::Center)
            .build();

        let crumb_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .child(&crumb_box)
            .build();

        // The scroller fills the bar so that clicking anywhere to the right of
        // the last crumb still lands on the "edit the path" gesture.
        let crumb_page = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["path-bar"])
            .hexpand(true)
            .build();
        crumb_page.append(&crumb_scroller);

        let entry = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text("Type a path…")
            .css_classes(["path-entry"])
            .build();
        entry.set_primary_icon_name(Some("folder-symbolic"));

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(90)
            .hexpand(true)
            .build();
        stack.add_named(&crumb_page, Some("crumbs"));
        stack.add_named(&entry, Some("entry"));

        let this = Rc::new(Self {
            stack,
            crumb_box,
            crumb_scroller,
            entry,
            current: RefCell::new(PathBuf::from("/")),
            on_navigate: RefCell::new(None),
            updating: std::cell::Cell::new(false),
        });

        this.wire_up();
        this
    }

    pub fn widget(&self) -> &gtk::Stack {
        &self.stack
    }

    pub fn connect_navigate(&self, f: impl Fn(PathBuf) + 'static) {
        *self.on_navigate.borrow_mut() = Some(Rc::new(f));
    }

    fn navigate(&self, path: PathBuf) {
        emit(&self.on_navigate, path);
    }

    fn wire_up(self: &Rc<Self>) {
        // Click anywhere on the breadcrumb strip that isn't a crumb button.
        // Buttons handle their own clicks first, so this only fires on the gaps.
        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        let weak = Rc::downgrade(self);
        click.connect_released(move |_, _, _, _| {
            if let Some(this) = weak.upgrade() {
                this.start_editing();
            }
        });
        self.crumb_scroller.add_controller(click);

        // Middle-click pastes the primary selection as a path, matching the
        // convention of every other location bar on the desktop.
        let paste = gtk::GestureClick::new();
        paste.set_button(gdk::BUTTON_MIDDLE);
        let weak = Rc::downgrade(self);
        paste.connect_released(move |_, _, _, _| {
            let Some(this) = weak.upgrade() else { return };
            let clipboard = this.stack.display().primary_clipboard();
            let weak_inner = Rc::downgrade(&this);
            glib::spawn_future_local(async move {
                let Ok(text) = clipboard.read_text_future().await else { return };
                let Some(text) = text else { return };
                let Some(this) = weak_inner.upgrade() else { return };
                if let Some(path) = this.resolve(text.as_str()) {
                    this.navigate(path);
                }
            });
        });
        self.crumb_scroller.add_controller(paste);

        let weak = Rc::downgrade(self);
        self.entry.connect_activate(move |entry| {
            let Some(this) = weak.upgrade() else { return };
            let text = entry.text().to_string();
            match this.resolve(&text) {
                Some(path) => {
                    this.show_crumbs();
                    this.navigate(path);
                }
                None => {
                    // Leave the text in place so the user can fix a typo rather
                    // than retyping the whole path.
                    entry.add_css_class("error");
                }
            }
        });

        let weak = Rc::downgrade(self);
        self.entry.connect_changed(move |entry| {
            let Some(this) = weak.upgrade() else { return };
            entry.remove_css_class("error");
            if this.updating.get() {
                return;
            }
            this.complete_inline(entry);
        });

        // Escape abandons editing; focus loss does the same, so the bar never
        // stays stuck in edit mode after the user clicks elsewhere.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            if key == gdk::Key::Escape {
                this.show_crumbs();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.entry.add_controller(keys);

        let focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        focus.connect_leave(move |_| {
            if let Some(this) = weak.upgrade() {
                this.show_crumbs();
            }
        });
        self.entry.add_controller(focus);
    }

    /// Switches to the editable path, selected so typing replaces it.
    pub fn start_editing(self: &Rc<Self>) {
        let path = self.current.borrow().clone();
        self.updating.set(true);
        self.entry.set_text(&path.to_string_lossy());
        self.updating.set(false);
        self.stack.set_visible_child_name("entry");
        self.entry.grab_focus();
        self.entry.select_region(0, -1);
    }

    /// Shows a single non-navigable crumb for a place that has no real path,
    /// such as the Trash. Editing is still available and still resolves paths.
    pub fn set_virtual(self: &Rc<Self>, label: &str, icon: &str) {
        *self.current.borrow_mut() = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));

        while let Some(child) = self.crumb_box.first_child() {
            self.crumb_box.remove(&child);
        }
        self.push_crumb(label, PathBuf::from("/"), true, Some(icon));
    }

    pub fn show_crumbs(&self) {
        self.stack.set_visible_child_name("crumbs");
    }

    /// Rebuilds the breadcrumb strip for `path`.
    pub fn set_path(self: &Rc<Self>, path: &Path) {
        *self.current.borrow_mut() = path.to_path_buf();

        while let Some(child) = self.crumb_box.first_child() {
            self.crumb_box.remove(&child);
        }

        let home = dirs::home_dir();
        // Inside the home directory, root the trail at a Home crumb instead of
        // spelling out /home/<user>, which is noise on every single path.
        let (mut prefix, components): (PathBuf, Vec<String>) = match &home {
            Some(h) if path == h || path.starts_with(h) => {
                let rest: Vec<String> = path
                    .strip_prefix(h)
                    .map(|r| {
                        r.components()
                            .filter_map(|c| match c {
                                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.push_crumb("Home", h.clone(), rest.is_empty(), Some("user-home-symbolic"));
                (h.clone(), rest)
            }
            _ => {
                let rest: Vec<String> = path
                    .components()
                    .filter_map(|c| match c {
                        Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                        _ => None,
                    })
                    .collect();
                // The root crumb is the icon alone: labelling it "/" would
                // render as "/ / tmp / …" once a separator is added after it.
                self.push_crumb("", PathBuf::from("/"), rest.is_empty(), Some("drive-harddisk-symbolic"));
                (PathBuf::from("/"), rest)
            }
        };

        let last = components.len().saturating_sub(1);
        for (index, name) in components.iter().enumerate() {
            prefix = prefix.join(name);
            self.push_crumb(name, prefix.clone(), index == last, None);
        }

        // Keep the deepest crumb in view; a long path otherwise shows only the
        // parts the user already knows.
        let scroller = self.crumb_scroller.clone();
        glib::idle_add_local_once(move || {
            let adj = scroller.hadjustment();
            adj.set_value(adj.upper() - adj.page_size());
        });
    }

    fn push_crumb(self: &Rc<Self>, label: &str, target: PathBuf, is_current: bool, icon: Option<&str>) {
        if self.crumb_box.first_child().is_some() {
            let sep = gtk::Label::builder()
                .label("/")
                .css_classes(["dim-label", "crumb-separator"])
                .build();
            self.crumb_box.append(&sep);
        }

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        if let Some(icon_name) = icon {
            content.append(&gtk::Image::from_icon_name(icon_name));
        }

        if !label.is_empty() {
            let text = gtk::Label::builder().label(label).build();
            // Ellipsizing gives a label a tiny minimum width, which lets the
            // enclosing box shrink and squeezes *every* crumb into "a…b".
            // Only opt into that for names actually long enough to need it.
            if label.chars().count() > MAX_CRUMB_CHARS {
                text.set_ellipsize(pango::EllipsizeMode::Middle);
                text.set_max_width_chars(MAX_CRUMB_CHARS as i32);
            }
            content.append(&text);
        }

        let button = gtk::Button::builder()
            .child(&content)
            .css_classes(["flat", "crumb"])
            .tooltip_text(target.to_string_lossy().as_ref())
            .build();

        if is_current {
            button.add_css_class("crumb-current");
        }

        let weak = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.navigate(target.clone());
            }
        });

        // Drop onto a crumb to move files up the tree, the way Nautilus allows.
        self.crumb_box.append(&button);
    }

    /// Turns typed text into a real directory path.
    ///
    /// Accepts `~`, `file://` URIs, environment variables and paths relative to
    /// the current directory, because all four turn up in things people paste.
    fn resolve(&self, text: &str) -> Option<PathBuf> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }

        let expanded = if let Some(rest) = text.strip_prefix("file://") {
            urlencoding::decode(rest).ok()?.into_owned()
        } else if text == "~" {
            dirs::home_dir()?.to_string_lossy().into_owned()
        } else if let Some(rest) = text.strip_prefix("~/") {
            dirs::home_dir()?.join(rest).to_string_lossy().into_owned()
        } else {
            expand_env(text)
        };

        let path = PathBuf::from(expanded);
        let path = if path.is_absolute() {
            path
        } else {
            self.current.borrow().join(path)
        };

        // Normalising here means `..` in a typed path works even when the
        // intermediate directory is a symlink the user didn't intend to follow.
        let normalized = normalize(&path);
        normalized.exists().then_some(normalized)
    }

    /// Appends the unique completion of the trailing path segment, selecting
    /// the added text so continuing to type overwrites it.
    fn complete_inline(&self, entry: &gtk::Entry) {
        let text = entry.text().to_string();
        // Only complete at the end of the line; completing mid-edit fights the
        // user's cursor.
        if entry.position() != text.chars().count() as i32 || text.ends_with('/') {
            return;
        }
        let Some((dir_part, prefix)) = text.rsplit_once('/') else { return };
        if prefix.is_empty() {
            return;
        }

        let dir = if dir_part.is_empty() {
            PathBuf::from("/")
        } else {
            let expanded = if let Some(rest) = dir_part.strip_prefix("~") {
                match dirs::home_dir() {
                    Some(h) => h.join(rest.trim_start_matches('/')),
                    None => return,
                }
            } else {
                PathBuf::from(dir_part)
            };
            if expanded.is_absolute() { expanded } else { self.current.borrow().join(expanded) }
        };

        let Ok(read) = std::fs::read_dir(&dir) else { return };
        let mut matches = read
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(prefix));

        let Some(first) = matches.next() else { return };
        // Ambiguous prefixes are left alone: silently picking one of several
        // candidates would send the user somewhere they didn't ask for.
        if matches.next().is_some() {
            return;
        }

        let completed = format!("{text}{}", &first[prefix.len()..]);
        self.updating.set(true);
        entry.set_text(&completed);
        entry.select_region(text.chars().count() as i32, -1);
        self.updating.set(false);
    }
}

/// Expands `$VAR` and `${VAR}` occurrences, leaving unknown names untouched.
fn expand_env(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&next) = chars.peek() {
            let valid = next.is_alphanumeric() || next == '_';
            if !valid || (braced && next == '}') {
                break;
            }
            name.push(next);
            chars.next();
        }
        if braced {
            chars.next(); // closing brace
        }
        match std::env::var(&name) {
            Ok(value) => out.push_str(&value),
            Err(_) => {
                out.push('$');
                out.push_str(&name);
            }
        }
    }
    out
}

/// Resolves `.` and `..` textually, without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() { PathBuf::from("/") } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_resolves_parent_components() {
        assert_eq!(normalize(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/a/./b")), PathBuf::from("/a/b"));
        assert_eq!(normalize(Path::new("/..")), PathBuf::from("/"));
    }

    #[test]
    fn env_expansion_leaves_unknown_variables_alone() {
        unsafe { std::env::set_var("FILEMAN_TEST_DIR", "/tmp/x") };
        assert_eq!(expand_env("$FILEMAN_TEST_DIR/y"), "/tmp/x/y");
        assert_eq!(expand_env("${FILEMAN_TEST_DIR}/y"), "/tmp/x/y");
        assert_eq!(expand_env("$NOT_SET_ANYWHERE_12345/y"), "$NOT_SET_ANYWHERE_12345/y");
    }
}

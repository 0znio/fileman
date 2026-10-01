//! The file-chooser portal backend: a daemon that lets Fileman be the dialog
//! every application opens when it asks for a file.
//!
//! # Why this is a separate program
//!
//! `xdg-desktop-portal` activates its backend on demand and then keeps it
//! alive for the session, so whatever this is costs memory from first use
//! until logout. The GTK backend sits at roughly 20 MB doing nothing.
//!
//! This one links no toolkit at all. It speaks D-Bus, and when a request
//! actually arrives it starts `fileman --file-chooser`, which draws the
//! dialog, prints its answer and exits. Idle cost is a few megabytes; the
//! toolkit is paid for only while a dialog is on screen, and handed back when
//! it closes.
//!
//! # Switching to it
//!
//! `fileman-portal --enable` writes one line into the user's own
//! `portals.conf`, which every `xdg-desktop-portal` since 1.18 reads ahead of
//! whatever the distribution shipped. No root, no files in `/usr`, same on
//! every distro. `--disable` takes the line out again.

use std::{
    collections::HashMap,
    io::Write,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
};

use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

#[path = "../portal/mod.rs"]
#[allow(dead_code)]
mod portal;

use portal::{Filter, Kind, Request, Response, Rule};

/// Portal response codes, from the specification.
const SUCCESS: u32 = 0;
const CANCELLED: u32 = 1;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--enable") => switch(true),
        Some("--disable") => switch(false),
        Some("--refresh") => {
            // Used by the installer: makes a newly installed backend visible
            // to the bus without waiting for the next login.
            portal::reload_dbus();
            Ok(())
        }
        Some("--status") => {
            status();
            Ok(())
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => {
            eprintln!("fileman-portal: unknown option {other}\n\n{USAGE}");
            std::process::exit(2);
        }
        None => serve(),
    }
}

const USAGE: &str = "\
fileman-portal — makes Fileman the file dialog other applications open

  fileman-portal            run the backend (started by xdg-desktop-portal)
  fileman-portal --enable   use Fileman for Open and Save dialogs
  fileman-portal --disable  hand them back to the previous backend
  fileman-portal --status   show which backend is in use
  fileman-portal --refresh  make a freshly installed backend visible to D-Bus";

fn switch(enable: bool) -> Result<(), Box<dyn std::error::Error>> {
    let path = portal::set_enabled(enable)?;
    if enable {
        println!("Fileman is now the file chooser.\nWrote {}", path.display());
    } else {
        println!("The file chooser is back to the desktop's default.\nWrote {}", path.display());
    }

    // Two caches stand between the configuration and it working, and leaving
    // either to the user means the feature looks broken: the bus has to notice
    // the backend exists, and the portal has to re-read which backend to use.
    portal::reload_dbus();
    if portal::restart_portal() {
        println!("\nThe portal has been restarted — it is in effect now.");
    } else {
        println!("\nRestart the portal for it to take effect now:");
        println!("  systemctl --user restart xdg-desktop-portal");
    }
    Ok(())
}

fn status() {
    let path = portal::user_config_path();
    if portal::is_enabled() {
        println!("File chooser: Fileman  ({})", path.display());
    } else {
        println!("File chooser: the desktop's default (Fileman is not configured)");
        println!("Enable it with: fileman-portal --enable");
    }
}

// ── the backend ─────────────────────────────────────────────────────────────

/// Process ids of dialogs currently on screen, so the portal can close one it
/// no longer wants an answer to.
///
/// The id rather than the `Child`, because the thread waiting for the dialog
/// owns that; a pid is enough to signal and can be shared.
type Running = Arc<Mutex<HashMap<OwnedObjectPath, u32>>>;

struct FileChooser {
    running: Running,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooser {
    #[allow(clippy::too_many_arguments)]
    async fn open_file(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        handle: OwnedObjectPath,
        app_id: String,
        parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let mut request = request_from(&options, &app_id, &title, &parent_window);
        request.kind_or_default = Some(Kind::Open);
        request.multiple = flag(&options, "multiple");
        request.directory = flag(&options, "directory");
        self.run(server, handle, request).await
    }

    async fn save_file(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        handle: OwnedObjectPath,
        app_id: String,
        parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let mut request = request_from(&options, &app_id, &title, &parent_window);
        request.kind_or_default = Some(Kind::Save);
        request.current_name = text(&options, "current_name");
        self.run(server, handle, request).await
    }

    async fn save_files(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        handle: OwnedObjectPath,
        app_id: String,
        parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let mut request = request_from(&options, &app_id, &title, &parent_window);
        request.kind_or_default = Some(Kind::SaveFiles);
        request.directory = true;
        request.files = byte_string_list(&options, "files");
        self.run(server, handle, request).await
    }

    /// The portal reads this to know what the backend supports.
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        3
    }
}

impl FileChooser {
    /// Shows the dialog and turns its answer into a portal reply.
    async fn run(
        &self,
        server: &zbus::ObjectServer,
        handle: OwnedObjectPath,
        request: Request,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let kind = request.kind();
        let child = match spawn_dialog(&request) {
            Ok(child) => child,
            Err(error) => {
                eprintln!("fileman-portal: could not start the dialog: {error}");
                return (CANCELLED, HashMap::new());
            }
        };
        if let Ok(mut running) = self.running.lock() {
            running.insert(handle.clone(), child.id());
        }
        // The object the portal calls `Close` on to withdraw the request.
        let _ = server
            .at(
                handle.clone(),
                RequestHandle { path: handle.clone(), running: Arc::clone(&self.running) },
            )
            .await;

        let answer = wait_for(child).await;

        if let Ok(mut running) = self.running.lock() {
            running.remove(&handle);
        }
        let _ = server.remove::<RequestHandle, _>(&handle).await;

        let Some(response) = answer else { return (CANCELLED, HashMap::new()) };
        if response.is_cancelled() {
            return (CANCELLED, HashMap::new());
        }

        let mut results: HashMap<String, OwnedValue> = HashMap::new();
        results.insert("uris".into(), owned(Value::from(response.uris.clone())));
        // Applications check this before writing; a chooser that never says
        // "writable" makes some of them refuse a perfectly good folder.
        if kind != Kind::Open {
            results.insert("writable".into(), owned(Value::from(true)));
        }
        if let Some(index) = response.current_filter
            && let Some(filter) = request.filters.get(index)
        {
            results.insert("current_filter".into(), owned(filter_value(filter)));
        }
        (SUCCESS, results)
    }
}

/// Waits for the dialog without blocking the bus.
///
/// Collecting the child's output is a blocking read, and doing it here would
/// stop this task answering anything else — including the `Close` that cancels
/// this very dialog. A thread does the waiting and hands the answer back.
async fn wait_for(child: Child) -> Option<Response> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::Builder::new()
        .name("fileman-portal-dialog".into())
        .spawn(move || {
            let answer = child.wait_with_output().ok().filter(|out| out.status.success());
            let _ = tx.send_blocking(answer.and_then(|out| serde_json::from_slice(&out.stdout).ok()));
        })
        .ok()?;
    rx.recv().await.ok().flatten()
}

/// The `Request` object the portal expects at `handle`, so it can cancel.
struct RequestHandle {
    path: OwnedObjectPath,
    running: Running,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestHandle {
    /// The application withdrew its request — close the dialog.
    fn close(&self) {
        let pid = self.running.lock().ok().and_then(|mut running| running.remove(&self.path));
        if let Some(pid) = pid {
            // SAFETY: `kill` with a pid this process started and has not yet
            // reaped; the worst outcome of a race is ESRCH, which is ignored.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        }
    }
}

fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let running: Running = Arc::new(Mutex::new(HashMap::new()));
    let _connection = zbus::blocking::connection::Builder::session()?
        .name(portal::DBUS_NAME)?
        .serve_at("/org/freedesktop/portal/desktop", FileChooser { running: running.clone() })?
        .build()?;

    // Nothing to do until a request arrives; the thread parks rather than
    // spinning, which is the whole point of a daemon this small.
    loop {
        std::thread::park();
    }
}

/// Starts the dialog, handing it the request on stdin.
fn spawn_dialog(request: &Request) -> std::io::Result<Child> {
    let json = serde_json::to_vec(request)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let mut child = Command::new(fileman_binary())
        .arg("--file-chooser")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("no stdin"))?
        .write_all(&json)?;
    Ok(child)
}

/// Prefers the Fileman installed beside this daemon, so a build run from a
/// source tree uses its own dialog rather than whatever is on `PATH`.
fn fileman_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("fileman")))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("fileman"))
}

// ── reading the portal's options ────────────────────────────────────────────

fn request_from(
    options: &HashMap<String, OwnedValue>,
    app_id: &str,
    title: &str,
    parent_window: &str,
) -> Request {
    Request {
        app_id: app_id.to_string(),
        title: title.to_string(),
        parent_window: (!parent_window.is_empty()).then(|| parent_window.to_string()),
        accept_label: text(options, "accept_label"),
        current_folder: byte_string(options, "current_folder").map(PathBuf::from),
        filters: filters(options),
        current_filter: None,
        ..Request::default()
    }
}

fn flag(options: &HashMap<String, OwnedValue>, key: &str) -> bool {
    options.get(key).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false)
}

fn text(options: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    options.get(key).and_then(|v| String::try_from(v.clone()).ok()).filter(|s| !s.is_empty())
}

/// Paths arrive as NUL-terminated byte arrays, not strings: a filename is
/// bytes on Linux and need not be valid UTF-8.
fn byte_string(options: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let bytes: Vec<u8> = options.get(key).and_then(|v| Vec::<u8>::try_from(v.clone()).ok())?;
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(&bytes);
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(bytes).into_owned())
}

fn byte_string_list(options: &HashMap<String, OwnedValue>, key: &str) -> Vec<String> {
    let Some(value) = options.get(key) else { return Vec::new() };
    let Ok(lists) = Vec::<Vec<u8>>::try_from(value.clone()) else { return Vec::new() };
    lists
        .into_iter()
        .map(|bytes| {
            let bytes = bytes.strip_suffix(&[0]).unwrap_or(&bytes).to_vec();
            String::from_utf8_lossy(&bytes).into_owned()
        })
        .collect()
}

/// `a(sa(us))`: a list of named filters, each a list of (kind, pattern).
fn filters(options: &HashMap<String, OwnedValue>) -> Vec<Filter> {
    let Some(value) = options.get("filters") else { return Vec::new() };
    let Ok(raw) = Vec::<(String, Vec<(u32, String)>)>::try_from(value.clone()) else {
        return Vec::new();
    };
    raw.into_iter()
        .map(|(name, rules)| Filter {
            name,
            rules: rules
                .into_iter()
                .map(|(kind, pattern)| match kind {
                    1 => Rule::Mime(pattern),
                    _ => Rule::Glob(pattern),
                })
                .collect(),
        })
        .collect()
}

fn filter_value(filter: &Filter) -> Value<'static> {
    let rules: Vec<(u32, String)> = filter
        .rules
        .iter()
        .map(|rule| match rule {
            Rule::Glob(pattern) => (0u32, pattern.clone()),
            Rule::Mime(pattern) => (1u32, pattern.clone()),
        })
        .collect();
    Value::from((filter.name.clone(), rules))
}

fn owned(value: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(value).expect("portal values are always convertible")
}

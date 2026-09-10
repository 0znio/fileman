//! Cloud drives: Google Drive, Proton Drive, Icedrive and friends.
//!
//! There is no native Linux client for most of these, and the ones that exist
//! are proprietary and single-vendor. rclone already speaks all of them, is
//! packaged everywhere, and — the part that matters here — can present a remote
//! as a FUSE mount. That turns a cloud account into an ordinary directory, so
//! every existing operation in this file manager works on it unchanged: copy,
//! compress, search, thumbnail, all of it.
//!
//! rclone is optional. Without it this module reports no accounts and the UI
//! offers to install it, rather than failing at the moment somebody clicks.
//!
//! # Where the account identity comes from
//!
//! Several accounts with the same provider must stay distinguishable — two
//! Google Drives are not interchangeable, and quietly writing to the wrong one
//! is the kind of mistake that is discovered much later. Every account is
//! therefore labelled with the identity it authenticated as, cached in
//! `cloud.json` beside the rest of the config. Credentials are never copied
//! there: they stay in rclone's own config, which is the only thing that has
//! them.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};

/// Providers offered by name. rclone supports dozens more; these are the ones
/// with a sign-in flow simple enough to drive from a dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provider {
    GoogleDrive,
    ProtonDrive,
    Icedrive,
    Dropbox,
    OneDrive,
    Nextcloud,
    /// Anything else already configured in rclone.
    Other,
}

impl Provider {
    pub const OFFERED: [Provider; 6] = [
        Provider::GoogleDrive,
        Provider::ProtonDrive,
        Provider::Icedrive,
        Provider::Dropbox,
        Provider::OneDrive,
        Provider::Nextcloud,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Provider::GoogleDrive => "Google Drive",
            Provider::ProtonDrive => "Proton Drive",
            Provider::Icedrive => "Icedrive",
            Provider::Dropbox => "Dropbox",
            Provider::OneDrive => "OneDrive",
            Provider::Nextcloud => "Nextcloud",
            Provider::Other => "Cloud storage",
        }
    }

    pub fn icon(self) -> &'static str {
        "folder-remote-symbolic"
    }

    /// The rclone backend type behind this provider.
    ///
    /// Icedrive and Nextcloud have no dedicated backend; both publish a WebDAV
    /// endpoint, which is what they are actually configured as.
    fn backend(self) -> &'static str {
        match self {
            Provider::GoogleDrive => "drive",
            Provider::ProtonDrive => "protondrive",
            Provider::Icedrive | Provider::Nextcloud => "webdav",
            Provider::Dropbox => "dropbox",
            Provider::OneDrive => "onedrive",
            Provider::Other => "",
        }
    }

    fn from_backend(backend: &str) -> Self {
        match backend {
            "drive" => Provider::GoogleDrive,
            "protondrive" => Provider::ProtonDrive,
            "dropbox" => Provider::Dropbox,
            "onedrive" => Provider::OneDrive,
            _ => Provider::Other,
        }
    }

    /// Whether signing in opens a browser (OAuth) or asks for a password.
    pub fn uses_oauth(self) -> bool {
        matches!(
            self,
            Provider::GoogleDrive | Provider::Dropbox | Provider::OneDrive
        )
    }

    /// Fixed endpoint for the WebDAV-based providers.
    fn webdav_url(self) -> Option<&'static str> {
        match self {
            Provider::Icedrive => Some("https://webdav.icedrive.io"),
            _ => None,
        }
    }
}

/// One connected cloud account.
#[derive(Debug, Clone)]
pub struct Account {
    /// The rclone remote name. Unique by construction — rclone will not hold
    /// two remotes with the same name.
    pub remote: String,
    pub provider: Provider,
    /// The email or username this account signed in as. This is what keeps two
    /// Google Drives apart in the sidebar.
    pub identity: String,
    pub mount_point: PathBuf,
    pub mounted: bool,
}

impl Account {
    /// What the sidebar shows: the provider, then who it is.
    pub fn display_name(&self) -> String {
        if self.identity.is_empty() {
            self.provider.label().to_string()
        } else {
            format!("{} — {}", self.provider.label(), self.identity)
        }
    }
}

/// Identities learned at sign-in, keyed by rclone remote name.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Identities {
    #[serde(default)]
    accounts: BTreeMap<String, String>,
}

fn identities_path() -> PathBuf {
    crate::config::Config::config_dir().join("cloud.json")
}

fn load_identities() -> Identities {
    std::fs::read_to_string(identities_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_identity(remote: &str, identity: &str) {
    let mut identities = load_identities();
    identities.accounts.insert(remote.to_string(), identity.to_string());
    let dir = crate::config::Config::config_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(&identities) {
        let _ = std::fs::write(identities_path(), json);
    }
}

fn forget_identity(remote: &str) {
    let mut identities = load_identities();
    if identities.accounts.remove(remote).is_some()
        && let Ok(json) = serde_json::to_string_pretty(&identities)
    {
        let _ = std::fs::write(identities_path(), json);
    }
}

/// Where cloud drives are mounted.
///
/// Under the data directory rather than the home folder, because these are
/// mount points the app manages, not places the user is meant to put things.
pub fn mount_root() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("fileman/cloud")
}

/// Whether cloud drives can work at all on this machine.
pub fn is_available() -> bool {
    rclone_path().is_some()
}

fn rclone_path() -> Option<PathBuf> {
    which("rclone")
}

/// FUSE mounting needs the unmount helper as well; rclone alone is not enough.
pub fn unmount_helper() -> Option<PathBuf> {
    which("fusermount3").or_else(|| which("fusermount"))
}

fn which(program: &str) -> Option<PathBuf> {
    // `PATH` is read directly rather than shelling out to `which`, which is
    // itself not guaranteed to be installed.
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| candidate.is_file())
}

/// What to tell somebody who has no rclone.
pub const INSTALL_HINT: &str = "Cloud drives need rclone, which handles the sign-in and \
                                presents the account as a folder.\n\n\
                                Arch: sudo pacman -S rclone\n\
                                Debian/Ubuntu: sudo apt install rclone\n\
                                Fedora: sudo dnf install rclone";

fn rclone() -> Result<Command, String> {
    let path = rclone_path().ok_or_else(|| INSTALL_HINT.to_string())?;
    let mut command = Command::new(path);
    // Never let rclone stop to ask a question on a terminal nobody is watching.
    command.arg("--non-interactive");
    Ok(command)
}

/// Every account rclone knows about, with its identity and mount state.
pub fn accounts() -> Vec<Account> {
    let Ok(mut command) = rclone() else { return Vec::new() };
    let Ok(output) = command.args(["config", "dump"]).output() else { return Vec::new() };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(config) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Vec::new();
    };
    let Some(remotes) = config.as_object() else { return Vec::new() };

    let identities = load_identities();
    let root = mount_root();

    let mut accounts: Vec<Account> = remotes
        .iter()
        .map(|(remote, settings)| {
            let backend = settings.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let provider = classify(remote, backend, settings);
            let identity = identities
                .accounts
                .get(remote)
                .cloned()
                // A remote configured outside Fileman has no cached identity,
                // but username-based backends carry it in the config itself.
                .or_else(|| {
                    settings
                        .get("user")
                        .or_else(|| settings.get("username"))
                        .and_then(|u| u.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let mount_point = root.join(remote);
            Account {
                mounted: is_mounted(&mount_point),
                remote: remote.clone(),
                provider,
                identity,
                mount_point,
            }
        })
        .collect();

    accounts.sort_by_key(|a| a.display_name().to_lowercase());
    accounts
}

/// Icedrive and Nextcloud are both plain `webdav` to rclone, so the endpoint is
/// what tells them apart.
fn classify(remote: &str, backend: &str, settings: &serde_json::Value) -> Provider {
    if backend != "webdav" {
        return Provider::from_backend(backend);
    }
    let url = settings.get("url").and_then(|u| u.as_str()).unwrap_or("");
    if url.contains("icedrive.") {
        return Provider::Icedrive;
    }
    if url.contains("/remote.php/dav") || remote.to_lowercase().contains("nextcloud") {
        return Provider::Nextcloud;
    }
    Provider::Other
}

/// Whether `path` is a mount point, read from the kernel rather than guessed.
///
/// A stale rclone process leaves the directory present but empty, and a mount
/// that died leaves it present and unreadable; only the mount table knows.
fn is_mounted(path: &Path) -> bool {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mountinfo") else { return false };
    let target = path.to_string_lossy();
    mounts.lines().any(|line| {
        // mountinfo field 5 is the mount point, with spaces escaped as \040.
        line.split_whitespace()
            .nth(4)
            .is_some_and(|point| point.replace("\\040", " ") == target)
    })
}

/// Mounts an account so it can be browsed as a folder.
///
/// `--vfs-cache-mode writes` is not a tuning choice but a correctness one: most
/// cloud backends cannot accept a partially written file, so without a local
/// write cache anything that opens a file for update — an editor, an archiver,
/// a copy that seeks — fails in ways that look like corruption.
pub fn mount(account: &Account) -> Result<(), String> {
    if account.mounted {
        return Ok(());
    }
    if unmount_helper().is_none() {
        return Err("Cloud drives need FUSE. Install fuse3 and try again.".into());
    }
    std::fs::create_dir_all(&account.mount_point)
        .map_err(|e| format!("Cannot create the mount point: {e}"))?;

    let output = rclone()?
        .args(["mount", &format!("{}:", account.remote)])
        .arg(&account.mount_point)
        .args([
            "--daemon",
            "--vfs-cache-mode",
            "writes",
            // Directory listings are what a file manager does constantly, and
            // an uncached one is a network round trip per navigation.
            "--dir-cache-time",
            "30s",
            "--poll-interval",
            "15s",
        ])
        .output()
        .map_err(|e| format!("Could not start rclone: {e}"))?;

    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(mount_failure(&message));
    }

    // `--daemon` returns as soon as the child is forked, before the mount is
    // usable. Navigating to a directory that is not mounted yet shows an empty
    // folder, so the mount table is polled briefly rather than assumed.
    for _ in 0..40 {
        if is_mounted(&account.mount_point) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err("rclone started but the drive did not appear. Check `rclone mount` by hand.".into())
}

fn mount_failure(stderr: &str) -> String {
    let lower = stderr.to_lowercase();
    if lower.contains("didn't find section") || lower.contains("not found in config") {
        return "That account is no longer in rclone's configuration.".into();
    }
    if lower.contains("token") || lower.contains("oauth") || lower.contains("unauthenticated") {
        return "The sign-in for that account has expired. Remove it and connect again.".into();
    }
    if lower.contains("transport endpoint") || lower.contains("mountpoint") {
        return "That mount point is in a bad state. Disconnect the drive and retry.".into();
    }
    let last = stderr.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
    if last.is_empty() {
        "rclone could not mount that account.".into()
    } else {
        format!("rclone could not mount that account: {last}")
    }
}

/// Disconnects an account, leaving the configuration in place.
pub fn unmount(account: &Account) -> Result<(), String> {
    if !account.mounted {
        return Ok(());
    }
    let helper = unmount_helper().ok_or("FUSE is not installed, so nothing can be unmounted.")?;
    let output = Command::new(helper)
        .arg("-u")
        .arg(&account.mount_point)
        .output()
        .map_err(|e| format!("Could not run the unmount helper: {e}"))?;

    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr);
    if message.to_lowercase().contains("busy") {
        return Err("Something is still using that drive. Close it and try again.".into());
    }
    Err(format!("Could not disconnect the drive: {}", message.trim()))
}

/// Details needed to add an account, whichever way it signs in.
pub struct NewAccount {
    pub provider: Provider,
    /// The label the user typed, used to derive the rclone remote name.
    pub name: String,
    pub username: String,
    pub password: String,
    /// Proton's second factor, when the account has one.
    pub totp: String,
    /// WebDAV endpoint for providers that do not have a fixed one.
    pub url: String,
}

/// Creates an rclone remote for a password-based provider.
///
/// The password is obscured by rclone before it is written, and is never held
/// anywhere in this process beyond the call: `rclone obscure` reads it on stdin
/// so it does not appear in the process list, where every other user on the
/// machine could read it.
pub fn connect_with_password(details: &NewAccount) -> Result<Account, String> {
    let remote = unique_remote_name(&details.name, details.provider);
    let backend = details.provider.backend();
    if backend.is_empty() {
        return Err("That provider cannot be set up from here.".into());
    }
    if details.username.trim().is_empty() {
        return Err("Enter the account's username or email.".into());
    }

    let obscured = obscure(&details.password)?;
    let mut args: Vec<String> =
        vec!["config".into(), "create".into(), remote.clone(), backend.into()];

    match details.provider {
        Provider::ProtonDrive => {
            args.push(format!("username={}", details.username.trim()));
            args.push(format!("password={obscured}"));
            if !details.totp.trim().is_empty() {
                args.push(format!("2fa={}", details.totp.trim()));
            }
        }
        Provider::Icedrive | Provider::Nextcloud | Provider::Other => {
            let url = details
                .provider
                .webdav_url()
                .map(str::to_string)
                .or_else(|| {
                    let url = details.url.trim();
                    (!url.is_empty()).then(|| url.to_string())
                })
                .ok_or("Enter the WebDAV address for that account.")?;
            args.push(format!("url={url}"));
            args.push("vendor=other".into());
            args.push(format!("user={}", details.username.trim()));
            args.push(format!("pass={obscured}"));
        }
        other => {
            return Err(format!("{} signs in through a browser, not a password.", other.label()));
        }
    }

    let output = rclone()?
        .args(&args)
        .output()
        .map_err(|e| format!("Could not run rclone: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "rclone refused the account: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let identity = details.username.trim().to_string();
    save_identity(&remote, &identity);

    // Prove the credentials work now rather than at the first navigation, and
    // remove the remote if they do not — a broken account in the sidebar is
    // worse than a failed sign-in the user can retry.
    if let Err(e) = verify(&remote) {
        let _ = forget(&remote);
        return Err(e);
    }

    Ok(Account {
        remote: remote.clone(),
        provider: details.provider,
        identity,
        mount_point: mount_root().join(&remote),
        mounted: false,
    })
}

/// Builds the command that runs a browser sign-in.
///
/// rclone opens the browser, runs a loopback server for the redirect, and
/// writes the token itself. This is handed back rather than run here because it
/// can take minutes of the user's attention, which must not block the UI.
pub fn oauth_command(details: &NewAccount) -> Result<(String, Vec<String>), String> {
    let path = rclone_path().ok_or_else(|| INSTALL_HINT.to_string())?;
    let remote = unique_remote_name(&details.name, details.provider);
    let backend = details.provider.backend();
    if backend.is_empty() {
        return Err("That provider cannot be set up from here.".into());
    }
    let mut args = vec![
        "config".to_string(),
        "create".to_string(),
        remote,
        backend.to_string(),
    ];
    if details.provider == Provider::GoogleDrive {
        // Full access, because a file manager that cannot see files it did not
        // create is not a file manager.
        args.push("scope=drive".into());
    }
    Ok((path.to_string_lossy().into_owned(), args))
}

/// Asks the provider who we just signed in as, and remembers it.
///
/// Without this two Google Drives are both called "Google Drive" and the user
/// has no way to tell which is which.
pub fn record_identity(remote: &str) -> String {
    let identity = fetch_identity(remote).unwrap_or_default();
    if !identity.is_empty() {
        save_identity(remote, &identity);
    }
    identity
}

fn fetch_identity(remote: &str) -> Option<String> {
    let output = rclone()
        .ok()?
        .args(["config", "userinfo", &format!("{remote}:"), "--json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let info: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    // Every backend spells it differently; the first one present wins.
    for key in ["email", "emailAddress", "user", "username", "displayName", "name"] {
        if let Some(value) = info.get(key).and_then(|v| v.as_str())
            && !value.is_empty()
        {
            return Some(value.to_string());
        }
    }
    None
}

/// A cheap round trip that fails loudly on bad credentials.
fn verify(remote: &str) -> Result<(), String> {
    let output = rclone()
        .map_err(|e| e.to_string())?
        .args(["lsjson", "--max-depth", "1", &format!("{remote}:")])
        .output()
        .map_err(|e| format!("Could not run rclone: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lower = stderr.to_lowercase();
    if lower.contains("401") || lower.contains("unauthor") || lower.contains("password") {
        return Err("Those credentials were rejected by the provider.".into());
    }
    if lower.contains("2fa") || lower.contains("totp") {
        return Err("That account needs a current two-factor code.".into());
    }
    Err(format!(
        "Could not reach the account: {}",
        stderr.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("unknown error")
    ))
}

/// Removes an account: unmounted, deleted from rclone, identity forgotten.
pub fn forget(remote: &str) -> Result<(), String> {
    let mount_point = mount_root().join(remote);
    if is_mounted(&mount_point)
        && let Some(helper) = unmount_helper()
    {
        let _ = Command::new(helper).arg("-u").arg(&mount_point).output();
    }
    let output = rclone()?
        .args(["config", "delete", remote])
        .output()
        .map_err(|e| format!("Could not run rclone: {e}"))?;
    forget_identity(remote);
    let _ = std::fs::remove_dir(&mount_point);
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Obscures a password the way rclone's config expects.
fn obscure(password: &str) -> Result<String, String> {
    use std::io::Write;

    let mut child = rclone()?
        .args(["obscure", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not run rclone: {e}"))?;

    child
        .stdin
        .take()
        .ok_or("Could not send the password to rclone.")?
        .write_all(password.as_bytes())
        .map_err(|e| format!("Could not send the password to rclone: {e}"))?;

    let output = child.wait_with_output().map_err(|e| format!("rclone failed: {e}"))?;
    if !output.status.success() {
        return Err("rclone could not store that password.".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Turns a display name into an rclone remote name that cannot collide.
///
/// rclone remote names are restricted, and a duplicate silently overwrites the
/// existing remote — which for the user means one Google Drive replacing
/// another. A numeric suffix is added until the name is free.
fn unique_remote_name(name: &str, provider: Provider) -> String {
    let taken: Vec<String> = accounts().into_iter().map(|a| a.remote).collect();
    let base = slug(name).unwrap_or_else(|| slug(provider.label()).unwrap_or_else(|| "cloud".into()));
    if !taken.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or(base)
}

/// rclone accepts letters, digits, underscore, hyphen and dot, and the name
/// cannot start with a hyphen.
fn slug(name: &str) -> Option<String> {
    let cleaned: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let collapsed = cleaned
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    (!collapsed.is_empty()).then_some(collapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_keep_two_accounts_of_one_provider_apart() {
        let make = |identity: &str| Account {
            remote: "x".into(),
            provider: Provider::GoogleDrive,
            identity: identity.into(),
            mount_point: PathBuf::new(),
            mounted: false,
        };
        assert_eq!(make("a@gmail.com").display_name(), "Google Drive — a@gmail.com");
        assert_ne!(make("a@gmail.com").display_name(), make("b@gmail.com").display_name());
        // Without an identity there is still a usable name.
        assert_eq!(make("").display_name(), "Google Drive");
    }

    #[test]
    fn remote_names_are_reduced_to_what_rclone_accepts() {
        assert_eq!(slug("Work Drive").as_deref(), Some("work-drive"));
        assert_eq!(slug("a@b.com").as_deref(), Some("a-b-com"));
        assert_eq!(slug("  --Photos!!  ").as_deref(), Some("photos"));
        assert_eq!(slug("   "), None, "a blank name cannot become a remote");
        assert_eq!(slug("!!!"), None);
    }

    /// Two WebDAV remotes look identical in rclone's config; the endpoint is
    /// the only thing that says which service they are.
    #[test]
    fn webdav_providers_are_told_apart_by_their_endpoint() {
        let with_url = |url: &str| {
            classify("remote", "webdav", &serde_json::json!({ "type": "webdav", "url": url }))
        };
        assert_eq!(with_url("https://webdav.icedrive.io"), Provider::Icedrive);
        assert_eq!(with_url("https://cloud.example.org/remote.php/dav"), Provider::Nextcloud);
        assert_eq!(with_url("https://files.example.org/dav"), Provider::Other);

        // Dedicated backends are read straight from the type.
        let typed = |backend: &str| classify("r", backend, &serde_json::json!({ "type": backend }));
        assert_eq!(typed("drive"), Provider::GoogleDrive);
        assert_eq!(typed("protondrive"), Provider::ProtonDrive);
        assert_eq!(typed("dropbox"), Provider::Dropbox);
    }

    #[test]
    fn oauth_providers_are_not_offered_a_password_field() {
        assert!(Provider::GoogleDrive.uses_oauth());
        assert!(Provider::OneDrive.uses_oauth());
        assert!(!Provider::ProtonDrive.uses_oauth());
        assert!(!Provider::Icedrive.uses_oauth());
    }

    /// A path that is not a mount point must never be reported as one, or the
    /// UI shows a connected drive that is really an empty folder.
    #[test]
    fn only_real_mount_points_are_reported_as_mounted() {
        let dir = crate::testing::TempDir::new("cloud");
        assert!(!is_mounted(dir.path()));
        assert!(!is_mounted(Path::new("/nonexistent-mount-point")));
        // The root always is one, which proves the parser reads the table.
        assert!(is_mounted(Path::new("/")));
    }
}

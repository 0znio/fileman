//! Downloading a file from a URL.
//!
//! A browser fetches a file over one connection. Most of the time the limit is
//! not your line but the server's per-connection shaping, so asking for several
//! byte ranges at once and writing them into one preallocated file is
//! materially faster on exactly the large files worth downloading from a file
//! manager. When the server won't play along — no `Accept-Ranges`, no length,
//! or a size too small to be worth splitting — it falls back to one stream and
//! nothing is lost.

use std::{
    fs,
    io::Read,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering},
    },
    time::Duration,
};

use super::{
    ops::{ITEM_INTERVAL_MS, JobHandle, JobKind, JobOutcome, Progress},
    parallel,
};

/// Read buffer per connection. Large enough that a fast link isn't syscall
/// bound, small enough that eight of them are not worth worrying about.
const CHUNK: usize = 256 * 1024;

/// Don't split anything smaller than this. Below it the extra requests cost
/// more in round-trips than the parallelism returns.
const MIN_PARALLEL_BYTES: u64 = 4 * 1024 * 1024;

/// Timeouts are per phase, never global.
///
/// A single global timeout is the obvious thing to reach for and is wrong here:
/// it bounds the whole transfer, so any download slower than the limit fails —
/// which is precisely the large files this feature exists for. A 140 MB fetch
/// died at 30 s under exactly that mistake. These instead bound the parts that
/// can hang without making progress, and leave the body free to take as long as
/// the file needs.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// What a probe of the URL tells us before committing to a download.
#[derive(Debug, Clone)]
pub struct Probe {
    pub url: String,
    /// Total size, when the server declares one.
    pub size: Option<u64>,
    /// Whether byte ranges are accepted, which is what makes splitting possible.
    pub supports_ranges: bool,
    /// The name to save as, from `Content-Disposition` or the URL path.
    pub filename: String,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_resolve(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        // Identify honestly. Some servers vary their behaviour by agent, and
        // pretending to be a browser to get better treatment is not our call
        // to make on the user's behalf.
        .user_agent(concat!("fileman/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Asks the server what it is offering, without downloading the body.
///
/// Used to fill in the size and the suggested filename before the user commits,
/// so the name can be changed with knowledge of what is actually there.
pub fn probe(url: &str) -> Result<Probe, String> {
    let url = normalise_url(url)?;
    let response = agent()
        .head(&url)
        .call()
        .map_err(|e| friendly_error(&e.to_string()))?;

    let headers = response.headers();
    let header = |name: &str| {
        headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
    };

    let size = header("content-length").and_then(|v| v.trim().parse::<u64>().ok());
    let supports_ranges = header("accept-ranges")
        .map(|v| v.to_lowercase().contains("bytes"))
        .unwrap_or(false);

    let filename = header("content-disposition")
        .as_deref()
        .and_then(filename_from_disposition)
        .or_else(|| filename_from_url(&url))
        .unwrap_or_else(|| "download".to_string());

    Ok(Probe { url, size, supports_ranges, filename: sanitize_filename(&filename) })
}

/// Adds a scheme when the user pasted a bare host, and rejects anything that
/// isn't http(s) — `file://` here would be a confusing way to copy a file.
fn normalise_url(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter a URL to download".into());
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let lower = with_scheme.to_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err("Only http and https addresses can be downloaded".into());
    }
    Ok(with_scheme)
}

/// Pulls a filename out of a `Content-Disposition` header.
///
/// Handles the `filename*=UTF-8''…` form as well as plain `filename=`, because
/// the extended form is what anything serving non-ASCII names actually sends
/// and it takes precedence when both are present.
fn filename_from_disposition(header: &str) -> Option<String> {
    if let Some(start) = header.to_lowercase().find("filename*=") {
        let value = header[start + "filename*=".len()..].trim();
        let value = value.split(';').next()?.trim().trim_matches('"');
        // RFC 5987: charset'language'percent-encoded-value
        let encoded = value.rsplit('\'').next()?;
        let decoded = urlencoding::decode(encoded).ok()?.into_owned();
        if !decoded.is_empty() {
            return Some(decoded);
        }
    }
    let start = header.to_lowercase().find("filename=")?;
    let value = header[start + "filename=".len()..].trim();
    let value = value.split(';').next()?.trim().trim_matches('"');
    (!value.is_empty()).then(|| value.to_string())
}

fn filename_from_url(url: &str) -> Option<String> {
    let without_query = url.split(['?', '#']).next()?;
    let last = without_query.rsplit('/').find(|s| !s.is_empty())?;
    let decoded = urlencoding::decode(last).ok()?.into_owned();
    (!decoded.is_empty()).then_some(decoded)
}

/// Reduces a server-supplied name to a single safe path component.
///
/// The name comes from a remote server, so it is treated the same way an
/// archive entry is: no directory separators, no `..`, nothing that could place
/// the file anywhere but where the user chose.
pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .replace(['/', '\\'], "_")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() { "download".to_string() } else { trimmed.to_string() }
}

/// How the work will be split, decided once so the UI can explain it.
fn connections_for(probe: &Probe) -> usize {
    let Some(size) = probe.size else { return 1 };
    if !probe.supports_ranges || size < MIN_PARALLEL_BYTES {
        return 1;
    }
    // Never more connections than there are whole megabytes to fetch, and never
    // more than the shared worker budget allows.
    let by_size = (size / (MIN_PARALLEL_BYTES / 4)).max(1) as usize;
    parallel::threads().min(by_size).max(1)
}

/// Spawns a download job writing to `dest`.
pub fn start(probe: Probe, dest: PathBuf) -> JobHandle {
    let (tx, rx) = async_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);

    std::thread::Builder::new()
        .name("fileman-download".into())
        .spawn(move || {
            let total = probe.size.unwrap_or(0);
            let _ = tx.send_blocking(Progress::Prepared { total_bytes: total, total_items: 1 });
            let name = dest
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let _ = tx.send_blocking(Progress::Item { name, done_items: 0 });

            let done = Arc::new(AtomicU64::new(0));
            let result = run(&probe, &dest, &worker_cancel, &tx, &done);

            let cancelled = worker_cancel.load(AtomicOrdering::Relaxed);
            let mut errors = Vec::new();
            let mut created = Vec::new();

            match result {
                Ok(()) if !cancelled => created.push(dest.clone()),
                Ok(()) => {
                    // A cancelled download leaves a partial file that looks
                    // complete in the listing; remove it.
                    let _ = fs::remove_file(&dest);
                }
                Err(e) => {
                    let _ = fs::remove_file(&dest);
                    errors.push((dest.clone(), e));
                }
            }

            let _ = tx.send_blocking(Progress::Finished(JobOutcome {
                cancelled,
                items_done: u64::from(errors.is_empty() && !cancelled),
                bytes_done: done.load(AtomicOrdering::Relaxed),
                errors,
                created,
            }));
        })
        .expect("spawn download thread");

    JobHandle::new(JobKind::Download, rx, cancel)
}

fn run(
    probe: &Probe,
    dest: &Path,
    cancel: &AtomicBool,
    tx: &async_channel::Sender<Progress>,
    done: &Arc<AtomicU64>,
) -> Result<(), String> {
    let connections = connections_for(probe);
    let reporter = Reporter::new(tx.clone(), Arc::clone(done));

    if connections <= 1 {
        return single(probe, dest, cancel, &reporter);
    }
    let size = probe.size.expect("connections_for only splits with a known size");
    parallel_ranges(probe, dest, size, connections, cancel, &reporter)
}

/// One connection, streamed straight to disk.
fn single(
    probe: &Probe,
    dest: &Path,
    cancel: &AtomicBool,
    reporter: &Reporter,
) -> Result<(), String> {
    let response = agent()
        .get(&probe.url)
        .call()
        .map_err(|e| friendly_error(&e.to_string()))?;

    let file = fs::File::create(dest).map_err(|e| format!("Cannot write {}: {e}", dest.display()))?;
    if let Some(size) = probe.size {
        let _ = file.set_len(size);
    }

    let mut body = response.into_body().into_reader();
    let mut buffer = vec![0u8; CHUNK];
    let mut offset = 0u64;

    loop {
        if cancel.load(AtomicOrdering::Relaxed) {
            return Ok(());
        }
        let read = body.read(&mut buffer).map_err(|e| format!("Download interrupted: {e}"))?;
        if read == 0 {
            break;
        }
        file.write_all_at(&buffer[..read], offset)
            .map_err(|e| format!("Cannot write {}: {e}", dest.display()))?;
        offset += read as u64;
        reporter.add(read as u64);
    }

    // The server may have declared more than it sent; trim rather than leave a
    // file padded with the zeros `set_len` created.
    let _ = file.set_len(offset);
    Ok(())
}

/// Several connections, each fetching its own byte range into one file.
fn parallel_ranges(
    probe: &Probe,
    dest: &Path,
    size: u64,
    connections: usize,
    cancel: &AtomicBool,
    reporter: &Reporter,
) -> Result<(), String> {
    let file = fs::File::create(dest).map_err(|e| format!("Cannot write {}: {e}", dest.display()))?;
    // Preallocate so the ranges can be written in any order and the filesystem
    // can pick one extent instead of growing the file under eight writers.
    file.set_len(size).map_err(|e| format!("Cannot size {}: {e}", dest.display()))?;
    let file = Arc::new(file);

    let span = size.div_ceil(connections as u64);
    let ranges: Vec<(u64, u64)> = (0..connections as u64)
        .map(|i| (i * span, ((i + 1) * span).min(size).saturating_sub(1)))
        .filter(|(start, end)| start <= end)
        .collect();

    let failure: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    parallel::for_each(&ranges, |&(start, end)| {
        if cancel.load(AtomicOrdering::Relaxed) || failure.lock().is_ok_and(|f| f.is_some()) {
            return;
        }
        if let Err(e) = fetch_range(probe, &file, start, end, cancel, reporter)
            && let Ok(mut slot) = failure.lock()
        {
            slot.get_or_insert(e);
        }
    });

    match failure.into_inner() {
        Ok(Some(e)) => Err(e),
        _ => Ok(()),
    }
}

fn fetch_range(
    probe: &Probe,
    file: &Arc<fs::File>,
    start: u64,
    end: u64,
    cancel: &AtomicBool,
    reporter: &Reporter,
) -> Result<(), String> {
    let response = agent()
        .get(&probe.url)
        .header("Range", format!("bytes={start}-{end}"))
        .call()
        .map_err(|e| friendly_error(&e.to_string()))?;

    // A server that ignores Range answers 200 with the whole body. Writing that
    // into one slice would corrupt the file, so bail and let the caller know.
    if response.status().as_u16() != 206 {
        return Err("The server ignored the range request".to_string());
    }

    let mut body = response.into_body().into_reader();
    let mut buffer = vec![0u8; CHUNK];
    let mut offset = start;

    loop {
        if cancel.load(AtomicOrdering::Relaxed) {
            return Ok(());
        }
        let read = body.read(&mut buffer).map_err(|e| format!("Download interrupted: {e}"))?;
        if read == 0 {
            break;
        }
        // Never write past the range this connection owns, however much the
        // server decides to send.
        let allowed = (end + 1).saturating_sub(offset).min(read as u64) as usize;
        if allowed == 0 {
            break;
        }
        file.write_all_at(&buffer[..allowed], offset)
            .map_err(|e| format!("Cannot write to the file: {e}"))?;
        offset += allowed as u64;
        reporter.add(allowed as u64);
    }
    Ok(())
}

/// Totals bytes across connections and throttles what reaches the UI.
struct Reporter {
    tx: async_channel::Sender<Progress>,
    done: Arc<AtomicU64>,
    started: std::time::Instant,
    last_ms: AtomicU64,
}

impl Reporter {
    fn new(tx: async_channel::Sender<Progress>, done: Arc<AtomicU64>) -> Self {
        Self { tx, done, started: std::time::Instant::now(), last_ms: AtomicU64::new(0) }
    }

    fn add(&self, n: u64) {
        let total = self.done.fetch_add(n, AtomicOrdering::Relaxed) + n;
        let now = self.started.elapsed().as_millis() as u64;
        let last = self.last_ms.load(AtomicOrdering::Relaxed);
        if u128::from(now.saturating_sub(last)) >= ITEM_INTERVAL_MS
            && self
                .last_ms
                .compare_exchange(last, now, AtomicOrdering::Relaxed, AtomicOrdering::Relaxed)
                .is_ok()
        {
            let _ = self.tx.send_blocking(Progress::Bytes { done_bytes: total });
        }
    }
}

/// Turns a transport error into something worth showing a person.
fn friendly_error(raw: &str) -> String {
    let lower = raw.to_lowercase();
    if lower.contains("dns") || lower.contains("resolve") {
        return "Could not find that server — check the address.".to_string();
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return "The server did not respond in time.".to_string();
    }
    if lower.contains("certificate") || lower.contains("tls") {
        return "The server's security certificate could not be verified.".to_string();
    }
    if lower.contains("404") {
        return "That address does not exist on the server (404).".to_string();
    }
    if lower.contains("403") {
        return "The server refused the request (403).".to_string();
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_gets_https_and_other_schemes_are_refused() {
        assert_eq!(normalise_url("example.com/f.zip").unwrap(), "https://example.com/f.zip");
        assert_eq!(normalise_url("http://x.dev/a").unwrap(), "http://x.dev/a");
        assert!(normalise_url("ftp://x.dev/a").is_err());
        assert!(normalise_url("file:///etc/passwd").is_err());
        assert!(normalise_url("   ").is_err());
    }

    #[test]
    fn filenames_come_from_the_disposition_header_when_present() {
        assert_eq!(
            filename_from_disposition("attachment; filename=\"report.pdf\"").as_deref(),
            Some("report.pdf")
        );
        // The extended form wins, and is percent-decoded.
        assert_eq!(
            filename_from_disposition(
                "attachment; filename=\"fallback.bin\"; filename*=UTF-8''caf%C3%A9%20menu.pdf"
            )
            .as_deref(),
            Some("café menu.pdf")
        );
        assert_eq!(filename_from_disposition("inline").as_deref(), None);
    }

    #[test]
    fn filenames_fall_back_to_the_url_path() {
        assert_eq!(filename_from_url("https://x.dev/a/b/linux.tar.xz").as_deref(), Some("linux.tar.xz"));
        assert_eq!(filename_from_url("https://x.dev/a/file%20name.zip").as_deref(), Some("file name.zip"));
        assert_eq!(filename_from_url("https://x.dev/f.bin?token=1#frag").as_deref(), Some("f.bin"));
        assert_eq!(filename_from_url("https://x.dev/").as_deref(), Some("x.dev"));
    }

    /// The name is chosen by a remote server, so it must not be able to escape
    /// the folder the user picked.
    #[test]
    fn a_server_cannot_choose_a_path() {
        // The property that matters is that whatever comes back is one inert
        // path component, not any particular spelling of it.
        for hostile in [
            "../../etc/passwd",
            "/etc/shadow",
            "..",
            "...",
            "",
            "  ",
            "with\nnewline.txt",
            "C:\\Windows\\system32",
            "a/b/c",
        ] {
            let safe = sanitize_filename(hostile);
            assert!(!safe.is_empty(), "{hostile:?} produced an empty name");
            assert!(!safe.contains('/'), "{hostile:?} kept a separator: {safe:?}");
            assert!(!safe.contains('\\'), "{hostile:?} kept a separator: {safe:?}");
            assert!(!safe.starts_with('.'), "{hostile:?} stayed hidden or relative: {safe:?}");
            assert!(!safe.chars().any(char::is_control), "{hostile:?} kept a control char");
            assert_eq!(
                Path::new(&safe).components().count(),
                1,
                "{hostile:?} produced more than one component: {safe:?}"
            );
        }

        // Ordinary names survive untouched.
        assert_eq!(sanitize_filename("  spaced.txt  "), "spaced.txt");
        assert_eq!(sanitize_filename("linux-6.9.tar.xz"), "linux-6.9.tar.xz");
    }

    #[test]
    fn splitting_only_happens_when_it_can_help() {
        let base = Probe {
            url: "https://x.dev/f".into(),
            size: Some(500 * 1024 * 1024),
            supports_ranges: true,
            filename: "f".into(),
        };
        assert!(connections_for(&base) > 1, "a large ranged file should split");

        let no_ranges = Probe { supports_ranges: false, ..base.clone() };
        assert_eq!(connections_for(&no_ranges), 1, "cannot split without ranges");

        let unknown = Probe { size: None, ..base.clone() };
        assert_eq!(connections_for(&unknown), 1, "cannot split without a length");

        let small = Probe { size: Some(100 * 1024), ..base.clone() };
        assert_eq!(connections_for(&small), 1, "not worth splitting something tiny");
    }

    /// Every byte of the file has to be owned by exactly one connection.
    #[test]
    fn ranges_tile_the_file_exactly_once() {
        for size in [1u64, 5, 1000, 1_048_576, 999_983] {
            for connections in 1..=8usize {
                let span = size.div_ceil(connections as u64);
                let ranges: Vec<(u64, u64)> = (0..connections as u64)
                    .map(|i| (i * span, ((i + 1) * span).min(size).saturating_sub(1)))
                    .filter(|(start, end)| start <= end)
                    .collect();

                let covered: u64 = ranges.iter().map(|(s, e)| e - s + 1).sum();
                assert_eq!(covered, size, "size {size} over {connections} connections");
                for pair in ranges.windows(2) {
                    assert_eq!(pair[0].1 + 1, pair[1].0, "gap or overlap in {ranges:?}");
                }
                assert!(ranges.iter().all(|&(_, e)| e < size), "range past the end");
            }
        }
    }
}


# Fileman

A GTK4/libadwaita file manager in Rust, built for a Linux desktop that dual-boots
Windows.

It does what a file manager is supposed to do — browse, copy, move, rename,
trash — and adds the three things that usually send you to a terminal: mounting
NTFS drives that Windows left dirty, extracting any archive format, and deleting
something so it doesn't come back.

## Features

**Browsing**

- Icon grid and details list, sharing one selection, sort and filter
- Back / forward / up, with mouse buttons 8 and 9 wired up. Back and Up often
  land in the same place, so each button's tooltip names where it will actually
  take you
- Breadcrumb location bar that becomes an editable path on click or `Ctrl+L`,
  with `~`, `$VAR`, `file://` and relative-path support plus inline completion
- Search (`Ctrl+F`) that filters the current folder instantly and then walks the
  whole subtree in the background, streaming matches in as it finds them
- Hidden-file toggle, folders-before-files
- Column-header sorting that the grid follows
- Icon size from 16 to 96 px, `Ctrl+scroll` to zoom
- Image thumbnails via the shared freedesktop cache — see Performance below
- Light / dark / follow-system theme
- Drag and drop between panes, onto folders, and onto sidebar rows

**Sidebar**

- Home and the XDG user folders — with a fallback for systems that have no
  `user-dirs.dirs`, where they would otherwise be missing entirely
- Favourites you add with `Ctrl+D`
- Trash, with a live count
- Every mountable volume, grouped: **Removable Devices**, **Windows**,
  **On This Computer**. Internal partitions with no user data — EFI, Microsoft
  Reserved, Windows Recovery, swap, the running root — are filtered out
- Live updates on hotplug, mount and unmount, including changes made by other
  applications

**Drives**

Mounting goes through UDisks2 over D-Bus, as your own user via polkit. There is
no `sudo`, no fstab editing, and no hardcoded `/dev/sdX` that breaks when a
drive re-enumerates.

**Archives**

Extraction is in-process through libarchive: zip, 7z, rar and rar5, tar with
every common compressor, plus cab, iso, xar, lha, ar, deb and rpm. Entries are
streamed one at a time, so extraction reports real byte progress and can be
cancelled mid-file. Encrypted archives prompt for a password.

Well-rooted archives extract in place; loose ones get a container folder so a
400-file tarball can't explode into your Downloads. Creating archives (zip,
tar.gz, tar.xz, tar.zst, 7z) is available too.

**Deleting**

Trash is the default and is undoable straight from the toast. `Shift+Delete`
deletes permanently. `Ctrl+Shift+Delete` shreds — see below.

## The NTFS problem

An NTFS volume that Windows didn't unmount cleanly — which, with Fast Startup
on, is *every* normal Windows shutdown — is refused by `ntfs-3g`, and most file
managers just show you the driver's error and stop.

Fileman detects that specific failure and offers the three real ways out,
ordered by risk:

| Option | What it does | Risk |
| --- | --- | --- |
| **Open read-only** (default) | Mounts read-only. Browse and copy files off. | None |
| **Repair and mount** | Runs `ntfsfix -d` to clear the dirty flag, schedules Windows' own chkdsk for next boot, retries. | Low; needs admin |
| **Force read-write** | Mounts with `remove_hiberfile`, discarding the hibernation image. | Your files are untouched, but a suspended Windows session can't be resumed |

The underlying driver message is always shown under "Technical details" — it's
often the only clue when none of the three work.

Repair needs `ntfsfix`, from the `ntfs-3g` package. It isn't currently installed
here; the dialog says so and hides the option rather than offering something
that will fail.

### Compared to `~/load-ssd.sh`

That script's core idea — resolve through `/dev/disk/by-uuid` rather than a
`/dev/sdX` that shifts — is right, and Fileman keeps it. What changes:

- **No `sudo`.** UDisks2 mounts as you, via polkit.
- **Internal drives too.** The script deliberately filters to USB disks, so the
  Windows, Fat Boy and Skinny Boy partitions are out of reach. Fileman lists them.
- **Dirty NTFS is handled**, instead of the mount just failing.
- **Stale mounts** are UDisks2's problem, not yours.
- **Unmount and eject** are in the same place as mount.

The script still has one thing Fileman doesn't: it works over SSH with no
display. Keep it for that.

> A note on the script itself: the NTFS branch passes `unmask=000`. `ntfs-3g`
> knows `umask`, `fmask` and `dmask` — there is no `unmask`, so that looks like
> a typo for `umask=000`. It evidently isn't fatal (the drive does mount), so
> the option is being tolerated and silently doing nothing; the `uid`/`gid` on
> the same line are what actually give you access. Worth fixing if you keep
> using the script.

## On shredding

Overwriting a file before unlinking it works on a filesystem that overwrites in
place, on a magnetic disk. That is a narrower set of conditions than it sounds.

Fileman checks the target's filesystem and storage before it starts, and warns
plainly when the guarantee doesn't hold:

- **btrfs, ZFS, bcachefs** — copy-on-write; overwrites go to new blocks and the
  originals can survive in old extents or snapshots
- **overlayfs** — the original lives in a lower layer an overwrite can't reach
- **F2FS, UBIFS, JFFS2** — log-structured; the original blocks are erased later, if ever
- **tmpfs** — may persist in swap
- **NFS, SMB, sshfs** — the server owns the storage
- **Any SSD** — wear levelling can relocate data before the overwrite lands

The dialog states which one applies and that the overwrite is best-effort. The
files are still deleted either way. If you need a real guarantee, full-disk
encryption is the answer, not more passes — the pass count is configurable in
Preferences mostly so you can turn it *down*.

The implementation overwrites with random data, finishes with a zero pass,
`fsync`s after each one so the passes can't be coalesced in page cache,
truncates, renames the entry to obscure the original name, and unlinks. Each
directory is `fsync`ed once at the end rather than once per file. Symlinks are
unlinked rather than followed — shredding through a link would destroy the
target instead.

## Performance

The two things that make a file manager feel slow are blocking the main thread
and doing work twice. Both are measurable, so both were measured: set
`FILEMAN_TRACE=1` and the app reports directory scan times and — more usefully —
any gap between main-loop ticks long enough for a person to notice.

```sh
FILEMAN_TRACE=1 fileman ~/Pictures
```

`FILEMAN_SNAPSHOT=<path>` is the companion for layout work: it renders the
window straight to a PNG and exits, so a spacing or truncation change can be
checked without taking a compositor screenshot.

```sh
FILEMAN_SNAPSHOT=/tmp/grid.png fileman ~/Downloads
```

Opening a folder of 46 4K wallpapers originally logged 46 stalls totalling
**8.2 seconds**, the worst a single 547 ms freeze, and decoded 92 thumbnails for
46 files. Four changes fixed it:

- **Thumbnails come from the shared freedesktop cache first.** Nautilus, Thunar
  and friends already populate `~/.cache/thumbnails`, so most images never need
  decoding at all. Anything Fileman does generate is written back — spec-correct
  filename, `Thumb::URI`/`Thumb::MTime` metadata, `0600`, atomic rename — so the
  cost is paid once for the whole desktop, not once per launch.
- **Decoding runs on threads Fileman owns.** gdk-pixbuf's `*_async` loaders are
  documented as threaded, but the stall detector showed otherwise; taking the
  thread removes the doubt. Concurrency is capped so a folder of 25 MB photos
  can't queue forty simultaneous decodes.
- **Identical requests are deduplicated** while in flight, and rows cancel their
  request when they scroll away — unless something else is waiting on it.
- **The hidden view is detached from the model.** A `GtkStack` keeps its
  inactive page alive, so the grid and the list were both binding every row and
  requesting every thumbnail twice, at two different sizes.

Result on the same folder: **one stall of ~140 ms** (window startup), 46 decodes
instead of 92, and on any later visit every thumbnail comes from cache with no
decoding at all.

### Compressing, extracting and shredding

These three were profiled the same way, on a 446 MB / 36,700-file corpus of real
system files plus some incompressible blobs, on ext4/NVMe. Every number below is
measured, before and after, on eight threads.

**Compression was single-threaded.** libarchive compresses on one core unless it
is told otherwise, which is the whole story for `.tar.xz`. Both liblzma and
libzstd split the input into independent blocks, so passing a thread count is
all that is needed:

| Format | Before | After |
| --- | --- | --- |
| `.tar.xz` | 77.7 s | **16.6 s** |
| `.tar.zst` | 1.26 s | **0.58 s** |

Deflate has no threaded encoder inside libarchive, so `.zip` is unavoidably
serial. `.tar.gz` has a way out: bsdtar writes an uncompressed tar to a pipe and
[`pigz`](https://zlib.net/pigz/) compresses it, producing an ordinary gzip
stream nothing downstream can distinguish. That path is taken automatically when
`pigz` is installed and skipped when it isn't.

**Extraction read every archive twice.** Listing a `.tar.gz` or `.tar.xz` means
decompressing all of it, and the old flow listed the archive to decide where to
put things and then decompressed it again to extract. Everything the listing was
for is now derived during the single extracting pass: entries land in a hidden
staging directory and are moved into place with a `rename` at the end, which
costs nothing regardless of size.

| Format | Before | After |
| --- | --- | --- |
| `.tar.xz` | 2.57 s | **1.70 s** |
| `.tar.zst` | 1.31 s | **1.08 s** |
| `.zip` | 1.86 s | **1.64 s** |

The progress bar is measured against *compressed* bytes consumed — the
uncompressed total isn't knowable without decompressing first, but the archive's
size on disk is known instantly, so this gives an exact percentage for free.

**Shredding was bound by `fsync` latency, not bandwidth.** One 256 MB file ran
at 2 GB/s while 2000 8 KB files took 9.3 s — 4.6 ms each, spent almost entirely
waiting for flushes to land. Three changes:

- **Files are shredded on several threads.** The workers sit in `fsync`, so they
  cost almost no CPU, and the device coalesces their flushes into shared journal
  commits.
- **One directory flush per directory**, not per file.
- **One random draw per pass**, not per chunk. The generator ran at roughly twice
  the disk's write rate, so the two took turns and each pass cost the sum rather
  than the maximum. This is not a security trade: the pass that matters is the
  write, and the last one is plain zeros by design.

| Workload (3 passes) | Before | After |
| --- | --- | --- |
| 2000 × 8 KB | 9.31 s | **1.79 s** |
| 200 × 1 MB | 1.35 s | **0.31 s** |
| 1 × 256 MB | 0.40 s | **0.27 s** |

**Deleting is already at the filesystem's ceiling** and was left alone on
purpose. Parallel `unlink` measured completely flat on ext4 — 0.50 s at one,
four, six and eight workers — because the journal serialises it, so threading it
would have been complexity for nothing. What did change is that a permanent
delete no longer walks the tree twice: it used to `stat` every file to compute a
byte total the progress bar never counted up to, since `remove_dir_all` reports
nothing and a 36,700-file folder showed "1 of 36743" before jumping to done.
Walking and unlinking in one pass is slightly faster (0.55 s → 0.53 s) and makes
the count mean something.

### Listing a directory

Opening an 8,463-file folder cost 613 ms in gio alone. Timing the enumeration
one attribute at a time found almost all of it in a single one:

| Attributes requested | Time |
| --- | --- |
| `standard::name` | 8.3 ms |
| `+ type, size, mtime, display-name` | 35.4 ms |
| `+ standard::content-type` | **582.6 ms** |

gio derives the content type per file, and for anything it cannot name from the
filename it opens the file and sniffs the contents. So Fileman does not ask for
it. It derives the type itself, memoised on the name's suffix — almost every
directory is a handful of suffixes repeated, so the second `.json` in a folder
of 8,463 costs nothing — and classifies extensionless files from their mode bits
instead of by reading them. **613 ms → 62 ms.**

The memo key is the whole suffix from the first dot, not the last extension.
That distinction matters: keying on the last extension makes `libc.so.6` a `.6`
file, and shared-mime-info reports `*.6` as a man page — every shared library on
the system mislabelled. Probing `x.so.6` still matches `*.so.*` and gets it
right.

Compared against gio's own answer over 7,536 files in `/usr/bin`, `/usr/lib`,
`/etc` and `/usr/share/doc`, 5.8% differ, all of them extensionless files where
gio sniffed a shebang. Over the folders a person actually browses — home,
Documents, Downloads, Pictures, a source tree — 0.16% differ, 14 files out of
8,621. Properties still shows gio's full answer for the one file being
inspected, where a single sniff costs nothing.

Sorting was the other hot path: an 8,500-entry folder is roughly 110,000
comparisons, and reaching an entry through `FileObject::entry()` deep-copies a
`PathBuf` and five `String`s on each side of every one. `compare_to` borrows
instead, taking the sort from 38.5 ms to 27.1 ms. The whole model chain —
splicing in batches, filtering, incremental sort — settles in 38.5 ms for 8,463
items; it was never where the time went.

### Threads

Compressing, extracting and shredding all draw from one budget, and it
deliberately does not take the machine over: the default is half the cores, so a
16-core machine runs jobs on eight and keeps eight for whatever else is running.
A job that finishes 15% sooner is a bad trade for a desktop that stutters while
it runs. Settings → Performance → Worker threads overrides it; 0 means
automatic.

Search is bounded the same way: the walk runs on a worker thread and streams
matches through a small bounded channel, so the walker is throttled to the speed
results are consumed rather than building a list in memory. `readdir` already
says whether an entry is a directory, so only entries whose *name* matches cost
a `stat`. Walking a 156,000-entry home directory takes well under a second, and
results appear while it is still going.

## Requirements

Runtime: `gtk4`, `libadwaita`, `libarchive`, `udisks2`, and a polkit agent
(any desktop session has one).

Optional: `pigz` for multi-threaded `.tar.gz` creation, `ntfs-3g` for NTFS
repair, `libarchive`'s `bsdtar` for creating
archives, `gvfs` for the trash on removable drives.

Build: Rust 1.92+ and the development headers for the above.

```sh
# Arch
sudo pacman -S --needed rust gtk4 libadwaita libarchive udisks2 ntfs-3g pigz
```

## Build and install

```sh
make            # cargo build --release
make test
make install    # to ~/.local — no root needed
```

For a system-wide install: `sudo make install PREFIX=/usr/local`.

Run it with `fileman [path]`. A second invocation opens a window in the running
instance rather than starting a new process.

## Keyboard shortcuts

`Ctrl+?` shows the full list in the app. The ones worth knowing:

| Key | Action |
| --- | --- |
| `Alt+←` / `Alt+→` / `Alt+↑` | Back / forward / up |
| `Ctrl+L` | Edit the location as text |
| `Ctrl+F` | Search this folder and everything under it |
| `Ctrl+H` | Show hidden files |
| `F2` | Rename |
| `Delete` / `Shift+Delete` / `Ctrl+Shift+Delete` | Trash / delete / shred |
| `Ctrl+E` / `Ctrl+Shift+E` | Extract here / compress |
| `Ctrl+D` | Add to Favourites |
| `Ctrl+Shift+V` | Switch grid and list |
| `Alt+Return` | Properties |
| `F9` | Toggle the sidebar |

## Layout

```
src/
  app.rs          application, command line, stylesheet
  config.rs       settings, persisted atomically to ~/.config/fileman
  history.rs      back/forward
  fs/             entry model, scanning, copy/move/delete, trash, shredding
  archive/        libarchive extraction and bsdtar creation
  drives/         UDisks2 over D-Bus, NTFS recovery
  ui/             window, actions, sidebar, path bar, views, dialogs
```

Long operations run on worker threads and report through an async channel;
conflicts are resolved by handing the UI a one-shot reply channel and blocking
the worker on it, so the decision stays synchronous with the copy loop without
the worker ever touching a widget.

## Known gaps

- Renaming several files at once isn't implemented (it toasts and does nothing)
- Thumbnails are still images only; video and document previews would need
  out-of-process thumbnailers
- No tabs, no split view, no network locations (`smb://`, `sftp://`)
- LUKS volumes are listed and labelled encrypted, but unlocking isn't wired up
- Search matches file *names*, not file contents

## Licence

MIT

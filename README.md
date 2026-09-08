<div align="center">

<img src="docs/icon.png" width="120" alt="Fileman">

# Fileman

**A fast GTK4/libadwaita file manager written in Rust, for Linux.**

</div>

![Fileman](docs/screenshot.png)

It browses, copies, moves and renames like any file manager, and handles the
things that usually send you to a terminal: mounting NTFS drives Windows left
dirty, unlocking encrypted volumes, extracting any archive format, downloading a
file without opening a browser, and deleting something for good.

## Features

**Browsing**
- Tabs — `Ctrl+T` opens one, `Ctrl+W` closes it, `Ctrl+Tab` cycles, and a
  folder's context menu can open it in its own tab. Each tab keeps its own
  history, scroll position and selection; the strip hides itself when only one
  is open
- Icon grid and details list sharing one selection, sort and filter
- Back / forward / up, mouse buttons 8 and 9, per-button tooltips saying where each goes
- Breadcrumb bar that becomes an editable path (`Ctrl+L`) with `~`, `$VAR`, `file://`,
  relative paths and inline completion
- Search (`Ctrl+F`) filters the folder instantly, then walks the whole subtree in the
  background, streaming matches in
- Hidden files, folders-first, column sorting the grid follows
- Icon size 16–96 px, `Ctrl+scroll` to zoom, with a readout that keeps up
- Image thumbnails via the shared freedesktop cache
- Light / dark / follow-system
- Drag and drop between panes, onto folders, and onto sidebar rows

**Sidebar**
- Grouped into **Places** (Home and the XDG user folders, with a fallback for
  systems that have no `user-dirs.dirs`) and **System**
- **Recent** — the freedesktop recent-files list, the same one the rest of the
  desktop writes to
- **Network** — shares gvfs has mounted into the session
- Favourites (`Ctrl+D`), Trash with a live count
- Every mountable volume, grouped into **Removable Devices**, **Windows** and **On This
  Computer**, each with a capacity ring. EFI, Microsoft Reserved, Recovery, swap and the
  running root are filtered out
- Live updates on hotplug, mount and unmount, including changes made by other apps

**Drives** — mounting goes through UDisks2 over D-Bus as your own user, via
polkit. No `sudo`, no fstab editing, no hardcoded `/dev/sdX` that breaks when a
drive re-enumerates. The button beside a mounted drive unmounts it and leaves it
listed; **Eject** — which powers the drive down so it disappears until you
unplug it — is in the right-click menu, named for what it does.

Mounting an internal partition needs authentication, and that prompt is drawn by
the desktop's polkit agent. If none is running the mount fails instantly with no
way to say yes, so Fileman checks and tells you which command starts one rather
than reporting a bare "not authorized".

**Archives** — extraction is in-process through libarchive: zip, 7z, rar/rar5,
tar with every common compressor, cab, iso, xar, lha, ar, deb, rpm. Entries are
streamed one at a time, so progress is real and extraction can be cancelled
mid-file. Encrypted archives prompt for a password. Well-rooted archives extract
in place; loose ones get a container folder. Creates zip, tar.gz, tar.xz,
tar.zst and 7z.

**Downloading** — `Ctrl+Shift+D` takes a URL and probes it, so the suggested
filename and the size come from the server rather than from guessing at the
address; you can rename it and choose the destination folder before anything
starts, and it fetches with progress and cancel. Large files that the server
serves with `Accept-Ranges` are split across several connections: measured on a
140 MB kernel tarball, **34.3 s on one connection against 9.1 s on eight —
3.75×** —
with byte-identical output. Anything the server won't split falls back to a
single stream.

**Encrypted drives** — a LUKS volume prompts for its passphrase and is unlocked
through UDisks2, then mounted. A wrong passphrase just asks again; unlocking only
establishes a device-mapper mapping and never writes to the LUKS header.

**Appearance** — light / dark / follow-system, and an accent colour (eight
presets or any colour you pick) that restyles the running window immediately.

**Deleting** — Trash by default, undoable from the toast. `Shift+Delete` deletes
permanently, `Ctrl+Shift+Delete` shreds.

## NTFS drives Windows left dirty

With Fast Startup on, *every* normal Windows shutdown leaves NTFS volumes
marked dirty. `ntfs-3g` refuses them and most file managers just show you the
driver error and stop.

Fileman detects that specific failure and offers the three real ways out:

| Option | What it does | Risk |
| --- | --- | --- |
| **Open read-only** (default) | Mounts read-only — browse and copy files off | None |
| **Repair and mount** | `ntfsfix -d` clears the dirty flag, schedules Windows' chkdsk for next boot, retries | Low; needs admin |
| **Force read-write** | Mounts with `remove_hiberfile`, discarding the hibernation image | Files are untouched, but a suspended Windows session can't resume |

The driver's own message is always shown under "Technical details". Repair needs
`ntfsfix` from the `ntfs-3g` package; if it isn't installed the dialog says so
and hides the option rather than offering something that will fail.

Separately, the in-kernel `ntfs3` driver rejects dirty volumes outright, and it
is what you get by default: `blkid` reports these volumes as `ntfs`, and the
kernel resolves that name straight to `ntfs3` (the module carries
`alias: fs-ntfs3`). Retrying with `fstype=ntfs` therefore changes nothing — it
is the same string the first attempt already used. Fileman asks for `ntfs-3g`
instead, which UDisks2 treats as a different filesystem and routes through the
FUSE helper; that driver mounts volumes Windows left dirty without complaint.

Which driver mounted a volume is remembered per UUID, so a drive that needed
`ntfs-3g` once goes straight there next time. That is not only about speed:
every mount attempt is a UDisks2 job, failures included, and other desktop
components watch those. `udiskie` pops a "Job failed" notification for a first
attempt that fails even when the retry immediately succeeds — so a mount that
worked still looks broken. Getting the driver right first time is the only way
to keep that quiet. If the volume later mounts with the kernel driver again, the
preference is dropped.

**Repair and mount** is the option that ends the problem rather than working
around it, so it leads when it is available: `ntfsfix` clears the dirty flag and
asks Windows to check the volume on its next boot, after which it mounts clean
on the faster in-kernel driver. It needs `ntfsfix`, which ships in **ntfsprogs**
— a separate package from `ntfs-3g` on most distributions. Without it the option
is unavailable and the dialog says so rather than quietly omitting it.

## Shredding

Overwriting before unlinking works on a filesystem that overwrites in place
(ext4 without data journalling, XFS, vfat, NTFS) on magnetic media. It is *not*
a guarantee on copy-on-write filesystems or flash, so Fileman detects btrfs,
ZFS, bcachefs, overlayfs, F2FS, tmpfs, network mounts and SSDs and says exactly
why the guarantee doesn't hold before you commit.

Files are overwritten with random data, finish with a zero pass, `fsync`ed after
each one, truncated, renamed, then unlinked. Symlinks are unlinked rather than
followed.

## Performance

Measured on a 446 MB / 36,700-file corpus, ext4 on NVMe, 8 threads.

| | Before | After |
| --- | --- | --- |
| Compress `.tar.xz` | 77.7 s | **16.6 s** |
| Compress `.tar.gz` | 6.29 s | **1.03 s** |
| Compress `.tar.zst` | 1.26 s | **0.58 s** |
| Extract `.tar.xz` | 2.57 s | **1.70 s** |
| Shred 2000 × 8 KB | 9.31 s | **1.79 s** |
| List an 8,463-file folder | 613 ms | **62 ms** |

What made the difference:

- **libarchive compresses on one core** unless told otherwise. Both liblzma and
  libzstd split input into independent blocks, so they just needed a thread
  count. Deflate has no threaded encoder, so `.tar.gz` pipes through `pigz`
  instead when it is installed.
- **Extraction used to read every archive twice** — once to list it, once to
  extract. Listing a `.tar.gz` means decompressing all of it. Entries now land
  in a staging directory and are moved into place with a `rename` at the end.
- **Shredding is bound by `fsync` latency, not bandwidth.** Files are shredded
  on several threads, which sit blocked in `fsync` and cost almost no CPU while
  the device coalesces their commits. One directory flush per directory, one
  random draw per pass.
- **Deleting was left alone deliberately** — parallel `unlink` measured
  completely flat on ext4, because the journal serialises it.
- **Listing** asked gio for `standard::content-type`, which costs a mime lookup
  per file and an open-and-read for anything it can't name. Fileman derives it
  itself, memoised on the name's suffix.

Jobs use half the machine's cores by default, so a long compress doesn't make
the desktop stutter. Settings → Performance overrides it.

`FILEMAN_TRACE=1` reports scan times and main-loop stalls;
`FILEMAN_SNAPSHOT=<path>` renders the window to a PNG and exits.

## Install

### Script

```sh
curl -fsSL https://raw.githubusercontent.com/0znio/fileman/main/install.sh | sh
```

It works out what this machine needs: installs runtime dependencies with the
distro's own package manager — apt, pacman, dnf, zypper, apk, xbps, emerge,
eopkg — then downloads the prebuilt binary if one will run here, and builds from
source if not.

Options go after `-s --`, because the script is being piped into `sh` rather
than run as a file:

```sh
curl -fsSL https://raw.githubusercontent.com/0znio/fileman/main/install.sh \
  | sh -s -- --from-source --prefix /usr/local
```

| Option | Effect |
| --- | --- |
| `--from-source` | Build with cargo instead of downloading |
| `--binary` | Download only; fail rather than build |
| `--prefix DIR` | Install under `DIR` (default `/usr/local`, or `~/.local` without root) |
| `--version TAG` | A specific release instead of the latest |
| `--no-deps` | Leave the package manager alone |
| `--uninstall` | Remove it again |

### Manual

Releases carry the binary and its checksums. Grab both from the
[latest release](https://github.com/0znio/fileman/releases/latest):

```sh
curl -fsSLO https://github.com/0znio/fileman/releases/latest/download/SHA256SUMS
curl -fsSLO https://github.com/0znio/fileman/releases/latest/download/fileman-0.2.0-x86_64-linux.tar.gz

sha256sum -c SHA256SUMS          # verify before running anything
tar -xzf fileman-*-x86_64-linux.tar.gz
cd fileman-*-x86_64-linux

install -Dm755 fileman ~/.local/bin/fileman
install -Dm644 share/applications/dev.fileman.Files.desktop \
  ~/.local/share/applications/dev.fileman.Files.desktop
```

You still need the runtime libraries — `gtk4`, `libadwaita`, `libarchive`,
`udisks2` and a polkit agent — from your distro's packages. `pigz` (threaded
`.tar.gz`), `ntfs-3g` (mounting NTFS volumes Windows left dirty) and
`ntfsprogs` (`ntfsfix`, which repairs them properly) are optional.

**The prebuilt binary needs glibc ≥ 2.39, GTK ≥ 4.12 and libadwaita ≥ 1.5** —
Ubuntu 24.04+, Debian 13+, Fedora 40+, Arch, openSUSE Tumbleweed and their
derivatives. On anything older, on musl (Alpine), or on a non-x86_64 machine,
build from source instead.

### From source

Needs Rust 1.92+ and the development headers for the libraries above.

```sh
git clone https://github.com/0znio/fileman.git && cd fileman
make            # cargo build --release
make test
make install    # to ~/.local — no root needed
```

System-wide: `sudo make install PREFIX=/usr/local`. Run with `fileman [path]`.

## Shortcuts

`Ctrl+?` lists them all in the app.

| Key | Action |
| --- | --- |
| `Alt+←` / `Alt+→` / `Alt+↑` | Back / forward / up |
| `Ctrl+L` | Edit location as text |
| `Ctrl+F` | Search this folder and below |
| `Ctrl+H` | Show hidden files |
| `F2` | Rename (when the file list has focus) |
| `Delete` / `Shift+Delete` / `Ctrl+Shift+Delete` | Trash / delete / shred (file list only, so text boxes keep their keys) |
| `Ctrl+E` / `Ctrl+Shift+E` | Extract here / compress |
| `Ctrl+T` / `Ctrl+W` | New tab / close tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `Ctrl+Shift+D` | Download from a URL |
| `Ctrl+D` | Add to Favourites |
| `Ctrl+Shift+V` | Switch grid and list |
| `Alt+Return` | Properties |
| `F9` | Toggle sidebar |

## Layout

```
src/
  app.rs        application, command line, stylesheet, debug hooks
  config.rs     settings, persisted atomically to ~/.config/fileman
  history.rs    back/forward
  testing.rs    self-cleaning scratch directories for tests
  fs/           entry model, scanning, copy/move/delete, trash, shredding,
                recent files, parallel downloads, the shared thread budget
  archive/      libarchive extraction and bsdtar creation
  drives/       UDisks2 over D-Bus, NTFS recovery, LUKS unlocking
  ui/           window and tabs, actions, sidebar, path bar, views, dialogs,
                accent colour
```

Long operations run on worker threads and report through an async channel.
Conflicts hand the UI a one-shot reply channel and block the worker on it, so
the decision stays synchronous with the copy loop without the worker ever
touching a widget.

## Known gaps

- Renaming several files at once isn't implemented
- Thumbnails are images only; video and document previews need out-of-process
  thumbnailers
- No split view
- Network browsing shows what gvfs has already mounted; connecting to a new
  `smb://` or `sftp://` server has to be done elsewhere first
- Search matches names, not contents
- Fileman is not yet a desktop file-chooser backend, so browsers and other apps
  still use the GTK dialog when they ask where to save. Doing that means
  implementing `org.freedesktop.impl.portal.FileChooser`

## Licence

MIT

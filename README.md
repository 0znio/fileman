# Fileman

A fast GTK4/libadwaita file manager written in Rust, for Linux.

![Fileman](docs/screenshot.png)

It browses, copies, moves and renames like any file manager, and handles the
three things that usually send you to a terminal: mounting NTFS drives Windows
left dirty, extracting any archive format, and deleting something for good.

## Features

**Browsing**
- Icon grid and details list sharing one selection, sort and filter
- Back / forward / up, mouse buttons 8 and 9, per-button tooltips saying where each goes
- Breadcrumb bar that becomes an editable path (`Ctrl+L`) with `~`, `$VAR`, `file://`, relative paths and inline completion
- Search (`Ctrl+F`) filters the folder instantly, then walks the whole subtree in the background, streaming matches in
- Hidden files, folders-first, column sorting the grid follows
- Icon size 16–96 px, `Ctrl+scroll` to zoom
- Image thumbnails via the shared freedesktop cache
- Light / dark / follow-system
- Drag and drop between panes, onto folders, and onto sidebar rows

**Sidebar**
- Home and XDG user folders, with a fallback for systems with no `user-dirs.dirs`
- Favourites (`Ctrl+D`), Trash with a live count
- Every mountable volume, grouped into **Removable Devices**, **Windows** and **On This Computer**, each with a capacity ring. EFI, Microsoft Reserved, Recovery, swap and the running root are filtered out
- Live updates on hotplug, mount and unmount, including changes made by other apps

**Drives** — mounting goes through UDisks2 over D-Bus as your own user, via
polkit. No `sudo`, no fstab editing, no hardcoded `/dev/sdX` that breaks when a
drive re-enumerates.

**Archives** — extraction is in-process through libarchive: zip, 7z, rar/rar5,
tar with every common compressor, cab, iso, xar, lha, ar, deb, rpm. Entries are
streamed one at a time, so progress is real and extraction can be cancelled
mid-file. Encrypted archives prompt for a password. Well-rooted archives extract
in place; loose ones get a container folder. Creates zip, tar.gz, tar.xz,
tar.zst and 7z.

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

Separately, UDisks2 prefers the in-kernel `ntfs3` driver, which rejects dirty
volumes outright. Fileman retries with `fstype=ntfs` to route the mount through
`ntfs-3g`, which mounts them read-write without a password.

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

```sh
curl -fsSL https://github.com/0znio/fileman/releases/latest/download/install.sh | sh
```

Options go after `-s --`, because the script is being piped into `sh` rather
than run as a file:

```sh
curl -fsSL .../install.sh | sh -s -- --from-source --prefix /usr/local
```

The script works out what this machine needs. It installs runtime dependencies
with the distro's own package manager — apt, pacman, dnf, zypper, apk, xbps,
emerge, eopkg — then downloads a prebuilt binary if one will run here, and
builds from source if not.

| Option | Effect |
| --- | --- |
| `--from-source` | Build with cargo instead of downloading |
| `--binary` | Download only; fail rather than build |
| `--prefix DIR` | Install under `DIR` (default `/usr/local`, or `~/.local` without root) |
| `--no-deps` | Leave the package manager alone |
| `--uninstall` | Remove it again |

**The prebuilt binary needs glibc ≥ 2.39, GTK ≥ 4.12 and libadwaita ≥ 1.5** —
Ubuntu 24.04+, Debian 13+, Fedora 40+, Arch, openSUSE Tumbleweed and their
derivatives. On anything older, on musl (Alpine), or on a non-x86_64 machine the
script builds from source instead, which needs Rust 1.92+.

### From a checkout

```sh
make            # cargo build --release
make test
make install    # to ~/.local — no root needed
```

System-wide: `sudo make install PREFIX=/usr/local`. Run with `fileman [path]`.

Dependencies, if you'd rather install them yourself: `gtk4`, `libadwaita`,
`libarchive`, `udisks2` and a polkit agent at runtime; `pigz` for threaded
`.tar.gz` and `ntfs-3g` for NTFS repair are optional.

## Shortcuts

`Ctrl+?` lists them all in the app.

| Key | Action |
| --- | --- |
| `Alt+←` / `Alt+→` / `Alt+↑` | Back / forward / up |
| `Ctrl+L` | Edit location as text |
| `Ctrl+F` | Search this folder and below |
| `Ctrl+H` | Show hidden files |
| `F2` | Rename |
| `Delete` / `Shift+Delete` / `Ctrl+Shift+Delete` | Trash / delete / shred |
| `Ctrl+E` / `Ctrl+Shift+E` | Extract here / compress |
| `Ctrl+D` | Add to Favourites |
| `Ctrl+Shift+V` | Switch grid and list |
| `Alt+Return` | Properties |
| `F9` | Toggle sidebar |

## Layout

```
src/
  app.rs        application, command line, stylesheet
  config.rs     settings, persisted atomically to ~/.config/fileman
  history.rs    back/forward
  fs/           entry model, scanning, copy/move/delete, trash, shredding
  archive/      libarchive extraction and bsdtar creation
  drives/       UDisks2 over D-Bus, NTFS recovery
  ui/           window, actions, sidebar, path bar, views, dialogs
```

Long operations run on worker threads and report through an async channel.
Conflicts hand the UI a one-shot reply channel and block the worker on it, so
the decision stays synchronous with the copy loop without the worker ever
touching a widget.

## Known gaps

- Renaming several files at once isn't implemented
- Thumbnails are images only; video and document previews need out-of-process thumbnailers
- No tabs, split view, or network locations (`smb://`, `sftp://`)
- LUKS volumes are listed and labelled, but unlocking isn't wired up
- Search matches names, not contents

## Licence

MIT

# fio-xfs

Async **userspace file I/O into XFS** — read files with no kernel, no mount
and no loop device; the XFS sibling of
[fio.ext4.rs](https://github.com/glennswest/fio.ext4.rs).

Why: images whose root is XFS (RHEL, Rocky, Alma cloud images) are imported
and verified the same way as ext4 ones. stormblock's engine import
(`POST /api/v1/volumes/import`, stormblock#147) finds, opens and walks XFS
filesystems — whole volumes and GPT partitions — with this crate, and
stormblock-registry imports media through that engine API. stormblock also
reads the XFS blanks it formats with `mkfs-xfs` through it.

## Status

v0.2.0 reads (#1). It does not write: seeding files into an XFS volume is
issue #5, and until then stormblock refuses `seed` on XFS templates.

| Reads | |
|---|---|
| Filesystems | v4 and v5 (CRC); on v5 the checksum of everything read is checked — superblock, inodes, bmap B+tree blocks, directory data blocks, attribute leaf/node blocks, remote attribute values and remote symlinks (free-space and inode B+trees and directory index blocks are not read at all) |
| Inodes | v1/v2/v3; local, extent-list and B+tree forks; bigtime; 64-bit extent counts |
| Directories | short form, block, leaf and node |
| Symlinks | inline and remote |
| Extended attributes | short form, leaf and node, remote values; XFS ACLs shown as `system.posix_acl_*`; parent pointers hidden |
| The log | checked, not replayed: `Volume::open` finds the log's head and tail as the kernel does before a mount, and opens only a filesystem whose last log record is an unmount record (#6). A log on its own device cannot be seen, so it is not checked (`LogState::External`) |
| Not read | files on a realtime device — `walk` lists them, reading one is `Error::Unsupported` (#7) |
| Refused at open | a dirty log — not cleanly unmounted, so changes may be in the log and not on disk (`Error::DirtyLog`; mount it once to replay it, or read it as it stands with `open_norecovery`); a log that cannot be made sense of (`Error::Corrupt`); v4 without v2 directories; v5 with unknown incompatible features or `NEEDSREPAIR` set; any superblock that fails its checksum |

## Library

```rust
use fio_xfs::{FileDevice, Volume};

let vol = Volume::open(FileDevice::open("rocky9.img").await?).await?;
let text = vol.read("/etc/os-release").await?;
let names = vol.read_dir("/etc").await?;
let st = vol.stat("/usr/bin/bash").await?;
let attrs = vol.list_xattrs("/usr/bin/ping").await?;

// Every name under a directory, sorted, depth first.
let tree = vol.walk("/").await?;

// The whole tree as a tar stream: modes, owners, nanosecond mtimes,
// symlinks, hard links, devices and xattrs (SCHILY.xattr.*) intact.
let report = vol.pack_tar_to(fio_xfs::tar::Io::new(file), "/").await?;

// Or into a local directory: files, directories, symlinks and hard links
// with modes and mtimes; all-zero 4 KiB pieces become holes. Device nodes,
// FIFOs, sockets and xattrs are counted in the report, not created;
// owners only with `ExtractOptions { owner: true }` (needs root).
vol.extract("/", dest, &Default::default()).await?;
```

An image taken from a running or crashed system is refused rather than
read stale:

```rust
match Volume::open(dev).await {
    Err(fio_xfs::Error::DirtyLog { head, tail }) => { /* not cleanly unmounted */ }
    r => { let vol = r?; /* vol.log_state() is Clean, or External */ }
}

// Read it anyway, as it stands on disk (like mounting with norecovery):
// changes still in the log are not seen.
let vol = Volume::open_norecovery(dev).await?;
println!("{}", vol.log_state()); // clean | dirty (head H, tail T) | external | unreadable
```

Nothing here replays the log; the kernel does, on the next mount.

Lookups resolve symlinks in the middle of a path (absolute targets from the
image's root, at most 40 hops); `stat`, `lookup`, `read_link` and
`list_xattrs` do not follow a final symlink, `read`, `read_range` and
`read_dir` do. A name is found by scanning the directory's data blocks, not
through its hash index, so one lookup in a very large directory costs a
read of the whole directory (#8); `walk`, `pack_tar_to` and `extract` go by
inode and do not pay that.

Below the path API is one by inode, for callers that walk a tree
themselves: `inode`, `extents`, `read_inode_range`, `read_dir_inode`,
`symlink_target`, `xattrs_inode`, `stat_inode` and `resolve`; `walk_each`
hands each name and its inode to a closure instead of collecting them;
`pack_tar` returns the archive as a `Vec<u8>`. `exists`, `get_xattr` and
`read_link_bytes` (a target that is not UTF-8) round out the path calls.

Anything that implements `fio_xfs::BlockDevice` (`size` and `read_at`) can
be read; `FileDevice` and `MemDevice` are provided.

## How it ships

A library crate with a CLI, not a service: no configuration, no ports.
Consumers depend on it by git tag, as they do on fio-ext4:

```toml
fio-xfs = { git = "https://github.com/glennswest/fio.xfs.rs", tag = "v0.2.0", default-features = false }
```

The default `cli` feature builds the `fio-xfs` binary (clap, anyhow, the
multi-threaded tokio runtime); `default-features = false` leaves the library
alone. It has no golden: stormcentral does not list it as a component, and
it ships inside whatever links it.

## CLI

All commands read `IMAGE` (a file or block device) and never write to it.
A filesystem with a dirty log is refused; `--norecovery` reads it as it
stands on disk, with a warning. `info` shows the log's state.

```
fio-xfs IMAGE info
fio-xfs IMAGE --norecovery COMMAND ...
fio-xfs IMAGE ls [PATH]
fio-xfs IMAGE stat PATH
fio-xfs IMAGE cat PATH
fio-xfs IMAGE readlink PATH
fio-xfs IMAGE xattrs PATH
fio-xfs IMAGE tree [PATH]
fio-xfs IMAGE extract DEST [--root PATH] [--owner]
fio-xfs IMAGE tar [-o FILE] [--root PATH]
```

## Tests

`cargo test` runs the unit tests everywhere, and two integration tests that
need real tools and say so and pass when they are missing:

- `tests/mkfs_images.rs` — images made by `mkfs.xfs -p` (a protofile) with
  attributes set by `xfs_db`, checked with `xfs_repair -n`, then read back:
  every directory form, extent and B+tree forks, inline and remote symlinks
  and attribute values, devices; on v5 and v4, with and without `ftype`,
  bigtime and 64-bit extent counts, 1 KiB blocks with 8 KiB directory
  blocks, 2 KiB inodes and 16 AGs. The tar output is read by GNU tar too.
  Every log is found clean, as `xfs_logprint` finds it; logs reformatted
  by `xfs_db logformat` in later cycles are clean; a log whose unmount
  record is broken is refused as dirty.
- `tests/kernel.rs` — **the kernel is the judge**: the host's own kernel
  boots under qemu/KVM (no root) with the test binary as its init, mounts a
  fresh image and writes a tree through ordinary system calls — hard links,
  POSIX ACLs, xattrs up to 20 KB, 20 000-name directories, holes, unwritten
  and reflinked extents, device nodes, sockets, pre-1970 and post-2038
  nanosecond times, non-UTF-8 names — then every name is read back and
  compared. Then a crash: the kernel writes and syncs and the VM powers off
  without unmounting; the image is refused as dirty with the log head and
  tail `xfs_logprint` reports, and after a second boot replays the log and
  unmounts, it opens clean with every file written before the crash.

They run on the build box through `sc-build`.

## Licence

MIT OR Apache-2.0.

# fio-xfs

Async **userspace file I/O into XFS** — read and write files with no
kernel, no mount and no loop device; the XFS sibling of
[fio.ext4.rs](https://github.com/glennswest/fio.ext4.rs).

Why: images whose root is XFS (RHEL, Rocky, Alma cloud images) are imported
and verified the same way as ext4 ones. stormblock's engine import
(`POST /api/v1/volumes/import`, stormblock#147) finds, opens and walks XFS
filesystems — whole volumes and GPT partitions — with this crate, and
stormblock-registry imports media through that engine API. stormblock also
reads the XFS blanks it formats with `mkfs-xfs` through it, and seeds files
into XFS templates with the write side (#5).

## Status

Reads (#1), refuses a filesystem with a dirty log (#6), and writes v5
filesystems (#5).

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

| Writes | |
|---|---|
| Filesystems | v5 (CRC) only — what `mkfs.xfs` has made by default since 2015 and all `mkfs-xfs` makes — with finobt, rmapbt, reflink, inobtcount, bigtime, 64-bit extent counts and sparse inodes, each on or off. Refused (`Error::Unsupported`): v4, case-insensitive names, parent pointers (on by default from xfsprogs 7: make the filesystem with `mkfs.xfs -n parent=0` to write into it; #20), metadir, zoned, a log that is not clean |
| Operations | `write` (create or replace a whole file), `mkdir`, `mkdir_all`, `symlink` (targets up to 1023 bytes), `mknod` (devices, FIFOs, sockets), `link`, `unlink`, `rmdir`, `chmod`, `chown`, `set_time`, `flush`; `*_with` variants take `Attrs` (mode, uid, gid) |
| Kept true | free space (by block and by length), inode chunks and free-inode records, reverse mappings, bmap B+trees for files with more extents than their inode holds, every directory form, every checksum, AGF/AGI/superblock counters — `xfs_repair -n` finds nothing, and the kernel mounts, reads and goes on writing the result |
| Not yet | extended attributes, rename, writing into the middle of a file, realtime files; rewriting or removing a file whose extents are shared (reflinked) is refused |

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

Writing needs a device that can be written (`FileDevice::open_rw`,
`MemDevice`, or any `BlockDevice` that implements `write_at`) and a
`flush` at the end:

```rust
use fio_xfs::{Attrs, FileDevice, Special, Volume};

let mut vol = Volume::open(FileDevice::open_rw("template.img").await?).await?;
vol.set_time(1_700_000_000); // reproducible images; otherwise "now"
vol.mkdir_all("/etc/stormblock").await?;
vol.write("/etc/stormblock/boot.toml", b"...").await?;
vol.write_with("/etc/shadow", b"...", &Attrs::mode(0o600)).await?;
vol.symlink("/bin", "usr/bin").await?;
vol.mknod("/dev/console", Special::CharDevice { major: 5, minor: 1 }, &Attrs::mode(0o600)).await?;
vol.flush().await?; // nothing is consistent on disk until this
```

File contents and inodes go to the device as they are written; directories
are kept as lists of names and laid out at `flush` in whichever form holds
them (short form, block, leaf, node), and each AG touched has its free-space,
inode and reverse-mapping B+trees rebuilt from memory at `flush`, then its
AGF and AGI and the superblock counters. Reads between writes see the
writes. The log is not written: the filesystem must be cleanly unmounted to
start with, and it stays so.

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
be read, and written when it also implements `write_at` (and `flush`);
`FileDevice` (`open` read-only, `open_rw`) and `MemDevice` are provided.

## How it ships

A library crate with a CLI, not a service: no configuration, no ports.
Consumers depend on it by git tag, as they do on fio-ext4:

```toml
fio-xfs = { git = "https://github.com/glennswest/fio.xfs.rs", tag = "v0.4.0", default-features = false }
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

`cargo test` runs the unit tests everywhere, and three integration tests
that need real tools and say so and pass when they are missing:

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
  Last, the kernel mounts images this crate wrote (4 KiB blocks, and 1 KiB
  blocks with 8 KiB directory blocks), reads every file, adds and removes
  names in every directory form and writes a file of its own; `xfs_repair
  -n` passes before and after, and the kernel's changes read back here.
- `tests/write.rs` — filesystems made by `mkfs.xfs`, written by this crate
  and checked by `xfs_repair -n`, then read back: files of every size,
  owners and modes, every directory form, inline and remote symlinks,
  devices, hard links, replaced and removed files, a fragmented file under
  a bmap B+tree, a filesystem written until it is full; on the default
  geometry, 1 KiB blocks with 8 KiB directory blocks, 2 KiB inodes, 16 AGs
  and with finobt, rmapbt, reflink, bigtime, inobtcount, nrext64 and sparse
  inodes all off; into an image xfsprogs populated; through `MemDevice`.

They run on the build box through `sc-build`.

**The kernel in a throwaway VM (#12).** `tests/vm/build-image.sh` (run by
sc-build) makes a UEFI disk image: the UEFI Shell starts the build box's
kernel with a busybox initramfs holding the xfs and loop modules, xfsprogs'
`mkfs.xfs` and `xfs_repair`, and `examples/vm_verify.rs`. Its init
(`tests/vm/init.sh`) runs seven geometries (512 MB – 2 GiB; 1, 4 and 16 KiB
blocks; 8 KiB directory blocks; 2 KiB inodes; 4 KiB sectors; 16 AGs; every
optional feature off): `mkfs.xfs`, then this crate writes a tree with a
manifest, `xfs_repair -n`, the kernel loop-mounts it and checks every name in
the manifest (contents by md5, mode, owner, link count, symlink targets,
device numbers, directory sizes), then adds and removes names in every
directory form and copies a file in, `xfs_repair -n`, this crate reads the
kernel's changes back and writes again over them, `xfs_repair -n`, and the
kernel checks the second manifest. It prints `VERIFY PASS` or
`VERIFY FAIL <why>` on serial; stormcentral boots it on a fresh pve VM and
destroys the VM after. No root anywhere.

    SC_BUILD_OUT=tmp/fio-xfs-verify.img SC_BUILD_OUT_TO=tmp/fio-xfs-verify.img \
      sc-build 'tests/vm/build-image.sh tmp/fio-xfs-verify.img'
    stormcentral testhost boot nanatest1 --image tmp/fio-xfs-verify.img \
      --expect 'VERIFY PASS' --fail 'VERIFY FAIL' --timeout 900 \
      --url http://stormcentral.g8.lo

## Licence

MIT OR Apache-2.0.

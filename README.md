# fio-xfs

Async **userspace file I/O into XFS** — read files with no kernel, no mount
and no loop device; the XFS sibling of
[fio.ext4.rs](https://github.com/glennswest/fio.ext4.rs).

Why: stormblock and the registry import and verify images whose root is XFS
(RHEL, Rocky, Alma cloud images) the same way they use `fio-ext4` today.

## Status

Reading (issue #1). Writing is next.

| Reads | |
|---|---|
| Filesystems | v4 and v5 (CRC); every v5 metadata checksum is checked |
| Inodes | v1/v2/v3; local, extent-list and B+tree forks; bigtime; 64-bit extent counts |
| Directories | short form, block, leaf and node |
| Symlinks | inline and remote |
| Extended attributes | short form, leaf and node, remote values; XFS ACLs shown as `system.posix_acl_*`; parent pointers hidden |
| Not read | files on a realtime device; the log (read cleanly unmounted filesystems) |

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

// Or into a local directory (no devices or xattrs; owners with --owner as root).
vol.extract("/", dest, &Default::default()).await?;
```

Anything that implements `fio_xfs::BlockDevice` (`size` and `read_at`) can
be read; `FileDevice` and `MemDevice` are provided.

## CLI

```
fio-xfs IMAGE info
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
- `tests/kernel.rs` — **the kernel is the judge**: the host's own kernel
  boots under qemu/KVM (no root) with the test binary as its init, mounts a
  fresh image and writes a tree through ordinary system calls — hard links,
  POSIX ACLs, xattrs up to 20 KB, 20 000-name directories, holes, unwritten
  and reflinked extents, device nodes, sockets, pre-1970 and post-2038
  nanosecond times, non-UTF-8 names — then every name is read back and
  compared.

They run on the build box through `sc-build`.

## Licence

MIT OR Apache-2.0.

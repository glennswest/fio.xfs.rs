# CLAUDE.md — fio.xfs.rs

Userspace file I/O into XFS. Follow fio.ext4.rs's structure and rules (read
its CLAUDE.md).

- **Crate:** `fio-xfs` (lib `fio_xfs`), binary `fio-xfs`
- **Version:** 0.4.0 — `Cargo.toml` and `VERSION` must match
- **Ships:** as a library by git tag (plus the `fio-xfs` CLI behind the
  default `cli` feature); no service, config or ports, and no golden —
  stormcentral does not list it as a component

## Work plan
- [x] Issue #1 — the read side (2026-09-25).
      - The format and the read layer live here: when this was written
        mkfs.xfs.rs was a scaffold. It has since grown an on-disk layer
        (mkfs-xfs v0.2.0: `structs`, `crc`, `device::BlockDevice`,
        `inspect`) — issue #4 proposes depending on it instead of keeping a
        second copy. `BlockDevice` here is this crate's own read-only trait
        (`size` + `read_at`).
      - Modules: `device`, `crc` (every v5 checksum is checked), `sb`,
        `inode`, `bmap`, `dir`, `attr`, `volume` (the API), `export` (tar
        and extract), `tar` (PAX writer); `fio-xfs` CLI.
- [x] Read: superblock/AGs, inodes (local, extents, B+tree forks), directories (short-form, block, leaf, node), symlinks, xattrs
- [x] Walk and extract a tree (`walk`, `pack_tar_to`, `extract`)
- [x] Tests against real XFS images: `tests/mkfs_images.rs` (mkfs.xfs -p +
      xfs_db attr_set, 7 geometries/versions) and `tests/kernel.rs` (dev's
      own kernel populates images under qemu/KVM, unprivileged)
- [ ] Issue #8 (P3) — hashed name lookup through the leaf/node index: `lookup` scans a
      directory's data blocks, which is fine for `walk` (by inode) but
      O(size) per path component in a huge directory
- [ ] Issue #4 — share mkfs-xfs's on-disk layer (format constants, CRC,
      device trait) rather than a second copy
- [x] Issue #6 — dirty log detected (2026-10-06, v0.3.0): `log` module
      ports the kernel's head/tail search; `Volume::open` refuses
      `Error::DirtyLog`, `open_norecovery` reads as-is, `log_state()`.
      Verified on dev: unit tests on hand-made logs, every mkfs image clean
      (xfs_logprint agrees), a kernel crash refused with xfs_logprint's head
      and tail, clean after the kernel replays it. stormblock#198 consumes it.
- [ ] Issue #7 (P3) — realtime-device files: `read_inode_range` returns
      `Unsupported` for `XFS_DIFLAG_REALTIME` inodes
- [x] Issue #5 — write (2026-10-06, v0.4.0): v5 only; `alloc` (per-AG state,
      tree rebuild at flush), `btree` (bulk loader), `dirwrite` (four directory
      forms), `write` (the Volume API). Verified: tests/write.rs (xfs_repair -n
      everywhere), the kernel test and the testhost boot VM (#12). Not yet:
      xattrs, rename, write_at/append, reflinked files.
- [x] Issue #14 (P0, with #13, #18, #19, #21) — write tests failing on the
      build VM (2026-10-10): the 2 KiB-inode fix (f5ca250) plus, since the
      Fedora 44 build VMs' mkfs.xfs 7.1.1 turns parent pointers on (which the
      writer refuses), the write tests make images with `-n parent=0`. The read
      tests keep mkfs's defaults. Verified: full sc-build of 1951acb passes.
- [ ] Issue #20 (P2) — write parent pointers (needs xattr writing)
- [x] Issue #12 — kernel verification in a throwaway VM (2026-10-06):
      `tests/vm/` (after mkfs.xfs.rs#11) + `examples/vm_verify.rs`, booted by
      `stormcentral testhost boot nanatest1`. First pass: run eebe9c7845
      (kernel 7.2.8, 62 s), 7 geometries, each written by us, xfs_repair -n
      clean, checked and changed by the kernel, read back and written again
      by us, clean.
- [x] Issue #9 — XFS media import (2026-09-27): done in stormblock#147, not
      here. The engine's `POST /api/v1/volumes/import` walks XFS (whole
      volumes and GPT partitions) with this crate; stormblock-registry imports
      media through that API. XFS for the registry's own PVC blank ladder
      (`pvc-ext4j-*`) is stormcos#91's decision, not work in this crate.

## Where things stand (2026-10-06)

v0.4.0: write support (#5) and the testhost boot VM check (#12) done and
tagged. Open: #4, #7, #8.

## Things learned the hard way (issue #1)

- **An attribute fork can exist and be empty.** Linux gives new inodes an
  attribute fork in extent format with zero extents when ACLs or LSM labels
  may follow; `xfs_inode_hasattr` counts that as no attributes. So must we.
- **A v5 remote symlink has one header per mapped extent**, not per block
  (`xfs_symlink_write_target`); the checksum covers the whole extent. Remote
  *attribute values* do have a header per block.
- **`mkfs.xfs -p` (6.15) writes a remote symlink needing two blocks with the
  second unmapped**; `xfs_repair -n` rejects the image. Protofile tests keep
  symlinks to one block; the kernel test covers two.
- **On an SELinux host `mkfs.xfs -p` copies each source file's label**, so
  protofile images carry `security.selinux` nobody asked for.
- **The log (#6):** every 512-byte basic block starts with its cycle
  number (a record header keeps it after its `0xFEEDBABE` magic); the head
  is where the cycle drops, and the log is clean when the record just
  before the head is an unmount record (one op, flag `0x20`). `xfs_logprint
  -f` means "this file *is* the log": run `xfs_logprint -t IMAGE` instead.
  A crash in the kernel test is `sync()` then power off without umount.
- **The testhost boot VM (#12):** `tests/vm/` builds a UEFI disk through
  sc-build (`SC_BUILD_OUT`), booted by `stormcentral testhost boot
  nanatest1` (fio.xfs.rs is in its project list). The guest runs dev's
  `mkfs.xfs`/`xfs_repair` copied in with their libraries.
- **Kernel tests without root:** dev's `/dev/kvm` is world-writable and the
  kernel and modules are readable, so `tests/kernel.rs` boots
  `/boot/vmlinuz-$(uname -r)` (or `/lib/modules/$(uname -r)/vmlinuz`) with an initramfs whose `/init` is the
  test binary itself (plus its shared libraries and `xfs.ko`).

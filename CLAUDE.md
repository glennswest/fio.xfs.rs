# CLAUDE.md — fio.xfs.rs

Userspace file I/O into XFS. Follow fio.ext4.rs's structure and rules (read
its CLAUDE.md).

- **Crate:** `fio-xfs` (lib `fio_xfs`), binary `fio-xfs`
- **Version:** 0.3.0 — `Cargo.toml` and `VERSION` must match
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
- [ ] Issue #5 — write (IN PROGRESS, 2026-10-06): stormblock seeds XFS
      templates with it (`mkdir_all` + `write` + `flush`, as with fio-ext4).
      Plan, v5 (CRC) filesystems only (mkfs-xfs makes nothing else; v4 is
      refused with `Unsupported`):
      - `BlockDevice::write_at`/`flush` (provided methods; read-only devices
        say `Unsupported`), `FileDevice::open_rw`, writable `MemDevice`.
      - `alloc` module: per-AG state loaded on first write (free extents
        from the bnobt, inobt records, rmap records, every block of the
        bno/cnt/rmap/inobt/finobt trees). Extents and inode chunks come out
        of it in memory.
      - File data and inodes are written through; directories are kept as
        entry lists and serialised at `flush` in whichever form fits
        (short form, block, leaf, node), so a big directory costs one write.
      - `flush` rebuilds each touched AG's bno/cnt/rmap/inobt/finobt trees
        from the in-memory records (bulk load, as xfs_repair phase 5 does),
        then the AGF, AGI and superblock counters.
      - API: write/write_with, mkdir(_with), mkdir_all(_with), symlink,
        mknod, link, unlink, rmdir, chmod, chown, set_time, flush.
      - Not in this round: xattrs, rename, append/write_at, reflinked files.
      - Verified by `xfs_repair -n` on every image written, by reading back,
        and by the kernel test mounting a written image.
- [ ] Issue #12 (P2) — kernel verification in a throwaway VM (IN PROGRESS,
      2026-10-06): `stormcentral testhost boot nanatest1` (fio.xfs.rs is in its
      project list). Recipe from mkfs.xfs.rs#11 (`tests/vm/`): sc-build runs
      `tests/vm/build-image.sh OUT` (UEFI Shell + dev's kernel + busybox
      initramfs with xfs/loop modules, `mkfs.xfs`, `xfs_repair`, and our
      `examples/vm_verify.rs`), out through `SC_BUILD_OUT`. Init per case:
      mkfs.xfs → vm-verify write (tree + manifest) → xfs_repair -n → kernel
      mounts and checks the manifest (md5, mode, owner, links, devices,
      counts) → kernel adds/removes → umount → xfs_repair -n → vm-verify
      check (reads the kernel's changes) and writes again over the kernel's
      trees → repair → mount. `VERIFY PASS` / `VERIFY FAIL <why>`.
- [x] Issue #9 — XFS media import (2026-09-27): done in stormblock#147, not
      here. The engine's `POST /api/v1/volumes/import` walks XFS (whole
      volumes and GPT partitions) with this crate; stormblock-registry imports
      media through that API. XFS for the registry's own PVC blank ladder
      (`pvc-ext4j-*`) is stormcos#91's decision, not work in this crate.

## Where things stand (2026-10-06)

v0.3.0: dirty-log detection (#6) done and tagged. Open: #4, #5, #7, #8.
In progress: #5 (write), see the work plan.

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
- **Kernel tests without root:** dev's `/dev/kvm` is world-writable and the
  kernel and modules are readable, so `tests/kernel.rs` boots
  `/boot/vmlinuz-$(uname -r)` (or `/lib/modules/$(uname -r)/vmlinuz`) with an initramfs whose `/init` is the
  test binary itself (plus its shared libraries and `xfs.ko`).

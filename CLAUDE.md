# CLAUDE.md — fio.xfs.rs

Userspace file I/O into XFS. Follow fio.ext4.rs's structure and rules (read
its CLAUDE.md).

- **Crate:** `fio-xfs` (lib `fio_xfs`), binary `fio-xfs`
- **Version:** 0.2.0 — `Cargo.toml` and `VERSION` must match
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
- [ ] Hashed name lookup through the leaf/node index: `lookup` scans a
      directory's data blocks, which is fine for `walk` (by inode) but
      O(size) per path component in a huge directory
- [ ] Issue #4 — share mkfs-xfs's on-disk layer (format constants, CRC,
      device trait) rather than a second copy
- [ ] Issue #5 (P3) — write: create files and directories, allocate extents,
      update the B+trees (clean writes on an unmounted filesystem); stormblock
      needs it to seed XFS templates
- [ ] Registry import of XFS images (stormblock already reads XFS blanks
      with this crate; the registry is its repo's work)

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
- **Kernel tests without root:** dev's `/dev/kvm` is world-writable and the
  kernel and modules are readable, so `tests/kernel.rs` boots
  `/boot/vmlinuz-$(uname -r)` (or `/lib/modules/$(uname -r)/vmlinuz`) with an initramfs whose `/init` is the
  test binary itself (plus its shared libraries and `xfs.ko`).

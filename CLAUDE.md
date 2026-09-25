# CLAUDE.md — fio.xfs.rs

Userspace file I/O into XFS. Follow fio.ext4.rs's structure and rules (read
its CLAUDE.md).

## Work plan
- [ ] Issue #1 — read side, in progress. Plan:
      - The format and the read layer live here for now: mkfs.xfs.rs is a
        scaffold, and nothing it has yet can be shared. When it grows an
        on-disk layer, move the structs there the way fio-ext4 sits on
        mkfs-ext4. `BlockDevice` is this crate's own trait (read-only
        `size` + `read_at`), so a consumer adapts its device in a few lines.
      - Modules: `device`, `crc` (CRC32C, every v5 checksum is checked),
        `sb`, `inode`, `bmap` (extent list and B+tree forks), `dir` (short
        form, block, leaf, node), `attr` (short form, leaf, node, remote
        values), `volume` (lookup/stat/read/read_dir/read_link/xattrs/walk),
        `extract` (to a directory) and `tar` (a PAX stream, for import).
      - A `fio-xfs` CLI: info, ls, stat, cat, tree, extract, tar.
      - Tests build images on dev with mkfs.xfs and populate them, then
        compare what this crate reads with what was written.
- [ ] Read: superblock/AGs, inodes (local, extents, B+tree forks), directories (short-form, block, leaf, node), symlinks, xattrs
- [ ] Walk and extract a tree (image import and verification in stormblock/registry)
- [ ] Write: create files and directories, allocate extents, update the B+trees (clean writes on an unmounted filesystem)
- [ ] Tests against real XFS images made by mkfs.xfs and populated by the kernel on dev

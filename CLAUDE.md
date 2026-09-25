# CLAUDE.md — fio.xfs.rs

Userspace file I/O into XFS. Follow fio.ext4.rs's structure and rules (read
its CLAUDE.md).

## Work plan
- [ ] Read: superblock/AGs, inodes (local, extents, B+tree forks), directories (short-form, block, leaf, node), symlinks, xattrs
- [ ] Walk and extract a tree (image import and verification in stormblock/registry)
- [ ] Write: create files and directories, allocate extents, update the B+trees (clean writes on an unmounted filesystem)
- [ ] Tests against real XFS images made by mkfs.xfs and populated by the kernel on dev

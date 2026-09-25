# Changelog

## [Unreleased]

### 2026-09-25
- **feat:** Read XFS (#1): superblock (v4 and v5, checksums checked), inodes with local, extent and B+tree forks, all four directory forms, inline and remote symlinks, extended attributes (short form, leaf, node, remote values, ACLs as `system.posix_acl_*`)
- **feat:** `Volume` API modelled on fio-ext4: `lookup`, `stat`, `read`, `read_range`, `read_dir`, `read_link`, `list_xattrs`, `get_xattr`, `walk`
- **feat:** Get a tree out: `pack_tar_to` (PAX tar with xattrs, hard links, devices) and `extract` to a local directory
- **feat:** `fio-xfs` CLI: info, ls, stat, cat, readlink, xattrs, tree, extract, tar
- **test:** real `mkfs.xfs` images in seven versions/geometries, and images populated by the Linux kernel under qemu/KVM, read back and compared
- **fix:** an attribute fork in extent format with no extents holds no attributes (Linux creates new inodes that way)
- **fix:** a v5 remote symlink has one header per mapped extent, not per block
- **chore:** Scaffold — the XFS sibling of the ext4 crate (owner: "add XFS to our formatting/mkfs and import tools as well as ext4")

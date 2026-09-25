# Changelog

## [Unreleased]
<!-- New unreleased changes go here -->

## [v0.2.0] — 2026-09-25

### Added
- Read XFS (#1): superblock (v4 and v5, checksums checked), inodes with local, extent and B+tree forks, all four directory forms, inline and remote symlinks, extended attributes (short form, leaf, node, remote values, ACLs as `system.posix_acl_*`)
- `Volume` API modelled on fio-ext4: `lookup`, `stat`, `read`, `read_range`, `read_dir`, `read_link`, `list_xattrs`, `get_xattr`, `walk`
- Get a tree out: `pack_tar_to` (PAX tar with xattrs, hard links, devices) and `extract` to a local directory
- `fio-xfs` CLI: info, ls, stat, cat, readlink, xattrs, tree, extract, tar

### Fixed
- an attribute fork in extent format with no extents holds no attributes (Linux creates new inodes that way)
- a v5 remote symlink has one header per mapped extent, not per block

### Tests
- real `mkfs.xfs` images in seven versions/geometries, and images populated by the Linux kernel under qemu/KVM, read back and compared

### Chores
- Scaffold — the XFS sibling of the ext4 crate (owner: "add XFS to our formatting/mkfs and import tools as well as ext4")

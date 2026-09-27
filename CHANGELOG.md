# Changelog

## [Unreleased]
<!-- New unreleased changes go here -->

### 2026-09-27
- **docs:** Third refresh from the code (no code changes since v0.2.0): README says XFS media import goes through stormblock's engine import (stormblock#147), which walks XFS with this crate; CLAUDE.md checks off the stale registry-import item (#9)
- **docs:** CLAUDE.md: where things stand at the session restart; #9 in the work plan
- **docs:** Second refresh from the code: README and crate docs say a dirty log is not detected (#6), how realtime files fail (#7), the lookup cost is #8, and what `Volume::open` refuses; CLAUDE.md work plan lists #6, #7 and #8
- **docs:** Refreshed from the code: the crate description no longer promises writing (issue #5); how it ships (git tag, `cli` feature, no golden); which v5 checksums are checked; symlink-following and lookup cost; what `extract` creates; the first reads `Volume::open` makes; mkfs-xfs's on-disk layer (#4) in the work plan

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

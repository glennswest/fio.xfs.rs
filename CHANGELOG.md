# Changelog

## [Unreleased]
<!-- New unreleased changes go here -->

## [v0.4.5] — 2026-11-21

### Fixed
- (#29; finding 287cd742841f) `extract` and `pack_tar_to` skip a hard-linked FIFO, device node or socket by every name, rather than linking later names to a path or archive member never made
- (#29; finding 97a24c584823) A superblock whose AG log does not match its AG size, or whose AG inode numbers would need more than 32 bits, is `Error::Corrupt`
- (#29; finding b8ffbc6331af) At the inode-space limit (`imax_pct`) the writer still uses free inodes in every AG before returning `Error::NoSpace`

### 2026-11-21 (#29)
- **fix:** (review finding 287cd742841f) `extract` put every multiply-linked inode in its hard-link map before deciding to skip its type, so the second name of a hard-linked FIFO or device node was hard-linked to a path never created and extraction failed; `pack_tar_to` likewise made later names of a socket links to a member it had left out. Special files are now skipped by every name (each listed in `specials_skipped`, their xattrs counted once), and an inode's first path is recorded only once it exists. Test: `tests/write.rs` `hard_linked_specials_are_skipped_by_every_name`
- **fix:** (review finding 97a24c584823) the superblock's AG log is now held to the AG size (rounded up to a power of two, as the kernel checks), AG log plus inodes-per-block log must fit a 32-bit AG inode number, and the directory block size uses `checked_shl`. The shift-before-bound part was already fixed by #25 (63188b025c7e). Unit test `log_fields_must_match_their_geometry` (including the finding's v4 block log of 44)
- **fix:** (review finding b8ffbc6331af) at the inode-space limit (`sb_imax_pct`) the writer returned `NoSpace` as soon as the preferred AG had no free inode, though other AGs' chunks did. The limit now stops only new chunks, and every AG is searched for a free inode first (as the kernel's `xfs_dialloc`); the limit is reckoned in whole chunks as `xfs_set_maxicount` does. Test: `tests/write.rs` `inode_limit_still_uses_other_ags_free_inodes`

## [v0.4.4] — 2026-11-21

### Fixed
- (security, #27; finding 15cd3aece9ef) A directory data block whose last 8 bytes begin a live entry, or whose free record runs past the names, is `Error::Corrupt`, not a panic in `read_dir`/`walk`

### 2026-11-21 (#27)
- **fix:** (review finding 15cd3aece9ef) a directory data block whose last 8 bytes began a live entry panicked (index out of bounds reading the name length) instead of returning `Error::Corrupt`, so a crafted image could kill `read_dir` or `walk`. The entry's fixed header must fit before its name length is read, and a free record that runs past the names' end is `Corrupt` too. Unit tests in `src/dir.rs`: the issue's 512-byte v4 example, every short tail in v4 and v5 blocks, an over-long free record

## [v0.4.3] — 2026-11-21

### Fixed
- (security, #26; finding 0f02ac950500) Changing the permission bits of a file with an access ACL (`chmod`, `write_with` on an existing file) is `Error::Unsupported`, not a mode the ACL silently overrides
- (#26; finding 1fed3c80f651) Every writer read after open is whole blocks at a block boundary, as the `BlockDevice` contract says
- (#26; findings 793cd4f33612, a72ebcf6af32, 03ce0a0cf15e) A flush that fails part-way can be retried: pending directories and AG state are kept until they are durable, and a failure that cannot be undone poisons the writer (`Error::Corrupt`)
- (#26; finding 6dd91ec6c081) Rewriting a directory allocates and writes the new blocks before freeing the old ones
- (#26; finding 8b48ae8c1697) Paths and symlink targets with an embedded NUL are `Error::InvalidPath`

### 2026-11-21 (#32)
- **chore:** `cargo clippy --all-targets -D warnings` clean again: two `needless_late_init` lints (clippy 1.95) in `btree::build_bmbt` and `dirwrite`'s node index, rewritten as `let … = if …`; no behaviour change

### 2026-11-20 (#26)
- **fix:** (security, review finding 0f02ac950500) `chmod`, and `write_with` on an existing file, left an access ACL granting what the new mode took away; changing the rwx bits of an inode with `system.posix_acl_access` is now `Error::Unsupported` before anything is written (setuid, setgid and sticky may still change)
- **fix:** (review finding 1fed3c80f651) the writer read inodes, AG headers, the AGFL and the superblock in pieces smaller than a block, against the `BlockDevice` contract; every read after open is now whole blocks at a block boundary (`device::read_aligned`)
- **fix:** (review findings 793cd4f33612, a72ebcf6af32, 03ce0a0cf15e) a flush that failed part-way dropped every pending directory and marked rebuilt AGs clean, so a retry could report success with names and allocation trees lost. Directories stay pending until written (keeping their inode and old fork as first read, so a retry is right even if the failed inode write reached the disk), AGs stay dirty until the device flush succeeds, and the writer keeps the primary superblock sector in memory rather than reading back one a failed write may have torn; `Ag::rebuild` restores the AG on error; a failure that cannot be undone poisons the writer (every later change and flush is `Error::Corrupt`)
- **fix:** (review finding 6dd91ec6c081) laying out a directory freed its old blocks before the new ones were allocated, so running out of space left the inode pointing at free blocks. The new layout is allocated and written first, the inode pointed at it, and only then the old blocks given back; on failure what was taken goes back. Replacing and removing files follow the same order
- **fix:** (review finding 8b48ae8c1697) a path or symlink target with an embedded NUL was written as an invalid name; every creation now refuses one (`Error::InvalidPath`) before allocating
- **test:** `tests/write.rs`: a contract-checking, failure-injecting device; a flush failed at nine points, with the failed write refused or landed, and retried (`xfs_repair -n` clean, every name read back); a failed inode write poisons the writer; NUL names; the ACL refusal

## [v0.4.2] — 2026-11-20

### Fixed
- (security, #25; finding 63188b025c7e) A crafted superblock with an out-of-range log field is `Error::Corrupt`, not a panic
- (#25; finding 01a341965933) Writing a filesystem with quotas clears the quota "checked" flags first, so the kernel recounts at its next quota mount; `Superblock::qflags`, `quota_counts_trusted()`

### 2026-11-20 (#25)
- **fix:** (security, review finding 63188b025c7e) `Superblock::parse` shifted by `block_log`, `inopb_log` and `agblk_log` before checking them, so a crafted superblock (v5 with a valid checksum, or v4) panicked on open; every log field (`dirblk_log` too) is now bounded before any shift and out-of-range geometry is `Error::Corrupt`. Unit tests try every value of each field, v4 and v5
- **fix:** (review finding 01a341965933) writing a filesystem with quotas left its quota counts marked checked, so the kernel trusted stale counts at the next quota mount. `Superblock` now has `qflags` and `quota_counts_trusted()`; the writer's first change clears the quota "checked" flags in the primary superblock and flushes it before anything else, so the kernel recounts. Tested in `tests/write.rs` and by the kernel test (recount matches `stat`; a control image with the flags put back keeps the stale count)

## [v0.4.1] — 2026-11-20

### Fixed
- (security, #23; findings 112925e105eb, 4d9887e4883e) `extract` no longer trusts on-disk names as host paths: names that are empty or hold `/` or NUL are `Corrupt` on every directory read, a name stored twice is `Corrupt` to `walk`, `extract` checks each path component and creates files with `O_EXCL`

### Changed
- (test, #14) Write tests make their images with `mkfs.xfs -n parent=0` (xfsprogs 7 turns parent pointers on)

### Documentation
- README: how names from the image are checked; doc refreshes of 2026-09-27 to 2026-11-14

### 2026-11-20
- **fix:** (security, P0, #23; review findings 112925e105eb, 4d9887e4883e) `extract` joined on-disk directory names onto `dest` unchecked, so a crafted image with a `../` name wrote outside `dest`, and one name stored twice (a symlink, then a file) wrote through the symlink. Directory reads now refuse a name that is empty or holds `/` or NUL as `Corrupt`; `walk` (and so `pack_tar_to` and `extract`) refuses a name stored twice; `extract` checks each path component (no empty, `.`, `..`) and creates files with `O_EXCL`. Tested by `tests/hostile_names.rs` (mkfs images with an entry rewritten in place) and a unit test of the path check

### 2026-11-14
- **docs:** Refreshed from the code (nothing changed since 2026-11-07 but a research note; src, tests and Cargo.toml are untouched since v0.4.0): README and CLAUDE.md rechecked against the code and the open issues (#4, #7, #8, #15, #16, #17, #20); nothing promised that the code does not do, so no new issues

### 2026-11-07
- **docs:** Refreshed from the code (no code changes since 2026-10-16; only research notes were added): CLAUDE.md says the kernel test and testhost VM use the build VM's kernel, `/dev/kvm`, `mkfs.xfs` and `xfs_repair`, not dev's (retired 2026-10-07; #22). README checked: nothing promised that the code does not do, so no new issues

### 2026-10-10
- **docs:** Refreshed from the code (changes since 2026-10-02: v0.3.0 dirty-log check, v0.4.0 write side, the #14 test fix): README lists every write refusal `check_writable` makes (no `ftype`, unknown or log-incompatible features, more than 64 inodes per block), the log limitations now filed (#15 replay, #16 torn-write trim, #17 external log), and that write tests use `-n parent=0` while read tests keep mkfs's defaults; CLAUDE.md work plan and status list #15, #16, #17. No docs promise found that the code does not keep, so no new issues
- **test:** The write tests, the kernel test's written images and the testhost VM check make their filesystems with `mkfs.xfs -n parent=0`: xfsprogs 7 (the Fedora 44 build VMs) turns parent pointers on by default, which the writer refuses, so every write test failed before reaching its checks (#14, #18, #19, #21). Writing parent pointers is #20

### 2026-09-28
- **docs:** Fourth refresh from the code (no code changes since v0.2.0; README, crate docs and CLAUDE.md rechecked against the code and against stormblock's use of v0.2.0): README lists the by-inode API (`inode`, `read_inode_range`, `walk_each`, …) and `pack_tar`, `exists`, `get_xattr`, `read_link_bytes`; no docs promise found that the code does not keep, so no new issues

### 2026-09-27
- **docs:** Third refresh from the code (no code changes since v0.2.0): README says XFS media import goes through stormblock's engine import (stormblock#147), which walks XFS with this crate; CLAUDE.md checks off the stale registry-import item (#9)
- **docs:** CLAUDE.md: where things stand at the session restart; #9 in the work plan
- **docs:** Second refresh from the code: README and crate docs say a dirty log is not detected (#6), how realtime files fail (#7), the lookup cost is #8, and what `Volume::open` refuses; CLAUDE.md work plan lists #6, #7 and #8
- **docs:** Refreshed from the code: the crate description no longer promises writing (issue #5); how it ships (git tag, `cli` feature, no golden); which v5 checksums are checked; symlink-following and lookup cost; what `extract` creates; the first reads `Volume::open` makes; mkfs-xfs's on-disk layer (#4) in the work plan

## [v0.4.0] — 2026-10-06

### Added
- Write v5 filesystems (#5): `Volume::write`/`write_with`, `mkdir`/`mkdir_with`, `mkdir_all`/`mkdir_all_with`, `symlink` (targets up to 1023 bytes), `mknod`, `link`, `unlink`, `rmdir`, `chmod`, `chown`, `set_time`, `flush`; `Attrs`, `Special`. Free space, inode chunks, free-inode records, reverse mappings and bmap B+trees are kept in memory, and the AG B+trees are rebuilt at `flush`; directories are laid out at `flush` in short form, block, leaf or node form
- `BlockDevice::write_at` and `flush` (provided; a device that does not implement them is read-only), `FileDevice::open_rw`, writable `MemDevice`
- `Superblock` gains `meta_uuid`, `inode_align`, `spino_align`, `imax_pct`, `features_log_incompat`, feature helpers and AG/inode number conversions; `FileType::code`/`from_ftype`; `Extent::encode`; `crc::stamp`

### Breaking
- `Error` gains `Exists`, `NotEmpty`, `NoSpace`

### Fixed
- unit test expectations for directory entry sizes and bmap block capacity (#11)

### Tests
- `tests/write.rs`: everything written is checked by `xfs_repair -n` across geometries and feature sets, and read back (#5, #14)
- the kernel test mounts images written here, reads them and changes them
- kernel verification in a throwaway VM through `stormcentral testhost boot` (#12): `tests/vm/`, `examples/vm_verify.rs`

## [v0.3.0] — 2026-10-06

### Added
- Dirty-log detection (#6): the `log` module finds the log's head and tail as the kernel does before a mount (`xlog_find_head`, `xlog_find_tail`, `xlog_check_unmount_rec`); `Volume::log_state()` returns `LogState::{Clean, Dirty { head, tail }, External, Unreadable}`; `Volume::open_norecovery` reads a dirty filesystem as it stands on disk; `Superblock` gains `versionnum`, `log_start`, `log_blocks`, `has_logv2()`, `has_external_log()`
- CLI: `info` shows the log's state; `--norecovery`

### Breaking
- `Volume::open` refuses a filesystem that was not cleanly unmounted (`Error::DirtyLog { head, tail }`) or whose log cannot be made sense of (`Error::Corrupt`). A log on its own device is not checked and not refused

### Tests
- every mkfs image's log is clean, agreeing with `xfs_logprint`; `xfs_db logformat` logs in later cycles are clean; a broken unmount record is refused; the kernel crashes a mount and the image is refused with `xfs_logprint`'s head and tail, then opens clean with every file once the kernel has replayed it; unit tests on hand-made logs

### Documentation
- README and crate docs: the log is checked at open, not replayed

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

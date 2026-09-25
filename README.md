# fio-xfs

Async **userspace file I/O into XFS** — read and write files with no kernel,
no mount and no loop device; the XFS sibling of
[fio.ext4.rs](https://github.com/glennswest/fio.ext4.rs).

Why: stormblock and the registry import and verify images whose root is XFS
(RHEL, Rocky, Alma cloud images) and write into XFS volumes, the same way they
use `fio-ext4` today.

## Status

Scaffold (2026-09-25). The plan is in CLAUDE.md.

//! The write side against the real tools: filesystems made by `mkfs.xfs`,
//! written by this crate, then checked by `xfs_repair -n` — which rebuilds
//! its own idea of every free extent, inode, reverse mapping, directory hash
//! and counter and compares — and read back.
//!
//! The kernel test (`tests/kernel.rs`) mounts an image written here as well.

mod common;

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};

use common::{build, bytes, dir_form, have, run, sparse, Image, Tree, Want};
use fio_xfs::inode::Format;
use fio_xfs::{Attrs, BlockDevice, Error, FileDevice, FileType, Special, Volume};

fn skip() -> bool {
    if !have("mkfs.xfs") || !have("xfs_repair") {
        eprintln!("SKIP: mkfs.xfs and xfs_repair are needed");
        return true;
    }
    false
}

/// An empty filesystem made by `mkfs.xfs`. Parent pointers (on by default
/// from xfsprogs 7) are turned off: the writer refuses them (#20).
fn fresh(opts: &[&str], size_mb: u64) -> Image {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fs.xfs");
    std::fs::File::create(&path).unwrap().set_len(size_mb << 20).unwrap();
    run(Command::new("mkfs.xfs").args(["-q", "-f", "-n", "parent=0"]).args(opts).arg(&path));
    Image { dir, path }
}

async fn open_rw(img: &Image) -> Volume<FileDevice> {
    Volume::open(FileDevice::open_rw(&img.path).await.unwrap()).await.unwrap()
}

/// `xfs_repair -n`, failing with everything it said.
fn repair_clean(img: &Image, when: &str) {
    let out = Command::new("xfs_repair").arg("-n").arg("-f").arg(&img.path).output().unwrap();
    assert!(
        out.status.success(),
        "xfs_repair -n {when}:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A long symlink target: past a 512-byte inode's fork, and past one 1 KiB
/// block's payload. XFS's longest is 1023 bytes.
fn long_target(len: usize) -> String {
    let mut t = String::from("../");
    while t.len() < len {
        t.push_str("a-directory-name/");
    }
    t.truncate(len);
    t
}

/// Write a tree with every form in it, recording what should be there.
async fn populate(vol: &mut Volume<FileDevice>, tree: &mut Tree, prefix: &str) {
    let p = |s: &str| format!("{prefix}{s}");
    vol.mkdir_all(&p("")).await.unwrap();
    let file = |tree: &mut Tree, vol_path: &str, data: Vec<u8>| {
        tree.file(vol_path.trim_start_matches('/'), data);
    };

    for (name, data) in [
        ("hello.txt", b"hello\n".to_vec()),
        ("empty", Vec::new()),
        ("big.bin", bytes(1, (3 << 20) + 123)),
        ("one-block", bytes(2, 4096)),
        ("odd", bytes(3, 5000)),
    ] {
        vol.write(&p(name), &data).await.unwrap();
        file(tree, &p(name), data);
    }
    vol.write_with(&p("setuid"), b"#!/bin/sh\n", &Attrs::mode(0o4755).owner(1000, 100)).await.unwrap();
    tree.items.insert(
        p("setuid").trim_start_matches('/').into(),
        Want::File { data: b"#!/bin/sh\n".to_vec(), mode: 0o4755, uid: 1000, gid: 100 },
    );

    // Directories in all four forms.
    for (dir, n, stem) in [("sf", 3, "f"), ("block", 40, "entry"), ("leaf", 400, "a-long-enough-name-"), ("node", 5000, "name-in-a-node-directory-")] {
        vol.mkdir(&p(dir)).await.unwrap();
        tree.dir(p(dir).trim_start_matches('/'));
        for i in 0..n {
            let path = p(&format!("{dir}/{stem}{i:05}"));
            let data = if i % 97 == 0 { format!("{i}\n").into_bytes() } else { Vec::new() };
            vol.write(&path, &data).await.unwrap();
            file(tree, &path, data);
        }
    }

    // Deep, made in one call, with an owner.
    vol.mkdir_all_with(&p("deep/er/still"), &Attrs::mode(0o700).owner(5, 6)).await.unwrap();
    for d in ["deep", "deep/er", "deep/er/still"] {
        tree.items.insert(p(d).trim_start_matches('/').into(), Want::Dir { mode: 0o700 });
    }

    vol.mkdir(&p("links")).await.unwrap();
    tree.dir(p("links").trim_start_matches('/'));
    for (name, target) in [("short", "../hello.txt".to_string()), ("long", long_target(900)), ("longest", long_target(1023)), ("abs", "/etc/x".into())] {
        vol.symlink(&p(&format!("links/{name}")), &target).await.unwrap();
        tree.items.insert(p(&format!("links/{name}")).trim_start_matches('/').into(), Want::Symlink(target.into_bytes()));
    }

    vol.mkdir(&p("devs")).await.unwrap();
    tree.dir(p("devs").trim_start_matches('/'));
    for (name, kind, want) in [
        ("null", Special::CharDevice { major: 1, minor: 3 }, Want::Char(1, 3)),
        ("sda", Special::BlockDevice { major: 8, minor: 0 }, Want::Block(8, 0)),
        ("big", Special::CharDevice { major: 259, minor: 70000 }, Want::Char(259, 70000)),
        ("fifo", Special::Fifo, Want::Fifo),
    ] {
        vol.mknod(&p(&format!("devs/{name}")), kind, &Attrs::mode(0o600)).await.unwrap();
        tree.items.insert(p(&format!("devs/{name}")).trim_start_matches('/').into(), want);
    }

    // A hard link, a replaced file (longer, then shorter), removals.
    vol.link(&p("hello.txt"), &p("sf/hello-again")).await.unwrap();
    file(tree, &p("sf/hello-again"), b"hello\n".to_vec());
    vol.write(&p("odd"), &bytes(4, 70_000)).await.unwrap();
    vol.write(&p("odd"), &bytes(5, 9_000)).await.unwrap();
    file(tree, &p("odd"), bytes(5, 9_000));
    for i in (1..40).step_by(3) {
        let path = p(&format!("block/entry{i:05}"));
        vol.unlink(&path).await.unwrap();
        tree.items.remove(path.trim_start_matches('/'));
    }
    vol.mkdir(&p("gone")).await.unwrap();
    vol.rmdir(&p("gone")).await.unwrap();

    // Freed blocks between used ones, for later writes to reuse.
    vol.mkdir(&p("frag")).await.unwrap();
    tree.dir(p("frag").trim_start_matches('/'));
    for i in 0..600 {
        vol.write(&p(&format!("frag/{i:04}")), &bytes(i, 4096)).await.unwrap();
    }
    for i in (0..600).step_by(2) {
        vol.unlink(&p(&format!("frag/{i:04}"))).await.unwrap();
    }
    for i in (1..600).step_by(2) {
        file(tree, &p(&format!("frag/{i:04}")), bytes(i, 4096));
    }
}

/// Read everything back through a fresh, read-only open and compare.
async fn verify(img: &Image, tree: &Tree) {
    let vol = img.open().await;
    let walked: BTreeMap<String, _> = vol.walk("/").await.unwrap().into_iter().map(|e| (e.path.clone(), e)).collect();
    let want: Vec<&String> = tree.items.keys().collect();
    let got: Vec<&String> = walked.keys().filter(|k| *k != "lost+found").collect();
    assert_eq!(got, want, "walk found a different set of names");
    for (path, want) in &tree.items {
        let st = walked[path].stat;
        let p = format!("/{path}");
        match want {
            Want::File { data, mode, uid, gid } => {
                assert!(st.is_file(), "{path}");
                assert_eq!(st.mode & 0o7777, *mode, "{path} mode");
                assert_eq!((st.uid, st.gid), (*uid, *gid), "{path} owner");
                assert_eq!(st.size, data.len() as u64, "{path} size");
                assert!(vol.read(&p).await.unwrap() == *data, "{path} contents");
            }
            Want::Dir { mode } => {
                assert!(st.is_dir(), "{path}");
                assert_eq!(st.mode & 0o7777, *mode, "{path} mode");
            }
            Want::Symlink(target) => {
                assert!(st.is_symlink(), "{path}");
                assert_eq!(vol.read_link_bytes(&p).await.unwrap(), *target, "{path} target");
            }
            Want::Char(a, b) => {
                assert_eq!(st.kind(), FileType::CharDevice, "{path}");
                assert_eq!(st.rdev, (*a, *b), "{path} rdev");
            }
            Want::Block(a, b) => {
                assert_eq!(st.kind(), FileType::BlockDevice, "{path}");
                assert_eq!(st.rdev, (*a, *b), "{path} rdev");
            }
            Want::Fifo => assert_eq!(st.kind(), FileType::Fifo, "{path}"),
        }
    }
}

/// The directory forms and the bmap B+tree came out as intended.
async fn verify_forms(img: &Image, prefix: &str) {
    let vol = img.open().await;
    let bl = vol.superblock().block_log;
    let at = |s: &str| format!("{prefix}{s}");
    let sf = vol.inode(vol.lookup(&at("sf")).await.unwrap()).await.unwrap();
    assert_eq!(sf.format, Format::Local, "sf");
    for (dir, form) in [("block", "block"), ("leaf", "leaf"), ("node", "node")] {
        let inode = vol.inode(vol.lookup(&at(dir)).await.unwrap()).await.unwrap();
        // 2 KiB inodes hold 40 names in short form, as the kernel would keep
        // them.
        if dir == "block" && vol.superblock().inode_size >= 2048 {
            assert_eq!(inode.format, Format::Local, "{dir} in a 2 KiB inode");
            continue;
        }
        let ext = vol.extents(&inode, false).await.unwrap();
        assert_eq!(dir_form(&ext, bl), form, "{dir}");
    }
}

/// The whole round: write, flush, repair, read back; then write more into
/// the same image, and check again.
async fn round(img: &Image) {
    let mut tree = Tree::default();
    let mut vol = open_rw(img).await;
    vol.set_time(1_700_000_000);
    populate(&mut vol, &mut tree, "/").await;
    vol.flush().await.unwrap();
    drop(vol);
    repair_clean(img, "after the first write");
    verify(img, &tree).await;
    verify_forms(img, "/").await;
    assert_eq!(img.open().await.stat("/hello.txt").await.unwrap().mtime.secs, 1_700_000_000);

    // Again, into the same filesystem, in a subdirectory.
    let mut vol = open_rw(img).await;
    populate(&mut vol, &mut tree, "/again/").await;
    tree.dir("again");
    // Grow directories that already exist on disk, across their forms.
    for i in 0..300 {
        let path = format!("/sf/added-{i:04}");
        vol.write(&path, b"x").await.unwrap();
        tree.file(&path[1..], b"x".to_vec());
    }
    vol.flush().await.unwrap();
    // Flushing twice is harmless.
    vol.flush().await.unwrap();
    drop(vol);
    repair_clean(img, "after the second write");
    verify(img, &tree).await;
    verify_forms(img, "/again/").await;
}

#[tokio::test]
async fn v5_default() {
    if skip() {
        return;
    }
    round(&fresh(&[], 512)).await;
}

#[tokio::test]
async fn v5_small_blocks_big_dir_blocks() {
    if skip() {
        return;
    }
    // Directory blocks span several filesystem blocks; the longest symlink
    // spans two blocks.
    round(&fresh(&["-b", "size=1024", "-n", "size=8192"], 512)).await;
}

#[tokio::test]
async fn v5_big_inodes() {
    if skip() {
        return;
    }
    round(&fresh(&["-i", "size=2048"], 512)).await;
}

#[tokio::test]
async fn v5_many_ags() {
    if skip() {
        return;
    }
    round(&fresh(&["-d", "agcount=16"], 2048)).await;
}

#[tokio::test]
async fn v5_minimal_features() {
    if skip() {
        return;
    }
    // No free-inode tree, no reverse mappings, no reflink, old timestamps
    // and extent counters, no sparse inodes.
    round(&fresh(
        &["-m", "finobt=0,rmapbt=0,reflink=0,bigtime=0,inobtcount=0", "-i", "nrext64=0,sparse=0"],
        512,
    ))
    .await;
}

#[tokio::test]
async fn fragmented_file_gets_a_bmap_btree() {
    if skip() {
        return;
    }
    // Fill the filesystem, free every other block of a run of one-block
    // files, and write into the holes: one extent per block, far more than
    // an inode holds, so the extents go in a B+tree.
    let img = fresh(&[], 300);
    let mut vol = open_rw(&img).await;
    let bs = vol.superblock().block_size as usize;
    let mut tree = Tree::default();
    tree.dir("frag");
    vol.mkdir("/frag").await.unwrap();
    for i in 0..1200u64 {
        vol.write(&format!("/frag/{i:04}"), &bytes(i, bs)).await.unwrap();
    }
    let mut size = 64 << 20;
    let mut n = 0;
    while size >= bs {
        match vol.write(&format!("/fill-{n}"), &vec![0u8; size]).await {
            Ok(_) => n += 1,
            Err(Error::NoSpace(_)) => size /= 2,
            Err(e) => panic!("{e}"),
        }
    }
    for i in 0..1200u64 {
        if i % 2 == 0 {
            vol.unlink(&format!("/frag/{i:04}")).await.unwrap();
        } else {
            tree.file(&format!("frag/{i:04}"), bytes(i, bs));
        }
    }
    let data = bytes(77, 500 * bs);
    vol.write("/frag-file", &data).await.unwrap();
    tree.file("frag-file", data);
    // The space it filled is not needed any more.
    for i in 0..n {
        vol.unlink(&format!("/fill-{i}")).await.unwrap();
    }
    vol.flush().await.unwrap();
    drop(vol);
    repair_clean(&img, "after writing a fragmented file");
    verify(&img, &tree).await;
    let vol = img.open().await;
    let inode = vol.inode(vol.lookup("/frag-file").await.unwrap()).await.unwrap();
    assert_eq!(inode.format, Format::Btree, "frag-file");
    assert!(vol.extents(&inode, false).await.unwrap().len() >= 300);

    // Replacing it frees the tree's blocks too.
    let mut vol = open_rw(&img).await;
    vol.write("/frag-file", b"short").await.unwrap();
    vol.flush().await.unwrap();
    drop(vol);
    repair_clean(&img, "after replacing the fragmented file");
}

#[tokio::test]
async fn into_a_populated_image() {
    if skip() || !have("xfs_db") {
        return;
    }
    // An image xfsprogs filled (every directory form, attributes), changed
    // here: names added to and removed from each form, files replaced.
    let mut tree = Tree::default();
    tree.dir("sf");
    tree.file("sf/a", b"a".to_vec());
    tree.dir("node");
    for i in 0..3000 {
        tree.file(&format!("node/name-in-a-node-directory-{i:05}"), Vec::new());
    }
    tree.file("big.bin", bytes(9, 1 << 20));
    tree.file("sparse.bin", sparse(50));
    // No parent pointers: the writer refuses them (#20).
    let img = build(&tree, &["-n", "parent=0"], 512);
    repair_clean(&img, "as made");

    let mut vol = open_rw(&img).await;
    for i in 0..1000 {
        let path = format!("node/name-in-a-node-directory-{i:05}");
        vol.unlink(&format!("/{path}")).await.unwrap();
        tree.items.remove(&path);
    }
    for i in 0..200 {
        let path = format!("sf/new-{i:03}");
        vol.write(&format!("/{path}"), &bytes(i, 100)).await.unwrap();
        tree.file(&path, bytes(i, 100));
    }
    vol.write("/big.bin", b"small now").await.unwrap();
    tree.file("big.bin", b"small now".to_vec());
    vol.write("/sparse.bin", &bytes(10, 123_456)).await.unwrap();
    tree.file("sparse.bin", bytes(10, 123_456));
    vol.flush().await.unwrap();
    drop(vol);
    repair_clean(&img, "after writing");
    verify(&img, &tree).await;
}

#[tokio::test]
async fn errors() {
    if skip() {
        return;
    }
    let img = fresh(&[], 512);
    let mut vol = open_rw(&img).await;
    vol.mkdir("/d").await.unwrap();
    vol.write("/d/f", b"x").await.unwrap();
    assert!(matches!(vol.mkdir("/d").await, Err(Error::Exists(_))));
    assert!(matches!(vol.write("/d", b"x").await, Err(Error::IsADirectory(_))));
    assert!(matches!(vol.rmdir("/d").await, Err(Error::NotEmpty(_))));
    assert!(matches!(vol.unlink("/d").await, Err(Error::IsADirectory(_))));
    assert!(matches!(vol.write("/nope/f", b"x").await, Err(Error::NotFound(_))));
    assert!(matches!(vol.write("/d/f/g", b"x").await, Err(Error::NotADirectory(_))));
    assert!(matches!(vol.unlink("/d/none").await, Err(Error::NotFound(_))));
    assert!(matches!(vol.mkdir("/").await, Err(Error::InvalidPath(_))));
    assert!(matches!(vol.symlink("/too-long", &"x".repeat(1024)).await, Err(Error::InvalidPath(_))));
    // Review finding 8b48ae8c1697: a NUL cannot be in an XFS name or a
    // symlink target, so every creation refuses one before allocating.
    assert!(matches!(vol.write("/a\0b", b"x").await, Err(Error::InvalidPath(_))));
    assert!(matches!(vol.write_with("/d/\0", b"x", &Attrs::default()).await, Err(Error::InvalidPath(_))));
    assert!(matches!(vol.mkdir("/nul\0dir").await, Err(Error::InvalidPath(_))));
    assert!(matches!(vol.mkdir_all("/d/x\0/y").await, Err(Error::InvalidPath(_))));
    assert!(matches!(vol.link("/d/f", "/d/l\0").await, Err(Error::InvalidPath(_))));
    assert!(matches!(vol.symlink("/s\0", "t").await, Err(Error::InvalidPath(_))));
    assert!(matches!(vol.symlink("/s", "a\0b").await, Err(Error::InvalidPath(_))));
    assert!(matches!(vol.mknod("/n\0", Special::Fifo, &Attrs::default()).await, Err(Error::InvalidPath(_))));
    assert!(matches!(vol.write("/d/x\0/f", b"x").await, Err(Error::InvalidPath(_))));
    assert!(!vol.exists("/s").await.unwrap());
    // Reads in the middle see what was written.
    assert_eq!(vol.read("/d/f").await.unwrap(), b"x");
    assert_eq!(vol.read_dir("/d").await.unwrap().len(), 1);
    assert_eq!(vol.read_dir("/").await.unwrap().len(), 1, "nothing left behind by the refusals");
    vol.flush().await.unwrap();
    drop(vol);
    repair_clean(&img, "after the error cases");

    // Filling the filesystem fails cleanly, and what was written before
    // still checks out.
    let mut vol = open_rw(&img).await;
    let mut n = 0u32;
    let err = loop {
        match vol.write(&format!("/fill-{n}"), &vec![n as u8; 8 << 20]).await {
            Ok(_) => n += 1,
            Err(e) => break e,
        }
    };
    assert!(matches!(err, Error::NoSpace(_)), "{err}");
    vol.flush().await.unwrap();
    drop(vol);
    repair_clean(&img, "after filling it");

    // A read-only device, a v4 filesystem: refused.
    let mut ro = img.open().await;
    assert!(matches!(ro.write("/x", b"x").await, Err(Error::Unsupported(_))));
    let v4 = fresh(&["-m", "crc=0"], 512);
    let mut vol = open_rw(&v4).await;
    assert!(matches!(vol.write("/x", b"x").await, Err(Error::Unsupported(_))));
}

#[tokio::test]
async fn in_memory() {
    if skip() {
        return;
    }
    // The same through a MemDevice: the image never touches a file until it
    // is checked.
    let img = fresh(&[], 300);
    let bytes_in = std::fs::read(&img.path).unwrap();
    let mut vol = Volume::open(fio_xfs::MemDevice::new(bytes_in)).await.unwrap();
    vol.mkdir_all("/etc/stormblock").await.unwrap();
    vol.write("/etc/stormblock/boot.toml", b"[boot]\nroot = \"xfs\"\n").await.unwrap();
    vol.write("/cmdline", b"console=ttyS0\n").await.unwrap();
    vol.flush().await.unwrap();
    std::fs::write(&img.path, vol.into_device().into_inner()).unwrap();
    repair_clean(&img, "after an in-memory write");
    let vol = img.open().await;
    assert_eq!(vol.read("/cmdline").await.unwrap(), b"console=ttyS0\n");
    assert_eq!(vol.read("/etc/stormblock/boot.toml").await.unwrap(), b"[boot]\nroot = \"xfs\"\n");
}

/// Review finding 01a341965933: the writer keeps no quota counts, so its
/// first change clears the quota "checked" flags in the superblock (the
/// kernel then recounts at its next quota mount) and keeps the accounting
/// and enforcement flags. Opening, reading or a refused change clear
/// nothing. The kernel test checks the recount itself.
#[tokio::test]
async fn first_change_invalidates_quota_counts() {
    use fio_xfs::sb::{quota, QFLAGS_OFFSET};
    if skip() {
        return;
    }
    const ENFD: u16 = 0x0002 | 0x0080 | 0x0200;
    let on = quota::ALL_ACCT | quota::UQUOTA_CHKD | quota::GQUOTA_CHKD | quota::PQUOTA_CHKD | ENFD;
    let img = fresh(&[], 512);
    let qflags = |path: &std::path::Path| {
        let b = std::fs::read(path).unwrap();
        u16::from_be_bytes([b[QFLAGS_OFFSET], b[QFLAGS_OFFSET + 1]])
    };
    {
        let mut b = std::fs::read(&img.path).unwrap();
        b[QFLAGS_OFFSET..QFLAGS_OFFSET + 2].copy_from_slice(&on.to_be_bytes());
        let sect = u16::from_be_bytes([b[102], b[103]]) as usize;
        fio_xfs::crc::stamp(&mut b[..sect], 224);
        std::fs::write(&img.path, b).unwrap();
    }

    let mut vol = open_rw(&img).await;
    assert!(vol.superblock().quota_counts_trusted());
    vol.read_dir("/").await.unwrap();
    assert!(vol.write("/no/such/dir/x", b"x").await.is_err());
    vol.flush().await.unwrap();
    drop(vol);
    assert_eq!(qflags(&img.path), on, "nothing changed, so nothing cleared");

    let mut vol = open_rw(&img).await;
    vol.write_with("/owned", &bytes(9, 100_000), &Attrs::mode(0o644).owner(1000, 1000)).await.unwrap();
    // Cleared on disk before the flush: a crash now still forces a recount.
    assert_eq!(qflags(&img.path), quota::ALL_ACCT | ENFD, "cleared at the first change");
    assert!(!vol.superblock().quota_counts_trusted());
    vol.flush().await.unwrap();
    drop(vol);
    assert_eq!(qflags(&img.path), quota::ALL_ACCT | ENFD);
    let vol = img.open().await;
    assert_eq!(vol.read("/owned").await.unwrap(), bytes(9, 100_000));
}

/// A device that holds the writer to the [`BlockDevice`] contract — once the
/// volume is open, reads are whole filesystem blocks at block boundaries;
/// writes are whole blocks, or a sector for the superblock and AG headers
/// (review finding 1fed3c80f651) — and whose writes can be made to fail from
/// the `n`th on: refused outright, or (`land`) written and then reported
/// failed, as a device can that loses its acknowledgement.
struct Strict {
    inner: fio_xfs::MemDevice,
    block: u64,
    sect: u64,
    strict: AtomicBool,
    writes: AtomicU64,
    fail_from: AtomicU64,
    land: AtomicBool,
}

impl Strict {
    fn new(bytes: Vec<u8>) -> Self {
        let sect = u16::from_be_bytes([bytes[102], bytes[103]]) as u64;
        let block = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as u64;
        Strict {
            inner: fio_xfs::MemDevice::new(bytes),
            block,
            sect,
            strict: AtomicBool::new(false),
            writes: AtomicU64::new(0),
            fail_from: AtomicU64::new(u64::MAX),
            land: AtomicBool::new(false),
        }
    }

    /// Fail every write from the `n`th from now (0: the next one).
    fn fail_after(&self, n: u64) {
        self.fail_from.store(self.writes.load(SeqCst) + n, SeqCst);
    }

    fn heal(&self) {
        self.fail_from.store(u64::MAX, SeqCst);
    }

    fn writes(&self) -> u64 {
        self.writes.load(SeqCst)
    }
}

#[async_trait::async_trait]
impl BlockDevice for Strict {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> fio_xfs::Result<()> {
        if self.strict.load(SeqCst) {
            assert!(
                offset % self.block == 0 && buf.len() as u64 % self.block == 0,
                "read of {} bytes at {offset}: not whole {}-byte blocks",
                buf.len(),
                self.block
            );
        }
        self.inner.read_at(offset, buf).await
    }
    async fn write_at(&self, offset: u64, buf: &[u8]) -> fio_xfs::Result<()> {
        let len = buf.len() as u64;
        assert!(
            (offset % self.block == 0 && len % self.block == 0) || (offset % self.sect == 0 && len == self.sect),
            "write of {len} bytes at {offset}: neither whole blocks nor a sector"
        );
        if self.writes.fetch_add(1, SeqCst) >= self.fail_from.load(SeqCst) {
            if self.land.load(SeqCst) {
                self.inner.write_at(offset, buf).await?;
            }
            return Err(std::io::Error::other("injected write failure").into());
        }
        self.inner.write_at(offset, buf).await
    }
}

async fn open_strict(bytes: Vec<u8>) -> Volume<Strict> {
    let vol = Volume::open(Strict::new(bytes)).await.unwrap();
    vol.device().strict.store(true, SeqCst);
    vol
}

/// 4 KiB blocks over 512-byte sectors, so that a sector or inode read is
/// smaller than a block.
fn strict_base() -> (Image, Vec<u8>) {
    let img = fresh(&["-s", "size=512", "-i", "size=512"], 300);
    let bytes = std::fs::read(&img.path).unwrap();
    (img, bytes)
}

/// Changes on disk, then more pending, every kind of write in them: a
/// directory that grows from block to leaf form (its old fork given back),
/// one that shrinks, a file replaced, chmod and chown, a remote symlink.
async fn staged(base: &[u8]) -> (Volume<Strict>, Tree) {
    let mut tree = Tree::default();
    let mut vol = open_strict(base.to_vec()).await;
    vol.mkdir("/d").await.unwrap();
    tree.dir("d");
    for i in 0..40 {
        vol.write(&format!("/d/f-{i:04}"), &bytes(i, 100)).await.unwrap();
        tree.file(&format!("d/f-{i:04}"), bytes(i, 100));
    }
    vol.write("/big", &bytes(1, 1 << 20)).await.unwrap();
    vol.flush().await.unwrap();

    for i in 40..400 {
        vol.write(&format!("/d/f-{i:04}"), &bytes(i, 10)).await.unwrap();
        tree.file(&format!("d/f-{i:04}"), bytes(i, 10));
    }
    for i in 0..6 {
        vol.unlink(&format!("/d/f-{i:04}")).await.unwrap();
        tree.items.remove(&format!("d/f-{i:04}"));
    }
    vol.write("/big", &bytes(2, 100_000)).await.unwrap();
    tree.file("big", bytes(2, 100_000));
    vol.chmod("/d/f-0010", 0o600).await.unwrap();
    tree.items.insert("d/f-0010".into(), Want::File { data: bytes(10, 100), mode: 0o600, uid: 0, gid: 0 });
    vol.chown("/d/f-0011", 7, 8).await.unwrap();
    tree.items.insert("d/f-0011".into(), Want::File { data: bytes(11, 100), mode: 0o644, uid: 7, gid: 8 });
    vol.write_with("/d/f-0012", b"again", &Attrs::mode(0o640).owner(3, 4)).await.unwrap();
    tree.items.insert("d/f-0012".into(), Want::File { data: b"again".to_vec(), mode: 0o640, uid: 3, gid: 4 });
    vol.mkdir("/e").await.unwrap();
    tree.dir("e");
    vol.symlink("/e/long", &long_target(900)).await.unwrap();
    tree.items.insert("e/long".into(), Want::Symlink(long_target(900).into_bytes()));
    (vol, tree)
}

/// Review findings 793cd4f33612, a72ebcf6af32, 03ce0a0cf15e and
/// 6dd91ec6c081: a flush whose writes fail part-way — in a directory's new
/// blocks, its inode, an AG's trees or headers, the superblock — returns
/// the error, keeps what it did not finish, and a second flush writes it
/// all: `xfs_repair -n` finds nothing and every name and byte reads back —
/// whether the failed writes reached the device or not. Every read the
/// writer makes goes through the contract-checking device.
#[tokio::test]
async fn failed_flush_can_be_retried() {
    if skip() {
        return;
    }
    let (img, base) = strict_base();
    let (mut vol, tree) = staged(&base).await;
    let before = vol.device().writes();
    vol.flush().await.unwrap();
    let total = vol.device().writes() - before;
    assert!(total > 10, "a flush of {total} writes");
    let mut points: Vec<u64> = vec![0, 1, 2, 3, total / 4, total / 2, total * 3 / 4, total - 2, total - 1];
    points.sort();
    points.dedup();

    for (n, land) in points.into_iter().flat_map(|n| [(n, false), (n, true)]) {
        let (mut vol, tree) = staged(&base).await;
        vol.device().land.store(land, SeqCst);
        vol.device().fail_after(n);
        let err = vol.flush().await.expect_err("a flush whose writes fail");
        assert!(matches!(err, Error::Io(_)), "write {n} of {total}: {err}");
        vol.device().heal();
        vol.flush().await.unwrap_or_else(|e| panic!("retry after write {n} of {total} failed: {e}"));
        vol.flush().await.unwrap();
        std::fs::write(&img.path, vol.into_device().inner.into_inner()).unwrap();
        repair_clean(&img, &format!("after write {n} of {total} failed (landed: {land}) and the flush was retried"));
        verify(&img, &tree).await;
    }

    // And the image a clean flush left.
    std::fs::write(&img.path, vol.into_device().inner.into_inner()).unwrap();
    repair_clean(&img, "after a clean flush on the strict device");
    verify(&img, &tree).await;
}

/// A failed write outside a flush: one into new blocks is given back and the
/// writer goes on; one to an inode leaves it neither old nor new, so every
/// later change and flush is refused rather than reporting success.
#[tokio::test]
async fn failed_inode_write_poisons_the_writer() {
    if skip() {
        return;
    }
    let (img, base) = strict_base();
    let mut vol = open_strict(base).await;
    vol.write("/first", b"one").await.unwrap();

    vol.device().fail_after(0);
    assert!(matches!(vol.write("/x", &bytes(3, 50_000)).await, Err(Error::Io(_))));
    vol.device().heal();
    vol.write("/x", &bytes(3, 50_000)).await.unwrap();
    vol.flush().await.unwrap();
    let (vol_bytes, mut vol) = {
        let dev = vol.into_device();
        let b = dev.inner.into_inner();
        (b.clone(), open_strict(b).await)
    };
    std::fs::write(&img.path, vol_bytes).unwrap();
    repair_clean(&img, "after a failed data write was given back");
    let mut tree = Tree::default();
    tree.file("first", b"one".to_vec());
    tree.file("x", bytes(3, 50_000));
    verify(&img, &tree).await;

    // chmod writes only the inode.
    vol.device().fail_after(0);
    assert!(matches!(vol.chmod("/x", 0o600).await, Err(Error::Io(_))));
    vol.device().heal();
    for r in [vol.flush().await, vol.write("/y", b"y").await.map(drop), vol.chmod("/first", 0o600).await] {
        match r {
            Err(Error::Corrupt(m)) => assert!(m.contains("earlier failure"), "{m}"),
            other => panic!("after a failed inode write: {other:?}"),
        }
    }
}

/// Review finding 0f02ac950500: changing the permission bits of a file with
/// an access ACL would leave the ACL granting what the mode took away (the
/// kernel rewrites the ACL with the mode; this writer cannot write
/// attributes), so it is refused, and nothing changes. Setuid and friends,
/// which are not in the ACL, and files without one, still change.
#[tokio::test]
async fn mode_change_refused_with_an_access_acl() {
    if skip() || !have("xfs_db") {
        return;
    }
    let mut tree = Tree::default();
    tree.file("acl", b"acl\n".to_vec());
    tree.file("plain", b"plain\n".to_vec());
    // Only the name matters to the check, and xfs_db sets text values.
    tree.attr("acl", "-r", "SGI_ACL_FILE", common::AttrValue::Text("x".into()));
    let img = build(&tree, &["-n", "parent=0"], 512);

    let mut vol = open_rw(&img).await;
    let refused = |r: fio_xfs::Result<()>| match r {
        Err(Error::Unsupported(m)) => assert!(m.contains("ACL"), "{m}"),
        other => panic!("a mode change under an ACL: {other:?}"),
    };
    refused(vol.chmod("/acl", 0o600).await);
    refused(vol.write_with("/acl", b"new\n", &Attrs::mode(0o666)).await.map(drop));
    assert_eq!(vol.read("/acl").await.unwrap(), b"acl\n", "the refused write changed nothing");
    vol.chmod("/acl", 0o4644).await.unwrap();
    vol.write_with("/acl", b"new\n", &Attrs::mode(0o4644).owner(1, 1)).await.unwrap();
    vol.chmod("/plain", 0o600).await.unwrap();
    vol.flush().await.unwrap();
    drop(vol);

    let vol = img.open().await;
    assert_eq!(vol.stat("/acl").await.unwrap().mode & 0o7777, 0o4644);
    assert_eq!(vol.read("/acl").await.unwrap(), b"new\n");
    assert_eq!(vol.stat("/plain").await.unwrap().mode & 0o7777, 0o600);
}

/// Review finding b8ffbc6331af: at the inode-space limit (`-i maxpct`) no
/// new chunk may be allocated, but free inodes in another AG's existing
/// chunks still may be. mkfs puts directories in AGs in turn, so `/d1` is
/// in AG 1 and the root's chunk in AG 0 keeps free inodes; filling `/d1`
/// to the limit must then go on into AG 0 until no inode is free anywhere.
#[tokio::test]
async fn inode_limit_still_uses_other_ags_free_inodes() {
    if skip() || !have("xfs_db") {
        return;
    }
    let mut tree = Tree::default();
    for d in ["d0", "d1", "d2", "d3"] {
        tree.dir(d);
    }
    // 2 KiB inodes, two in a block: 1% of 76800 blocks is 1536 inodes.
    let img = build(&tree, &["-n", "parent=0", "-d", "agcount=2", "-i", "size=2048,maxpct=1"], 300);
    let mut vol = open_rw(&img).await;
    let ag_of = |vol: &Volume<FileDevice>, ino: u64| vol.superblock().ino_to_agino(ino).0;
    let d1 = vol.stat("/d1").await.unwrap().inode;
    assert_eq!(ag_of(&vol, d1), 1, "mkfs put /d1 in AG 1");
    let before = vol.superblock().clone();
    assert!(before.ifree > 0 && before.imax_pct == 1);

    let mut n = 0u64;
    let mut in_ag0 = 0u64;
    let err = loop {
        match vol.write(&format!("/d1/f{n}"), b"").await {
            Ok(ino) => {
                if ag_of(&vol, ino) == 0 {
                    in_ag0 += 1;
                }
                n += 1;
            }
            Err(e) => break e,
        }
        assert!(n < 4096, "the inode limit never stopped allocation");
    };
    assert!(matches!(&err, Error::NoSpace(m) if m.contains("1%")), "{err}");
    assert!(in_ag0 > 0, "no file took one of AG 0's free inodes");
    vol.flush().await.unwrap();
    drop(vol);
    repair_clean(&img, "after filling to the inode limit");

    let vol = img.open().await;
    let sb = vol.superblock();
    assert_eq!(sb.ifree, 0, "free inodes left when allocation stopped");
    assert!(sb.icount <= 1536, "{} inodes past the 1% limit", sb.icount);
    assert_eq!(sb.icount - sb.ifree, before.icount - before.ifree + n, "inodes in use");
    assert_eq!(vol.read_dir("/d1").await.unwrap().len() as u64, n);
}

/// The ustar members of an archive this crate wrote: (name, type flag,
/// link target), PAX extended headers left out.
fn tar_members(tar: &[u8]) -> Vec<(String, u8, String)> {
    let field = |b: &[u8]| String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap()).into_owned();
    let mut out = Vec::new();
    let mut at = 0;
    while at + 512 <= tar.len() && tar[at..at + 512].iter().any(|&b| b != 0) {
        let h = &tar[at..at + 512];
        let size = u64::from_str_radix(field(&h[124..136]).trim(), 8).unwrap_or(0) as usize;
        if h[156] != b'x' && h[156] != b'g' {
            out.push((field(&h[0..100]), h[156], field(&h[157..257])));
        }
        at += 512 + size.div_ceil(512) * 512;
    }
    out
}

/// Review finding 287cd742841f: hard links to a FIFO and to a socket.
/// Extracting skips every name of each (they take `mknod`) rather than
/// linking the second to a path never made, and a tar archive leaves out
/// both names of the socket rather than making the second a link to a
/// member that is not there. A hard-linked regular file still links.
#[tokio::test]
async fn hard_linked_specials_are_skipped_by_every_name() {
    if skip() {
        return;
    }
    let img = fresh(&[], 300);
    let mut vol = open_rw(&img).await;
    vol.mkdir("/d").await.unwrap();
    vol.mknod("/d/fifo", Special::Fifo, &Attrs::default()).await.unwrap();
    vol.link("/d/fifo", "/d/fifo2").await.unwrap();
    vol.mknod("/d/sock", Special::Socket, &Attrs::default()).await.unwrap();
    vol.link("/d/sock", "/d/sock2").await.unwrap();
    vol.write("/d/f", b"data\n").await.unwrap();
    vol.link("/d/f", "/d/f2").await.unwrap();
    vol.flush().await.unwrap();
    drop(vol);
    repair_clean(&img, "after linking specials");

    let vol = img.open().await;
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("x");
    let r = vol.extract("/", &dest, &fio_xfs::ExtractOptions::default()).await.unwrap();
    let mut skipped = r.specials_skipped.clone();
    skipped.sort();
    assert_eq!(skipped, ["d/fifo", "d/fifo2", "d/sock", "d/sock2"], "every name of each special");
    assert_eq!((r.files, r.hard_links), (1, 1), "{r:?}");
    for p in ["d/fifo", "d/fifo2", "d/sock", "d/sock2"] {
        assert!(std::fs::symlink_metadata(dest.join(p)).is_err(), "{p} was created");
    }
    use std::os::unix::fs::MetadataExt;
    let (a, b) = (std::fs::metadata(dest.join("d/f")).unwrap(), std::fs::metadata(dest.join("d/f2")).unwrap());
    assert_eq!((a.ino(), a.nlink()), (b.ino(), 2));
    assert_eq!(std::fs::read(dest.join("d/f2")).unwrap(), b"data\n");

    let p = vol.pack_tar_to(Vec::new(), "/").await.unwrap();
    let sink = vol.pack_tar("/").await.unwrap();
    assert_eq!((p.sockets_skipped, p.hard_links, p.devices, p.files), (2, 2, 1, 1), "{p:?}");
    let members = tar_members(&sink);
    let names: Vec<&str> = members.iter().map(|m| m.0.as_str()).collect();
    assert!(!names.iter().any(|n| n.contains("sock")), "{names:?}");
    for (i, (name, kind, link)) in members.iter().enumerate() {
        if *kind == b'1' {
            assert!(names[..i].contains(&link.as_str()), "{name} links to {link}, not an earlier member: {names:?}");
        }
    }
    let fifo2 = members.iter().find(|m| m.0 == "d/fifo2").expect("d/fifo2 in the archive");
    assert_eq!((fifo2.1, fifo2.2.as_str()), (b'1', "d/fifo"));
}

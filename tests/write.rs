//! The write side against the real tools: filesystems made by `mkfs.xfs`,
//! written by this crate, then checked by `xfs_repair -n` — which rebuilds
//! its own idea of every free extent, inode, reverse mapping, directory hash
//! and counter and compares — and read back.
//!
//! The kernel test (`tests/kernel.rs`) mounts an image written here as well.

mod common;

use std::collections::BTreeMap;
use std::process::Command;

use common::{build, bytes, dir_form, have, run, sparse, Image, Tree, Want};
use fio_xfs::inode::Format;
use fio_xfs::{Attrs, Error, FileDevice, FileType, Special, Volume};

fn skip() -> bool {
    if !have("mkfs.xfs") || !have("xfs_repair") {
        eprintln!("SKIP: mkfs.xfs and xfs_repair are needed");
        return true;
    }
    false
}

/// An empty filesystem made by `mkfs.xfs`.
fn fresh(opts: &[&str], size_mb: u64) -> Image {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fs.xfs");
    std::fs::File::create(&path).unwrap().set_len(size_mb << 20).unwrap();
    run(Command::new("mkfs.xfs").args(["-q", "-f"]).args(opts).arg(&path));
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
    let img = build(&tree, &[], 512);
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
    // Reads in the middle see what was written.
    assert_eq!(vol.read("/d/f").await.unwrap(), b"x");
    assert_eq!(vol.read_dir("/d").await.unwrap().len(), 1);
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

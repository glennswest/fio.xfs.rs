//! Real filesystems, made by `mkfs.xfs` and populated by xfsprogs (`-p`
//! protofile, `xfs_db attr_set`), read back and compared with what was put
//! in — for every directory, fork and attribute form, on v5 and v4, and on
//! geometries that move the structures around.

mod common;

use std::collections::BTreeMap;
use std::process::Command;

use common::{build, bytes, dir_form, have, run, sparse, AttrValue, Image, Tree, Want};
use fio_xfs::inode::Format;
use fio_xfs::{Error, ExtractOptions, FileType, Volume};

/// A symlink target too long for a 512-byte inode.
///
/// Kept within one 1 KiB block's payload (968 bytes): `mkfs.xfs -p` 6.15
/// writes a remote symlink spanning two blocks with its second block
/// unmapped, which `xfs_repair -n` rejects. The kernel test covers the
/// two-block case with a symlink the kernel writes.
fn long_target() -> Vec<u8> {
    let mut t = b"../".to_vec();
    while t.len() < 900 {
        t.extend_from_slice(b"a-directory-name/");
    }
    t.truncate(900);
    t
}

fn standard() -> Tree {
    let mut t = Tree::default();
    t.file("hello.txt", b"hello\n".to_vec());
    t.file("empty", Vec::new());
    t.file("big.bin", bytes(1, (3 << 20) + 123));
    t.file("sparse.bin", sparse(200));
    t.items.insert(
        "setuid".into(),
        Want::File { data: b"#!/bin/sh\n".to_vec(), mode: 0o4755, uid: 1000, gid: 100 },
    );
    t.dir("sf");
    for i in 0..3 {
        t.file(&format!("sf/f{i}"), format!("short form {i}\n").into_bytes());
    }
    t.dir("block");
    for i in 0..40 {
        t.file(&format!("block/entry{i:03}"), Vec::new());
    }
    t.dir("leaf");
    for i in 0..400 {
        t.file(&format!("leaf/a-long-enough-name-{i:05}"), Vec::new());
    }
    t.dir("node");
    for i in 0..5000 {
        t.file(&format!("node/name-in-a-node-directory-{i:05}"), Vec::new());
    }
    t.dir("links");
    t.items.insert("links/short".into(), Want::Symlink(b"../hello.txt".to_vec()));
    t.items.insert("links/long".into(), Want::Symlink(long_target()));
    t.items.insert("links/sfdir".into(), Want::Symlink(b"../sf".to_vec()));
    t.items.insert("links/abs".into(), Want::Symlink(b"/sf/f1".to_vec()));
    t.items.insert("links/loop".into(), Want::Symlink(b"loop".to_vec()));
    t.dir("devs");
    t.items.insert("devs/null".into(), Want::Char(1, 3));
    t.items.insert("devs/sda".into(), Want::Block(8, 0));
    t.items.insert("devs/big".into(), Want::Char(259, 70000));
    t.items.insert("devs/fifo".into(), Want::Fifo);

    // Short form.
    t.attr("hello.txt", "-u", "greeting", AttrValue::Text("hello".into()));
    t.attr("hello.txt", "-s", "selinux", AttrValue::Text("system_u:object_r:etc_t:s0".into()));
    t.attr("hello.txt", "-r", "secret", AttrValue::Text("x".into()));
    // One leaf block.
    for i in 0..40 {
        t.attr("big.bin", "-u", &format!("leaf{i:02}"), AttrValue::Fill(60));
    }
    // Several leaves under a node.
    for i in 0..400 {
        t.attr("sparse.bin", "-u", &format!("node{i:03}"), AttrValue::Fill(40));
    }
    // A value in blocks of its own, next to a local one.
    t.attr("setuid", "-u", "remote", AttrValue::Fill(3000));
    t.attr("setuid", "-u", "local", AttrValue::Text("x".into()));
    // On a directory.
    t.attr("sf", "-s", "selinux", AttrValue::Text("system_u:object_r:usr_t:s0".into()));
    t
}

fn skip() -> bool {
    if !have("mkfs.xfs") || !have("xfs_db") {
        eprintln!("SKIP: mkfs.xfs and xfs_db are needed to make test images");
        return true;
    }
    false
}

/// Read everything back and compare it with the tree.
async fn verify(img: &Image, tree: &Tree) {
    img.check();
    let vol = img.open().await;

    let walked: BTreeMap<String, _> =
        vol.walk("/").await.unwrap().into_iter().map(|e| (e.path.clone(), e)).collect();
    let want: Vec<&String> = tree.items.keys().collect();
    let got: Vec<&String> = walked.keys().collect();
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
                if !data.is_empty() {
                    assert!(vol.read(&p).await.unwrap() == *data, "{path} contents");
                }
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

    // Directory listings agree with the walk, types included.
    for (dir, n) in [("sf", 3), ("block", 40), ("leaf", 400), ("node", 5000)] {
        let entries = vol.read_dir(dir).await.unwrap();
        assert_eq!(entries.len(), n, "{dir}");
        assert!(entries.iter().all(|e| e.kind == FileType::File && !e.is_dir));
    }

    // Paths through symlinks, `.` and `..`.
    assert_eq!(vol.read("/links/short").await.unwrap(), b"hello\n");
    assert_eq!(vol.read("links/abs").await.unwrap(), b"short form 1\n");
    assert_eq!(vol.read("/links/sfdir/f2").await.unwrap(), b"short form 2\n");
    assert_eq!(vol.read("/sf/../node/./../hello.txt").await.unwrap(), b"hello\n");
    assert_eq!(vol.read("/../../hello.txt").await.unwrap(), b"hello\n");
    assert_eq!(vol.read_dir("/links/sfdir").await.unwrap().len(), 3);
    assert!(matches!(vol.read("/links/loop").await, Err(Error::SymlinkLoop(_))));
    assert!(matches!(vol.read("/nope").await, Err(Error::NotFound(_))));
    assert!(matches!(vol.read("/hello.txt/x").await, Err(Error::NotADirectory(_))));
    assert!(matches!(vol.read("/sf").await, Err(Error::IsADirectory(_))));
    assert!(vol.exists("/links/long").await.unwrap(), "a dangling link exists");
    assert!(!vol.exists("/node/no-such-name").await.unwrap());
    assert_eq!(vol.stat("/links/short").await.unwrap().kind(), FileType::Symlink);

    // Ranges, across extents and holes.
    let sp = sparse(200);
    for (off, len) in [(0u64, 10u64), (4090, 20), (8190, 9000), (sp.len() as u64 - 5, 100)] {
        let got = vol.read_range("/sparse.bin", off, len).await.unwrap();
        let end = (off + len).min(sp.len() as u64) as usize;
        assert!(got == sp[off as usize..end], "range {off}+{len}");
    }

    // Extended attributes.
    let paths: std::collections::BTreeSet<&String> = tree.attrs.iter().map(|a| &a.0).collect();
    for path in paths {
        let mut got: Vec<_> =
            vol.list_xattrs(path).await.unwrap().into_iter().map(|x| (x.name, x.value)).collect();
        got.sort();
        let got = tree.unlabelled(path, got);
        assert!(got == tree.attrs_of(path), "{path} attributes: {:?}", got.iter().map(|g| &g.0).collect::<Vec<_>>());
    }
    assert_eq!(vol.get_xattr("/hello.txt", "user.greeting").await.unwrap().unwrap(), b"hello");
    let none: Vec<_> = vol.list_xattrs("/empty").await.unwrap().into_iter().map(|x| (x.name, x.value)).collect();
    assert!(tree.unlabelled("empty", none).is_empty());

    verify_tar(&vol, tree, img).await;
    verify_extract(&vol, tree, &walked).await;
}

/// The forms the standard tree is built to produce, on the default geometry.
async fn verify_forms(img: &Image) {
    let vol = img.open().await;
    let log = vol.superblock().block_log;
    let inode = |p: &'static str| inode_at(&vol, p);

    assert_eq!(inode("/sf").await.format, Format::Local, "short-form directory");
    for (dir, form) in [("/block", "block"), ("/leaf", "leaf"), ("/node", "node")] {
        let i = inode(dir).await;
        let ext = vol.extents(&i, false).await.unwrap();
        assert_eq!(dir_form(&ext, log), form, "{dir}");
    }
    assert_eq!(inode("/big.bin").await.format, Format::Extents);
    let sp = inode("/sparse.bin").await;
    assert_eq!(sp.format, Format::Btree, "the sparse file's extents need a B+tree");
    assert_eq!(vol.extents(&sp, false).await.unwrap().len(), 200);
    assert_eq!(inode("/links/short").await.format, Format::Local, "inline symlink");
    assert_eq!(inode("/links/long").await.format, Format::Extents, "remote symlink");
    assert_eq!(inode("/hello.txt").await.aformat, Some(Format::Local), "short-form attributes");
    let leaf = inode("/big.bin").await;
    assert_eq!(leaf.aformat, Some(Format::Extents));
    assert_eq!(vol.extents(&leaf, true).await.unwrap().iter().map(|e| e.count).sum::<u64>(), 1, "one attribute leaf");
    let node = inode("/sparse.bin").await;
    assert!(vol.extents(&node, true).await.unwrap().iter().map(|e| e.count).sum::<u64>() > 2, "attribute node and leaves");
}

async fn inode_at<D: fio_xfs::BlockDevice>(vol: &Volume<D>, path: &str) -> fio_xfs::Inode {
    vol.inode(vol.lookup(path).await.unwrap()).await.unwrap()
}

#[derive(Debug, Default)]
struct TarEntry {
    kind: u8,
    link: Vec<u8>,
    mode: u32,
    uid: u64,
    data: Vec<u8>,
    xattrs: Vec<(String, Vec<u8>)>,
    dev: (u64, u64),
}

fn octal(f: &[u8]) -> u64 {
    let s = std::str::from_utf8(f).unwrap().trim_matches(|c: char| c == '\0' || c == ' ');
    if s.is_empty() {
        0
    } else {
        u64::from_str_radix(s, 8).unwrap()
    }
}

fn cstr(f: &[u8]) -> Vec<u8> {
    f[..f.iter().position(|&c| c == 0).unwrap_or(f.len())].to_vec()
}

/// A small tar reader: ustar headers, PAX records.
fn parse_tar(b: &[u8]) -> BTreeMap<Vec<u8>, TarEntry> {
    let mut out = BTreeMap::new();
    let mut off = 0;
    let mut pax: Vec<(String, Vec<u8>)> = Vec::new();
    loop {
        let h = &b[off..off + 512];
        if h.iter().all(|&c| c == 0) {
            assert!(b[off + 512..off + 1024].iter().all(|&c| c == 0), "two zero blocks end it");
            assert_eq!(off + 1024, b.len());
            break;
        }
        let stored = octal(&h[148..156]);
        let sum: u64 = h.iter().enumerate().map(|(i, &c)| if (148..156).contains(&i) { 32 } else { c as u64 }).sum();
        assert_eq!(stored, sum, "header checksum");
        let mut size = octal(&h[124..136]) as usize;
        let data_at = off + 512;
        let kind = h[156];
        if kind == b'x' {
            let mut rec = &b[data_at..data_at + size];
            while !rec.is_empty() {
                let sp = rec.iter().position(|&c| c == b' ').unwrap();
                let len: usize = std::str::from_utf8(&rec[..sp]).unwrap().parse().unwrap();
                let kv = &rec[sp + 1..len - 1];
                let eq = kv.iter().position(|&c| c == b'=').unwrap();
                pax.push((String::from_utf8(kv[..eq].to_vec()).unwrap(), kv[eq + 1..].to_vec()));
                rec = &rec[len..];
            }
            off = data_at + size.div_ceil(512) * 512;
            continue;
        }
        let mut e = TarEntry {
            kind,
            link: cstr(&h[157..257]),
            mode: octal(&h[100..108]) as u32,
            uid: octal(&h[108..116]),
            dev: (octal(&h[329..337]), octal(&h[337..345])),
            ..Default::default()
        };
        let mut path = cstr(&h[..100]);
        for (k, v) in pax.drain(..) {
            match k.as_str() {
                "path" => path = v,
                "linkpath" => e.link = v,
                "size" => size = std::str::from_utf8(&v).unwrap().parse().unwrap(),
                "uid" => e.uid = std::str::from_utf8(&v).unwrap().parse().unwrap(),
                _ => {
                    if let Some(n) = k.strip_prefix("SCHILY.xattr.") {
                        e.xattrs.push((n.to_string(), v));
                    }
                }
            }
        }
        e.xattrs.sort();
        e.data = b[data_at..data_at + size].to_vec();
        off = data_at + size.div_ceil(512) * 512;
        assert!(out.insert(path, e).is_none(), "each name once");
    }
    out
}

async fn verify_tar<D: fio_xfs::BlockDevice>(vol: &Volume<D>, tree: &Tree, img: &Image) {
    let archive = vol.pack_tar("/").await.unwrap();
    let entries = parse_tar(&archive);
    let mut want_names: Vec<Vec<u8>> = tree
        .items
        .iter()
        .map(|(p, w)| {
            let mut n = p.as_bytes().to_vec();
            if matches!(w, Want::Dir { .. }) {
                n.push(b'/');
            }
            n
        })
        .collect();
    want_names.sort();
    assert_eq!(entries.keys().cloned().collect::<Vec<_>>(), want_names);
    for (path, want) in &tree.items {
        let key = if matches!(want, Want::Dir { .. }) { format!("{path}/") } else { path.clone() };
        let e = &entries[key.as_bytes()];
        match want {
            Want::File { data, mode, uid, .. } => {
                assert_eq!(e.kind, b'0', "{path}");
                assert!(e.data == *data, "{path} contents in the archive");
                assert_eq!((e.mode, e.uid), (*mode as u32, *uid as u64), "{path}");
            }
            Want::Dir { .. } => assert_eq!(e.kind, b'5'),
            Want::Symlink(t) => assert_eq!((e.kind, &e.link), (b'2', t), "{path}"),
            Want::Char(a, b) => assert_eq!((e.kind, e.dev), (b'3', (*a as u64, *b as u64)), "{path}"),
            Want::Block(a, b) => assert_eq!((e.kind, e.dev), (b'4', (*a as u64, *b as u64)), "{path}"),
            Want::Fifo => assert_eq!(e.kind, b'6'),
        }
        let got = tree.unlabelled(path, e.xattrs.clone());
        assert!(got == tree.attrs_of(path), "{path} attributes in the archive");
    }

    // GNU tar reads it too, names and all.
    if have("tar") {
        let a = img.dir.path().join("out.tar");
        std::fs::write(&a, &archive).unwrap();
        let listed = run(Command::new("tar").arg("-tf").arg(&a));
        assert_eq!(listed.lines().count(), tree.items.len(), "GNU tar lists every name");
        assert!(listed.lines().any(|l| l == "links/long"));
    }
}

async fn verify_extract<D: fio_xfs::BlockDevice>(
    vol: &Volume<D>,
    tree: &Tree,
    walked: &BTreeMap<String, fio_xfs::WalkEntry>,
) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let dest = tempfile::tempdir().unwrap();
    let out = dest.path().join("x");
    let r = vol.extract("/", &out, &ExtractOptions::default()).await.unwrap();
    assert_eq!(r.specials_skipped.len(), 4, "devices and FIFOs are not created");
    assert!(r.xattrs_skipped as usize >= tree.attrs.len());
    for (path, want) in &tree.items {
        let p = out.join(path);
        match want {
            Want::File { data, mode, .. } => {
                assert!(std::fs::read(&p).unwrap() == *data, "{path} extracted");
                let md = std::fs::metadata(&p).unwrap();
                assert_eq!(md.permissions().mode() & 0o7777, *mode as u32, "{path} mode");
                let st = walked[path].stat;
                assert_eq!((md.mtime(), md.mtime_nsec() as u32), (st.mtime.secs, st.mtime.nsecs), "{path} mtime");
            }
            Want::Dir { mode } => {
                let md = std::fs::metadata(&p).unwrap();
                assert!(md.is_dir());
                assert_eq!(md.permissions().mode() & 0o7777, *mode as u32, "{path} mode");
            }
            Want::Symlink(t) => {
                use std::os::unix::ffi::OsStrExt;
                assert_eq!(std::fs::read_link(&p).unwrap().as_os_str().as_bytes(), &t[..], "{path}");
            }
            _ => assert!(std::fs::symlink_metadata(&p).is_err(), "{path} not created"),
        }
    }
    // The sparse file stays sparse.
    let md = std::fs::metadata(out.join("sparse.bin")).unwrap();
    assert!(md.blocks() * 512 < md.len(), "holes kept as holes");
}

#[tokio::test]
async fn v5_default() {
    if skip() {
        return;
    }
    let tree = standard();
    let img = build(&tree, &[], 512);
    let vol = img.open().await;
    let sb = vol.superblock();
    assert!(sb.is_v5() && sb.has_ftype());
    verify_forms(&img).await;
    verify(&img, &tree).await;
}

#[tokio::test]
async fn v5_no_bigtime_no_nrext64() {
    if skip() {
        return;
    }
    let tree = standard();
    let img = build(&tree, &["-m", "bigtime=0", "-i", "nrext64=0"], 512);
    verify_forms(&img).await;
    verify(&img, &tree).await;
}

#[tokio::test]
async fn v5_small_blocks_big_dir_blocks() {
    if skip() {
        return;
    }
    // 1 KiB blocks and 8 KiB directory blocks: a directory block spans
    // several filesystem blocks, and a remote symlink several blocks.
    let tree = standard();
    let img = build(&tree, &["-b", "size=1024", "-n", "size=8192"], 512);
    assert_eq!(img.open().await.superblock().dir_block_size(), 8192);
    verify(&img, &tree).await;
}

#[tokio::test]
async fn v5_big_inodes() {
    if skip() {
        return;
    }
    // 2 KiB inodes: far more fits inline.
    let tree = standard();
    let img = build(&tree, &["-i", "size=2048"], 512);
    verify(&img, &tree).await;
}

#[tokio::test]
async fn v5_many_ags() {
    if skip() {
        return;
    }
    // Small AGs, so block and inode numbers carry an AG number that is not
    // a linear offset.
    let tree = standard();
    let img = build(&tree, &["-d", "agcount=16"], 2048);
    assert!(img.open().await.superblock().ag_count >= 16);
    verify(&img, &tree).await;
}

#[tokio::test]
async fn v4_with_ftype() {
    if skip() {
        return;
    }
    let tree = standard();
    let img = build(&tree, &["-m", "crc=0", "-n", "ftype=1"], 512);
    let vol = img.open().await;
    assert!(!vol.superblock().is_v5() && vol.superblock().has_ftype());
    verify(&img, &tree).await;
}

#[tokio::test]
async fn v4_without_ftype() {
    if skip() {
        return;
    }
    // No file type in directory entries: it comes from each inode instead.
    let tree = standard();
    let img = build(&tree, &["-m", "crc=0", "-n", "ftype=0"], 512);
    assert!(!img.open().await.superblock().has_ftype());
    verify(&img, &tree).await;
}

#[tokio::test]
async fn corruption_is_caught() {
    if skip() {
        return;
    }
    let mut tree = Tree::default();
    tree.file("a", b"abc".to_vec());
    let img = build(&tree, &[], 300);
    let vol = img.open().await;
    let ino = vol.lookup("/a").await.unwrap();
    let at = vol.superblock().ino_to_byte(ino).unwrap();
    drop(vol);

    // One flipped bit in the inode's timestamps fails its checksum.
    let mut data = std::fs::read(&img.path).unwrap();
    data[at as usize + 40] ^= 1;
    let vol = Volume::open(fio_xfs::MemDevice::new(data.clone())).await.unwrap();
    assert!(matches!(vol.stat("/a").await, Err(Error::Corrupt(_))));

    // And one in the superblock refuses the filesystem.
    data[at as usize + 40] ^= 1;
    data[200] ^= 1;
    assert!(matches!(Volume::open(fio_xfs::MemDevice::new(data)).await, Err(Error::Corrupt(_))));

    // Something that is not XFS at all.
    let r = Volume::open(fio_xfs::MemDevice::new(vec![0u8; 1 << 20])).await;
    assert!(matches!(r, Err(Error::NotXfs(_))));
}

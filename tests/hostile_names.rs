//! Names an image can hold but no sound filesystem does — `../` in a
//! directory entry, a `/`, one name twice — must not reach the host when a
//! tree is walked, packed or extracted (review findings 112925e105eb and
//! 4d9887e4883e). Images are made by `mkfs.xfs -p`, then one directory
//! entry is rewritten in place and its inode's checksum stamped again, as
//! an attacker handing over an image would.

mod common;

use std::os::unix::fs::FileExt;

use common::{build, have, Image, Tree, Want};
use fio_xfs::{Error, ExtractOptions};

fn skip() -> bool {
    if !have("mkfs.xfs") {
        eprintln!("SKIP: mkfs.xfs is needed to make test images");
        return true;
    }
    false
}

/// Rewrite the name `from` in short-form directory `dir` to `to` (the same
/// length), and stamp the inode's CRC again on a v5 filesystem.
async fn rename_raw(img: &Image, dir: &str, from: &[u8], to: &[u8]) {
    assert_eq!(from.len(), to.len());
    let (at, size) = {
        let vol = img.open().await;
        let ino = vol.lookup(dir).await.unwrap();
        let sb = vol.superblock();
        (sb.ino_to_byte(ino).unwrap(), sb.inode_size as usize)
    };
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&img.path).unwrap();
    let mut buf = vec![0u8; size];
    f.read_exact_at(&mut buf, at).unwrap();
    assert_eq!(&buf[..2], b"IN");
    let pos = buf.windows(from.len()).position(|w| w == from).expect("name in the directory's inode");
    buf[pos..pos + to.len()].copy_from_slice(to);
    if buf[4] == 3 {
        fio_xfs::crc::stamp(&mut buf, 100);
    }
    f.write_all_at(&buf, at).unwrap();
}

fn file(t: &mut Tree, path: &str, data: &[u8]) {
    t.items.insert(path.into(), Want::File { data: data.to_vec(), mode: 0o644, uid: 0, gid: 0 });
}

#[tokio::test]
async fn dot_dot_name_is_refused_and_nothing_escapes() {
    if skip() {
        return;
    }
    let mut t = Tree::default();
    t.dir("d");
    file(&mut t, "d/aaaaaaaaaaaa", b"pwned\n");
    let img = build(&t, &[], 64);
    rename_raw(&img, "/d", b"aaaaaaaaaaaa", b"../../escape").await;

    let vol = img.open().await;
    assert!(matches!(vol.walk("/").await, Err(Error::Corrupt(_))));
    assert!(matches!(vol.pack_tar("/").await, Err(Error::Corrupt(_))));

    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("a").join("dest");
    let r = vol.extract("/", &dest, &ExtractOptions::default()).await;
    assert!(matches!(r, Err(Error::Corrupt(_))), "{r:?}");
    // dest/d/../../escape is out/a/escape.
    assert!(!out.path().join("a").join("escape").exists(), "a file was written outside dest");
}

#[tokio::test]
async fn slash_in_name_is_refused() {
    if skip() {
        return;
    }
    let mut t = Tree::default();
    t.dir("d");
    file(&mut t, "d/abcdef", b"x\n");
    let img = build(&t, &[], 64);
    rename_raw(&img, "/d", b"abcdef", b"ab/def").await;

    let vol = img.open().await;
    assert!(matches!(vol.walk("/").await, Err(Error::Corrupt(_))));
    assert!(matches!(vol.read_dir("/d").await, Err(Error::Corrupt(_))));
}

#[tokio::test]
async fn duplicate_name_cannot_write_through_a_symlink() {
    if skip() {
        return;
    }
    let out = tempfile::tempdir().unwrap();
    let victim = out.path().join("victim");
    std::fs::write(&victim, b"original\n").unwrap();

    // d/link -> victim, and a regular file d/linq renamed to d/link: two
    // entries with one name, the symlink sorted first on extract.
    let mut t = Tree::default();
    t.dir("d");
    t.items.insert("d/link".into(), Want::Symlink(victim.to_str().unwrap().as_bytes().to_vec()));
    file(&mut t, "d/linq", b"pwned\n");
    let img = build(&t, &[], 64);
    rename_raw(&img, "/d", b"linq", b"link").await;

    let vol = img.open().await;
    assert!(matches!(vol.walk("/").await, Err(Error::Corrupt(_))));
    let r = vol.extract("/", &out.path().join("dest"), &ExtractOptions::default()).await;
    assert!(r.is_err(), "{r:?}");
    assert_eq!(std::fs::read(&victim).unwrap(), b"original\n", "extract wrote through a symlink");
}

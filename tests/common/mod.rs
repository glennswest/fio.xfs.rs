//! Shared by the integration tests: building real XFS images with the real
//! tools, and describing what was put in them.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use fio_xfs::bmap::Extent;
use fio_xfs::{FileDevice, Volume};

/// Whether a tool can be run here. A test that needs a missing one says so
/// and passes, so `cargo test` stays green on a machine without xfsprogs.
pub fn have(tool: &str) -> bool {
    Command::new(tool).arg("-V").output().map(|o| o.status.success()).unwrap_or(false)
}

/// Run a command, failing the test with its output if it fails.
pub fn run(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
    if !out.status.success() {
        panic!(
            "{cmd:?} failed: {}\n{}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Deterministic bytes: the same seed always gives the same content.
pub fn bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// What one name in a built image should read back as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Want {
    File { data: Vec<u8>, mode: u16, uid: u32, gid: u32 },
    Dir { mode: u16 },
    Symlink(Vec<u8>),
    Char(u32, u32),
    Block(u32, u32),
    Fifo,
}

/// A tree to build: paths (relative, `/`-separated) to what they are.
#[derive(Default)]
pub struct Tree {
    pub items: BTreeMap<String, Want>,
    /// Extended attributes to set with `xfs_db`: path, namespace flag
    /// (`-u`, `-r`, `-s`), name, and either a string value or a length of
    /// `v` fill.
    pub attrs: Vec<(String, &'static str, String, AttrValue)>,
}

#[derive(Clone)]
pub enum AttrValue {
    Text(String),
    Fill(usize),
}

impl AttrValue {
    pub fn bytes(&self) -> Vec<u8> {
        match self {
            AttrValue::Text(s) => s.as_bytes().to_vec(),
            AttrValue::Fill(n) => vec![b'v'; *n],
        }
    }
}

impl Tree {
    pub fn file(&mut self, path: &str, data: Vec<u8>) {
        self.items.insert(path.into(), Want::File { data, mode: 0o644, uid: 0, gid: 0 });
    }
    pub fn dir(&mut self, path: &str) {
        self.items.insert(path.into(), Want::Dir { mode: 0o755 });
    }
    pub fn attr(&mut self, path: &str, ns: &'static str, name: &str, v: AttrValue) {
        self.attrs.push((path.into(), ns, name.into(), v));
    }

    /// The Linux name of an attribute this tree sets.
    pub fn attr_name(ns: &str, name: &str) -> String {
        match ns {
            "-r" => format!("trusted.{name}"),
            "-s" => format!("security.{name}"),
            _ => format!("user.{name}"),
        }
    }

    /// The attributes a path should read back with, sorted.
    pub fn attrs_of(&self, path: &str) -> Vec<(String, Vec<u8>)> {
        let mut v: Vec<_> = self
            .attrs
            .iter()
            .filter(|a| a.0 == path)
            .map(|a| (Self::attr_name(a.1, &a.2), a.3.bytes()))
            .collect();
        v.sort();
        v
    }

    /// Write a protofile for `mkfs.xfs -p`, with file contents in `src`.
    fn protofile(&self, src: &Path) -> String {
        let mut out = String::from("/dev/null\n0 0\nd--755 0 0\n");
        let mut open: Vec<&str> = Vec::new();
        // A protofile nests, so a directory's names must follow it: order by
        // component, not by string ("a-b" sorts between "a" and "a/b").
        let mut items: Vec<_> = self.items.iter().collect();
        items.sort_by(|a, b| a.0.split('/').cmp(b.0.split('/')));
        for (i, (path, want)) in items.into_iter().enumerate() {
            let parts: Vec<&str> = path.split('/').collect();
            let (parent, name) = (&parts[..parts.len() - 1], parts[parts.len() - 1]);
            while open.len() > parent.len() || open.iter().zip(parent).any(|(a, b)| a != b) {
                open.pop();
                out.push_str("$\n");
            }
            assert_eq!(open, parent, "{path}: parent directory must come first");
            let perm = |mode: u16| {
                format!(
                    "{}{}{:03o}",
                    if mode & 0o4000 != 0 { 'u' } else { '-' },
                    if mode & 0o2000 != 0 { 'g' } else { '-' },
                    mode & 0o777
                )
            };
            match want {
                Want::File { data, mode, uid, gid } => {
                    let f = src.join(format!("f{i}"));
                    std::fs::write(&f, data).unwrap();
                    out.push_str(&format!("{name} -{} {uid} {gid} {}\n", perm(*mode), f.display()));
                }
                Want::Dir { mode } => {
                    out.push_str(&format!("{name} d{} 0 0\n", perm(*mode)));
                    open.push(name);
                }
                Want::Symlink(t) => {
                    out.push_str(&format!("{name} l--777 0 0 {}\n", String::from_utf8(t.clone()).unwrap()))
                }
                Want::Char(a, b) => out.push_str(&format!("{name} c--666 0 0 {a} {b}\n")),
                Want::Block(a, b) => out.push_str(&format!("{name} b--660 0 6 {a} {b}\n")),
                Want::Fifo => out.push_str(&format!("{name} p--644 0 0\n")),
            }
        }
        for _ in 0..open.len() + 1 {
            out.push_str("$\n");
        }
        out
    }
}

/// A sparse file: 4 KiB of data every 8 KiB, so each piece is its own
/// extent and there are too many to fit in the inode.
pub fn sparse(pieces: usize) -> Vec<u8> {
    let mut v = vec![0u8; pieces * 8192 - 4096];
    for i in 0..pieces {
        v[i * 8192..i * 8192 + 4096].copy_from_slice(&bytes(1000 + i as u64, 4096));
    }
    v
}

/// A built image and the scratch directory it lives in.
pub struct Image {
    pub dir: tempfile::TempDir,
    pub path: PathBuf,
}

/// Format an image with `mkfs.xfs` (`opts` added to the command line),
/// populate it from `tree` through a protofile, and set its attributes with
/// `xfs_db`. Both are xfsprogs' libxfs — the kernel's own XFS code, built
/// for userspace.
pub fn build(tree: &Tree, opts: &[&str], size_mb: u64) -> Image {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let proto = dir.path().join("proto");
    std::fs::write(&proto, tree.protofile(&src)).unwrap();
    let path = dir.path().join("fs.xfs");
    std::fs::File::create(&path).unwrap().set_len(size_mb << 20).unwrap();
    run(Command::new("mkfs.xfs").args(["-q", "-f"]).args(opts).arg("-p").arg(&proto).arg(&path));
    if !tree.attrs.is_empty() {
        let mut cmd = Command::new("xfs_db");
        cmd.arg("-x");
        for (p, ns, name, v) in &tree.attrs {
            cmd.arg("-c").arg(format!("path /{p}"));
            cmd.arg("-c").arg(match v {
                AttrValue::Text(s) => format!("attr_set {ns} {name} {s}"),
                AttrValue::Fill(n) => format!("attr_set {ns} -v {n} {name}"),
            });
        }
        cmd.arg(&path);
        let out = run(&mut cmd);
        assert!(!out.to_lowercase().contains("error") && !out.contains("failed"), "xfs_db: {out}");
    }
    Image { dir, path }
}

impl Image {
    pub async fn open(&self) -> Volume<FileDevice> {
        Volume::open(FileDevice::open(&self.path).await.unwrap()).await.unwrap()
    }

    /// `xfs_repair -n`: the image is sound before anything reads it.
    pub fn check(&self) {
        if have("xfs_repair") {
            run(Command::new("xfs_repair").arg("-n").arg("-f").arg(&self.path));
        }
    }
}

/// Byte offsets (in a directory's logical space) where the leaf and free
/// index blocks start; which of them a directory has names its form.
const LEAF_OFFSET: u64 = 1 << 35;
const FREE_OFFSET: u64 = 1 << 36;

/// The form of a non-short-form directory, from its extents.
pub fn dir_form(extents: &[Extent], block_log: u8) -> &'static str {
    let leaf = LEAF_OFFSET >> block_log;
    let free = FREE_OFFSET >> block_log;
    let data_blocks: u64 = extents.iter().filter(|e| e.offset < leaf).map(|e| e.count).sum();
    let has_leaf = extents.iter().any(|e| e.end() > leaf && e.offset < free);
    let has_free = extents.iter().any(|e| e.end() > free);
    match (has_leaf, has_free) {
        (false, false) if data_blocks > 0 => "block",
        (true, false) => "leaf",
        (true, true) => "node",
        _ => "unknown",
    }
}

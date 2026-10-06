//! The kernel is the judge: a tree written by the real Linux XFS driver,
//! read back by this crate.
//!
//! The test binary is its own init. On the host it builds an initramfs
//! holding itself, the shared libraries it links and the kernel modules XFS
//! needs, formats an image with `mkfs.xfs`, and boots the host's kernel on it
//! under qemu/KVM. Inside, running as PID 1, it mounts the image, writes the
//! tree below through ordinary system calls — hard links, ACLs, xattrs of
//! every size, device nodes, sockets, nanosecond and pre-1970 and post-2038
//! times, holes, unwritten extents, reflinks, big directories, names that are
//! not UTF-8 — unmounts cleanly and powers off. Back on the host, every name
//! is read back and compared, and the tar stream is checked too.
//!
//! Then a crash: the VM mounts a fresh image, writes, syncs and powers off
//! without unmounting. The log is dirty and `Volume::open` must refuse it,
//! with the head and tail `xfs_logprint` finds. A second boot mounts it (the
//! kernel replays the log) and unmounts cleanly, and then it opens and
//! everything written before the crash is there.
//!
//! Last, the other way round: this crate writes an image (every directory
//! form, a big file, a symlink), the kernel mounts it, reads every file back,
//! then adds and removes names in each directory and writes a file of its
//! own — growing the B+trees this crate built — and unmounts. `xfs_repair -n`
//! passes after each side, and the kernel's changes read back here.
//!
//! Needs `/dev/kvm`, `qemu-system-x86_64`, `mkfs.xfs` and a readable kernel
//! and modules for the running kernel; without them it says what is missing
//! and passes. No root: the VM is root inside, the host side never is.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DONE: &str = "FIO-XFS-POPULATED";

/// On the kernel command line: what the guest does instead of populating.
const CRASH: &str = "fioxfs.crash";
const REMOUNT: &str = "fioxfs.remount";
const WRITTEN: &str = "fioxfs.written";

/// What this crate writes for the kernel to read, under `w/`: paths and
/// contents. Directories are made as needed.
fn written_files() -> Vec<(String, Vec<u8>)> {
    let mut v = vec![
        ("w/hello".to_string(), b"hello from fio-xfs\n".to_vec()),
        ("w/big".to_string(), bytes(11, (3 << 20) + 5)),
    ];
    for (dir, n) in WRITTEN_DIRS {
        for i in 0..n {
            v.push((format!("w/{dir}/name-{i:05}"), piece(i)));
        }
    }
    v
}

/// Directories of every form, and how many names each starts with.
const WRITTEN_DIRS: [(&str, usize); 4] = [("sf", 3), ("block", 40), ("leaf", 400), ("node", 3000)];
/// Names the kernel adds to each; it removes every seventh of ours.
const KERNEL_ADDS: usize = 60;

/// Files the crash run writes, under `crash/`.
const CRASH_FILES: usize = 200;

fn crash_data(i: usize) -> Vec<u8> {
    bytes(50_000 + i as u64, 100 + i * 37)
}

// ---- the tree ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Dir,
    File(Content),
    Symlink(Vec<u8>),
    /// A second name for an earlier file.
    HardLink(Vec<u8>),
    /// A copy sharing the earlier file's blocks (`FICLONE`).
    Reflink(Vec<u8>),
    Char(u32, u32),
    Block(u32, u32),
    Fifo,
    Socket,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Content {
    Bytes(Vec<u8>),
    /// `pieces` 4 KiB pieces of data at every other 4 KiB, written last to
    /// first so nothing is ever preallocated past EOF: the holes stay holes.
    Sparse(usize),
    /// `len` bytes preallocated (unwritten), with data written at `at`.
    Unwritten { len: u64, at: u64, data: Vec<u8> },
}

impl Content {
    fn bytes(&self) -> Vec<u8> {
        match self {
            Content::Bytes(b) => b.clone(),
            Content::Sparse(n) => {
                let mut v = vec![0u8; n * 8192 - 4096];
                for i in 0..*n {
                    v[i * 8192..i * 8192 + 4096].copy_from_slice(&piece(i));
                }
                v
            }
            Content::Unwritten { len, at, data } => {
                let mut v = vec![0u8; *len as usize];
                v[*at as usize..*at as usize + data.len()].copy_from_slice(data);
                v
            }
        }
    }
}

fn piece(i: usize) -> Vec<u8> {
    bytes(7000 + i as u64, 4096)
}

fn bytes(seed: u64, len: usize) -> Vec<u8> {
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

#[derive(Debug, Clone)]
struct Item {
    path: Vec<u8>,
    kind: Kind,
    mode: u32,
    uid: u32,
    gid: u32,
    /// Modification time, when the tree sets one.
    mtime: Option<(i64, u32)>,
    xattrs: Vec<(String, Vec<u8>)>,
}

fn item(path: impl AsRef<[u8]>, kind: Kind, mode: u32) -> Item {
    Item { path: path.as_ref().to_vec(), kind, mode, uid: 0, gid: 0, mtime: None, xattrs: Vec::new() }
}

/// A Linux `system.posix_acl_*` value: version 2, then (tag, perm, id).
fn acl(entries: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut v = 2u32.to_le_bytes().to_vec();
    for &(tag, perm, id) in entries {
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&perm.to_le_bytes());
        v.extend_from_slice(&id.to_le_bytes());
    }
    v
}

const ACL_USER_OBJ: u16 = 0x01;
const ACL_USER: u16 = 0x02;
const ACL_GROUP_OBJ: u16 = 0x04;
const ACL_MASK: u16 = 0x10;
const ACL_OTHER: u16 = 0x20;
const NO_ID: u32 = u32::MAX;

/// The tree, in creation order: every directory before what is in it.
fn tree() -> Vec<Item> {
    let mut t = Vec::new();
    let file = |p: &str, b: Vec<u8>| item(p, Kind::File(Content::Bytes(b)), 0o644);

    t.push(item("etc", Kind::Dir, 0o755));
    let mut os = file("etc/os-release", b"NAME=\"Rocky Linux\"\nVERSION_ID=\"9.6\"\n".to_vec());
    os.mtime = Some((1_700_000_000, 123_456_789));
    os.xattrs.push(("security.fio-test".into(), b"system_u:object_r:etc_t:s0\0".to_vec()));
    t.push(os);
    t.push(file("etc/empty", Vec::new()));

    let mut old = file("etc/before-1970", b"old\n".to_vec());
    old.mtime = Some((-1000, 5));
    t.push(old);
    let mut future = file("etc/after-2038", b"future\n".to_vec());
    future.mtime = Some((4_000_000_000, 999_999_999));
    t.push(future);

    t.push(item("usr", Kind::Dir, 0o755));
    t.push(item("usr/bin", Kind::Dir, 0o755));
    let mut big = file("usr/bin/big", bytes(1, (5 << 20) + 17));
    big.mode = 0o755;
    t.push(big);
    let mut suid = file("usr/bin/su", bytes(2, 30_000));
    (suid.mode, suid.uid, suid.gid) = (0o4755, 0, 0);
    suid.xattrs.push(("user.small".into(), b"x".to_vec()));
    t.push(suid);
    let mut owned = file("usr/bin/owned", b"mine\n".to_vec());
    (owned.mode, owned.uid, owned.gid) = (0o2750, 1000, 100);
    t.push(owned);

    t.push(item("data", Kind::Dir, 0o755));
    t.push(item("data/sparse", Kind::File(Content::Sparse(300)), 0o644));
    t.push(item(
        "data/prealloc",
        Kind::File(Content::Unwritten { len: 1 << 20, at: 300_000, data: bytes(3, 10_000) }),
        0o644,
    ));
    t.push(item("data/clone", Kind::Reflink(b"usr/bin/big".to_vec()), 0o644));

    t.push(item("links", Kind::Dir, 0o755));
    t.push(item("links/hard-a", Kind::File(Content::Bytes(b"linked\n".to_vec())), 0o600));
    t.push(item("links/hard-b", Kind::HardLink(b"links/hard-a".to_vec()), 0o600));
    t.push(item("links/rel", Kind::Symlink(b"../etc/os-release".to_vec()), 0o777));
    t.push(item("links/abs", Kind::Symlink(b"/usr/bin".to_vec()), 0o777));
    let mut long = b"/".to_vec();
    while long.len() < 1020 {
        long.extend_from_slice(b"deep/");
    }
    t.push(item("links/long", Kind::Symlink(long), 0o777));

    t.push(item("dev", Kind::Dir, 0o755));
    t.push(item("dev/null", Kind::Char(1, 3), 0o666));
    t.push(item("dev/vda", Kind::Block(252, 0), 0o660));
    t.push(item("dev/nvme", Kind::Char(259, 70_000), 0o600));
    t.push(item("dev/fifo", Kind::Fifo, 0o644));
    t.push(item("dev/sock", Kind::Socket, 0o755));

    // ACLs: XFS keeps its own format on disk; reads must show Linux's.
    let mut shared = item("shared", Kind::Dir, 0o775);
    let dflt = acl(&[
        (ACL_USER_OBJ, 7, NO_ID),
        (ACL_USER, 5, 1000),
        (ACL_GROUP_OBJ, 5, NO_ID),
        (ACL_MASK, 7, NO_ID),
        (ACL_OTHER, 5, NO_ID),
    ]);
    shared.xattrs.push(("system.posix_acl_default".into(), dflt));
    t.push(shared);
    let mut f = file("shared/acl", b"acl\n".to_vec());
    // Mode follows the ACL: owner 7, mask 5 (the group bits), other 4.
    f.mode = 0o754;
    f.xattrs.push((
        "system.posix_acl_access".into(),
        acl(&[
            (ACL_USER_OBJ, 7, NO_ID),
            (ACL_USER, 4, 1000),
            (ACL_GROUP_OBJ, 5, NO_ID),
            (ACL_MASK, 5, NO_ID),
            (ACL_OTHER, 4, NO_ID),
        ]),
    ));
    t.push(f);

    // Attributes in every form.
    let mut many = file("data/attrs-node", b"attrs\n".to_vec());
    for i in 0..500 {
        many.xattrs.push((format!("user.attribute-number-{i:04}"), bytes(100 + i, 50 + (i as usize % 40))));
    }
    many.xattrs.push(("user.remote".into(), bytes(99, 20_000)));
    many.xattrs.push(("trusted.overlay.opaque".into(), b"y".to_vec()));
    t.push(many);
    let mut leaf = file("data/attrs-leaf", b"attrs\n".to_vec());
    for i in 0..20 {
        leaf.xattrs.push((format!("user.leaf{i}"), bytes(200 + i, 100)));
    }
    t.push(leaf);

    // Directories in every form, one with names that are not UTF-8.
    t.push(item("dirs", Kind::Dir, 0o755));
    t.push(item("dirs/empty", Kind::Dir, 0o700));
    for (d, n) in [("dirs/block", 30), ("dirs/leaf", 300), ("dirs/node", 20_000)] {
        t.push(item(d, Kind::Dir, 0o755));
        for i in 0..n {
            t.push(file(&format!("{d}/a-reasonably-long-file-name-{i:06}"), Vec::new()));
        }
    }
    let mut raw = b"dirs/caf".to_vec();
    raw.push(0xe9);
    t.push(item(raw, Kind::File(Content::Bytes(b"latin-1 name\n".to_vec())), 0o644));

    // Times last set on directories, after everything is in them.
    t.iter_mut().filter(|i| i.path == b"etc").for_each(|i| i.mtime = Some((1_600_000_000, 1)));
    t
}

// ---- inside the VM -----------------------------------------------------------

mod guest {
    use super::*;

    fn cstr(p: &[u8]) -> CString {
        CString::new(p).unwrap()
    }

    fn check(what: &str, r: libc::c_int) {
        if r < 0 {
            let e = std::io::Error::last_os_error();
            println!("FIO-XFS-FAILED {what}: {e}");
            power_off();
        }
    }

    fn power_off() -> ! {
        unsafe {
            libc::sync();
            libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
        }
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    fn mount(src: &str, dst: &str, fs: &str, data: &str) {
        let (s, d, f, o) = (cstr(src.as_bytes()), cstr(dst.as_bytes()), cstr(fs.as_bytes()), cstr(data.as_bytes()));
        let r = unsafe { libc::mount(s.as_ptr(), d.as_ptr(), f.as_ptr(), 0, o.as_ptr() as *const _) };
        check(&format!("mount {dst}"), r);
    }

    pub fn run() -> ! {
        mount("devtmpfs", "/dev", "devtmpfs", "");
        mount("proc", "/proc", "proc", "");

        let mut mods: Vec<_> = std::fs::read_dir("/modules").unwrap().map(|e| e.unwrap().path()).collect();
        mods.sort();
        for m in mods {
            let f = std::fs::File::open(&m).unwrap();
            use std::os::fd::AsRawFd;
            let name = m.file_name().unwrap().to_string_lossy().into_owned();
            let compressed = !name.ends_with(".ko");
            let empty = cstr(b"");
            let flags = if compressed { 4 } else { 0 }; // MODULE_INIT_COMPRESSED_FILE
            let r = unsafe { libc::syscall(libc::SYS_finit_module, f.as_raw_fd(), empty.as_ptr(), flags) };
            if r < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::EEXIST) {
                    println!("FIO-XFS-FAILED loading {name}: {e}");
                    power_off();
                }
            }
        }

        let start = Instant::now();
        while !Path::new("/dev/vda").exists() {
            if start.elapsed() > Duration::from_secs(20) {
                println!("FIO-XFS-FAILED no /dev/vda");
                power_off();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
        let m = cstr(b"/mnt");
        if cmdline.split_whitespace().any(|w| w == CRASH) {
            // Write, push it all to the disk, and stop without unmounting:
            // the log is left without an unmount record.
            mount("/dev/vda", "/mnt", "xfs", "");
            check("mkdir crash", unsafe { libc::mkdir(cstr(b"/mnt/crash").as_ptr(), 0o755) });
            for i in 0..CRASH_FILES {
                std::fs::write(format!("/mnt/crash/file-{i:04}"), crash_data(i)).unwrap();
            }
            unsafe { libc::sync() };
            println!("{DONE}");
            power_off();
        }
        if cmdline.split_whitespace().any(|w| w == WRITTEN) {
            mount("/dev/vda", "/mnt", "xfs", "");
            check_written();
            check("umount", unsafe { libc::umount(m.as_ptr()) });
            println!("{DONE}");
            power_off();
        }
        if cmdline.split_whitespace().any(|w| w == REMOUNT) {
            // Mounting replays the log; unmounting leaves it clean.
            mount("/dev/vda", "/mnt", "xfs", "");
            check("umount", unsafe { libc::umount(m.as_ptr()) });
            println!("{DONE}");
            power_off();
        }
        // No speculative preallocation past EOF, so the sparse file's holes
        // are the ones it was written with.
        mount("/dev/vda", "/mnt", "xfs", "allocsize=4k");
        populate(Path::new("/mnt"));
        check("umount", unsafe { libc::umount(m.as_ptr()) });
        println!("{DONE}");
        power_off();
    }

    fn fail(what: String) -> ! {
        println!("FIO-XFS-FAILED {what}");
        power_off();
    }

    /// Read what this crate wrote, then change it.
    fn check_written() {
        let mnt = Path::new("/mnt");
        for (p, data) in written_files() {
            match std::fs::read(mnt.join(&p)) {
                Ok(d) if d == data => {}
                Ok(d) => fail(format!("{p}: {} bytes read back differ from {} written", d.len(), data.len())),
                Err(e) => fail(format!("reading {p}: {e}")),
            }
        }
        match std::fs::read_link(mnt.join("w/link")) {
            Ok(t) if t == Path::new("sf/name-00001") => {}
            other => fail(format!("w/link: {other:?}")),
        }
        for (dir, n) in WRITTEN_DIRS {
            let count = std::fs::read_dir(mnt.join("w").join(dir)).map(|d| d.count()).unwrap_or(usize::MAX);
            if count != n {
                fail(format!("w/{dir} lists {count} names, {n} were written"));
            }
            for i in 0..KERNEL_ADDS {
                if let Err(e) = std::fs::write(mnt.join(format!("w/{dir}/kernel-{i:03}")), piece(i + 7)) {
                    fail(format!("w/{dir}/kernel-{i:03}: {e}"));
                }
            }
            for i in (0..n).step_by(7) {
                if let Err(e) = std::fs::remove_file(mnt.join(format!("w/{dir}/name-{i:05}"))) {
                    fail(format!("removing w/{dir}/name-{i:05}: {e}"));
                }
            }
        }
        if let Err(e) = std::fs::write(mnt.join("w/kernel-big"), bytes(12, 2 << 20)) {
            fail(format!("w/kernel-big: {e}"));
        }
    }

    fn populate(root: &Path) {
        let tree = tree();
        let at = |p: &[u8]| root.join(std::ffi::OsStr::from_bytes(p));
        for it in &tree {
            let path = at(&it.path);
            let c = cstr(path.as_os_str().as_bytes());
            let what = String::from_utf8_lossy(&it.path).into_owned();
            match &it.kind {
                Kind::Dir => check(&what, unsafe { libc::mkdir(c.as_ptr(), 0o700) }),
                Kind::File(content) => write_file(&path, content),
                Kind::Symlink(t) => {
                    let t = cstr(t);
                    check(&what, unsafe { libc::symlink(t.as_ptr(), c.as_ptr()) });
                }
                Kind::HardLink(first) => {
                    let f = cstr(at(first).as_os_str().as_bytes());
                    check(&what, unsafe { libc::link(f.as_ptr(), c.as_ptr()) });
                    continue;
                }
                Kind::Reflink(src) => {
                    let s = std::fs::File::open(at(src)).unwrap();
                    let d = std::fs::File::create(&path).unwrap();
                    use std::os::fd::AsRawFd;
                    const FICLONE: libc::c_ulong = 0x4004_9409;
                    check(&what, unsafe { libc::ioctl(d.as_raw_fd(), FICLONE, s.as_raw_fd()) });
                }
                Kind::Char(a, b) | Kind::Block(a, b) => {
                    let t = if matches!(it.kind, Kind::Char(..)) { libc::S_IFCHR } else { libc::S_IFBLK };
                    check(&what, unsafe { libc::mknod(c.as_ptr(), t | 0o600, libc::makedev(*a, *b)) });
                }
                Kind::Fifo => check(&what, unsafe { libc::mknod(c.as_ptr(), libc::S_IFIFO | 0o600, 0) }),
                Kind::Socket => check(&what, unsafe { libc::mknod(c.as_ptr(), libc::S_IFSOCK | 0o600, 0) }),
            }
            // Owner, then mode (a change of owner clears set-ID bits), then
            // attributes (an access ACL rewrites the mode to match itself).
            if !matches!(it.kind, Kind::Symlink(_)) {
                check(&what, unsafe { libc::chown(c.as_ptr(), it.uid, it.gid) });
                check(&what, unsafe { libc::chmod(c.as_ptr(), it.mode) });
                for (name, value) in &it.xattrs {
                    let n = cstr(name.as_bytes());
                    let r = unsafe {
                        libc::lsetxattr(c.as_ptr(), n.as_ptr(), value.as_ptr() as *const _, value.len(), 0)
                    };
                    check(&format!("{what} {name}"), r);
                }
            }
        }
        // Times last: writing into a directory moves its mtime.
        for it in tree.iter().rev() {
            if let Some((s, ns)) = it.mtime {
                let c = cstr(at(&it.path).as_os_str().as_bytes());
                let ts = [
                    libc::timespec { tv_sec: s, tv_nsec: ns as i64 },
                    libc::timespec { tv_sec: s, tv_nsec: ns as i64 },
                ];
                check("utimensat", unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) });
            }
        }
    }

    fn write_file(path: &Path, content: &Content) {
        use std::os::unix::fs::FileExt;
        let f = std::fs::File::create(path).unwrap();
        match content {
            Content::Bytes(b) => f.write_all_at(b, 0).unwrap(),
            Content::Sparse(n) => {
                // Last piece first: every later write lands inside EOF.
                for i in (0..*n).rev() {
                    f.write_all_at(&piece(i), i as u64 * 8192).unwrap();
                    if i % 16 == 0 {
                        f.sync_data().unwrap();
                    }
                }
            }
            Content::Unwritten { len, at, data } => {
                use std::os::fd::AsRawFd;
                check("fallocate", unsafe { libc::fallocate(f.as_raw_fd(), 0, 0, *len as i64) });
                f.write_all_at(data, *at).unwrap();
            }
        }
        f.sync_all().unwrap();
    }
}

// ---- on the host ---------------------------------------------------------------

/// A `newc` cpio archive, which is what an initramfs is.
struct Cpio {
    out: Vec<u8>,
    ino: u32,
}

impl Cpio {
    fn entry(&mut self, name: &str, mode: u32, data: &[u8], rdev: (u32, u32)) {
        self.ino += 1;
        let name = name.trim_start_matches('/');
        let hdr = format!(
            "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
            self.ino, mode, 0, 0, 1, 0, data.len(), 0, 0, rdev.0, rdev.1, name.len() + 1, 0
        );
        self.out.extend_from_slice(hdr.as_bytes());
        self.out.extend_from_slice(name.as_bytes());
        self.out.push(0);
        self.pad();
        self.out.extend_from_slice(data);
        self.pad();
    }
    fn pad(&mut self) {
        while self.out.len() % 4 != 0 {
            self.out.push(0);
        }
    }
    fn dir(&mut self, name: &str) {
        self.entry(name, 0o040755, &[], (0, 0));
    }
    fn finish(mut self) -> Vec<u8> {
        self.entry("TRAILER!!!", 0, &[], (0, 0));
        self.out
    }
}

fn output(cmd: &mut Command) -> Option<String> {
    let o = cmd.output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Modules (with dependencies first) that must be loaded for `name`; empty
/// when it is built in.
fn module_files(name: &str, kver: &str, out: &mut Vec<PathBuf>) -> Result<(), String> {
    // Not a module at all: built in, or not in this kernel — the guest says
    // which if it matters. Only XFS itself must be found.
    let Some(file) = output(Command::new("modinfo").args(["-k", kver, "-n", name])) else {
        return if name == "xfs" { Err("modinfo xfs failed".into()) } else { Ok(()) };
    };
    let file = file.trim();
    if file.is_empty() || file.contains("(builtin)") {
        return Ok(());
    }
    let deps = output(Command::new("modinfo").args(["-k", kver, "-F", "depends", name])).unwrap_or_default();
    for d in deps.trim().split(',').filter(|d| !d.is_empty()) {
        module_files(d, kver, out)?;
    }
    let p = PathBuf::from(file);
    if !out.contains(&p) {
        out.push(p);
    }
    Ok(())
}

fn skip(why: &str) {
    println!("SKIP kernel test: {why}");
}

fn initramfs(kver: &str) -> Result<Vec<u8>, String> {
    let exe = std::env::current_exe().unwrap();
    let mut c = Cpio { out: Vec::new(), ino: 0 };
    for d in ["dev", "proc", "mnt", "modules", "root"] {
        c.dir(d);
    }
    c.entry("dev/console", 0o020600, &[], (5, 1));
    c.entry("init", 0o100755, &std::fs::read(&exe).unwrap(), (0, 0));

    // The shared libraries it links, at the paths it will look for them.
    let ldd = output(Command::new("ldd").arg(&exe)).ok_or("ldd failed")?;
    let mut dirs = std::collections::BTreeSet::new();
    for line in ldd.lines() {
        let Some(lib) = line.split_whitespace().find(|w| w.starts_with('/')) else { continue };
        let lib = Path::new(lib);
        let mut parts = Vec::new();
        for a in lib.parent().unwrap().ancestors() {
            if a != Path::new("/") {
                parts.push(a.to_path_buf());
            }
        }
        for d in parts.into_iter().rev() {
            if dirs.insert(d.clone()) {
                c.dir(d.to_str().unwrap());
            }
        }
        let data = std::fs::read(lib).map_err(|e| format!("{}: {e}", lib.display()))?;
        c.entry(lib.to_str().unwrap(), 0o100755, &data, (0, 0));
    }

    let mut mods = Vec::new();
    for m in ["virtio_pci", "virtio_blk", "xfs"] {
        module_files(m, kver, &mut mods)?;
    }
    for (i, m) in mods.iter().enumerate() {
        let data = std::fs::read(m).map_err(|e| format!("{}: {e}", m.display()))?;
        let name = m.file_name().unwrap().to_str().unwrap();
        // Decompress on the host when it can; otherwise the kernel does.
        let (name, data) = match name.rsplit_once('.') {
            Some((base, "xz")) if output(Command::new("xz").arg("-V")).is_some() => {
                (base.to_string(), decompress("xz", m)?)
            }
            Some((base, "zst")) if output(Command::new("zstd").arg("-V")).is_some() => {
                (base.to_string(), decompress("zstd", m)?)
            }
            _ => (name.to_string(), data),
        };
        c.entry(&format!("modules/{i:02}-{name}"), 0o100644, &data, (0, 0));
    }
    Ok(c.finish())
}

fn decompress(tool: &str, file: &Path) -> Result<Vec<u8>, String> {
    let o = Command::new(tool).arg("-dc").arg(file).output().map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err(format!("{tool} -dc {}", file.display()));
    }
    Ok(o.stdout)
}

fn boot(kernel: &Path, initrd: &Path, image: &Path, mode: &str) -> Result<String, String> {
    let mut child = Command::new("qemu-system-x86_64")
        .args(["-enable-kvm", "-cpu", "host", "-m", "1024", "-smp", "2", "-nographic", "-no-reboot"])
        .arg("-kernel")
        .arg(kernel)
        .arg("-initrd")
        .arg(initrd)
        .arg("-append")
        .arg(format!("console=ttyS0 panic=-1 rdinit=/init selinux=0 quiet {mode}"))
        .arg("-drive")
        .arg(format!("file={},format=raw,if=virtio,cache=unsafe", image.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("qemu: {e}"))?;
    let start = Instant::now();
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(600) {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let o = child.wait_with_output().map_err(|e| e.to_string())?;
    Ok(format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

fn main() {
    if std::process::id() == 1 {
        guest::run();
    }
    // `cargo test -- --list` and friends: nothing to list here.
    if std::env::args().any(|a| a == "--list") {
        return;
    }

    let kver = output(Command::new("uname").arg("-r")).unwrap_or_default().trim().to_string();
    let kernel = PathBuf::from(format!("/boot/vmlinuz-{kver}"));
    let kernel = if kernel.exists() { kernel } else { PathBuf::from(format!("/lib/modules/{kver}/vmlinuz")) };
    if std::fs::File::open(&kernel).is_err() {
        return skip(&format!("cannot read the kernel ({})", kernel.display()));
    }
    if std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_err() {
        return skip("cannot open /dev/kvm");
    }
    for tool in ["qemu-system-x86_64", "mkfs.xfs", "modinfo", "ldd"] {
        if Command::new(tool).arg("--version").output().is_err() {
            return skip(&format!("{tool} is missing"));
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let initrd = dir.path().join("initrd");
    std::fs::write(&initrd, initramfs(&kver).expect("initramfs")).unwrap();

    for (label, opts) in [("v5", &[][..]), ("v5-1k", &["-b", "size=1024", "-n", "size=8192"][..])] {
        let image = dir.path().join(format!("{label}.xfs"));
        std::fs::File::create(&image).unwrap().set_len(1 << 30).unwrap();
        let o = Command::new("mkfs.xfs").args(["-q", "-f"]).args(opts).arg(&image).output().unwrap();
        assert!(o.status.success(), "mkfs.xfs: {}", String::from_utf8_lossy(&o.stderr));

        let t = Instant::now();
        let console = boot(&kernel, &initrd, &image, "").expect("boot");
        if !console.contains(DONE) {
            let tail: Vec<&str> = console.lines().rev().take(60).collect();
            panic!("{label}: the VM did not finish populating:\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
        }
        println!("{label}: populated by Linux {kver} in {:.1}s", t.elapsed().as_secs_f64());

        if Command::new("xfs_repair").arg("-V").output().is_ok() {
            let o = Command::new("xfs_repair").args(["-n", "-f"]).arg(&image).output().unwrap();
            assert!(o.status.success(), "xfs_repair -n: {}", String::from_utf8_lossy(&o.stdout));
        }

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(verify(&image));
        println!("{label}: every name read back and matched");
    }

    crash(&kernel, &initrd, dir.path());
    written(&kernel, &initrd, dir.path());
    println!("test result: ok. kernel-populated images verified, a crashed one refused, a written one mounted");
}

fn boot_ok(kernel: &Path, initrd: &Path, image: &Path, mode: &str) {
    let console = boot(kernel, initrd, image, mode).expect("boot");
    if !console.contains(DONE) {
        let tail: Vec<&str> = console.lines().rev().take(60).collect();
        panic!("{mode}: the VM did not finish:\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
    }
}

/// What `xfs_logprint -t` says of the log: clean or not, head and tail.
fn logprint(image: &Path) -> Option<(bool, u64, u64)> {
    let o = Command::new("xfs_logprint").arg("-t").arg(image).output().ok()?;
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let line = text.lines().find(|l| l.contains("log tail:"))?;
    let num = |key: &str| -> Option<u64> {
        let rest = &line[line.find(key)? + key.len()..];
        rest.split_whitespace().next()?.parse().ok()
    };
    Some((line.contains("<CLEAN>"), num("head:")?, num("tail:")?))
}

/// A mount that never unmounted is refused; once the kernel has replayed
/// its log, it reads.
fn crash(kernel: &Path, initrd: &Path, dir: &Path) {
    use fio_xfs::{Error, FileDevice, LogState, Volume};
    let image = dir.join("crash.xfs");
    std::fs::File::create(&image).unwrap().set_len(1 << 30).unwrap();
    let o = Command::new("mkfs.xfs").args(["-q", "-f"]).arg(&image).output().unwrap();
    assert!(o.status.success(), "mkfs.xfs: {}", String::from_utf8_lossy(&o.stderr));
    boot_ok(kernel, initrd, &image, CRASH);

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let (head, tail) = rt.block_on(async {
        let r = Volume::open(FileDevice::open(&image).await.unwrap()).await;
        let Err(Error::DirtyLog { head, tail }) = r else {
            panic!("crashed: Volume::open gave {:?}, not DirtyLog", r.map(|v| v.log_state().clone()));
        };
        let vol = Volume::open_norecovery(FileDevice::open(&image).await.unwrap()).await.unwrap();
        assert_eq!(vol.log_state(), &LogState::Dirty { head, tail });
        (head, tail)
    });
    println!("crashed: refused, log dirty from {tail} to {head}");
    if let Some((clean, h, t)) = logprint(&image) {
        assert!(!clean, "xfs_logprint says the crashed log is clean");
        assert_eq!((head, tail), (h, t), "head and tail as xfs_logprint finds them");
        println!("crashed: xfs_logprint agrees (head {h}, tail {t})");
    }

    boot_ok(kernel, initrd, &image, REMOUNT);
    rt.block_on(async {
        let vol = Volume::open(FileDevice::open(&image).await.unwrap()).await.unwrap();
        assert_eq!(vol.log_state(), &LogState::Clean);
        for i in 0..CRASH_FILES {
            let got = vol.read(&format!("/crash/file-{i:04}")).await.unwrap();
            assert!(got == crash_data(i), "crash/file-{i:04} after replay");
        }
    });
    if let Some((clean, _, _)) = logprint(&image) {
        assert!(clean, "xfs_logprint says the replayed log is dirty");
    }
    println!("crashed: after the kernel replayed the log, clean and every file read back");
}

fn repair_clean(image: &Path, when: &str) {
    if Command::new("xfs_repair").arg("-V").output().is_ok() {
        let o = Command::new("xfs_repair").args(["-n", "-f"]).arg(image).output().unwrap();
        assert!(o.status.success(), "xfs_repair -n {when}: {}", String::from_utf8_lossy(&o.stdout));
    }
}

/// An image written by this crate, mounted, read and changed by the kernel.
fn written(kernel: &Path, initrd: &Path, dir: &Path) {
    use fio_xfs::{FileDevice, Volume};
    for (label, opts) in [("written", &[][..]), ("written-1k", &["-b", "size=1024", "-n", "size=8192"][..])] {
        let image = dir.join(format!("{label}.xfs"));
        std::fs::File::create(&image).unwrap().set_len(1 << 30).unwrap();
        let o = Command::new("mkfs.xfs").args(["-q", "-f"]).args(opts).arg(&image).output().unwrap();
        assert!(o.status.success(), "mkfs.xfs: {}", String::from_utf8_lossy(&o.stderr));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut vol = Volume::open(FileDevice::open_rw(&image).await.unwrap()).await.unwrap();
            for (p, data) in written_files() {
                let parent = &p[..p.rfind('/').unwrap()];
                vol.mkdir_all(&format!("/{parent}")).await.unwrap();
                vol.write(&format!("/{p}"), &data).await.unwrap();
            }
            vol.symlink("/w/link", "sf/name-00001").await.unwrap();
            vol.flush().await.unwrap();
        });
        repair_clean(&image, &format!("{label}: as fio-xfs wrote it"));

        boot_ok(kernel, initrd, &image, WRITTEN);
        println!("{label}: the kernel mounted it and read every file back");
        repair_clean(&image, &format!("{label}: after the kernel changed it"));

        rt.block_on(async {
            let vol = Volume::open(FileDevice::open(&image).await.unwrap()).await.unwrap();
            for (dir, n) in WRITTEN_DIRS {
                let names = vol.read_dir(&format!("/w/{dir}")).await.unwrap().len();
                assert_eq!(names, n - n.div_ceil(7) + KERNEL_ADDS, "w/{dir}");
                for i in 0..KERNEL_ADDS {
                    let got = vol.read(&format!("/w/{dir}/kernel-{i:03}")).await.unwrap();
                    assert!(got == piece(i + 7), "w/{dir}/kernel-{i:03}");
                }
                assert!(!vol.exists(&format!("/w/{dir}/name-00000")).await.unwrap());
            }
            assert!(vol.read("/w/kernel-big").await.unwrap() == bytes(12, 2 << 20), "w/kernel-big");
        });
        println!("{label}: xfs_repair agrees before and after, and the kernel's changes read back");
    }
}

async fn verify(image: &Path) {
    use fio_xfs::{FileDevice, FileType, Volume};
    let vol = Volume::open(FileDevice::open(image).await.unwrap()).await.unwrap();
    let tree = tree();
    let walked: BTreeMap<Vec<u8>, fio_xfs::WalkEntry> =
        vol.walk("/").await.unwrap().into_iter().map(|e| (e.raw_path.clone(), e)).collect();
    let want: Vec<&Vec<u8>> = {
        let mut v: Vec<_> = tree.iter().map(|i| &i.path).collect();
        v.sort();
        v
    };
    assert_eq!(walked.keys().collect::<Vec<_>>(), want, "the same names");

    let by_path: BTreeMap<&[u8], &Item> = tree.iter().map(|i| (&i.path[..], i)).collect();
    for it in &tree {
        let name = String::from_utf8_lossy(&it.path).into_owned();
        let e = &walked[&it.path];
        let st = e.stat;
        let inode = vol.inode(st.inode).await.unwrap();
        // What the tree says, following a hard link or reflink to its source.
        let base = match &it.kind {
            Kind::HardLink(p) => by_path[&p[..]],
            _ => it,
        };
        if !matches!(it.kind, Kind::Symlink(_)) {
            assert_eq!(st.mode & 0o7777, base.mode as u16, "{name} mode");
            assert_eq!((st.uid, st.gid), (base.uid, base.gid), "{name} owner");
            let mut got: Vec<_> = vol.xattrs_inode(&inode).await.unwrap().into_iter().map(|x| (x.name, x.value)).collect();
            got.retain(|x| x.0 != "security.selinux");
            got.sort();
            let mut want = base.xattrs.clone();
            want.sort();
            assert!(got == want, "{name} attributes: got {:?}", got.iter().map(|g| &g.0).collect::<Vec<_>>());
        }
        if let Some((s, ns)) = base.mtime {
            assert_eq!((st.mtime.secs, st.mtime.nsecs), (s, ns), "{name} mtime");
        }
        let content = |c: &Content| c.bytes();
        let want_data = match &base.kind {
            Kind::File(c) => Some(content(c)),
            Kind::Reflink(src) => match &by_path[&src[..]].kind {
                Kind::File(c) => Some(content(c)),
                _ => unreachable!(),
            },
            _ => None,
        };
        match &base.kind {
            Kind::Dir => assert!(st.is_dir(), "{name}"),
            Kind::File(_) | Kind::Reflink(_) => {
                assert!(st.is_file(), "{name}");
                let want = want_data.unwrap();
                let ext = vol.extents(&inode, false).await.unwrap();
                let got = vol.read_inode_range(&inode, &ext, 0, inode.size).await.unwrap();
                assert!(got == want, "{name} contents ({} bytes read, {} written)", got.len(), want.len());
            }
            Kind::Symlink(t) => assert_eq!(&vol.symlink_target(&inode).await.unwrap(), t, "{name}"),
            Kind::Char(a, b) => assert_eq!((st.kind(), st.rdev), (FileType::CharDevice, (*a, *b)), "{name}"),
            Kind::Block(a, b) => assert_eq!((st.kind(), st.rdev), (FileType::BlockDevice, (*a, *b)), "{name}"),
            Kind::Fifo => assert_eq!(st.kind(), FileType::Fifo, "{name}"),
            Kind::Socket => assert_eq!(st.kind(), FileType::Socket, "{name}"),
            Kind::HardLink(_) => unreachable!(),
        }
    }

    // Hard links share an inode.
    let a = walked[&b"links/hard-a".to_vec()].stat;
    let b = walked[&b"links/hard-b".to_vec()].stat;
    assert_eq!((a.inode, a.links), (b.inode, 2));

    // The structures the tree is built to produce, so the claim that each
    // form is read is a claim about this image.
    use fio_xfs::inode::Format;
    let sparse = vol.inode(vol.lookup("/data/sparse").await.unwrap()).await.unwrap();
    assert_eq!(sparse.format, Format::Btree, "sparse file in a B+tree fork");
    assert_eq!(vol.extents(&sparse, false).await.unwrap().len(), 300);
    let pre = vol.inode(vol.lookup("/data/prealloc").await.unwrap()).await.unwrap();
    assert!(vol.extents(&pre, false).await.unwrap().iter().any(|e| e.unwritten), "unwritten extents");
    let node = vol.inode(vol.lookup("/data/attrs-node").await.unwrap()).await.unwrap();
    assert!(matches!(node.aformat, Some(Format::Extents | Format::Btree)), "attributes in blocks");
    assert!(vol.extents(&node, true).await.unwrap().iter().map(|e| e.count).sum::<u64>() > 3, "attribute node");
    let empty = vol.inode(vol.lookup("/dirs/empty").await.unwrap()).await.unwrap();
    assert_eq!(empty.format, Format::Local);
    assert_eq!(vol.read_dir("/dirs/node").await.unwrap().len(), 20_000);

    // Paths through the kernel's symlinks.
    assert_eq!(vol.read("/links/rel").await.unwrap(), b"NAME=\"Rocky Linux\"\nVERSION_ID=\"9.6\"\n");
    assert_eq!(vol.read("/links/abs/owned").await.unwrap(), b"mine\n");

    // And the archive carries all of it.
    let tar = vol.pack_tar("/").await.unwrap();
    let r = vol.pack_tar_to(Vec::new(), "/").await.unwrap();
    assert_eq!(r.hard_links, 1);
    assert_eq!(r.sockets_skipped, 1);
    assert_eq!(r.devices, 4);
    assert_eq!(r.symlinks, 3);
    let acl_rec = b"SCHILY.xattr.system.posix_acl_access=";
    assert!(tar.windows(acl_rec.len()).any(|w| w == acl_rec), "ACL in the archive");
    if Command::new("tar").arg("--version").output().is_ok() {
        let p = image.with_extension("tar");
        std::fs::write(&p, &tar).unwrap();
        let o = Command::new("tar").arg("-tvf").arg(&p).output().unwrap();
        assert!(o.status.success(), "GNU tar: {}", String::from_utf8_lossy(&o.stderr));
        let listing = String::from_utf8_lossy(&o.stdout);
        assert!(listing.contains("links/hard-b link to links/hard-a"), "hard link in the archive");
        assert_eq!(listing.lines().count(), tree.len() - 1, "every name but the socket");
    }
    let _ = std::io::stdout().flush();
}

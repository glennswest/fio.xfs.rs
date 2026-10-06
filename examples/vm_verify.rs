//! The guest side of the kernel-verification VM (#12, `tests/vm/`).
//!
//! ```text
//! vm_verify write  IMAGE MANIFEST   # write the first tree with this crate
//! vm_verify check  IMAGE            # read back what the kernel changed
//! vm_verify write2 IMAGE MANIFEST   # write again, over the kernel's trees
//! ```
//!
//! Each write leaves a manifest, one line per name, tab-separated, for the
//! guest's init to check through the real kernel with busybox:
//!
//! ```text
//! F path mode uid gid size md5 nlink     regular file
//! D path mode uid gid entries             directory
//! L path target                           symlink
//! C path major minor / B path major minor device (hex, as stat %t %T)
//! P path                                  FIFO
//! G path                                  must not exist
//! ```

use std::fmt::Write as _;

use fio_xfs::{Attrs, Error, FileDevice, Special, Volume};

/// Directories of every form, and the names each starts with.
const DIRS: [(&str, usize); 4] = [("sf", 3), ("block", 40), ("leaf", 400), ("node", 5000)];
/// Files the kernel adds to each (`kernel-NNN`); it removes our `name-*0`.
const KERNEL_ADDS: usize = 30;

fn content(dir: &str, i: usize) -> Vec<u8> {
    format!("{dir}/{i}\n").repeat(i % 7).into_bytes()
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

fn long_target(len: usize) -> String {
    let mut t = String::from("../");
    while t.len() < len {
        t.push_str("a-directory-name/");
    }
    t.truncate(len);
    t
}

/// What was written, line by line.
#[derive(Default)]
struct Manifest(String);

impl Manifest {
    fn file(&mut self, path: &str, data: &[u8], mode: u16, uid: u32, gid: u32, nlink: u32) {
        let _ = writeln!(self.0, "F\t{path}\t{mode:o}\t{uid}\t{gid}\t{}\t{}\t{nlink}", data.len(), md5_hex(data));
    }
    fn dir(&mut self, path: &str, mode: u16, uid: u32, gid: u32, entries: usize) {
        let _ = writeln!(self.0, "D\t{path}\t{mode:o}\t{uid}\t{gid}\t{entries}");
    }
    fn link(&mut self, path: &str, target: &str) {
        let _ = writeln!(self.0, "L\t{path}\t{target}");
    }
    fn dev(&mut self, kind: char, path: &str, major: u32, minor: u32) {
        let _ = writeln!(self.0, "{kind}\t{path}\t{major:x}\t{minor:x}");
    }
    fn gone(&mut self, path: &str) {
        let _ = writeln!(self.0, "G\t{path}");
    }
}

async fn write(image: &str) -> Result<Manifest, Error> {
    let mut vol = Volume::open(FileDevice::open_rw(image).await?).await?;
    vol.set_time(1_700_000_000);
    let mut m = Manifest::default();
    vol.mkdir("/t").await?;

    for (name, data) in [
        ("hello", b"hello from fio-xfs\n".to_vec()),
        ("empty", Vec::new()),
        ("big", bytes(1, (3 << 20) + 123)),
        ("odd", bytes(3, 5000)),
    ] {
        vol.write(&format!("/t/{name}"), &data).await?;
        // `hello` gets a second name below, and its line with it.
        if name != "hello" {
            m.file(&format!("t/{name}"), &data, 0o644, 0, 0, 1);
        }
    }
    vol.write_with("/t/setuid", b"#!/bin/sh\n", &Attrs::mode(0o4755).owner(1000, 100)).await?;
    m.file("t/setuid", b"#!/bin/sh\n", 0o4755, 1000, 100, 1);
    // Replaced: longer, then shorter.
    vol.write("/t/replaced", &bytes(4, 70_000)).await?;
    vol.write("/t/replaced", &bytes(5, 9_000)).await?;
    m.file("t/replaced", &bytes(5, 9_000), 0o644, 0, 0, 1);

    for (dir, n) in DIRS {
        vol.mkdir(&format!("/t/{dir}")).await?;
        for i in 0..n {
            vol.write(&format!("/t/{dir}/name-{i:05}"), &content(dir, i)).await?;
        }
    }
    // A hard link into the short-form directory.
    vol.link("/t/hello", "/t/sf/hello-again").await?;
    m.file("t/hello", b"hello from fio-xfs\n", 0o644, 0, 0, 2);
    for (dir, n) in DIRS {
        let extra = usize::from(dir == "sf");
        m.dir(&format!("t/{dir}"), 0o755, 0, 0, n + extra);
        for i in 0..n {
            m.file(&format!("t/{dir}/name-{i:05}"), &content(dir, i), 0o644, 0, 0, 1);
        }
    }

    vol.mkdir_all_with("/t/deep/er/still", &Attrs::mode(0o700).owner(5, 6)).await?;
    m.dir("t/deep/er/still", 0o700, 5, 6, 0);

    vol.mkdir("/t/links").await?;
    for (name, target) in [("short", "../hello".to_string()), ("long", long_target(900)), ("longest", long_target(1023))] {
        vol.symlink(&format!("/t/links/{name}"), &target).await?;
        m.link(&format!("t/links/{name}"), &target);
    }

    vol.mkdir("/t/devs").await?;
    for (name, kind, c, maj, min) in [
        ("null", Special::CharDevice { major: 1, minor: 3 }, 'C', 1, 3),
        ("sda", Special::BlockDevice { major: 8, minor: 0 }, 'B', 8, 0),
        ("big", Special::CharDevice { major: 259, minor: 70000 }, 'C', 259, 70000),
    ] {
        vol.mknod(&format!("/t/devs/{name}"), kind, &Attrs::mode(0o600)).await?;
        m.dev(c, &format!("t/devs/{name}"), maj, min);
    }
    vol.mknod("/t/devs/fifo", Special::Fifo, &Attrs::mode(0o644)).await?;
    let _ = writeln!(m.0, "P\tt/devs/fifo");

    vol.mkdir("/t/gone").await?;
    vol.rmdir("/t/gone").await?;
    m.gone("t/gone");
    vol.write("/t/unlinked", b"x").await?;
    vol.unlink("/t/unlinked").await?;
    m.gone("t/unlinked");

    vol.flush().await?;
    Ok(m)
}

/// What the kernel wrote (`init.sh` writes it the same way).
fn kernel_content(dir: &str, i: usize) -> Vec<u8> {
    format!("kernel {dir} {i}\n").into_bytes()
}

async fn check(image: &str) -> Result<(), String> {
    let vol = Volume::open(FileDevice::open(image).await.map_err(|e| e.to_string())?)
        .await
        .map_err(|e| e.to_string())?;
    for (dir, n) in DIRS {
        for i in 0..KERNEL_ADDS {
            let p = format!("/t/{dir}/kernel-{i:03}");
            let got = vol.read(&p).await.map_err(|e| format!("{p}: {e}"))?;
            if got != kernel_content(dir, i) {
                return Err(format!("{p}: the kernel's contents read back differently"));
            }
        }
        for i in 0..n {
            let p = format!("/t/{dir}/name-{i:05}");
            let there = vol.exists(&p).await.map_err(|e| format!("{p}: {e}"))?;
            if there != (i % 10 != 0) {
                return Err(format!("{p}: exists = {there} after the kernel's removals"));
            }
        }
        let names = vol.read_dir(&format!("/t/{dir}")).await.map_err(|e| e.to_string())?.len();
        let want = n - n.div_ceil(10) + KERNEL_ADDS + usize::from(dir == "sf");
        if names != want {
            return Err(format!("t/{dir}: {names} names, want {want}"));
        }
    }
    let busybox = std::fs::read("/bin/busybox").map_err(|e| e.to_string())?;
    if vol.read("/t/kernel-busybox").await.map_err(|e| e.to_string())? != busybox {
        return Err("t/kernel-busybox differs from /bin/busybox".into());
    }
    Ok(())
}

async fn write2(image: &str) -> Result<Manifest, Error> {
    let mut vol = Volume::open(FileDevice::open_rw(image).await?).await?;
    let mut m = Manifest::default();
    for (dir, _) in DIRS {
        for i in 0..100 {
            let p = format!("t/{dir}/r2-{i:03}");
            let data = bytes(i as u64, i * 37);
            vol.write(&format!("/{p}"), &data).await?;
            m.file(&p, &data, 0o644, 0, 0, 1);
        }
        for i in 0..10 {
            let p = format!("t/{dir}/kernel-{i:03}");
            vol.unlink(&format!("/{p}")).await?;
            m.gone(&p);
        }
    }
    let big = bytes(42, 5 << 20);
    vol.write("/t/kernel-busybox", &big).await?;
    m.file("t/kernel-busybox", &big, 0o755, 0, 0, 1);
    vol.mkdir("/t/r2dir").await?;
    for i in 0..2000 {
        vol.write(&format!("/t/r2dir/{i:05}-a-longer-name-for-a-node-directory"), b"").await?;
    }
    m.dir("t/r2dir", 0o755, 0, 0, 2000);
    vol.flush().await?;
    Ok(m)
}

fn main() {
    // MD5 checks itself before anything relies on it (RFC 1321's tests).
    assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(
        md5_hex(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"),
        "57edf4a22be3c955ac49da2e2107b67a"
    );

    let args: Vec<String> = std::env::args().collect();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let result = match (args.get(1).map(String::as_str), args.get(2), args.get(3)) {
        (Some("write"), Some(img), Some(out)) => {
            rt.block_on(write(img)).map_err(|e| e.to_string()).and_then(|m| std::fs::write(out, m.0).map_err(|e| e.to_string()))
        }
        (Some("write2"), Some(img), Some(out)) => {
            rt.block_on(write2(img)).map_err(|e| e.to_string()).and_then(|m| std::fs::write(out, m.0).map_err(|e| e.to_string()))
        }
        (Some("check"), Some(img), None) => rt.block_on(check(img)),
        _ => Err("usage: vm_verify write|write2 IMAGE MANIFEST | vm_verify check IMAGE".into()),
    };
    if let Err(e) = result {
        eprintln!("vm_verify: {e}");
        std::process::exit(1);
    }
}

/// MD5 (RFC 1321), for manifests busybox's `md5sum` can check.
fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4,
        11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32).collect();
    let mut h: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_le_bytes());
    for chunk in msg.chunks(64) {
        let w: Vec<u32> = (0..16).map(|i| u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap())).collect();
        let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let t = d;
            d = c;
            c = b;
            b = b.wrapping_add(a.wrapping_add(f).wrapping_add(k[i]).wrapping_add(w[g]).rotate_left(S[i]));
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
    }
    h.iter().flat_map(|x| x.to_le_bytes()).map(|b| format!("{b:02x}")).collect()
}

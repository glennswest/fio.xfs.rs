//! Writing tar archives: POSIX ustar with PAX extended headers for anything
//! ustar cannot hold — long names, large sizes and IDs, sub-second times and
//! extended attributes (as `SCHILY.xattr.*`, which GNU tar and every
//! container runtime read).

use crate::error::Result;
use crate::inode::Timestamp;

/// A byte stream an archive can be written to.
#[allow(async_fn_in_trait)]
pub trait Sink {
    /// Write all of `buf`.
    async fn write_all(&mut self, buf: &[u8]) -> Result<()>;

    /// Flush anything buffered. Called once, when the archive is complete.
    async fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

impl Sink for Vec<u8> {
    async fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.extend_from_slice(buf);
        Ok(())
    }
}

/// A [`Sink`] over any tokio writer — a file, a socket, stdout.
pub struct Io<T> {
    inner: T,
}

impl<T> Io<T> {
    /// Wrap a writer.
    pub fn new(inner: T) -> Self {
        Io { inner }
    }

    /// The writer back.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> Sink for Io<T> {
    async fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        Ok(self.inner.write_all(buf).await?)
    }
    async fn flush(&mut self) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        Ok(self.inner.flush().await?)
    }
}

/// What an archive entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EntryKind {
    /// Regular file, followed by its contents.
    #[default]
    File,
    /// A second name for a file already in the archive.
    HardLink,
    /// Symbolic link.
    Symlink,
    /// Character device.
    CharDevice,
    /// Block device.
    BlockDevice,
    /// Directory.
    Directory,
    /// FIFO.
    Fifo,
}

impl EntryKind {
    fn flag(self) -> u8 {
        match self {
            EntryKind::File => b'0',
            EntryKind::HardLink => b'1',
            EntryKind::Symlink => b'2',
            EntryKind::CharDevice => b'3',
            EntryKind::BlockDevice => b'4',
            EntryKind::Directory => b'5',
            EntryKind::Fifo => b'6',
        }
    }
}

/// One entry's metadata.
#[derive(Debug, Clone, Default)]
pub struct Header {
    /// Path within the archive, relative. A directory's gets a trailing `/`.
    pub path: Vec<u8>,
    /// What it is.
    pub kind: EntryKind,
    /// Link target, for symlinks and hard links.
    pub link: Vec<u8>,
    /// Permission bits (`0o7777`).
    pub mode: u32,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Modification time.
    pub mtime: Timestamp,
    /// Bytes of content that follow; regular files only.
    pub size: u64,
    /// Device major number.
    pub major: u32,
    /// Device minor number.
    pub minor: u32,
    /// Extended attributes.
    pub xattrs: Vec<(String, Vec<u8>)>,
}

/// Writes entries to a [`Sink`].
pub struct Writer<K> {
    sink: K,
    /// Content bytes still owed for the current entry.
    owed: u64,
    /// Padding owed after them.
    pad: usize,
}

const BLOCK: usize = 512;

fn octal(field: &mut [u8], v: u64) -> bool {
    let digits = field.len() - 1;
    if digits < 22 && v >= 1u64 << (3 * digits) {
        return false;
    }
    let s = format!("{v:0digits$o}");
    field[..digits].copy_from_slice(s.as_bytes());
    field[digits] = 0;
    true
}

fn pax_record(out: &mut Vec<u8>, key: &str, value: &[u8]) {
    // "<len> <key>=<value>\n", where <len> counts itself.
    let body = key.len() + value.len() + 3;
    let mut len = body + 1;
    while len != body + len.to_string().len() {
        len = body + len.to_string().len();
    }
    out.extend_from_slice(format!("{len} {key}=").as_bytes());
    out.extend_from_slice(value);
    out.push(b'\n');
}

fn pax_time(t: Timestamp) -> String {
    if t.nsecs == 0 {
        t.secs.to_string()
    } else if t.secs >= 0 {
        format!("{}.{:09}", t.secs, t.nsecs)
    } else {
        format!("-{}.{:09}", -(t.secs + 1), 1_000_000_000 - t.nsecs)
    }
}

fn ustar(path: &[u8], kind: u8, h: &Header, size: u64) -> ([u8; BLOCK], bool) {
    let mut b = [0u8; BLOCK];
    let mut fits = true;
    let n = path.len().min(100);
    b[..n].copy_from_slice(&path[..n]);
    fits &= path.len() <= 100;
    fits &= octal(&mut b[100..108], h.mode as u64 & 0o7777);
    fits &= octal(&mut b[108..116], h.uid as u64);
    fits &= octal(&mut b[116..124], h.gid as u64);
    fits &= octal(&mut b[124..136], size);
    fits &= h.mtime.secs >= 0 && octal(&mut b[136..148], h.mtime.secs.max(0) as u64);
    b[156] = kind;
    let n = h.link.len().min(100);
    b[157..157 + n].copy_from_slice(&h.link[..n]);
    fits &= h.link.len() <= 100;
    b[257..263].copy_from_slice(b"ustar\0");
    b[263..265].copy_from_slice(b"00");
    if matches!(h.kind, EntryKind::CharDevice | EntryKind::BlockDevice) {
        fits &= octal(&mut b[329..337], h.major as u64);
        fits &= octal(&mut b[337..345], h.minor as u64);
    }
    b[148..156].fill(b' ');
    let sum: u32 = b.iter().map(|&x| x as u32).sum();
    b[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
    (b, fits)
}

impl<K: Sink> Writer<K> {
    /// Start an archive.
    pub fn new(sink: K) -> Self {
        Writer { sink, owed: 0, pad: 0 }
    }

    /// Begin an entry. A regular file's `size` bytes of content must follow
    /// through [`Writer::data`] before the next entry.
    pub async fn begin(&mut self, h: &Header) -> Result<()> {
        assert_eq!(self.owed, 0, "the previous entry's content is incomplete");
        self.finish_entry().await?;
        let size = if h.kind == EntryKind::File { h.size } else { 0 };
        let (block, fits) = ustar(&h.path, h.kind.flag(), h, size);

        let mut pax = Vec::new();
        if !fits || h.mtime.nsecs != 0 {
            if h.path.len() > 100 {
                pax_record(&mut pax, "path", &h.path);
            }
            if h.link.len() > 100 {
                pax_record(&mut pax, "linkpath", &h.link);
            }
            pax_record(&mut pax, "size", size.to_string().as_bytes());
            pax_record(&mut pax, "uid", h.uid.to_string().as_bytes());
            pax_record(&mut pax, "gid", h.gid.to_string().as_bytes());
            pax_record(&mut pax, "mtime", pax_time(h.mtime).as_bytes());
        }
        for (name, value) in &h.xattrs {
            pax_record(&mut pax, &format!("SCHILY.xattr.{name}"), value);
        }
        if !pax.is_empty() {
            let mut name = b"PaxHeaders/".to_vec();
            let base = h.path.rsplit(|&c| c == b'/').find(|c| !c.is_empty()).unwrap_or(b"x");
            name.extend_from_slice(&base[..base.len().min(80)]);
            let ph = Header { mode: 0o644, mtime: Timestamp { secs: h.mtime.secs.max(0), nsecs: 0 }, ..Default::default() };
            let (xb, _) = ustar(&name, b'x', &ph, pax.len() as u64);
            self.sink.write_all(&xb).await?;
            self.sink.write_all(&pax).await?;
            self.sink.write_all(&[0u8; BLOCK][..(BLOCK - pax.len() % BLOCK) % BLOCK]).await?;
        }
        self.sink.write_all(&block).await?;
        self.owed = size;
        self.pad = ((BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64) as usize;
        Ok(())
    }

    /// Content of the current entry.
    pub async fn data(&mut self, buf: &[u8]) -> Result<()> {
        assert!(buf.len() as u64 <= self.owed, "more content than the header's size");
        self.owed -= buf.len() as u64;
        self.sink.write_all(buf).await
    }

    async fn finish_entry(&mut self) -> Result<()> {
        if self.pad > 0 {
            let pad = std::mem::take(&mut self.pad);
            self.sink.write_all(&[0u8; BLOCK][..pad]).await?;
        }
        Ok(())
    }

    /// Write an entry and all of its content at once.
    pub async fn append(&mut self, h: &Header, data: &[u8]) -> Result<()> {
        self.begin(h).await?;
        self.data(data).await
    }

    /// End the archive with its two zero blocks, and flush.
    pub async fn finish(&mut self) -> Result<()> {
        assert_eq!(self.owed, 0, "the last entry's content is incomplete");
        self.finish_entry().await?;
        self.sink.write_all(&[0u8; 2 * BLOCK]).await?;
        self.sink.flush().await
    }

    /// The sink back.
    pub fn into_inner(self) -> K {
        self.sink
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pax_record_counts_its_own_length() {
        let mut v = Vec::new();
        pax_record(&mut v, "path", b"a");
        assert_eq!(v, b"9 path=a\n");
        let mut v = Vec::new();
        pax_record(&mut v, "path", &[b'x'; 95]);
        assert_eq!(v.len(), 104);
        assert!(v.starts_with(b"104 path="));
    }

    #[test]
    fn negative_fractional_time() {
        assert_eq!(pax_time(Timestamp { secs: -2, nsecs: 500_000_000 }), "-1.500000000");
        assert_eq!(pax_time(Timestamp { secs: 5, nsecs: 1 }), "5.000000001");
    }

    #[tokio::test]
    async fn archive_is_whole_blocks() {
        let mut w = Writer::new(Vec::new());
        let h = Header { path: b"a".to_vec(), size: 3, mode: 0o644, ..Default::default() };
        w.append(&h, b"abc").await.unwrap();
        w.finish().await.unwrap();
        let v = w.into_inner();
        assert_eq!(v.len(), 4 * BLOCK);
        assert_eq!(&v[BLOCK..BLOCK + 3], b"abc");
    }
}

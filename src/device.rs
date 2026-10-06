//! The device seam: where the bytes of a filesystem come from.

use async_trait::async_trait;
use std::io;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::error::Result;

/// Something a filesystem can be read from — an image file, a volume, memory
/// — and, for the write side, written to.
///
/// Reading is all the read side asks for. Every read it issues is whole
/// filesystem blocks at a block boundary, except in
/// [`crate::Volume::open`], before the block size is known: 512 bytes at
/// offset 0, then (for a larger sector size) the superblock's whole sector.
///
/// Writing ([`crate::Volume::write`] and the rest) needs [`write_at`] as
/// well. Writes are whole filesystem blocks at a block boundary, except the
/// superblock and AG headers, which are written a sector at a time. A device
/// that does not override it is read-only, and every write to it fails with
/// [`Error::Unsupported`](crate::Error::Unsupported).
///
/// [`write_at`]: BlockDevice::write_at
#[async_trait]
pub trait BlockDevice: Send + Sync {
    /// Total addressable size in bytes.
    fn size(&self) -> u64;

    /// Read exactly `buf.len()` bytes starting at `offset`.
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Write all of `buf` starting at `offset`.
    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let _ = (offset, buf);
        Err(crate::Error::Unsupported("the device is read-only".into()))
    }

    /// Make everything written so far durable.
    async fn flush(&self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl<T: BlockDevice + ?Sized> BlockDevice for Box<T> {
    fn size(&self) -> u64 {
        (**self).size()
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf).await
    }
    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        (**self).write_at(offset, buf).await
    }
    async fn flush(&self) -> Result<()> {
        (**self).flush().await
    }
}

#[async_trait]
impl<T: BlockDevice + ?Sized> BlockDevice for std::sync::Arc<T> {
    fn size(&self) -> u64 {
        (**self).size()
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf).await
    }
    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        (**self).write_at(offset, buf).await
    }
    async fn flush(&self) -> Result<()> {
        (**self).flush().await
    }
}

fn check_range(size: u64, offset: u64, len: usize) -> Result<()> {
    check_range_for("read", size, offset, len)
}

fn check_range_for(what: &str, size: u64, offset: u64, len: usize) -> Result<()> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= size => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("{what} of {len} bytes at {offset} is past the end of a {size}-byte device"),
        )
        .into()),
    }
}

/// A filesystem image in memory, readable and writable.
pub struct MemDevice {
    data: std::sync::Mutex<Vec<u8>>,
    size: u64,
}

impl MemDevice {
    /// Wrap the bytes of an image.
    pub fn new(data: Vec<u8>) -> Self {
        let size = data.len() as u64;
        MemDevice { data: std::sync::Mutex::new(data), size }
    }

    /// The image's bytes.
    pub fn into_inner(self) -> Vec<u8> {
        self.data.into_inner().unwrap_or_else(|e| e.into_inner())
    }

    fn bytes(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl BlockDevice for MemDevice {
    fn size(&self) -> u64 {
        self.size
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(self.size, offset, buf.len())?;
        let at = offset as usize;
        buf.copy_from_slice(&self.bytes()[at..at + buf.len()]);
        Ok(())
    }
    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        check_range_for("write", self.size, offset, buf.len())?;
        let at = offset as usize;
        self.bytes()[at..at + buf.len()].copy_from_slice(buf);
        Ok(())
    }
}

/// A filesystem image in a file, or a block device opened as one.
pub struct FileDevice {
    file: tokio::sync::Mutex<tokio::fs::File>,
    size: u64,
    path: String,
    writable: bool,
}

impl FileDevice {
    /// Open an image read-only.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path.as_ref(), false).await
    }

    /// Open an image for reading and writing.
    pub async fn open_rw(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path.as_ref(), true).await
    }

    async fn open_with(path: &Path, writable: bool) -> Result<Self> {
        let mut file = tokio::fs::OpenOptions::new().read(true).write(writable).open(path).await?;
        // A block device reports a zero length in its metadata; seeking to the
        // end works for both.
        let size = file.seek(io::SeekFrom::End(0)).await?;
        Ok(FileDevice {
            file: tokio::sync::Mutex::new(file),
            size,
            path: path.display().to_string(),
            writable,
        })
    }
}

#[async_trait]
impl BlockDevice for FileDevice {
    fn size(&self) -> u64 {
        self.size
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(self.size, offset, buf.len()).map_err(|e| {
            crate::Error::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{}: {e}", self.path),
            ))
        })?;
        let mut file = self.file.lock().await;
        file.seek(io::SeekFrom::Start(offset)).await?;
        file.read_exact(buf).await?;
        Ok(())
    }
    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if !self.writable {
            return Err(crate::Error::Unsupported(format!("{}: opened read-only", self.path)));
        }
        check_range_for("write", self.size, offset, buf.len()).map_err(|e| {
            crate::Error::Io(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{}: {e}", self.path)))
        })?;
        let mut file = self.file.lock().await;
        file.seek(io::SeekFrom::Start(offset)).await?;
        file.write_all(buf).await?;
        Ok(())
    }
    async fn flush(&self) -> Result<()> {
        if self.writable {
            let mut file = self.file.lock().await;
            file.flush().await?;
            file.sync_data().await?;
        }
        Ok(())
    }
}

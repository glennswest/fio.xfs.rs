//! The device seam: where the bytes of a filesystem come from.

use async_trait::async_trait;
use std::io;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::error::Result;

/// Something a filesystem can be read from — an image file, a volume, memory.
///
/// Reading is all this crate asks for. Every read it issues is whole
/// filesystem blocks at a block boundary, except the first read of
/// [`crate::Volume::open`], which is the first 4 KiB of the device before the
/// block size is known.
#[async_trait]
pub trait BlockDevice: Send + Sync {
    /// Total addressable size in bytes.
    fn size(&self) -> u64;

    /// Read exactly `buf.len()` bytes starting at `offset`.
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;
}

#[async_trait]
impl<T: BlockDevice + ?Sized> BlockDevice for Box<T> {
    fn size(&self) -> u64 {
        (**self).size()
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf).await
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
}

fn check_range(size: u64, offset: u64, len: usize) -> Result<()> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= size => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("read of {len} bytes at {offset} is past the end of a {size}-byte device"),
        )
        .into()),
    }
}

/// A filesystem image in memory.
pub struct MemDevice {
    data: Vec<u8>,
}

impl MemDevice {
    /// Wrap the bytes of an image.
    pub fn new(data: Vec<u8>) -> Self {
        MemDevice { data }
    }

    /// The image's bytes.
    pub fn into_inner(self) -> Vec<u8> {
        self.data
    }
}

#[async_trait]
impl BlockDevice for MemDevice {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(self.size(), offset, buf.len())?;
        let at = offset as usize;
        buf.copy_from_slice(&self.data[at..at + buf.len()]);
        Ok(())
    }
}

/// A filesystem image in a file, or a block device opened as one.
pub struct FileDevice {
    file: tokio::sync::Mutex<tokio::fs::File>,
    size: u64,
    path: String,
}

impl FileDevice {
    /// Open an image read-only.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut file = tokio::fs::File::open(path).await?;
        // A block device reports a zero length in its metadata; seeking to the
        // end works for both.
        let size = file.seek(io::SeekFrom::End(0)).await?;
        Ok(FileDevice {
            file: tokio::sync::Mutex::new(file),
            size,
            path: path.display().to_string(),
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
}

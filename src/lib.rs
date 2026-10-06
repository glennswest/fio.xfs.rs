//! Async userspace file I/O into **XFS**.
//!
//! Read and write the files inside an XFS filesystem image or volume with
//! no kernel, no mount and no loop device — so it works unprivileged, in a
//! container, on a Mac, and against storage that is not a block device at
//! all.
//!
//! ```no_run
//! use fio_xfs::{FileDevice, Volume};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let vol = Volume::open(FileDevice::open("rocky9.img").await?).await?;
//!
//! let release = vol.read("/etc/os-release").await?;
//! for entry in vol.read_dir("/etc").await? {
//!     println!("{}", entry.name);
//! }
//!
//! // The whole tree, as a tar stream.
//! let archive = vol.pack_tar("/").await?;
//! # Ok(())
//! # }
//! ```
//!
//! # What it reads
//!
//! Version 4 and version 5 (CRC) filesystems, with every v5 metadata
//! checksum checked: inodes of every version, with local, extent-list and
//! B+tree forks; directories in all four forms (short form, block, leaf and
//! node); inline and remote symlinks; extended attributes in short form,
//! leaf and node blocks, with remote values, and XFS's ACLs presented as
//! Linux presents them (`system.posix_acl_*`); bigtime timestamps and
//! 64-bit extent counters.
//!
//! What it does not: files on a realtime device, and replaying the log.
//! [`Volume::open`] looks at the log and refuses a filesystem that was not
//! cleanly unmounted ([`Error::DirtyLog`]): what is on disk may be stale.
//! [`Volume::open_norecovery`] reads one anyway, as it stands on disk, and
//! [`Volume::log_state`] says how the log was found.
//!
//! # Writing
//!
//! Version 5 filesystems can be written: [`Volume::write`],
//! [`Volume::mkdir_all`], [`Volume::symlink`], [`Volume::mknod`],
//! [`Volume::link`], [`Volume::unlink`], [`Volume::rmdir`],
//! [`Volume::chmod`], [`Volume::chown`] — then [`Volume::flush`], before
//! which nothing on disk is consistent. See [`write`] for how.
//!
//! ```no_run
//! use fio_xfs::{FileDevice, Volume};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut vol = Volume::open(FileDevice::open_rw("template.img").await?).await?;
//! vol.mkdir_all("/etc/stormblock").await?;
//! vol.write("/etc/stormblock/boot.toml", b"[boot]\n").await?;
//! vol.flush().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Getting a tree out
//!
//! [`Volume::walk`] lists a tree; [`Volume::pack_tar_to`] streams it as a
//! tar archive with everything intact (the import path);
//! [`Volume::extract`] copies it into a local directory.
//!
//! The sibling [`fio-ext4`](https://github.com/glennswest/fio.ext4.rs) does
//! the same for ext2/3/4, and its API is the model for this one.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod bytes;
mod alloc;
pub mod attr;
pub mod bmap;
mod btree;
pub mod crc;
pub mod device;
pub mod dir;
mod dirwrite;
pub mod error;
pub mod export;
pub mod inode;
pub mod log;
pub mod sb;
pub mod tar;
pub mod volume;
pub mod write;

pub use attr::Xattr;
pub use device::{BlockDevice, FileDevice, MemDevice};
pub use dir::FileType;
pub use error::{Error, Result};
pub use export::{ExtractOptions, ExtractReport, PackReport};
pub use inode::{Inode, Timestamp};
pub use log::LogState;
pub use sb::Superblock;
pub use volume::{Entry, Stat, Volume, WalkEntry};
pub use write::{Attrs, Special};

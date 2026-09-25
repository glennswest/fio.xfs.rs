//! Extended attributes: short form in the inode, or leaf and node blocks in
//! the attribute fork, with large values in blocks of their own.

use crate::bytes::{be16, be32, be64};
use crate::crc;
use crate::error::{corrupt, Result};
use crate::sb::Superblock;

/// One extended attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xattr {
    /// The full name, such as `security.selinux`.
    pub name: String,
    /// The value, which is bytes rather than text.
    pub value: Vec<u8>,
}

// Namespace and state bits of an attribute entry.
const LOCAL: u8 = 0x01;
const ROOT: u8 = 0x02;
const SECURE: u8 = 0x04;
const PARENT: u8 = 0x08;
const INCOMPLETE: u8 = 0x80;

const LEAF_MAGIC_V4: u16 = 0xfbee;
const LEAF_MAGIC_V5: u16 = 0x3bee;
const NODE_MAGIC_V4: u16 = 0xfebe;
const NODE_MAGIC_V5: u16 = 0x3ebe;
const RMT_MAGIC: u32 = 0x5841_524d;

/// Header length of a v5 remote value block.
pub const RMT_HDR: usize = 56;

/// An attribute's name and where its value is.
pub enum Value {
    /// The value, stored with the name.
    Inline(Vec<u8>),
    /// Stored in blocks of its own in the attribute fork.
    Remote {
        /// The first logical block.
        blk: u32,
        /// Its length in bytes.
        len: u32,
    },
}

/// A name as found, before its value is fetched.
pub struct Found {
    /// The full name.
    pub name: String,
    /// The value, or where it is.
    pub value: Value,
}

/// The name a namespace bit gives an on-disk attribute name, or `None` for
/// entries that are not user-visible attributes at all.
fn full_name(flags: u8, name: &[u8]) -> Option<String> {
    if flags & (INCOMPLETE | PARENT) != 0 {
        return None;
    }
    let name = String::from_utf8_lossy(name);
    Some(if flags & SECURE != 0 {
        format!("security.{name}")
    } else if flags & ROOT != 0 {
        match &*name {
            // Linux shows XFS's own ACL attributes as the POSIX ones; the
            // value is converted to match by `posix_acl`.
            "SGI_ACL_FILE" => "system.posix_acl_access".into(),
            "SGI_ACL_DEFAULT" => "system.posix_acl_default".into(),
            _ => format!("trusted.{name}"),
        }
    } else {
        format!("user.{name}")
    })
}

/// Rewrite an XFS on-disk ACL into the `system.posix_acl_*` value Linux
/// reports for it.
pub fn posix_acl(xfs: &[u8]) -> Result<Vec<u8>> {
    if xfs.len() < 4 {
        return Err(corrupt("ACL shorter than its count"));
    }
    let count = be32(xfs, 0) as usize;
    if xfs.len() < 4 + count * 12 {
        return Err(corrupt("ACL shorter than its entries"));
    }
    let mut out = Vec::with_capacity(4 + count * 8);
    out.extend_from_slice(&2u32.to_le_bytes());
    for i in 0..count {
        let e = &xfs[4 + i * 12..];
        let tag = be32(e, 0);
        let id = be32(e, 4);
        let perm = be16(e, 8);
        // Only named users and groups carry an ID; the rest read as -1.
        let id = if tag == 0x02 || tag == 0x08 { id } else { u32::MAX };
        out.extend_from_slice(&(tag as u16).to_le_bytes());
        out.extend_from_slice(&perm.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
    }
    Ok(out)
}

/// Parse a short-form attribute fork.
pub fn parse_shortform(fork: &[u8]) -> Result<Vec<Found>> {
    if fork.len() < 4 {
        return Err(corrupt("short-form attributes: no header"));
    }
    let total = (be16(fork, 0) as usize).min(fork.len());
    let count = fork[2] as usize;
    let mut p = 4;
    let mut out = Vec::new();
    for _ in 0..count {
        if p + 3 > total {
            return Err(corrupt("short-form attribute runs off the fork"));
        }
        let namelen = fork[p] as usize;
        let valuelen = fork[p + 1] as usize;
        let flags = fork[p + 2];
        let name_at = p + 3;
        let end = name_at + namelen + valuelen;
        if end > total {
            return Err(corrupt("short-form attribute runs off the fork"));
        }
        if let Some(name) = full_name(flags, &fork[name_at..name_at + namelen]) {
            out.push(Found { name, value: Value::Inline(fork[name_at + namelen..end].to_vec()) });
        }
        p = end;
    }
    Ok(out)
}

/// What an attribute fork block turned out to be.
pub enum Block {
    /// A leaf.
    Leaf {
        /// Its names.
        found: Vec<Found>,
        /// The next leaf's block; 0 for none.
        forw: u32,
    },
    /// An interior node.
    Node {
        /// Its leftmost child's block.
        first_child: u32,
    },
}

/// Parse one attribute fork block (`fsb` is for messages).
pub fn parse_block(sb: &Superblock, buf: &[u8], ino: u64, fsb: u64) -> Result<Block> {
    let magic = be16(buf, 8);
    let v5 = sb.is_v5();
    let (leaf, node) = if v5 {
        (LEAF_MAGIC_V5, NODE_MAGIC_V5)
    } else {
        (LEAF_MAGIC_V4, NODE_MAGIC_V4)
    };
    if magic != leaf && magic != node {
        return Err(corrupt(format!("inode {ino}: attribute block {fsb} magic {magic:#x}")));
    }
    if v5 {
        if !crc::verify(buf, 12) {
            return Err(corrupt(format!("inode {ino}: attribute block {fsb} checksum")));
        }
        if be64(buf, 48) != ino {
            return Err(corrupt(format!("inode {ino}: attribute block {fsb} owned by {}", be64(buf, 48))));
        }
    }
    if magic == node {
        let (count_at, entries) = if v5 { (56, 64) } else { (12, 16) };
        if be16(buf, count_at) == 0 {
            return Err(corrupt(format!("inode {ino}: empty attribute node {fsb}")));
        }
        return Ok(Block::Node { first_child: be32(buf, entries + 4) });
    }

    let (count_at, entries) = if v5 { (56, 80) } else { (12, 32) };
    let count = be16(buf, count_at) as usize;
    if entries + count * 8 > buf.len() {
        return Err(corrupt(format!("inode {ino}: attribute leaf {fsb} count {count}")));
    }
    let mut found = Vec::with_capacity(count);
    for i in 0..count {
        let e = entries + i * 8;
        let nameidx = be16(buf, e + 4) as usize;
        let flags = buf[e + 6];
        let bad = || corrupt(format!("inode {ino}: attribute leaf {fsb} entry {i}"));
        if flags & LOCAL != 0 {
            // valuelen u16, namelen u8, name, value.
            if nameidx + 3 > buf.len() {
                return Err(bad());
            }
            let valuelen = be16(buf, nameidx) as usize;
            let namelen = buf[nameidx + 2] as usize;
            let name_at = nameidx + 3;
            if name_at + namelen + valuelen > buf.len() {
                return Err(bad());
            }
            if let Some(name) = full_name(flags, &buf[name_at..name_at + namelen]) {
                let value = buf[name_at + namelen..name_at + namelen + valuelen].to_vec();
                found.push(Found { name, value: Value::Inline(value) });
            }
        } else {
            // valueblk u32, valuelen u32, namelen u8, name.
            if nameidx + 9 > buf.len() {
                return Err(bad());
            }
            let blk = be32(buf, nameidx);
            let len = be32(buf, nameidx + 4);
            let namelen = buf[nameidx + 8] as usize;
            let name_at = nameidx + 9;
            if name_at + namelen > buf.len() {
                return Err(bad());
            }
            if let Some(name) = full_name(flags, &buf[name_at..name_at + namelen]) {
                found.push(Found { name, value: Value::Remote { blk, len } });
            }
        }
    }
    Ok(Block::Leaf { found, forw: be32(buf, 0) })
}

/// Blocks a remote value of `len` bytes occupies.
pub fn remote_blocks(sb: &Superblock, len: u32) -> u64 {
    let per = if sb.is_v5() { sb.block_size as u64 - RMT_HDR as u64 } else { sb.block_size as u64 };
    (len as u64).div_ceil(per)
}

/// The value bytes of one remote value block, checked.
pub fn remote_payload<'a>(sb: &Superblock, buf: &'a [u8], ino: u64, want: usize) -> Result<&'a [u8]> {
    if !sb.is_v5() {
        return Ok(&buf[..want.min(buf.len())]);
    }
    if be32(buf, 0) != RMT_MAGIC || !crc::verify(buf, 12) || be64(buf, 32) != ino {
        return Err(corrupt(format!("inode {ino}: remote attribute value block")));
    }
    let bytes = be32(buf, 8) as usize;
    if RMT_HDR + bytes > buf.len() {
        return Err(corrupt(format!("inode {ino}: remote attribute value of {bytes} bytes")));
    }
    Ok(&buf[RMT_HDR..RMT_HDR + bytes])
}

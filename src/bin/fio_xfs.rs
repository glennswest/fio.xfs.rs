//! `fio-xfs` — look inside an XFS image, and get files out of it, with no
//! mount.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fio_xfs::tar::Io;
use fio_xfs::{ExtractOptions, FileDevice, FileType, Volume};
use tokio::io::AsyncWriteExt;

#[derive(Parser)]
#[command(name = "fio-xfs", version, about = "Read an XFS filesystem image without mounting it")]
struct Cli {
    /// The image or block device.
    image: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the superblock's geometry and features.
    Info,
    /// List a directory.
    Ls {
        #[arg(default_value = "/")]
        path: String,
    },
    /// Show a path's inode.
    Stat { path: String },
    /// Write a file's contents to stdout.
    Cat { path: String },
    /// Print a symlink's target.
    Readlink { path: String },
    /// List a path's extended attributes.
    Xattrs { path: String },
    /// List every name under a directory.
    Tree {
        #[arg(default_value = "/")]
        path: String,
    },
    /// Copy a tree into a local directory.
    Extract {
        /// Where to write; created if missing, and must be empty.
        dest: PathBuf,
        /// The directory in the image to copy.
        #[arg(long, default_value = "/")]
        root: String,
        /// Set owners and groups (needs root).
        #[arg(long)]
        owner: bool,
    },
    /// Write a tree as a tar archive.
    Tar {
        /// Write here rather than to stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// The directory in the image to archive.
        #[arg(long, default_value = "/")]
        root: String,
    },
}

fn kind_char(k: FileType) -> char {
    match k {
        FileType::Directory => 'd',
        FileType::Symlink => 'l',
        FileType::CharDevice => 'c',
        FileType::BlockDevice => 'b',
        FileType::Fifo => 'p',
        FileType::Socket => 's',
        FileType::File => '-',
        FileType::Unknown => '?',
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let dev = FileDevice::open(&cli.image).await.with_context(|| format!("opening {}", cli.image.display()))?;
    let vol = Volume::open(dev).await?;
    let mut out = tokio::io::stdout();
    match cli.cmd {
        Cmd::Info => {
            let sb = vol.superblock();
            let uuid: String = sb.uuid.iter().map(|b| format!("{b:02x}")).collect();
            println!("version        {}", sb.version);
            println!("uuid           {uuid}");
            println!("label          {}", sb.label);
            println!("block size     {}", sb.block_size);
            println!("blocks         {}", sb.data_blocks);
            println!("free blocks    {}", sb.free_blocks);
            println!("AGs            {} x {} blocks", sb.ag_count, sb.ag_blocks);
            println!("sector size    {}", sb.sector_size);
            println!("inode size     {}", sb.inode_size);
            println!("inodes         {} ({} free)", sb.icount, sb.ifree);
            println!("dir block size {}", sb.dir_block_size());
            println!("root inode     {}", sb.root_ino);
            println!("ftype          {}", sb.has_ftype());
            println!("incompat       {:#x}", sb.features_incompat);
            println!("ro_compat      {:#x}", sb.features_ro_compat);
        }
        Cmd::Ls { path } => {
            let mut entries = vol.read_dir(&path).await?;
            entries.sort_by(|a, b| a.raw_name.cmp(&b.raw_name));
            for e in entries {
                println!("{} {:>12} {}", kind_char(e.kind), e.inode, e.name);
            }
        }
        Cmd::Stat { path } => {
            let st = vol.stat(&path).await?;
            println!("inode   {}", st.inode);
            println!("type    {:?}", st.kind());
            println!("mode    {:o}", st.mode & 0o7777);
            println!("links   {}", st.links);
            println!("uid/gid {}/{}", st.uid, st.gid);
            println!("size    {}", st.size);
            println!("blocks  {}", st.blocks);
            println!("mtime   {}.{:09}", st.mtime.secs, st.mtime.nsecs);
            if st.rdev != (0, 0) {
                println!("rdev    {},{}", st.rdev.0, st.rdev.1);
            }
        }
        Cmd::Cat { path } => {
            out.write_all(&vol.read(&path).await?).await?;
            out.flush().await?;
        }
        Cmd::Readlink { path } => println!("{}", vol.read_link(&path).await?),
        Cmd::Xattrs { path } => {
            for x in vol.list_xattrs(&path).await? {
                let text = x.value.strip_suffix(&[0]).unwrap_or(&x.value);
                match std::str::from_utf8(text) {
                    Ok(s) if !s.chars().any(char::is_control) => println!("{}=\"{s}\"", x.name),
                    _ => {
                        let hex: String = x.value.iter().map(|b| format!("{b:02x}")).collect();
                        println!("{}=0x{hex}", x.name);
                    }
                }
            }
        }
        Cmd::Tree { path } => {
            for e in vol.walk(&path).await? {
                println!("{} {:o} {:>10} {}", kind_char(e.stat.kind()), e.stat.mode & 0o7777, e.stat.size, e.path);
            }
        }
        Cmd::Extract { dest, root, owner } => {
            let r = vol.extract(&root, &dest, &ExtractOptions { owner }).await?;
            eprintln!(
                "{} files ({} bytes), {} directories, {} symlinks, {} hard links",
                r.files, r.bytes, r.directories, r.symlinks, r.hard_links
            );
            if !r.specials_skipped.is_empty() {
                eprintln!("{} device nodes, FIFOs and sockets not created", r.specials_skipped.len());
            }
            if r.xattrs_skipped > 0 {
                eprintln!("{} extended attributes not applied (use `tar` to keep them)", r.xattrs_skipped);
            }
        }
        Cmd::Tar { output, root } => {
            let r = match output {
                Some(p) => {
                    let f = tokio::io::BufWriter::new(tokio::fs::File::create(&p).await?);
                    vol.pack_tar_to(Io::new(f), &root).await?
                }
                None => vol.pack_tar_to(Io::new(tokio::io::BufWriter::new(out)), &root).await?,
            };
            eprintln!(
                "{} files ({} bytes), {} directories, {} symlinks, {} hard links, {} devices, {} xattrs",
                r.files, r.bytes, r.directories, r.symlinks, r.hard_links, r.devices, r.xattrs
            );
        }
    }
    Ok(())
}

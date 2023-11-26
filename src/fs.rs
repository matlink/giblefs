use crate::git::GitRepo;
use crate::inode::InodeGen;
use anyhow::Result;
use fuse::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request,
};
use git2::{ObjectType, Oid};
use lazy_static::lazy_static;
use libc::ENOENT;
use log::{debug, error};
use std::convert::TryFrom;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use time::Timespec;

pub mod attr;

pub use attr::{FileAttrBuilder, ToFileAttr};

lazy_static! {
    static ref UNIX_EPOCH: Timespec = Timespec::new(0, 0);
    static ref TTL: Timespec = Timespec::new(1, 0);
    static ref ROOT_ATTR: FileAttr = FileAttr {
        ino: 1,
        size: 0,
        blocks: 0,
        atime: *UNIX_EPOCH, // 1970-01-01 00:00:00
        mtime: *UNIX_EPOCH,
        ctime: *UNIX_EPOCH,
        crtime: *UNIX_EPOCH,
        kind: FileType::Directory,
        perm: 0o755,
        nlink: 2,
        uid: 501,
        gid: 20,
        rdev: 0,
        flags: 0,
    };
}

pub struct GilberFS {
    repo: GitRepo,
    builder: FileAttrBuilder,
}

impl GilberFS {
    pub fn new(repo: PathBuf, uid: libc::uid_t, gid: libc::gid_t) -> Result<Self> {
        let builder = FileAttrBuilder::new().uid(uid).gid(gid);

        Ok(GilberFS {
            repo: GitRepo::new(repo, InodeGen::new())?,
            builder,
        })
    }

    fn lookup_commit(&mut self, hash: &str) -> Result<FileAttr> {
        let oid = Oid::from_str(hash)?;
        let commit = self.repo.get_tree_by_commit(oid)?;
        Ok(commit.to_file_attr(self.builder.clone()))
    }
}

impl Filesystem for GilberFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        if parent == 1 {
            // looking up by commit hash
            if let Some(hash) = name.to_str() {
                if let Ok(attr) = self.lookup_commit(hash) {
                    reply.entry(&TTL, &attr, 0);
                    return;
                }
            }

            reply.error(ENOENT);
            return;
        }

        // you can only `lookup` on a tree
        let tree = match self.repo.get_tree_by_inode(parent.into()) {
            Ok(tree) => tree,
            Err(_) => {
                reply.error(ENOENT);
                return;
            }
        };

        // entry not found
        let (oid, kind) = match tree.as_ref().get_path(&Path::new(&name)) {
            Ok(entry) => (entry.id(), entry.kind()),
            Err(_) => {
                reply.error(ENOENT);
                return;
            }
        };

        let parent = tree.parent();

        drop(tree);

        match kind {
            Some(ObjectType::Blob) => {
                if let Ok(blob) = self.repo.get_blob(parent, oid) {
                    reply.entry(&TTL, &blob.to_file_attr(self.builder.clone()), 0);
                    return;
                }
            }
            Some(ObjectType::Tree) => {
                if let Ok(tree) = self.repo.get_tree(parent, oid) {
                    reply.entry(&TTL, &tree.to_file_attr(self.builder.clone()), 0);
                    return;
                }
            }
            _ => (),
        }

        reply.error(ENOENT);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        if ino == 1 {
            reply.attr(&TTL, &ROOT_ATTR);
        } else if let Ok(tree) = self.repo.get_tree_by_inode(ino.into()) {
            reply.attr(&TTL, &tree.to_file_attr(self.builder.clone()));
        } else if let Ok(blob) = self.repo.get_blob_by_inode(ino.into()) {
            reply.attr(&TTL, &blob.to_file_attr(self.builder.clone()));
        } else {
            reply.error(ENOENT);
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        reply: ReplyData,
    ) {
        if ino == 1 {
            reply.error(libc::EISDIR);
        } else if let Ok(blob) = self.repo.get_blob_by_inode(ino.into()) {
            if let (Ok(offset), Ok(size)) = (usize::try_from(offset), usize::try_from(size)) {
                let content = blob.as_ref().content();
                reply.data(&content[offset..(std::cmp::min(offset + size, content.len()))])
            } else {
                // offset or size is too big for us to handle
                reply.error(libc::EINVAL)
            }
        } else if let Ok(_) = self.repo.get_tree_by_inode(ino.into()) {
            reply.error(libc::EISDIR);
        } else {
            reply.error(ENOENT);
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if ino == 1 {
            reply.error(ENOENT);
            return;
        }

        let offset = if let Ok(offset) = usize::try_from(offset) {
            offset
        } else {
            error!("invalid offset: {}", offset);
            reply.error(libc::EINVAL);
            return;
        };

        let tree = match self.repo.get_tree_by_inode(ino.into()) {
            Ok(tree) => tree,
            Err(_) => {
                reply.error(ENOENT);
                return;
            }
        };
        let parent = tree.parent();
        let ino = tree.inode();
        let entries: Vec<_> = tree
            .as_ref()
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let oid = entry.id();
                let name = OsStr::from_bytes(entry.name_bytes());
                let name = OsString::from(name);
                let mode = entry.filemode();
                let kind = entry.kind();
                (idx as i64 + 3, oid, name, kind, mode)
            })
            .collect();
        drop(tree);

        if !(offset >= 1) {
            reply.add(ino.value(), 1, FileType::Directory, ".");
            debug!("{} {} {}", ino.value(), 1, ".");
        }

        if !(offset >= 2) {
            reply.add(ino.parent(), 2, FileType::Directory, "..");
            debug!("{} {} {}", ino.parent(), 2, "..");
        }

        let offset = offset.saturating_sub(2);

        for (idx, oid, name, kind, _mode) in entries.into_iter().skip(offset) {
            if let Ok((ino, _, obj)) = self.repo.get_object(parent, oid, kind) {
                debug!("{} {} {:?}", ino.value(), idx, &name);
                match obj.kind() {
                    Some(ObjectType::Blob) => {
                        // handle blobs
                        reply.add(ino.value(), idx, FileType::RegularFile, name);
                    }
                    Some(ObjectType::Tree) => {
                        // handle trees
                        reply.add(ino.value(), idx, FileType::Directory, name);
                    }
                    Some(kind) => {
                        error!("received impossible object type {} for {}", kind, oid);
                    }
                    None => {
                        error!("unable to detect object type for {}", oid);
                    }
                }
            } else {
                error!("unable to find {}", oid);
            }
        }

        reply.ok();
    }
}

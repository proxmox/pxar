//! Proxmox backup archive format handling.
//!
//! This implements a reader and writer for the proxmox archive format (.pxar).

use std::ffi::OsStr;
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[macro_use]
mod macros;

pub mod format;

pub(crate) mod util;

mod poll_fn;

pub mod accessor;
pub mod decoder;

/// File metadata found in pxar archives.
///
/// This includes the usual data you'd get from `stat()` as well as ACLs, extended attributes, file
/// capabilities and more.
#[derive(Clone, Debug, Default)]
pub struct Metadata {
    /// Data typically found in a `stat()` call.
    pub stat: format::Entry,

    /// Extended attributes.
    pub xattrs: Vec<format::XAttr>,

    /// ACLs.
    pub acl: Acl,

    /// File capabilities.
    pub fcaps: Option<format::FCaps>,

    /// Quota project id.
    pub quota_project_id: Option<format::QuotaProjectId>,
}

/// ACL entries of a pxar archive.
///
/// This contains all the various ACL entry types supported by the pxar archive format.
#[derive(Clone, Debug, Default)]
pub struct Acl {
    /// User ACL list.
    pub users: Vec<format::acl::User>,

    /// Group ACL list.
    pub groups: Vec<format::acl::Group>,

    /// Group object ACL entry.
    pub group_obj: Option<format::acl::GroupObject>,

    /// Default permissions.
    pub default: Option<format::acl::Default>,

    /// Default user permissions.
    pub default_users: Vec<format::acl::User>,

    /// Default group permissions.
    pub default_groups: Vec<format::acl::Group>,
}

/// Pxar archive entry kind.
///
/// Identifies whether the entry is a file, symlink, directory, etc.
#[derive(Clone, Debug)]
pub enum EntryKind {
    /// Symbolic links.
    Symlink(format::Symlink),

    /// Hard links, relative to the root of the current archive.
    Hardlink(format::Hardlink),

    /// Device node.
    Device(format::Device),

    /// Regular file.
    File { size: u64 },

    /// Directory entry. When iterating through an archive, the contents follow next.
    Directory,

    /// End of a directory. This is for internal use to remember the goodbye-table of a directory
    /// entry. Will not occur during normal iteration.
    EndOfDirectory,
}

/// A pxar archive entry. This contains the current path, file metadata and entry type specific
/// information.
#[derive(Clone, Debug)]
pub struct Entry {
    path: PathBuf,
    metadata: Metadata,
    kind: EntryKind,
    offset: Option<u64>,
}

/// General accessors.
impl Entry {
    /// Clear everything except for the path.
    fn clear_data(&mut self) {
        self.metadata = Metadata::default();
        self.kind = EntryKind::EndOfDirectory;
        self.offset = None;
    }

    fn internal_default() -> Self {
        Self {
            path: PathBuf::default(),
            metadata: Metadata::default(),
            kind: EntryKind::EndOfDirectory,
            offset: None,
        }
    }

    fn take(&mut self) -> Self {
        let this = mem::replace(self, Self::internal_default());
        self.path = this.path.clone();
        this
    }

    /// If the underlying I/O implementation supports seeking, this will be filled with the start
    /// offset of this entry, allowing one to jump back to this entry later on.
    #[inline]
    pub fn offset(&self) -> Option<u64> {
        self.offset
    }

    /// Get the full path of this file within the current pxar directory structure.
    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Convenience method to get just the file name portion of the current path.
    #[inline]
    pub fn file_name(&self) -> &OsStr {
        self.path.file_name().unwrap_or(OsStr::new(""))
    }

    /// Get the file metadata.
    #[inline]
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Get the value of the symbolic link if it is one.
    pub fn get_symlink(&self) -> Option<&OsStr> {
        match &self.kind {
            EntryKind::Symlink(link) => Some(OsStr::from_bytes(&link.data)),
            _ => None,
        }
    }

    /// Get the value of the hard link if it is one.
    pub fn get_hardlink(&self) -> Option<&OsStr> {
        match &self.kind {
            EntryKind::Hardlink(link) => Some(OsStr::from_bytes(&link.data)),
            _ => None,
        }
    }

    /// Get the value of the device node if it is one.
    pub fn get_device(&self) -> Option<format::Device> {
        match &self.kind {
            EntryKind::Device(dev) => Some(dev.clone()),
            _ => None,
        }
    }
}

/// Convenience helpers.
impl Entry {
    /// Check whether this is a directory.
    pub fn is_dir(&self) -> bool {
        match self.kind {
            EntryKind::Directory { .. } => true,
            _ => false,
        }
    }

    /// Check whether this is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        match self.kind {
            EntryKind::Symlink(_) => true,
            _ => false,
        }
    }

    /// Check whether this is a hard link.
    pub fn is_hardlink(&self) -> bool {
        match self.kind {
            EntryKind::Hardlink(_) => true,
            _ => false,
        }
    }

    /// Check whether this is a device node.
    pub fn is_device(&self) -> bool {
        match self.kind {
            EntryKind::Device(_) => true,
            _ => false,
        }
    }

    /// Check whether this is a regular file.
    pub fn is_regular_file(&self) -> bool {
        match self.kind {
            EntryKind::File { .. } => true,
            _ => false,
        }
    }
}

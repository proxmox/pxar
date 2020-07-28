//! Proxmox backup archive format handling.
//!
//! This implements a reader and writer for the proxmox archive format (.pxar).

use std::ffi::OsStr;
use std::mem;
use std::path::{Path, PathBuf};

#[macro_use]
mod macros;

pub mod format;

pub(crate) mod util;

mod poll_fn;

pub mod accessor;
pub mod binary_tree_array;
pub mod decoder;
pub mod encoder;

/// Reexport of `format::Entry`. Since this conveys mostly information found via the `stat` syscall
/// we mostly use this name for public interfaces.
#[doc(inline)]
pub use format::Entry as Stat;

#[doc(inline)]
pub use format::mode;

/// File metadata found in pxar archives.
///
/// This includes the usual data you'd get from `stat()` as well as ACLs, extended attributes, file
/// capabilities and more.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "test-harness", derive(Eq, PartialEq))]
pub struct Metadata {
    /// Data typically found in a `stat()` call.
    pub stat: Stat,

    /// Extended attributes.
    pub xattrs: Vec<format::XAttr>,

    /// ACLs.
    pub acl: Acl,

    /// File capabilities.
    pub fcaps: Option<format::FCaps>,

    /// Quota project id.
    pub quota_project_id: Option<format::QuotaProjectId>,
}

impl From<Stat> for Metadata {
    fn from(stat: Stat) -> Self {
        Self {
            stat,
            ..Default::default()
        }
    }
}

impl From<&std::fs::Metadata> for Metadata {
    fn from(meta: &std::fs::Metadata) -> Metadata {
        // NOTE: fill the remaining metadata via feature flags?
        Self::from(Stat::from(meta))
    }
}

impl From<std::fs::Metadata> for Metadata {
    fn from(meta: std::fs::Metadata) -> Metadata {
        Self::from(&meta)
    }
}

/// Convenience helpers.
impl Metadata {
    /// Get the file type (`mode & mode::IFMT`).
    #[inline]
    pub fn file_type(&self) -> u64 {
        self.stat.file_type()
    }

    /// Get the file mode bits (`mode & !mode::IFMT`).
    #[inline]
    pub fn file_mode(&self) -> u64 {
        self.stat.file_mode()
    }

    /// Check whether this is a directory.
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.stat.is_dir()
    }

    /// Check whether this is a symbolic link.
    #[inline]
    pub fn is_symlink(&self) -> bool {
        self.stat.is_symlink()
    }

    /// Check whether this is a device node.
    #[inline]
    pub fn is_device(&self) -> bool {
        self.stat.is_device()
    }

    /// Check whether this is a regular file.
    #[inline]
    pub fn is_regular_file(&self) -> bool {
        self.stat.is_regular_file()
    }

    /// Check whether this is a named pipe (FIFO).
    #[inline]
    pub fn is_fifo(&self) -> bool {
        self.stat.is_fifo()
    }

    /// Check whether this is a named socket.
    #[inline]
    pub fn is_socket(&self) -> bool {
        self.stat.is_socket()
    }

    /// Get the mtime as duration since the epoch. an `Ok` value is a positive duration, an `Err`
    /// value is a negative duration.
    pub fn mtime_as_duration(&self) -> format::SignedDuration {
        self.stat.mtime_as_duration()
    }

    /// A more convenient way to create metadata for a regular file.
    pub fn file_builder(mode: u64) -> MetadataBuilder {
        Self::builder(mode::IFREG | (mode & !mode::IFMT))
    }

    /// A more convenient way to create metadata for a directory file.
    pub fn dir_builder(mode: u64) -> MetadataBuilder {
        Self::builder(mode::IFDIR | (mode & !mode::IFMT))
    }

    /// A more convenient way to create generic metadata.
    pub fn builder(mode: u64) -> MetadataBuilder {
        MetadataBuilder::new(mode)
    }
}

impl From<MetadataBuilder> for Metadata {
    fn from(builder: MetadataBuilder) -> Self {
        builder.build()
    }
}

pub struct MetadataBuilder {
    inner: Metadata,
}

impl MetadataBuilder {
    pub const fn new(type_and_mode: u64) -> Self {
        Self {
            inner: Metadata {
                stat: Stat {
                    mode: type_and_mode,
                    flags: 0,
                    uid: 0,
                    gid: 0,
                    mtime: format::StatxTimestamp::zero(),
                },
                xattrs: Vec::new(),
                acl: Acl {
                    users: Vec::new(),
                    groups: Vec::new(),
                    group_obj: None,
                    default: None,
                    default_users: Vec::new(),
                    default_groups: Vec::new(),
                },
                fcaps: None,
                quota_project_id: None,
            },
        }
    }

    pub fn build(self) -> Metadata {
        self.inner
    }

    /// Set the file type (`mode & mode::IFMT`).
    pub const fn file_type(mut self, mode: u64) -> Self {
        self.inner.stat.mode = (self.inner.stat.mode & !mode::IFMT) | (mode & mode::IFMT);
        self
    }

    /// Set the file mode bits (`mode & !mode::IFMT`).
    pub const fn file_mode(mut self, mode: u64) -> Self {
        self.inner.stat.mode = (self.inner.stat.mode & mode::IFMT) | (mode & !mode::IFMT);
        self
    }

    /// Set the modification time from a statx timespec value.
    pub fn mtime_full(mut self, mtime: format::StatxTimestamp) -> Self {
        self.inner.stat.mtime = mtime;
        self
    }

    /// Set the modification time from a duration since the epoch (`SystemTime::UNIX_EPOCH`).
    pub fn mtime_unix(self, mtime: std::time::Duration) -> Self {
        self.mtime_full(format::StatxTimestamp::from_duration_since_epoch(mtime))
    }

    /// Set the modification time from a system time.
    pub fn mtime(self, mtime: std::time::SystemTime) -> Self {
        self.mtime_full(mtime.into())
    }

    /// Set the ownership information.
    pub const fn owner(self, uid: u32, gid: u32) -> Self {
        self.uid(uid).gid(gid)
    }

    /// Set the owning user id.
    pub const fn uid(mut self, uid: u32) -> Self {
        self.inner.stat.uid = uid;
        self
    }

    /// Set the owning user id.
    pub const fn gid(mut self, gid: u32) -> Self {
        self.inner.stat.gid = gid;
        self
    }

    /// Add an extended attribute.
    pub fn xattr<N: AsRef<[u8]>, V: AsRef<[u8]>>(mut self, name: N, value: V) -> Self {
        self.inner.xattrs.push(format::XAttr::new(name, value));
        self
    }

    /// Add a user ACL entry.
    pub fn acl_user(mut self, entry: format::acl::User) -> Self {
        self.inner.acl.users.push(entry);
        self
    }

    /// Add a group ACL entry.
    pub fn acl_group(mut self, entry: format::acl::Group) -> Self {
        self.inner.acl.groups.push(entry);
        self
    }

    /// Add a user default-ACL entry.
    pub fn default_acl_user(mut self, entry: format::acl::User) -> Self {
        self.inner.acl.default_users.push(entry);
        self
    }

    /// Add a group default-ACL entry.
    pub fn default_acl_group(mut self, entry: format::acl::Group) -> Self {
        self.inner.acl.default_groups.push(entry);
        self
    }

    /// Set the default ACL entry for a directory.
    pub const fn default_acl(mut self, entry: Option<format::acl::Default>) -> Self {
        self.inner.acl.default = entry;
        self
    }

    /// Set the quota project id.
    pub fn quota_project_id(mut self, id: Option<u64>) -> Self {
        self.inner.quota_project_id = id.map(|projid| format::QuotaProjectId { projid });
        self
    }

    /// Set the raw file capability data.
    pub fn fcaps(mut self, fcaps: Option<Vec<u8>>) -> Self {
        self.inner.fcaps = fcaps.map(|data| format::FCaps { data });
        self
    }
}

/// ACL entries of a pxar archive.
///
/// This contains all the various ACL entry types supported by the pxar archive format.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "test-harness", derive(Eq, PartialEq))]
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

impl Acl {
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
            && self.groups.is_empty()
            && self.group_obj.is_none()
            && self.default.is_none()
            && self.default_users.is_empty()
            && self.default_groups.is_empty()
    }
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

    /// Named unix socket.
    Socket,

    /// Named pipe.
    Fifo,

    /// Regular file.
    File { size: u64, offset: Option<u64> },

    /// Directory entry. When iterating through an archive, the contents follow next.
    Directory,

    /// End of a directory. This is for internal use to remember the goodbye-table of a directory
    /// entry. Will not occur during normal iteration.
    GoodbyeTable,
}

/// A pxar archive entry. This contains the current path, file metadata and entry type specific
/// information.
#[derive(Clone, Debug)]
pub struct Entry {
    path: PathBuf,
    metadata: Metadata,
    kind: EntryKind,
}

/// General accessors.
impl Entry {
    /// Clear everything except for the path.
    fn clear_data(&mut self) {
        self.metadata = Metadata::default();
        self.kind = EntryKind::GoodbyeTable;
    }

    fn internal_default() -> Self {
        Self {
            path: PathBuf::default(),
            metadata: Metadata::default(),
            kind: EntryKind::GoodbyeTable,
        }
    }

    fn take(&mut self) -> Self {
        let this = mem::replace(self, Self::internal_default());
        self.path = this.path.clone();
        this
    }

    /// Get the full path of this file within the current pxar directory structure.
    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Convenience method to get just the file name portion of the current path.
    #[inline]
    pub fn file_name(&self) -> &OsStr {
        self.path.file_name().unwrap_or_else(|| OsStr::new(""))
    }

    /// Get the file metadata.
    #[inline]
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Take out the metadata.
    #[inline]
    pub fn into_metadata(self) -> Metadata {
        self.metadata
    }

    /// Get the entry-type specific information.
    pub fn kind(&self) -> &EntryKind {
        &self.kind
    }

    /// Get the value of the symbolic link if it is one.
    pub fn get_symlink(&self) -> Option<&OsStr> {
        match &self.kind {
            EntryKind::Symlink(link) => Some(link.as_ref()),
            _ => None,
        }
    }

    /// Get the value of the hard link if it is one.
    pub fn get_hardlink(&self) -> Option<&OsStr> {
        match &self.kind {
            EntryKind::Hardlink(link) => Some(link.as_ref()),
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

    /// Get the file size if this is a regular file, or `None`.
    pub fn file_size(&self) -> Option<u64> {
        match self.kind {
            EntryKind::File { size, .. } => Some(size),
            _ => None,
        }
    }
}

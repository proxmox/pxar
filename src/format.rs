//! *pxar* binary format definition
//!
//! Please note the all values are stored in little endian ordering.
//!
//! The Archive contains a list of items. Each item starts with a `Header`, followed by the
//! item data.

use std::cmp::Ordering;
use std::ffi::{CStr, OsStr};
use std::io;
use std::mem::size_of;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use endian_trait::Endian;
use siphasher::sip::SipHasher24;

pub mod acl;

/// While these constants correspond to `libc::S_` constants, we need these to be fixed for the
/// format itself, so we redefine them here.
///
/// Additionally this gets rid of a bunch of casts between u32 and u64.
///
/// You can usually find the values for these in `/usr/include/linux/stat.h`.
#[rustfmt::skip]
pub mod mode {
    pub const IFMT   : u64 = 0o0170000;

    pub const IFSOCK : u64 = 0o0140000;
    pub const IFLNK  : u64 = 0o0120000;
    pub const IFREG  : u64 = 0o0100000;
    pub const IFBLK  : u64 = 0o0060000;
    pub const IFDIR  : u64 = 0o0040000;
    pub const IFCHR  : u64 = 0o0020000;
    pub const IFIFO  : u64 = 0o0010000;

    pub const ISUID  : u64 = 0o0004000;
    pub const ISGID  : u64 = 0o0002000;
    pub const ISVTX  : u64 = 0o0001000;
}

pub const PXAR_ENTRY: u64 = 0x1396fabcea5bbb51;
pub const PXAR_FILENAME: u64 = 0x6dbb6ebcb3161f0b;
pub const PXAR_SYMLINK: u64 = 0x664a6fb6830e0d6c;
pub const PXAR_DEVICE: u64 = 0xac3dace369dfe643;
pub const PXAR_XATTR: u64 = 0xb8157091f80bc486;
pub const PXAR_ACL_USER: u64 = 0x297dc88b2ef12faf;
pub const PXAR_ACL_GROUP: u64 = 0x36f2acb56cb3dd0b;
pub const PXAR_ACL_GROUP_OBJ: u64 = 0x23047110441f38f3;
pub const PXAR_ACL_DEFAULT: u64 = 0xfe3eeda6823c8cd0;
pub const PXAR_ACL_DEFAULT_USER: u64 = 0xbdf03df9bd010a91;
pub const PXAR_ACL_DEFAULT_GROUP: u64 = 0xa0cb1168782d1f51;
pub const PXAR_FCAPS: u64 = 0xf7267db0afed0629;
pub const PXAR_QUOTA_PROJID: u64 = 0x161baf2d8772a72b;

/// Marks item as hardlink
/// compute_goodbye_hash(b"__PROXMOX_FORMAT_HARDLINK__");
pub const PXAR_HARDLINK: u64 = 0x2c5e06f634f65b86;
/// Marks the beginnig of the payload (actual content) of regular files
pub const PXAR_PAYLOAD: u64 = 0x8b9e1d93d6dcffc9;
/// Marks item as entry of goodbye table
pub const PXAR_GOODBYE: u64 = 0xdfd35c5e8327c403;
/// The end marker used in the GOODBYE object
pub const PXAR_GOODBYE_TAIL_MARKER: u64 = 0x57446fa533702943;

#[derive(Debug, Endian)]
#[repr(C)]
pub struct Header {
    /// The item type (see `PXAR_` constants).
    pub htype: u64,
    /// The size of the item, including the size of `Header`.
    full_size: u64,
}

impl Header {
    #[inline]
    pub fn with_full_size(htype: u64, full_size: u64) -> Self {
        Self { htype, full_size }
    }

    #[inline]
    pub fn with_content_size(htype: u64, content_size: u64) -> Self {
        Self::with_full_size(htype, content_size + size_of::<Header>() as u64)
    }

    #[inline]
    pub fn full_size(&self) -> u64 {
        self.full_size
    }

    #[inline]
    pub fn content_size(&self) -> u64 {
        self.full_size() - (size_of::<Self>() as u64)
    }
}

#[derive(Clone, Debug, Default, Endian)]
#[repr(C)]
pub struct Entry {
    pub mode: u64,
    pub flags: u64,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u64,
}

/// Builder pattern methods.
impl Entry {
    pub const fn mode(self, mode: u64) -> Self {
        Self { mode, ..self }
    }

    pub const fn flags(self, flags: u64) -> Self {
        Self { flags, ..self }
    }

    pub const fn uid(self, uid: u32) -> Self {
        Self { uid, ..self }
    }

    pub const fn gid(self, gid: u32) -> Self {
        Self { gid, ..self }
    }

    pub const fn mtime(self, mtime: u64) -> Self {
        Self { mtime, ..self }
    }

    pub const fn set_dir(self) -> Self {
        let mode = self.mode;
        self.mode((mode & !mode::IFMT) | mode::IFDIR)
    }

    pub const fn set_regular_file(self) -> Self {
        let mode = self.mode;
        self.mode((mode & !mode::IFMT) | mode::IFREG)
    }

    pub const fn set_symlink(self) -> Self {
        let mode = self.mode;
        self.mode((mode & !mode::IFMT) | mode::IFLNK)
    }

    pub const fn set_blockdev(self) -> Self {
        let mode = self.mode;
        self.mode((mode & !mode::IFMT) | mode::IFBLK)
    }

    pub const fn set_chardev(self) -> Self {
        let mode = self.mode;
        self.mode((mode & !mode::IFMT) | mode::IFCHR)
    }

    pub const fn set_fifo(self) -> Self {
        let mode = self.mode;
        self.mode((mode & !mode::IFMT) | mode::IFIFO)
    }
}

/// Convenience accessor methods.
impl Entry {
    /// Get the mtime as duration since the epoch.
    pub fn mtime_as_duration(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.mtime)
    }

    /// Get the file type portion of the mode bitfield.
    pub fn get_file_bits(&self) -> u64 {
        self.mode & mode::IFMT
    }

    /// Get the permission portion of the mode bitfield.
    pub fn get_permission_bits(&self) -> u64 {
        self.mode & !mode::IFMT
    }
}

/// Convenience methods.
impl Entry {
    /// Get the file type (`mode & mode::IFMT`).
    pub fn file_type(&self) -> u64 {
        self.mode & mode::IFMT
    }

    /// Get the file mode bits (`mode & !mode::IFMT`).
    pub fn file_mode(&self) -> u64 {
        self.mode & !mode::IFMT
    }

    /// Check whether this is a directory.
    pub fn is_dir(&self) -> bool {
        (self.mode & mode::IFMT) == mode::IFDIR
    }

    /// Check whether this is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        (self.mode & mode::IFMT) == mode::IFLNK
    }

    /// Check whether this is a device node.
    pub fn is_device(&self) -> bool {
        let fmt = self.mode & mode::IFMT;
        fmt == mode::IFCHR || fmt == mode::IFBLK
    }

    /// Check whether this is a regular file.
    pub fn is_regular_file(&self) -> bool {
        (self.mode & mode::IFMT) == mode::IFREG
    }

    /// Check whether this is a named pipe (FIFO).
    pub fn is_fifo(&self) -> bool {
        (self.mode & mode::IFMT) == mode::IFIFO
    }

    /// Check whether this is a named socket.
    pub fn is_socket(&self) -> bool {
        (self.mode & mode::IFMT) == mode::IFSOCK
    }
}

impl From<&std::fs::Metadata> for Entry {
    fn from(meta: &std::fs::Metadata) -> Entry {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        let this = Entry::default();

        #[cfg(unix)]
        let this = this
            .uid(meta.uid())
            .gid(meta.gid())
            .mode(meta.mode() as u64)
            .mtime(meta.mtime() as u64);

        let file_type = meta.file_type();
        let mode = this.mode;
        let this = if file_type.is_dir() {
            this.mode(mode | mode::IFDIR)
        } else if file_type.is_symlink() {
            this.mode(mode | mode::IFLNK)
        } else {
            this.mode(mode | mode::IFREG)
        };

        this
    }
}

#[derive(Clone, Debug)]
pub struct Filename {
    pub name: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Symlink {
    pub data: Vec<u8>,
}

impl Symlink {
    pub fn as_os_str(&self) -> &OsStr {
        self.as_ref()
    }
}

impl AsRef<[u8]> for Symlink {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl AsRef<OsStr> for Symlink {
    fn as_ref(&self) -> &OsStr {
        OsStr::from_bytes(&self.data[..self.data.len().max(1) - 1])
    }
}

#[derive(Clone, Debug)]
pub struct Hardlink {
    pub data: Vec<u8>,
}

impl Hardlink {
    pub fn as_os_str(&self) -> &OsStr {
        self.as_ref()
    }
}

impl AsRef<[u8]> for Hardlink {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl AsRef<OsStr> for Hardlink {
    fn as_ref(&self) -> &OsStr {
        OsStr::from_bytes(&self.data[..self.data.len().max(1) - 1])
    }
}

#[derive(Clone, Debug, Eq)]
#[repr(C)]
pub struct XAttr {
    pub(crate) data: Vec<u8>,
    pub(crate) name_len: usize,
}

impl XAttr {
    pub fn new<N: AsRef<[u8]>, V: AsRef<[u8]>>(name: N, value: V) -> Self {
        let name = name.as_ref();
        let value = value.as_ref();
        let mut data = Vec::with_capacity(name.len() + value.len() + 1);
        data.extend(name);
        data.push(0);
        data.extend(value);
        Self {
            data,
            name_len: name.len(),
        }
    }

    pub fn name(&self) -> &CStr {
        unsafe {
            CStr::from_bytes_with_nul_unchecked(&self.data[..self.name_len+1])
        }
    }

    pub fn value(&self) -> &[u8] {
        &self.data[(self.name_len + 1)..]
    }
}

impl Ord for XAttr {
    fn cmp(&self, other: &XAttr) -> Ordering {
        self.name().cmp(&other.name())
    }
}

impl PartialOrd for XAttr {
    fn partial_cmp(&self, other: &XAttr) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for XAttr {
    fn eq(&self, other: &XAttr) -> bool {
        self.name() == other.name()
    }
}

#[derive(Clone, Debug, Endian)]
#[repr(C)]
pub struct Device {
    pub major: u64,
    pub minor: u64,
}

#[cfg(target_os = "linux")]
impl Device {
    /// Get a `dev_t` value for this device.
    #[rustfmt::skip]
    pub fn to_dev_t(&self) -> u64 {
        // see bits/sysmacros.h
        ((self.major & 0x0000_0fff) << 8) |
        ((self.major & 0xffff_f000) << 32) |
         (self.minor & 0x0000_00ff) |
        ((self.minor & 0xffff_ff00) << 12)
    }
}

#[derive(Clone, Debug)]
#[repr(C)]
pub struct FCaps {
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Endian)]
#[repr(C)]
pub struct QuotaProjectId {
    pub projid: u64,
}

#[derive(Clone, Debug, Endian)]
#[repr(C)]
pub struct GoodbyeItem {
    /// SipHash24 of the directory item name. The last GOODBYE item uses the special hash value
    /// `PXAR_GOODBYE_TAIL_MARKER`.
    pub hash: u64,

    /// The offset from the start of the GOODBYE object to the start of the matching directory item
    /// (point to a FILENAME). The last GOODBYE item points to the start of the matching ENTRY
    /// object.
    pub offset: u64,

    /// The overall size of the directory item. This includes the FILENAME header. In other words,
    /// `goodbye_start - offset + size` points to the end of the directory.
    ///
    /// The last GOODBYE item repeats the size of the GOODBYE item.
    pub size: u64,
}

impl GoodbyeItem {
    pub fn new(name: &[u8], offset: u64, size: u64) -> Self {
        let hash = hash_filename(name);
        Self { hash, offset, size }
    }
}

pub fn hash_filename(name: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut hasher = SipHasher24::new_with_keys(0x8574442b0f1d84b3, 0x2736ed30d1c22ec1);
    hasher.write(name);
    hasher.finish()
}

pub fn path_is_legal_component(path: &Path) -> bool {
    let mut components = path.components();
    match components.next() {
        Some(std::path::Component::Normal(_)) => (),
        _ => return false,
    }
    components.next().is_none()
}

pub fn check_file_name(path: &Path) -> io::Result<()> {
    if !path_is_legal_component(path) {
        io_bail!("invalid file name in archive: {:?}", path);
    } else {
        Ok(())
    }
}

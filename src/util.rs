#![allow(dead_code)]

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

// from /usr/include/linux/magic.h
// and from casync util.h
#[rustfmt::skip]
#[allow(clippy::unreadable_literal)]
mod consts {
    pub const BINFMTFS_MAGIC      : i64 = 0x42494e4d;
    pub const CGROUP2_SUPER_MAGIC : i64 = 0x63677270;
    pub const CGROUP_SUPER_MAGIC  : i64 = 0x0027e0eb;
    pub const CONFIGFS_MAGIC      : i64 = 0x62656570;
    pub const DEBUGFS_MAGIC       : i64 = 0x64626720;
    pub const DEVPTS_SUPER_MAGIC  : i64 = 0x00001cd1;
    pub const EFIVARFS_MAGIC      : i64 = 0xde5e81e4;
    pub const FUSE_CTL_SUPER_MAGIC: i64 = 0x65735543;
    pub const HUGETLBFS_MAGIC     : i64 = 0x958458f6;
    pub const MQUEUE_MAGIC        : i64 = 0x19800202;
    pub const NFSD_MAGIC          : i64 = 0x6e667364;
    pub const PROC_SUPER_MAGIC    : i64 = 0x00009fa0;
    pub const PSTOREFS_MAGIC      : i64 = 0x6165676C;
    pub const RPCAUTH_GSSMAGIC    : i64 = 0x67596969;
    pub const SECURITYFS_MAGIC    : i64 = 0x73636673;
    pub const SELINUX_MAGIC       : i64 = 0xf97cff8c;
    pub const SMACK_MAGIC         : i64 = 0x43415d53;
    pub const RAMFS_MAGIC         : i64 = 0x858458f6;
    pub const TMPFS_MAGIC         : i64 = 0x01021994;
    pub const SYSFS_MAGIC         : i64 = 0x62656572;
    pub const MSDOS_SUPER_MAGIC   : i64 = 0x00004d44;
    pub const BTRFS_SUPER_MAGIC   : i64 = 0x9123683E;
    pub const FUSE_SUPER_MAGIC    : i64 = 0x65735546;
    pub const EXT4_SUPER_MAGIC    : i64 = 0x0000EF53;
    pub const XFS_SUPER_MAGIC     : i64 = 0x58465342;
    pub const ZFS_SUPER_MAGIC     : i64 = 0x2FC12FC1;
}

pub fn is_virtual_file_system(magic: i64) -> bool {
    match magic {
        consts::BINFMTFS_MAGIC
        | consts::CGROUP2_SUPER_MAGIC
        | consts::CGROUP_SUPER_MAGIC
        | consts::CONFIGFS_MAGIC
        | consts::DEBUGFS_MAGIC
        | consts::DEVPTS_SUPER_MAGIC
        | consts::EFIVARFS_MAGIC
        | consts::FUSE_CTL_SUPER_MAGIC
        | consts::HUGETLBFS_MAGIC
        | consts::MQUEUE_MAGIC
        | consts::NFSD_MAGIC
        | consts::PROC_SUPER_MAGIC
        | consts::PSTOREFS_MAGIC
        | consts::RPCAUTH_GSSMAGIC
        | consts::SECURITYFS_MAGIC
        | consts::SELINUX_MAGIC
        | consts::SMACK_MAGIC
        | consts::SYSFS_MAGIC => true,
        _ => false,
    }
}

/// Helper function to extract file names from binary archive.
pub fn read_os_string(buffer: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(if buffer.ends_with(&[0]) {
        &buffer[..(buffer.len() - 1)]
    } else {
        buffer
    })
    .into()
}

#[inline]
pub fn vec_new(size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    unsafe {
        data.set_len(size);
    }
    data
}

pub fn io_err_other<E: std::fmt::Display>(err: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, err.to_string())
}

pub fn poll_result_once<T, R>(mut fut: T) -> io::Result<R>
where
    T: Future<Output = io::Result<R>>,
{
    let waker = std::task::RawWaker::new(std::ptr::null(), &WAKER_VTABLE);
    let waker = unsafe { std::task::Waker::from_raw(waker) };
    let mut cx = Context::from_waker(&waker);
    unsafe {
        match Pin::new_unchecked(&mut fut).poll(&mut cx) {
            Poll::Pending => Err(io_err_other("got Poll::Pending synchronous context")),
            Poll::Ready(r) => r,
        }
    }
}

const WAKER_VTABLE: std::task::RawWakerVTable =
    std::task::RawWakerVTable::new(forbid_clone, forbid_wake, forbid_wake, ignore_drop);

unsafe fn forbid_clone(_: *const ()) -> std::task::RawWaker {
    panic!("tried to clone waker for synchronous task");
}

unsafe fn forbid_wake(_: *const ()) {
    panic!("tried to wake synchronous task");
}

unsafe fn ignore_drop(_: *const ()) {}

pub const MAX_PATH_LEN:u64 = 4 * 1024;
// let's play it safe
pub const MAX_FILENAME_LEN:u64 = MAX_PATH_LEN;
// name + attr
pub const MAX_XATTR_LEN:u64 = 255 + 64*1024;

pub fn validate_filename(name: &[u8]) -> io::Result<()> {
    if name.is_empty() {
        io_bail!("illegal path found (empty)");
    }

    if name.contains(&b'/') {
        io_bail!("illegal path found (contains slashes, this is a security concern)");
    }

    if name == b"." {
        io_bail!("illegal path found: '.'");
    }

    if name == b".." {
        io_bail!("illegal path found: '..'");
    }

    if (name.len() as u64) > MAX_FILENAME_LEN {
        io_bail!("filename too long (> {})", MAX_FILENAME_LEN);
    }

    Ok(())
}

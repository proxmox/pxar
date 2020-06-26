//! ACL related data

use std::cmp::Ordering;

use endian_trait::Endian;

pub const READ: u64 = 4;
pub const WRITE: u64 = 2;
pub const EXECUTE: u64 = 1;
pub const NO_MASK: u64 = std::u64::MAX;

/// ACL permission bits.
///
/// While this is normally just a bitfield, the `NO_MASK` special value makes this a value of 2
/// possible "types", so we don't use `bitflags!` for this.
#[derive(Clone, Copy, Debug, Endian, Eq, Ord, PartialEq, PartialOrd)]
pub struct Permissions(pub u64);

impl Permissions {
    pub const NO_MASK: Permissions = Permissions(NO_MASK);
}

#[derive(Clone, Debug, Endian, Eq, PartialEq)]
#[repr(C)]
pub struct User {
    pub uid: u64,
    pub permissions: Permissions,
    //pub name: Vec<u64>, not impl for now
}

impl User {
    pub fn new(uid: u64, permissions: u64) -> Self {
        Self {
            uid,
            permissions: Permissions(permissions),
        }
    }
}

// TODO if also name is impl, sort by uid, then by name and last by permissions
impl Ord for User {
    fn cmp(&self, other: &User) -> Ordering {
        match self.uid.cmp(&other.uid) {
            // uids are equal, entries ordered by permissions
            Ordering::Equal => self.permissions.cmp(&other.permissions),
            // uids are different, entries ordered by uid
            uid_order => uid_order,
        }
    }
}

impl PartialOrd for User {
    fn partial_cmp(&self, other: &User) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Endian, Eq, PartialEq)]
#[repr(C)]
pub struct Group {
    pub gid: u64,
    pub permissions: Permissions,
    //pub name: Vec<u64>, not impl for now
}

impl Group {
    pub fn new(gid: u64, permissions: u64) -> Self {
        Self {
            gid,
            permissions: Permissions(permissions),
        }
    }
}

// TODO if also name is impl, sort by gid, then by name and last by permissions
impl Ord for Group {
    fn cmp(&self, other: &Group) -> Ordering {
        match self.gid.cmp(&other.gid) {
            // gids are equal, entries are ordered by permissions
            Ordering::Equal => self.permissions.cmp(&other.permissions),
            // gids are different, entries ordered by gid
            gid_ordering => gid_ordering,
        }
    }
}

impl PartialOrd for Group {
    fn partial_cmp(&self, other: &Group) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Endian, Eq, PartialEq)]
#[repr(C)]
pub struct GroupObject {
    pub permissions: Permissions,
}

#[derive(Clone, Debug, Endian)]
#[cfg_attr(feature = "test-harness", derive(Eq, PartialEq))]
#[repr(C)]
pub struct Default {
    pub user_obj_permissions: Permissions,
    pub group_obj_permissions: Permissions,
    pub other_permissions: Permissions,
    pub mask_permissions: Permissions,
}

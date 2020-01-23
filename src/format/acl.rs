//! ACL related data

use std::cmp::Ordering;

use endian_trait::Endian;

bitflags::bitflags! {
    /// ACL permission bits.
    #[derive(Endian)]
    pub struct Permissions: u64 {
        const PXAR_ACL_PERMISSION_READ = 4;
        const PXAR_ACL_PERMISSION_WRITE = 2;
        const PXAR_ACL_PERMISSION_EXECUTE = 1;
    }
}

#[derive(Clone, Debug, Endian, Eq)]
#[repr(C)]
pub struct User {
    pub uid: u64,
    pub permissions: Permissions,
    //pub name: Vec<u64>, not impl for now
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

impl PartialEq for User {
    fn eq(&self, other: &User) -> bool {
        self.uid == other.uid && self.permissions == other.permissions
    }
}

#[derive(Clone, Debug, Endian, Eq)]
#[repr(C)]
pub struct Group {
    pub gid: u64,
    pub permissions: Permissions,
    //pub name: Vec<u64>, not impl for now
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

impl PartialEq for Group {
    fn eq(&self, other: &Group) -> bool {
        self.gid == other.gid && self.permissions == other.permissions
    }
}

#[derive(Clone, Debug, Endian)]
#[repr(C)]
pub struct GroupObject {
    pub permissions: Permissions,
}

#[derive(Clone, Debug, Endian)]
#[repr(C)]
pub struct Default {
    pub user_obj_permissions: Permissions,
    pub group_obj_permissions: Permissions,
    pub other_permissions: Permissions,
    pub mask_permissions: Permissions,
}

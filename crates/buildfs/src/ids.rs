//! NFSv4 owner strings <-> local uids/gids.
//!
//! The server renders ids numerically (the macOS client maps those
//! correctly), but the client sends names in SETATTR: `root`,
//! `wheel@host.local`, and the well-known principal `EVERYONE@` for the
//! `everyone` group (gid 12). Names resolve through the local user/group
//! databases, which is right for a localhost-only server.

use std::collections::HashMap;
use std::ffi::CString;

use embednfs::{IdMapper, parse_numeric_id};
use parking_lot::Mutex;

#[derive(Default)]
pub struct SystemIdMapper {
    users: Mutex<HashMap<String, Option<u32>>>,
    groups: Mutex<HashMap<String, Option<u32>>>,
}

/// `name@domain` -> `name`. Ownership is local, so the domain is ignored.
fn local_part(s: &str) -> &str {
    s.split('@').next().unwrap_or(s)
}

fn lookup_user(name: &str) -> Option<u32> {
    let cname = CString::new(name).ok()?;
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: zeroed passwd is a valid out-parameter for getpwnam_r.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the duration of the call and the
    // buffer length matches the buffer.
    let rc = unsafe { libc::getpwnam_r(cname.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut out) };
    (rc == 0 && !out.is_null()).then_some(pwd.pw_uid)
}

fn lookup_group(name: &str) -> Option<u32> {
    let cname = CString::new(name).ok()?;
    let mut buf = vec![0 as libc::c_char; 16384];
    // SAFETY: zeroed group is a valid out-parameter for getgrnam_r.
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::group = std::ptr::null_mut();
    // SAFETY: as above.
    let rc = unsafe { libc::getgrnam_r(cname.as_ptr(), &mut grp, buf.as_mut_ptr(), buf.len(), &mut out) };
    (rc == 0 && !out.is_null()).then_some(grp.gr_gid)
}

impl IdMapper for SystemIdMapper {
    fn owner(&self, uid: u32) -> String {
        uid.to_string()
    }

    fn group(&self, gid: u32) -> String {
        gid.to_string()
    }

    fn parse_owner(&self, owner: &str) -> Option<u32> {
        if let Some(id) = parse_numeric_id(owner) {
            return Some(id);
        }
        let name = local_part(owner);
        if name.is_empty() {
            return None;
        }
        *self
            .users
            .lock()
            .entry(name.to_owned())
            .or_insert_with(|| lookup_user(name))
    }

    fn parse_group(&self, group: &str) -> Option<u32> {
        if let Some(id) = parse_numeric_id(group) {
            return Some(id);
        }
        // NFSv4 well-known principal; macOS uses it for its `everyone` group.
        let name = match group {
            "EVERYONE@" => "everyone",
            other => local_part(other),
        };
        if name.is_empty() {
            return None;
        }
        *self
            .groups
            .lock()
            .entry(name.to_owned())
            .or_insert_with(|| lookup_group(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_names_numbers_and_principals() {
        let m = SystemIdMapper::default();
        assert_eq!(m.parse_owner("0"), Some(0));
        assert_eq!(m.parse_owner("501@localdomain"), Some(501));
        assert_eq!(m.parse_owner("root"), Some(0));
        assert_eq!(m.parse_owner("root@Some-Host.local"), Some(0));
        assert_eq!(m.parse_group("wheel@Some-Host.local"), Some(0));
        assert_eq!(m.parse_group("EVERYONE@"), lookup_group("everyone"));
        assert_eq!(m.parse_owner("no-such-user-buildfs"), None);
        assert_eq!(m.parse_group(""), None);
    }
}

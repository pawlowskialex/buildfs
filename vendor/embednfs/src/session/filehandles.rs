use crate::internal::ServerObject;

use super::StateManager;

/// Maximum NFSv4 filehandle size (RFC 8881 `NFS4_FHSIZE`).
const NFS4_FHSIZE: usize = 128;

/// Filehandle tags used in stable (stateless) mode.
const FH_TAG_FS: u8 = 1;
const FH_TAG_NAMED_ATTR_DIR: u8 = 2;
const FH_TAG_NAMED_ATTR_FILE: u8 = 3;
/// Named attribute whose name is too long to embed; resolved via the table.
const FH_TAG_TABLE: u8 = 4;

impl StateManager {
    pub(crate) fn alloc_connection_id(&self) -> u64 {
        self.next_connectionid
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Switches filehandle encoding to stateless mode where object ids are
    /// embedded directly into filehandles. Must be called before serving.
    pub(crate) fn set_stable_filehandles(&mut self, stable: bool) {
        self.stable_filehandles = stable;
    }

    /// Cheap syntactic filehandle check used by PUTFH.
    pub(crate) fn fh_has_valid_format(&self, fh: &embednfs_proto::NfsFh4) -> bool {
        let len = fh.0.len();
        if self.stable_filehandles {
            (9..=NFS4_FHSIZE).contains(&len)
                && matches!(
                    fh.0.first(),
                    Some(&(FH_TAG_FS | FH_TAG_NAMED_ATTR_DIR | FH_TAG_NAMED_ATTR_FILE | FH_TAG_TABLE))
                )
        } else {
            len == std::mem::size_of::<u64>()
        }
    }

    /// Uses a persistent server identity (owner major id and scope) so that
    /// clients recognize a restarted server as the same server and run
    /// normal reboot recovery instead of treating it as a new server.
    pub(crate) fn set_server_identity(&mut self, identity: bytes::Bytes) {
        self.server_owner.major_id = identity.clone();
        self.server_scope = identity;
    }

    pub(crate) fn object_to_fh(&self, object: &ServerObject) -> embednfs_proto::NfsFh4 {
        if self.stable_filehandles
            && let Some(fh) = encode_stable_fh(object)
        {
            return embednfs_proto::NfsFh4(fh.into());
        }
        if let Some(fh) = self.object_to_fh.get(object) {
            return embednfs_proto::NfsFh4(fh.value().clone().into());
        }
        let fh_num = self
            .next_fh
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let fh = if self.stable_filehandles {
            let mut fh = Vec::with_capacity(9);
            fh.push(FH_TAG_TABLE);
            fh.extend_from_slice(&fh_num.to_be_bytes());
            fh
        } else {
            fh_num.to_be_bytes().to_vec()
        };
        let _ = self.fh_to_object.insert(fh.clone(), object.clone());
        let _ = self.object_to_fh.insert(object.clone(), fh.clone());
        embednfs_proto::NfsFh4(fh.into())
    }

    pub(crate) fn fh_to_object(&self, fh: &embednfs_proto::NfsFh4) -> Option<ServerObject> {
        if self.stable_filehandles
            && let Some(object) = decode_stable_fh(fh.0.as_ref())
        {
            return Some(object);
        }
        self.fh_to_object
            .get(fh.0.as_ref())
            .map(|r| r.value().clone())
    }
}

fn encode_stable_fh(object: &ServerObject) -> Option<Vec<u8>> {
    let (tag, id, name) = match object {
        ServerObject::Fs(id) => (FH_TAG_FS, *id, None),
        ServerObject::NamedAttrDir(id) => (FH_TAG_NAMED_ATTR_DIR, *id, None),
        ServerObject::NamedAttrFile { parent, name } => {
            (FH_TAG_NAMED_ATTR_FILE, *parent, Some(name.as_bytes()))
        }
    };
    let name_len = name.map_or(0, <[u8]>::len);
    if 9 + name_len > NFS4_FHSIZE {
        return None;
    }
    let mut fh = Vec::with_capacity(9 + name_len);
    fh.push(tag);
    fh.extend_from_slice(&id.to_be_bytes());
    if let Some(name) = name {
        fh.extend_from_slice(name);
    }
    Some(fh)
}

fn decode_stable_fh(fh: &[u8]) -> Option<ServerObject> {
    let (&tag, rest) = fh.split_first()?;
    let id_bytes: [u8; 8] = rest.get(..8)?.try_into().ok()?;
    let id = u64::from_be_bytes(id_bytes);
    let tail = rest.get(8..)?;
    match tag {
        FH_TAG_FS if tail.is_empty() => Some(ServerObject::Fs(id)),
        FH_TAG_NAMED_ATTR_DIR if tail.is_empty() => Some(ServerObject::NamedAttrDir(id)),
        FH_TAG_NAMED_ATTR_FILE => Some(ServerObject::NamedAttrFile {
            parent: id,
            name: String::from_utf8(tail.to_vec()).ok()?,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_fh_roundtrip() {
        let objects = [
            ServerObject::Fs(42),
            ServerObject::NamedAttrDir(7),
            ServerObject::NamedAttrFile {
                parent: 9,
                name: "com.apple.FinderInfo".into(),
            },
        ];
        for object in objects {
            let fh = encode_stable_fh(&object).unwrap_or_default();
            assert_eq!(decode_stable_fh(&fh), Some(object));
        }
    }

    #[test]
    fn long_named_attr_falls_back_to_table() {
        let mut state = StateManager::new();
        state.set_stable_filehandles(true);
        let object = ServerObject::NamedAttrFile {
            parent: 1,
            name: "x".repeat(200),
        };
        let fh = state.object_to_fh(&object);
        assert!(fh.0.len() <= NFS4_FHSIZE);
        assert_eq!(state.fh_to_object(&fh), Some(object));
    }

    #[test]
    fn garbage_fh_is_rejected() {
        let mut state = StateManager::new();
        state.set_stable_filehandles(true);
        assert_eq!(
            state.fh_to_object(&embednfs_proto::NfsFh4(vec![1, 2, 3].into())),
            None
        );
    }
}

//! Finding other Marvs on this machine (mesh v2). New in marv-rs (docs/MESH.md, topology 2).
//!
//! Each switchboard also listens on `$XDG_RUNTIME_DIR/marv/mesh/<session>.sock`
//! (folder 0700, socket 0600: the user only). `marv mesh ls` lists them. A
//! cross-process wire is approved on **both** ends ([`crate::wire::Grant::remote`]).
//! Without `XDG_RUNTIME_DIR` there's no discovery: Marv won't fall back to a
//! world-readable `/tmp`.

use std::path::{Path, PathBuf};

use crate::receptacle::valid_segment;

/// `$XDG_RUNTIME_DIR/marv/mesh`, or `None` without a runtime dir.
pub fn mesh_dir(env: &marv_mcp::Env) -> Option<PathBuf> {
    let base = env.get("XDG_RUNTIME_DIR").filter(|d| Path::new(d).is_absolute())?;
    Some(Path::new(base).join("marv").join("mesh"))
}

/// A session's discovery socket. Session ids are one name segment, so the path can't escape the folder.
pub fn session_socket(dir: &Path, session: &str) -> Option<PathBuf> {
    valid_segment(session).then(|| dir.join(format!("{session}.sock")))
}

/// A switchboard found by `marv mesh ls`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSwitchboard {
    pub session: String,
    pub socket: PathBuf,
}

/// Every live switchboard in the folder (stale sockets skipped), owned by this user.
pub async fn list(_dir: &Path) -> crate::Result<Vec<PeerSwitchboard>> {
    todo!("port: mesh v2: read_dir, check owner uid and 0600, connect to drop stale sockets")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sockets_live_under_the_runtime_dir_only() {
        let env = |v: &str| [("XDG_RUNTIME_DIR".to_string(), v.to_string())].into_iter().collect::<marv_mcp::Env>();
        assert_eq!(mesh_dir(&env("/run/user/1000")), Some(PathBuf::from("/run/user/1000/marv/mesh")));
        assert_eq!(mesh_dir(&env("relative")), None);
        assert_eq!(mesh_dir(&Default::default()), None);
        let dir = PathBuf::from("/run/user/1000/marv/mesh");
        assert_eq!(session_socket(&dir, "abc-1"), Some(dir.join("abc-1.sock")));
        assert_eq!(session_socket(&dir, "../x"), None);
    }
}

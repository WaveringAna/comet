//! Method-level authorization for inbound Nova connections.
//!
//! A local (in-process or loopback) caller is fully trusted — it is the UI on the same
//! machine. A *remote* peer is a different device on the LAN/WAN: by default it may only
//! read live state (harness/model lists, doc watches) and queue commands into a shared
//! chat. A paired `Admin` represents another device owned by the same user and can also
//! configure Pi, repositories, terminals, uploads, and updates.
//!
//! The policy is expressed as an explicit allow-list per role so adding a new RPC
//! requires a deliberate authz decision rather than defaulting to "allowed".

use std::collections::HashSet;

use nova_rpc::methods;

use crate::trust::Role;

/// Who is calling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerKind {
    /// In-process UI / loopback IPC — full trust.
    Local,
    /// A paired remote Nova peer.
    Remote { role: Role },
}

impl CallerKind {
    pub fn is_local(self) -> bool {
        matches!(self, CallerKind::Local)
    }
}

/// The default remote-friendly surface: the read + command-plane RPCs a peer needs to
/// drive a chat on another device. Kept narrow on purpose.
fn remote_peer_set() -> HashSet<&'static str> {
    [
        methods::LIST_HARNESSES,
        methods::LIST_MODELS,
        methods::QUEUE_COMMAND,
        methods::WATCH_DOC_MESSAGES,
        methods::WATCH_CHATS,
        methods::WATCH_DEVICES,
        methods::WATCH_SESSIONS,
        methods::WATCH_PROJECTS,
        methods::LOCAL_DEVICE,
        methods::WATCH_CHECKOUT_DIFFS,
        methods::LIST_REPOS,
        methods::LIST_BRANCHES,
        methods::LIST_REFS,
        methods::LIST_FOLDERS,
        methods::TOOL_OUTPUT,
        methods::TOOL_DIFF,
        crate::methods::NOVA_SYNC_HEADS,
        crate::methods::NOVA_SYNC_APPLY,
    ]
    .into_iter()
    .collect()
}

/// Admins represent another device owned by the same user. They additionally get repo
/// mutation, terminals, worktrees, agent-account flows, Pi credentials, uploads, and
/// updates. Nova pairing and trust-management methods stay local-only.
fn remote_admin_set() -> HashSet<&'static str> {
    let mut s = remote_peer_set();
    s.extend([
        methods::MUTATE,
        methods::ADD_REPO,
        methods::CLONE_REPO,
        methods::CREATE_REPO,
        methods::SWITCH_REF,
        methods::CREATE_WORKTREE,
        methods::DELETE_WORKTREE,
        methods::OPEN_TERMINAL,
        methods::SUBSCRIBE_TERMINAL,
        methods::WRITE_TERMINAL,
        methods::RESIZE_TERMINAL,
        methods::CLOSE_TERMINAL,
        methods::UPDATE_STATUS,
        methods::LIST_AGENT_ACCOUNTS,
        methods::ACTIVATE_AGENT_ACCOUNT,
        methods::FORGET_AGENT_ACCOUNT,
        methods::START_AGENT_LOGIN,
        methods::COMPLETE_AGENT_LOGIN,
        methods::POLL_AGENT_LOGIN,
        methods::CANCEL_AGENT_LOGIN,
        methods::GET_PI_SETTINGS,
        methods::SET_PI_SETTING,
        methods::SET_PI_CREDENTIAL,
        methods::REMOVE_PI_CREDENTIAL,
        methods::SET_PI_OPENAI_COMPATIBLE,
        methods::PI_PACKAGE_ACTION,
        methods::UPLOAD_CHUNK,
        methods::UPLOAD_COMMIT,
        methods::READ_ATTACHMENT_CHUNK,
        methods::APPLY_UPDATE,
    ]);
    s
}

/// Decision returned by [`authorize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(&'static str),
}

/// Is `method` reachable by `caller`?
pub fn authorize(caller: CallerKind, method: &str) -> Decision {
    if caller.is_local() {
        return Decision::Allow;
    }
    let allowed = match caller {
        CallerKind::Remote { role: Role::Admin } => remote_admin_set(),
        _ => remote_peer_set(),
    };
    if allowed.contains(method) {
        Decision::Allow
    } else {
        Decision::Deny("method not in remote allow-list")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_rpc::methods;

    #[test]
    fn local_can_call_anything() {
        for m in [methods::APPLY_UPDATE, methods::QUEUE_COMMAND] {
            assert_eq!(authorize(CallerKind::Local, m), Decision::Allow);
        }
    }

    #[test]
    fn remote_peer_can_queue_and_watch_but_not_credentials() {
        let peer = CallerKind::Remote { role: Role::Peer };
        assert_eq!(authorize(peer, methods::QUEUE_COMMAND), Decision::Allow);
        assert_eq!(
            authorize(peer, methods::WATCH_DOC_MESSAGES),
            Decision::Allow
        );
        assert!(matches!(
            authorize(peer, methods::SET_PI_CREDENTIAL),
            Decision::Deny(_)
        ));
        assert!(matches!(
            authorize(peer, methods::APPLY_UPDATE),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn remote_peer_cannot_open_terminal_but_admin_can() {
        let peer = CallerKind::Remote { role: Role::Peer };
        assert!(matches!(
            authorize(peer, methods::OPEN_TERMINAL),
            Decision::Deny(_)
        ));
        let admin = CallerKind::Remote { role: Role::Admin };
        assert_eq!(authorize(admin, methods::OPEN_TERMINAL), Decision::Allow);
        assert!(matches!(
            authorize(peer, methods::MUTATE),
            Decision::Deny(_)
        ));
        assert_eq!(authorize(admin, methods::MUTATE), Decision::Allow);
    }

    #[test]
    fn admin_can_configure_pi() {
        let admin = CallerKind::Remote { role: Role::Admin };
        assert_eq!(
            authorize(admin, methods::SET_PI_CREDENTIAL),
            Decision::Allow
        );
        assert_eq!(authorize(admin, methods::UPLOAD_COMMIT), Decision::Allow);
    }

    #[test]
    fn unknown_method_denied_for_remote() {
        let peer = CallerKind::Remote { role: Role::Peer };
        assert!(matches!(
            authorize(peer, "SomeBrandNewMethod"),
            Decision::Deny(_)
        ));
        // But allowed for local (the service will then return UnknownMethod).
        assert_eq!(
            authorize(CallerKind::Local, "SomeBrandNewMethod"),
            Decision::Allow
        );
    }
}

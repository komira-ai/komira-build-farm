//! `GET /v1/nodes/{id}/storage-health`: a storage host's own storage health, and nothing
//! else of the storage (`docs/api.md`).
//!
//! A host's updater asks it, over mutual TLS on the worker listener, to judge the update of its
//! storage process. The listener lets a host's updater make this call for the node in its
//! certificate only ([`super`]), so the answer is built to hold nothing another node could use
//! to act:
//!
//! - `half`: how the node stands to the storage members. `member` (its storage half is the
//!   member whose address it enrolled with, joined as the controller and safe-to-stop join
//!   them), `no_storage` (it enrolled with no storage address), `unknown_node` (the
//!   enrollment registry does not know it) or `unlinked` (no member, or more than one, has
//!   its address; `why` says which, and may repeat the address the node itself enrolled with).
//! - `member`: for a `member` node only, that member's own `id`, `state` and `build_commit`.
//!   No other member is named, and no other node's address.
//! - `storage`: two numbers about the storage as a whole, `members_active` and
//!   `under_replicated_blobs` (`null` while unknown, never zero), as `GET /v1/storage`
//!   counts them.
//!
//! A read of the members and the registry: it changes nothing. A server that keeps no state
//! store keeps no members and answers `404`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::Response;
use kbf_map::MemberId;
use kbf_state::Store;
use kbf_types::WorkerId;
use serde::Serialize;
use tokio::task::JoinError;

use crate::api::{error, json, member_failure, no_members};
use crate::members::{Members, MembersLook, MembershipError};
use crate::storage_gate::{Link, advertised, link};
use crate::storage_health::StorageHealth;

/// How a node stands to the storage members.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Half {
    /// Its storage half is a member (the `member` field).
    Member,
    /// It enrolled with no storage address.
    NoStorage,
    /// The enrollment registry does not know it.
    UnknownNode,
    /// No member, or more than one, has its address.
    Unlinked,
}

/// The node's own storage member.
#[derive(Debug, PartialEq, Eq, Serialize)]
struct OwnMember {
    id: u64,
    state: &'static str,
    /// What its registration said of its build; `null` if it said none or its row lapsed.
    build_commit: Option<String>,
}

/// The two numbers about the storage as a whole.
#[derive(Debug, PartialEq, Eq, Serialize)]
struct Whole {
    members_active: usize,
    under_replicated_blobs: Option<u64>,
}

/// The answer.
#[derive(Debug, PartialEq, Eq, Serialize)]
struct Own {
    node: String,
    half: Half,
    #[serde(skip_serializing_if = "Option::is_none")]
    why: Option<String>,
    member: Option<OwnMember>,
    storage: Whole,
}

/// The answer for `host`, from the addresses nodes enrolled with (`adverts`), the members, what
/// each registered of its build and the storage health.
fn own_view(
    host: &WorkerId,
    adverts: &BTreeMap<String, String>,
    look: &MembersLook,
    commits: &BTreeMap<MemberId, Option<String>>,
    health: &StorageHealth,
) -> Own {
    let storage = Whole {
        members_active: health.members_active,
        under_replicated_blobs: health.under_replicated_blobs,
    };
    let node = host.to_string();
    let (half, why, member) = if adverts.contains_key(host.as_str()) {
        match link(host, adverts, look) {
            Link::NoStorage => (Half::NoStorage, None, None),
            Link::Member(m) => {
                let build_commit = commits.get(&m.id).cloned().flatten();
                let own = OwnMember {
                    id: m.id.0,
                    state: m.state.name(),
                    build_commit,
                };
                (Half::Member, None, Some(own))
            }
            Link::Unlinked(why) => (Half::Unlinked, Some(why), None),
        }
    } else {
        let why = format!("{host} is not in the enrollment registry");
        (Half::UnknownNode, Some(why), None)
    };
    Own {
        node,
        half,
        why,
        member,
        storage,
    }
}

/// Why the read failed: the status and the text.
#[derive(Debug)]
struct Failure(StatusCode, String);

impl From<MembershipError> for Failure {
    fn from(e: MembershipError) -> Self {
        Self(member_failure(&e), e.to_string())
    }
}

impl Failure {
    /// The registry could not be read (the text says so).
    fn registry(why: String) -> Self {
        Self(StatusCode::SERVICE_UNAVAILABLE, why)
    }
}

/// The blocking read: the store's calls block (its trait says so).
fn read(host: &WorkerId, members: &Members, store: &dyn Store) -> Result<Own, Failure> {
    let look = members.look()?;
    let commits = members
        .node_facts()?
        .into_iter()
        .map(|(id, facts)| (id, facts.build_commit))
        .collect();
    let health = members.storage_health()?;
    let adverts = advertised(store).map_err(Failure::registry)?;
    Ok(own_view(host, &adverts, &look, &commits, &health))
}

/// The answer to the call, from what the blocking read returned.
fn answer(read: Result<Result<Own, Failure>, JoinError>) -> Response {
    match read {
        Ok(Ok(own)) => json(StatusCode::OK, &own),
        Ok(Err(Failure(code, why))) => error(code, &why),
        Err(e) => {
            let why = format!("storage health task: {e}");
            error(StatusCode::INTERNAL_SERVER_ERROR, &why)
        }
    }
}

/// The answer for `host`, which the caller has checked is the node of the certificate. `None`
/// for either of `members` and `store` is a server that keeps no members.
pub(super) async fn one(
    host: WorkerId,
    members: Option<Arc<Members>>,
    store: Option<Arc<dyn Store>>,
) -> Response {
    let (Some(members), Some(store)) = (members, store) else {
        return no_members();
    };
    answer(tokio::task::spawn_blocking(move || read(&host, &members, store.as_ref())).await)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kbf_map::MemberState;

    use super::*;
    use crate::members::MemberLook;

    fn member(id: u64, addr: &str, state: MemberState) -> MemberLook {
        MemberLook {
            id: MemberId(id),
            addr: addr.to_owned(),
            state,
            boot: 1,
            planned_down: false,
            heard_ago: Some(Duration::from_secs(1)),
            repair_debt: Some(0),
            open_writes: Some(0),
            reported_generation: Some(1),
            marked_ago: None,
        }
    }

    fn look(members: Vec<MemberLook>) -> MembersLook {
        MembersLook {
            generation: 1,
            members,
        }
    }

    fn health(members_active: usize, owed: Option<u64>) -> StorageHealth {
        StorageHealth {
            members_total: 3,
            members_active,
            not_active: Vec::new(),
            silent: Vec::new(),
            unreported: Vec::new(),
            debts_total: 0,
            under_replicated_blobs: owed,
            nodes: Vec::new(),
        }
    }

    fn adverts() -> BTreeMap<String, String> {
        [
            ("st1", "192.0.2.1:8982"),
            ("st2", "192.0.2.2:8982"),
            ("st3", "192.0.2.9:8982"),
            ("st4", "192.0.2.4:8982"),
            ("dm1", ""),
        ]
        .into_iter()
        .map(|(id, addr)| (id.to_owned(), addr.to_owned()))
        .collect()
    }

    fn members() -> MembersLook {
        look(vec![
            member(1, "192.0.2.1:8982", MemberState::Active),
            member(2, "192.0.2.2:8982", MemberState::Down),
            member(4, "192.0.2.4:8982", MemberState::Active),
            member(5, "192.0.2.4:8982", MemberState::Down),
        ])
    }

    fn view(id: &str) -> Own {
        let commits = BTreeMap::from([
            (MemberId(1), Some("abc123".to_owned())),
            (MemberId(2), None),
        ]);
        own_view(
            &WorkerId::new(id),
            &adverts(),
            &members(),
            &commits,
            &health(2, Some(0)),
        )
    }

    /// A node is shown its own member (state and build), joined by the address it enrolled
    /// with, and the two storage-wide numbers; no other member appears.
    #[test]
    fn a_node_sees_its_own_member_and_the_storage_numbers() {
        let own = view("st1");
        assert_eq!(own.half, Half::Member);
        assert_eq!(
            own.member,
            Some(OwnMember {
                id: 1,
                state: "active",
                build_commit: Some("abc123".to_owned())
            })
        );
        assert_eq!(own.why, None);
        assert_eq!(
            own.storage,
            Whole {
                members_active: 2,
                under_replicated_blobs: Some(0)
            }
        );
        let wire = serde_json::to_value(&own).expect("JSON");
        assert_eq!(
            wire,
            serde_json::json!({
                "node": "st1",
                "half": "member",
                "member": {"id": 1, "state": "active", "build_commit": "abc123"},
                "storage": {"members_active": 2, "under_replicated_blobs": 0},
            })
        );
        let text = wire.to_string();
        for other in ["192.0.2", "st2", "\"id\":2"] {
            assert!(!text.contains(other), "{other}: {text}");
        }
    }

    /// A member that never said its build shows `null`, and a number the storage cannot say
    /// stays `null` and is not zero.
    #[test]
    fn what_is_unknown_stays_null() {
        let own = view("st2");
        assert_eq!(own.member.expect("a member").build_commit, None);
        let unknown = own_view(
            &WorkerId::new("st2"),
            &adverts(),
            &members(),
            &BTreeMap::new(),
            &health(0, None),
        );
        assert_eq!(unknown.storage.under_replicated_blobs, None);
        let wire = serde_json::to_value(&unknown).expect("JSON");
        assert!(wire["storage"]["under_replicated_blobs"].is_null());
    }

    /// Each way a node can fail to have a member is its own answer with no member in it: no
    /// storage address, an address no member registered, an address two members registered,
    /// and a node the registry does not know.
    #[test]
    fn a_node_without_a_member_is_told_why() {
        for (id, half, why) in [
            ("dm1", Half::NoStorage, None),
            ("st3", Half::Unlinked, Some("no storage member registered")),
            ("st4", Half::Unlinked, Some("more than one storage member")),
            ("ghost", Half::UnknownNode, Some("not in the enrollment")),
        ] {
            let own = view(id);
            assert_eq!(own.half, half, "{id}");
            assert_eq!(own.member, None, "{id}");
            match why {
                None => assert_eq!(own.why, None, "{id}"),
                Some(text) => {
                    let said = own.why.unwrap_or_default();
                    assert!(said.contains(text), "{id}: {said}");
                    // Only the node's own address may appear, never another node's.
                    for other in ["192.0.2.1", "192.0.2.2"] {
                        assert!(!said.contains(other), "{id}: {said}");
                    }
                }
            }
        }
    }

    async fn body(answer: Response) -> (StatusCode, serde_json::Value) {
        let code = answer.status();
        let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
            .await
            .expect("a body");
        (code, serde_json::from_slice(&bytes).expect("JSON"))
    }

    /// A failed read keeps its status and text, and a panicked task is a 500.
    #[tokio::test]
    async fn each_outcome_is_answered_with_its_status() {
        let (code, said) = body(answer(Ok(Ok(view("st1"))))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(said["half"], "member");

        let down = Failure::registry("the registry is down".to_owned());
        let (code, said) = body(answer(Ok(Err(down)))).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(said["error"].as_str().unwrap_or_default().contains("down"));

        let broken = MembershipError::NotRegistered(MemberId(1));
        let (code, _) = body(answer(Ok(Err(Failure::from(broken))))).await;
        assert_eq!(code, StatusCode::NOT_FOUND);

        let panicked =
            tokio::task::spawn_blocking(|| -> Result<Own, Failure> { panic!("the read panicked") })
                .await;
        let (code, said) = body(answer(panicked)).await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(said["error"].as_str().unwrap_or_default().contains("task"));
    }
}

use kbf_map::{Map, Member, MemberId, MemberState, Rule};

use crate::StorageError;
use crate::replicate::check_generation;

/// Catches an off-by-one at the generation boundary, and 0 ("no map seen") treated as
/// current: behind is stale, equal is served, ahead is unavailable.
#[test]
fn behind_is_stale_equal_is_served_ahead_is_unavailable() {
    let member = Member {
        id: MemberId(1),
        addr: "127.0.0.1:1".to_owned(),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    let map = Map::new(7, vec![member], Rule::Rendezvous, 1, 1, Vec::new()).expect("map");
    for behind in [0, 1, 6] {
        assert!(matches!(
            check_generation(&map, behind),
            Err(StorageError::StaleMap { current: 7 })
        ));
    }
    assert!(check_generation(&map, 7).is_ok());
    for ahead in [8, u64::MAX] {
        assert!(matches!(
            check_generation(&map, ahead),
            Err(StorageError::Unavailable(_))
        ));
    }
}

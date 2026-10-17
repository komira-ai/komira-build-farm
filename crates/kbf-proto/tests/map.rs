//! `kbf.map.v1` and `kbf_map`: lossless both ways, strict on the way in.
//!
//! Catches:
//! - a state, rule or field lost or swapped in either direction (every state round trips,
//!   a whole map round trips);
//! - an unspecified or unknown enum value, or a replication factor that does not fit a
//!   `u8`, read as a default instead of refused;
//! - a map the domain refuses getting in through the wire;
//! - the generation moving off field 1.

use kbf_map::{DenyEntry, Map, MapError, Member, MemberId, MemberState, Rule};
use kbf_proto::MapDecodeError;
use kbf_proto::kbf::map::v1 as pb;
use prost::Message;

const STATES: [MemberState; 5] = [
    MemberState::Joining,
    MemberState::CatchingUp,
    MemberState::Active,
    MemberState::Draining,
    MemberState::Down,
];

fn member(id: u64, state: MemberState) -> Member {
    Member {
        id: MemberId(id),
        addr: format!("host-{id}:7000"),
        zone: if id.is_multiple_of(2) {
            "a".to_owned()
        } else {
            String::new()
        },
        weight: 1 + id as u32,
        state,
    }
}

fn map_of_all_states() -> Map {
    let members = STATES
        .iter()
        .enumerate()
        .map(|(i, s)| member(i as u64 + 1, *s))
        .collect();
    Map::new(
        9,
        members,
        Rule::Rendezvous,
        2,
        4,
        vec![
            DenyEntry::Serial("a1b".to_owned()),
            DenyEntry::Node("mac-07".to_owned()),
        ],
    )
    .unwrap()
}

#[test]
fn every_state_and_the_whole_map_round_trip_through_bytes() {
    let map = map_of_all_states();
    let wire = pb::Map::from(&map);
    for (wire_member, state) in wire.members.iter().zip(STATES) {
        assert_eq!(Member::try_from(wire_member.clone()).unwrap().state, state);
    }
    let decoded = pb::Map::decode(wire.encode_to_vec().as_slice()).unwrap();
    assert_eq!(Map::try_from(decoded).unwrap(), map);
}

#[test]
fn distinct_states_have_distinct_numbers() {
    let numbers: Vec<i32> = STATES
        .iter()
        .map(|s| pb::MemberState::from(*s) as i32)
        .collect();
    assert_eq!(numbers, [1, 2, 3, 4, 5]);
}

#[test]
fn an_unspecified_or_unknown_state_is_refused() {
    for bad in [0, 6, -1, 99] {
        let mut m = pb::Member::from(&member(1, MemberState::Active));
        m.state = bad;
        assert_eq!(Member::try_from(m), Err(MapDecodeError::UnknownState(bad)));
    }
}

#[test]
fn an_unspecified_or_unknown_rule_is_refused() {
    for bad in [0, 2, 77] {
        let mut m = pb::Map::from(&map_of_all_states());
        m.rule = bad;
        assert_eq!(Map::try_from(m), Err(MapDecodeError::UnknownRule(bad)));
    }
}

#[test]
fn a_replication_factor_above_255_is_refused() {
    let mut m = pb::Map::from(&map_of_all_states());
    m.replication = 256;
    assert_eq!(Map::try_from(m), Err(MapDecodeError::Replication(256)));
}

#[test]
fn a_member_with_a_bad_state_refuses_the_map() {
    let mut m = pb::Map::from(&map_of_all_states());
    m.members[2].state = 42;
    assert_eq!(Map::try_from(m), Err(MapDecodeError::UnknownState(42)));
}

#[test]
fn a_map_the_domain_refuses_is_refused_with_the_reason() {
    let mut m = pb::Map::from(&map_of_all_states());
    m.generation = 0;
    let e = Map::try_from(m).unwrap_err();
    assert_eq!(e, MapDecodeError::Invalid(MapError::ZeroGeneration));
    assert_eq!(
        e.to_string(),
        "invalid map: generation 0 is reserved for \"no map seen\""
    );
    assert_eq!(
        MapDecodeError::UnknownState(8).to_string(),
        "unknown member state 8"
    );
    assert_eq!(
        MapDecodeError::UnknownRule(8).to_string(),
        "unknown placement rule 8"
    );
    assert_eq!(
        MapDecodeError::Replication(300).to_string(),
        "replication factor 300 is above 255"
    );
}

// Catches: the generation moved off field 1 (tag 0x08, varint) or the members off field 2.
#[test]
fn the_generation_is_field_one() {
    let wire = pb::Map {
        generation: 5,
        members: vec![pb::Member::default()],
        ..Default::default()
    };
    let bytes = wire.encode_to_vec();
    assert_eq!(&bytes[..2], [0x08, 5]);
    assert_eq!(bytes[2], 0x12, "members are field 2, length-delimited");
}

// Catches: a denied entry that is not read through the one parser, a spelling kept on the
// wire after decoding, a malformed entry accepted, or a repeat that differs only in
// spelling slipping past the duplicate check.
#[test]
fn denied_entries_are_parsed_canonically_and_refused_when_bad_or_repeated() {
    let mut wire = pb::Map::from(&map_of_all_states());
    assert_eq!(
        wire.denied,
        ["serial a1b", "node mac-07"],
        "the canonical form"
    );
    wire.denied = vec!["serial 00:A1:B".to_owned()];
    let map = Map::try_from(wire.clone()).unwrap();
    assert_eq!(pb::Map::from(&map).denied, ["serial a1b"]);

    wire.denied = vec!["serial xyz".to_owned()];
    let e = Map::try_from(wire.clone()).unwrap_err();
    assert!(matches!(e, MapDecodeError::Denied { .. }), "{e}");
    assert_eq!(
        e.to_string(),
        "denied entry \"serial xyz\": a serial is hex digits, colons allowed: \"xyz\""
    );

    wire.denied = vec!["serial a1b".to_owned(), "serial 0A:1B".to_owned()];
    assert_eq!(
        Map::try_from(wire).unwrap_err(),
        MapDecodeError::Invalid(MapError::DuplicateDenied(DenyEntry::Serial(
            "a1b".to_owned()
        )))
    );
}

//! A map is refused unless it can place a digest. One test per refusal, each checking the
//! error and its text, plus the accepted map and its accessors.

mod common;

use common::member;
use kbf_map::{DenyEntry, Map, MapError, Member, MemberId, MemberState::Active, Rule};

fn build(generation: u64, members: Vec<Member>, r: u8, deny: &[&str]) -> Result<Map, MapError> {
    let deny = deny
        .iter()
        .map(|s| s.parse().expect("a deny entry"))
        .collect();
    Map::new(generation, members, Rule::Rendezvous, r, 3, deny)
}

fn two() -> Vec<Member> {
    vec![member(1, 1, Active), member(2, 5, Active)]
}

// Catches: a refusal that is lost, or a valid map refused; and accessors that do not
// return what the map was built with.
#[test]
fn a_valid_map_keeps_what_it_was_given() {
    let m = build(7, two(), 2, &["node old-node"]).unwrap();
    assert_eq!((m.generation(), m.r(), m.min_protocol()), (7, 2, 3));
    assert_eq!(m.rule(), Rule::Rendezvous);
    assert_eq!(m.members(), &two()[..]);
    assert_eq!(m.denied(), [DenyEntry::Node("old-node".to_owned())]);
    assert_eq!(m.member(MemberId(2)).map(|x| x.weight), Some(5));
    assert_eq!(m.member(MemberId(9)), None);
    assert_eq!(MemberId(2).to_string(), "m2");
}

// Catches: generation 0 accepted ("no map seen" on the wire would be a real map).
#[test]
fn generation_zero_is_refused() {
    let e = build(0, two(), 1, &[]).unwrap_err();
    assert_eq!(e, MapError::ZeroGeneration);
    assert!(e.to_string().contains("generation 0"));
}

// Catches: a replication factor of 0, which would own nothing.
#[test]
fn replication_zero_is_refused() {
    let e = build(1, two(), 0, &[]).unwrap_err();
    assert_eq!(e, MapError::ZeroReplication);
    assert!(e.to_string().contains("factor 0"));
}

// Catches: more owners than members (and an empty member list at any r).
#[test]
fn replication_above_the_member_count_is_refused() {
    let e = build(1, two(), 3, &[]).unwrap_err();
    assert_eq!(e, MapError::ReplicationExceedsMembers { r: 3, members: 2 });
    assert!(e.to_string().contains("exceeds the 2 members"));
    assert!(build(1, Vec::new(), 1, &[]).is_err());
}

// Catches: two members with one id (the second would shadow the first).
#[test]
fn a_duplicate_member_id_is_refused() {
    let members = vec![
        member(4, 1, Active),
        member(5, 1, Active),
        member(4, 2, Active),
    ];
    let e = build(1, members, 1, &[]).unwrap_err();
    assert_eq!(e, MapError::DuplicateMember(MemberId(4)));
    assert!(e.to_string().contains("m4 appears twice"));
}

// Catches: a weight of 0, a member that can never win (and a score of 0 / -ln u).
#[test]
fn a_zero_weight_is_refused() {
    let e = build(1, vec![member(1, 1, Active), member(2, 0, Active)], 1, &[]).unwrap_err();
    assert_eq!(e, MapError::ZeroWeight(MemberId(2)));
    assert!(e.to_string().contains("m2 has weight 0"));
}

// Catches: a member nobody can dial.
#[test]
fn an_empty_address_is_refused() {
    let mut blank = member(2, 1, Active);
    blank.addr.clear();
    let e = build(1, vec![member(1, 1, Active), blank], 1, &[]).unwrap_err();
    assert_eq!(e, MapError::EmptyAddr(MemberId(2)));
    assert!(e.to_string().contains("m2 has no address"));
}

// Catches: a deny list that names one thing twice, including two spellings of one serial
// (they are equal once canonical), while one value under two kinds is two entries.
#[test]
fn a_duplicate_deny_entry_is_refused() {
    let e = build(1, two(), 1, &["node a", "node b", "node a"]).unwrap_err();
    assert_eq!(
        e,
        MapError::DuplicateDenied(DenyEntry::Node("a".to_owned()))
    );
    assert!(e.to_string().contains("\"node a\" appears twice"));
    let e = build(1, two(), 1, &["serial 0A:1B", "serial 1b2c", "serial a1b"]).unwrap_err();
    assert_eq!(
        e,
        MapError::DuplicateDenied(DenyEntry::Serial("a1b".to_owned()))
    );
    assert!(build(1, two(), 1, &["node ab", "serial ab"]).is_ok());
}

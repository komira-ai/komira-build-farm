//! The in-memory backend, the fault injector and the values they speak in.

use std::sync::Arc;
use std::time::Duration;

use tierstore::{Backend, ByteRange, Error, FaultyBackend, MemoryBackend, Op};

use crate::support::{body, check_ranges, keys};

/// Asserts that a second put of a key is refused with `AlreadyExists` and leaves the first
/// object's bytes in place, on two separate backends (the hot and the cold tier are two).
#[tokio::test]
async fn a_second_put_of_a_key_is_refused_and_changes_nothing() {
    let first = MemoryBackend::new();
    let second = MemoryBackend::new();
    for backend in [&first, &second] {
        backend.put("k", body("one")).await.unwrap();
        let again = backend.put("k", body("two")).await;
        assert_eq!(again, Err(Error::AlreadyExists("k".to_owned())));
        assert_eq!(backend.get("k", None).await.unwrap(), body("one"));
    }
}

/// Asserts that ranges are byte-exact on a backend: the first byte, the last byte, a slice
/// in the middle, a range running past the end (cut at the end), empty ranges inside the
/// object and at its end, and an error for a range starting past the end.
#[tokio::test]
async fn ranges_are_byte_exact() {
    let backend = MemoryBackend::new();
    backend.put("k", body("0123456789")).await.unwrap();
    check_ranges(&backend, "k").await;
}

/// Asserts that a range reports the offset and the size it was made with.
#[test]
fn a_range_reports_what_it_was_made_with() {
    let range = ByteRange::new(3, 4);
    assert_eq!(range.offset(), 3);
    assert_eq!(range.size(), 4);
}

/// Asserts that head gives the size and a content tag that is equal for equal bytes and
/// different for different bytes of the same size.
#[tokio::test]
async fn head_gives_the_size_and_a_tag_of_the_content() {
    let backend = MemoryBackend::new();
    backend.put("a", body("same")).await.unwrap();
    backend.put("b", body("same")).await.unwrap();
    backend.put("c", body("diff")).await.unwrap();
    let a = backend.head("a").await.unwrap();
    assert_eq!(a.size, 4);
    assert_eq!(a, backend.head("b").await.unwrap());
    assert_ne!(a, backend.head("c").await.unwrap());
}

/// Asserts that reading, heading or listing nothing gives `NotFound` or an empty listing,
/// and that deleting a key that is not there succeeds.
#[tokio::test]
async fn an_absent_key_is_not_found_and_deleting_it_succeeds() {
    let backend = MemoryBackend::new();
    let missing = Error::NotFound("k".to_owned());
    assert_eq!(backend.get("k", None).await.unwrap_err(), missing);
    assert_eq!(backend.head("k").await.unwrap_err(), missing);
    assert_eq!(backend.delete("k").await, Ok(()));
    assert_eq!(keys(&backend).await, Vec::<String>::new());
}

/// Asserts that a delete removes the object and that a deleted key can be put again.
#[tokio::test]
async fn a_deleted_key_is_gone_and_can_be_put_again() {
    let backend = MemoryBackend::new();
    backend.put("k", body("old")).await.unwrap();
    backend.delete("k").await.unwrap();
    assert_eq!(keys(&backend).await, Vec::<String>::new());
    backend.put("k", body("new")).await.unwrap();
    assert_eq!(backend.get("k", None).await.unwrap(), body("new"));
}

/// Asserts that a listing holds the keys under the prefix, in key order, with their sizes.
#[tokio::test]
async fn a_listing_holds_the_keys_under_a_prefix_in_order() {
    let backend = MemoryBackend::new();
    backend.put("b/2", body("bb")).await.unwrap();
    backend.put("a/1", body("a")).await.unwrap();
    backend.put("b/1", body("b")).await.unwrap();
    assert_eq!(keys(&backend).await, ["a/1", "b/1", "b/2"]);
    let listed = backend.list("b/").await.unwrap();
    let sizes: Vec<u64> = listed.iter().map(|object| object.size).collect();
    assert_eq!(sizes, [1, 2]);
    let names: Vec<&str> = listed.iter().map(|object| object.key.as_str()).collect();
    assert_eq!(names, ["b/1", "b/2"]);
}

/// Asserts that every error says what happened and to which key, and that only
/// `Unavailable` says the backend as a whole is out of reach.
#[test]
fn errors_say_what_happened() {
    let missing = Error::NotFound("k".to_owned());
    assert_eq!(missing.to_string(), "object k not found");
    assert!(!missing.is_unavailable());
    let exists = Error::AlreadyExists("k".to_owned());
    assert_eq!(exists.to_string(), "object k already exists");
    assert!(!exists.is_unavailable());
    let down = Error::Unavailable("no route".to_owned());
    assert_eq!(down.to_string(), "backend unavailable: no route");
    assert!(down.is_unavailable());
    let other = Error::Other("odd".to_owned());
    assert_eq!(other.to_string(), "backend error: odd");
    assert!(!other.is_unavailable());
}

fn faulty() -> FaultyBackend {
    FaultyBackend::new(Arc::new(MemoryBackend::new()))
}

/// Asserts that a fault injector with nothing planned passes every kind of call through
/// and counts each of them.
#[tokio::test]
async fn an_injector_with_no_plan_passes_calls_through_and_counts_them() {
    let backend = faulty();
    backend.put("k", body("v")).await.unwrap();
    assert_eq!(backend.get("k", None).await.unwrap(), body("v"));
    assert_eq!(backend.head("k").await.unwrap().size, 1);
    assert_eq!(keys(&backend).await, ["k"]);
    backend.delete("k").await.unwrap();
    for op in [Op::Put, Op::Get, Op::Head, Op::List, Op::Delete] {
        assert_eq!(backend.calls(op), 1, "{op:?}");
    }
}

/// Asserts that the next `n` calls of a kind fail with `Unavailable` without reaching the
/// wrapped backend, that the call after them succeeds, and that other kinds are unaffected.
#[tokio::test]
async fn the_next_n_calls_of_a_kind_fail_and_then_calls_succeed() {
    let backend = faulty();
    backend.fail_next(Op::Put, 2);
    for _ in 0..2 {
        let failed = backend.put("k", body("v")).await;
        assert_eq!(failed.map_err(|e| e.is_unavailable()), Err(true));
        assert_eq!(keys(&backend).await, Vec::<String>::new());
    }
    backend.put("k", body("v")).await.unwrap();
    assert_eq!(keys(&backend).await, ["k"]);
    assert_eq!(backend.calls(Op::Put), 3);
}

/// Asserts that every kind of call can be failed on its own, and that a failed call leaves
/// the stored objects as they were.
#[tokio::test]
async fn every_kind_of_call_can_be_failed() {
    let backend = faulty();
    backend.put("k", body("v")).await.unwrap();
    for op in [Op::Put, Op::Get, Op::Head, Op::List, Op::Delete] {
        backend.fail_next(op, 1);
    }
    let put = backend.put("j", body("v")).await.unwrap_err();
    let get = backend.get("k", None).await.unwrap_err();
    let head = backend.head("k").await.unwrap_err();
    let list = backend.list("").await.unwrap_err();
    let delete = backend.delete("k").await.unwrap_err();
    for error in [put, get, head, list, delete] {
        assert!(error.is_unavailable(), "{error}");
    }
    assert_eq!(keys(&backend).await, ["k"]);
}

/// Asserts that a backend that is down fails every kind of call until it is up again.
#[tokio::test]
async fn a_backend_that_is_down_fails_every_call_until_it_is_up() {
    let backend = faulty();
    backend.set_down(true);
    assert!(backend.put("k", body("v")).await.is_err());
    assert!(backend.list("").await.is_err());
    backend.set_down(false);
    backend.put("k", body("v")).await.unwrap();
    assert_eq!(keys(&backend).await, ["k"]);
}

/// Asserts that a delayed call waits the delay before it runs, that other kinds do not, and
/// that clearing the delay stops it.
#[tokio::test(start_paused = true)]
async fn a_delayed_call_waits_before_it_runs() {
    let backend = faulty();
    backend.set_delay(Op::Put, Some(Duration::from_secs(5)));
    let start = tokio::time::Instant::now();
    backend.put("k", body("v")).await.unwrap();
    let waited = start.elapsed();
    assert!(waited >= Duration::from_secs(5));
    backend.get("k", None).await.unwrap();
    backend.set_delay(Op::Put, None);
    backend.put("j", body("v")).await.unwrap();
    assert_eq!(start.elapsed(), waited);
}

use std::cell::RefCell;

use super::*;

#[test]
fn a_status_says_whether_to_delete_again_stop_or_fail() {
    assert_eq!(after_delete(true, 0), Deleted::Some);
    assert_eq!(after_delete(false, NOT_FOUND), Deleted::Nothing);
    // A process without the entitlement cannot reach the data-protection keychain.
    assert_eq!(after_delete(true, MISSING_ENTITLEMENT), Deleted::Nothing);
    // No file keychain for the user: there is none to hold an item.
    for status in [-25294, -25307, -25291, -25308, -60008] {
        assert_eq!(after_delete(false, status), Deleted::Nothing, "{status}");
        // The data-protection keychain does not answer so: that is a failure.
        assert_eq!(
            after_delete(true, status),
            Deleted::Failed(status),
            "{status}"
        );
    }
    assert_eq!(after_delete(true, -25293), Deleted::Failed(-25293));
    assert_eq!(after_delete(false, -50), Deleted::Failed(-50));
}

#[test]
fn a_status_says_whether_an_item_is_left() {
    assert_eq!(after_find(true, 0), Left::Items);
    assert_eq!(after_find(true, NOT_FOUND), Left::None);
    assert_eq!(after_find(true, MISSING_ENTITLEMENT), Left::None);
    assert_eq!(after_find(false, -25294), Left::None);
    assert_eq!(after_find(true, -25294), Left::Failed(-25294));
    assert_eq!(after_find(false, -50), Left::Failed(-50));
}

#[test]
fn every_class_is_deleted_in_the_data_protection_keychain_and_checked_and_nothing_left_is_clean() {
    let deleted = RefCell::new(Vec::new());
    let found = RefCell::new(Vec::new());
    let failed = wipe(
        &mut |class, dp| {
            deleted.borrow_mut().push((class, dp));
            NOT_FOUND
        },
        &mut |class, dp| {
            found.borrow_mut().push((class, dp));
            NOT_FOUND
        },
    );
    assert!(failed.is_empty(), "{failed:?}");
    let all: Vec<(usize, bool)> = (0..CLASSES.len()).map(|c| (c, true)).collect();
    assert_eq!(*deleted.borrow(), all);
    assert_eq!(*found.borrow(), all);
}

#[test]
fn items_that_survive_or_a_failure_fail_naming_the_class() {
    // Class 0: one deletion removes an item, the next finds none, but the check still sees
    // one. Class 3: the deletion fails. Class 2: it never stops returning items. Class 4:
    // the check fails.
    let rounds = RefCell::new(0);
    let failed = wipe(
        &mut |class, _| match class {
            0 => {
                let mut n = rounds.borrow_mut();
                *n += 1;
                if *n == 1 { 0 } else { NOT_FOUND }
            }
            3 => -50,
            2 => 0,
            _ => NOT_FOUND,
        },
        &mut |class, _| match class {
            0 => 0,
            4 => -50,
            _ => NOT_FOUND,
        },
    );
    assert_eq!(
        *rounds.borrow(),
        2,
        "asked again after it deleted something"
    );
    assert_eq!(failed.len(), 4, "{failed:?}");
    assert!(
        failed
            .iter()
            .any(|l| l == "generic password in the data-protection keychain: items left"),
        "{failed:?}"
    );
    assert!(
        failed
            .iter()
            .any(|l| l == "key in the data-protection keychain: delete status -50"),
        "{failed:?}"
    );
    assert!(
        failed
            .iter()
            .any(|l| l
                .starts_with("certificate in the data-protection keychain: still returned after")),
        "{failed:?}"
    );
    assert!(
        failed
            .iter()
            .any(|l| l == "identity in the data-protection keychain: find status -50"),
        "{failed:?}"
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn off_macos_there_is_no_keychain_to_wipe() {
    assert!(wipe_user_keychains().is_empty());
}

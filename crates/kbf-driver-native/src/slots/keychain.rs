//! The keychain items of the pool user, deleted by the pool user.
//!
//! The per-user data-protection keychain `secd` keeps for a uid outlives the lease that
//! filled it, and an item stored there must not be readable by the next lease on the
//! same uid. The clean deletes every item of every class in the data-protection keychain,
//! as the uid (the `keychain` mode of [`super::entry`]), then asks for each class once more
//! and fails, naming what is left. The file-based keychains are not asked: the search list
//! of a user holds the system keychain too, which the user may not change (a deletion there
//! answers `errSecInvalidOwnerEdit`), and the user's own live in its home, which the home's
//! empty removes and its look checks.
//!
//! The decisions are plain functions over a status code, so they are tested on every
//! platform; the calls into Security.framework are macOS's.

/// `errSecItemNotFound`: nothing (more) matches.
pub(crate) const NOT_FOUND: i32 = -25300;
/// `errSecMissingEntitlement`: a process without the keychain entitlements has no access
/// to the data-protection keychain, and neither has any other process of the uid that
/// lacks them.
pub(crate) const MISSING_ENTITLEMENT: i32 = -34018;
/// Why there is no file-based keychain to ask: no such keychain (`errSecNoSuchKeychain`),
/// no default one (`errSecNoDefaultKeychain`), not available (`errSecNotAvailable`), or
/// the interaction it would need is not allowed (`errSecInteractionNotAllowed`), or the
/// CSSM answer for a missing one (-60008). A user with
/// no file keychain has none to leave an item in.
const NO_FILE_KEYCHAIN: [i32; 5] = [-25294, -25307, -25291, -25308, -60008];

/// The classes of item, by the name a failure gives.
pub(crate) const CLASSES: [&str; 5] = [
    "generic password",
    "internet password",
    "certificate",
    "key",
    "identity",
];

/// What a status says of a deletion.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Deleted {
    /// Something was deleted: ask again.
    Some,
    /// Nothing is left, or there is nothing to reach.
    Nothing,
    /// The deletion failed with this status.
    Failed(i32),
}

/// What a status says of the question whether any item is left.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Left {
    Items,
    None,
    Failed(i32),
}

/// Whether `status` says this process cannot reach the keychain at all (`dp`: the
/// data-protection one), so that there is nothing it could delete from it.
fn unreachable_keychain(dp: bool, status: i32) -> bool {
    status == MISSING_ENTITLEMENT || (!dp && NO_FILE_KEYCHAIN.contains(&status))
}

/// [`Deleted`] from the status of `SecItemDelete`.
pub(crate) fn after_delete(dp: bool, status: i32) -> Deleted {
    match status {
        0 => Deleted::Some,
        NOT_FOUND => Deleted::Nothing,
        other if unreachable_keychain(dp, other) => Deleted::Nothing,
        other => Deleted::Failed(other),
    }
}

/// [`Left`] from the status of `SecItemCopyMatching`.
pub(crate) fn after_find(dp: bool, status: i32) -> Left {
    match status {
        0 => Left::Items,
        NOT_FOUND => Left::None,
        other if unreachable_keychain(dp, other) => Left::None,
        other => Left::Failed(other),
    }
}

/// How many times a class is deleted before it is said to keep returning items.
pub(crate) const ROUNDS: usize = 8;

/// Deletes every item of each class in the data-protection keychain through `delete(class index, dp)` (a status) and checks
/// with `find(class index, dp)`: the lines of what failed or is left, none when the user's
/// keychains hold nothing.
pub(crate) fn wipe(
    delete: &mut dyn FnMut(usize, bool) -> i32,
    find: &mut dyn FnMut(usize, bool) -> i32,
) -> Vec<String> {
    let mut failed = Vec::new();
    let dp = true;
    for (index, class) in CLASSES.iter().enumerate() {
        let mut rounds = 0;
        loop {
            match after_delete(dp, delete(index, dp)) {
                Deleted::Some if rounds + 1 < ROUNDS => rounds += 1,
                Deleted::Some => {
                    failed.push(format!(
                        "{class} in the data-protection keychain: still returned after {ROUNDS} deletions"
                    ));
                    break;
                }
                Deleted::Nothing => break,
                Deleted::Failed(status) => {
                    failed.push(format!(
                        "{class} in the data-protection keychain: delete status {status}"
                    ));
                    break;
                }
            }
        }
        match after_find(dp, find(index, dp)) {
            Left::None => {}
            Left::Items => {
                failed.push(format!(
                    "{class} in the data-protection keychain: items left"
                ));
            }
            Left::Failed(status) => failed.push(format!(
                "{class} in the data-protection keychain: find status {status}"
            )),
        }
    }
    failed
}

/// Deletes the calling user's keychain items: the lines of what could not be deleted or is
/// left. Off macOS there is no keychain.
#[cfg(not(target_os = "macos"))]
pub(crate) fn wipe_user_keychains() -> Vec<String> {
    Vec::new()
}

#[cfg(target_os = "macos")]
pub(crate) fn wipe_user_keychains() -> Vec<String> {
    real::wipe_all()
}

#[cfg(target_os = "macos")]
mod real {
    use std::ffi::c_void;

    type CfRef = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFBooleanTrue: CfRef;
        static kCFBooleanFalse: CfRef;
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;
        fn CFDictionaryCreateMutable(
            allocator: CfRef,
            capacity: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> *mut c_void;
        fn CFDictionarySetValue(dict: *mut c_void, key: CfRef, value: CfRef);
        fn CFRelease(item: CfRef);
    }

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecClass: CfRef;
        static kSecClassGenericPassword: CfRef;
        static kSecClassInternetPassword: CfRef;
        static kSecClassCertificate: CfRef;
        static kSecClassKey: CfRef;
        static kSecClassIdentity: CfRef;
        static kSecUseDataProtectionKeychain: CfRef;
        static kSecMatchLimit: CfRef;
        static kSecMatchLimitAll: CfRef;
        fn SecItemDelete(query: CfRef) -> i32;
        fn SecItemCopyMatching(query: CfRef, result: *mut CfRef) -> i32;
    }

    /// Runs `call` on a query for the class `class` (an index of [`super::CLASSES`]) in the
    /// data-protection keychain (`dp`) or the file one, matching every item.
    fn with_query(class: usize, dp: bool, call: impl FnOnce(CfRef) -> i32) -> i32 {
        // SAFETY: the Core Foundation and Security constants are valid for the life of the
        // process; the dictionary is created with the standard CFType callbacks, so it
        // retains its keys and values, is used only inside this function and is released
        // at its end; `call` is given the dictionary and does not keep it.
        unsafe {
            let classes = [
                kSecClassGenericPassword,
                kSecClassInternetPassword,
                kSecClassCertificate,
                kSecClassKey,
                kSecClassIdentity,
            ];
            let query = CFDictionaryCreateMutable(
                std::ptr::null(),
                0,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            );
            CFDictionarySetValue(query, kSecClass, classes[class]);
            CFDictionarySetValue(
                query,
                kSecUseDataProtectionKeychain,
                if dp { kCFBooleanTrue } else { kCFBooleanFalse },
            );
            CFDictionarySetValue(query, kSecMatchLimit, kSecMatchLimitAll);
            let status = call(query.cast_const());
            CFRelease(query.cast_const());
            status
        }
    }

    pub(super) fn wipe_all() -> Vec<String> {
        super::wipe(
            // SAFETY: `query` is the live dictionary `with_query` made.
            &mut |class, dp| with_query(class, dp, |query| unsafe { SecItemDelete(query) }),
            &mut |class, dp| {
                with_query(class, dp, |query| {
                    let mut found: CfRef = std::ptr::null();
                    // SAFETY: `query` is the live dictionary; no result is asked for, so
                    // `found` stays unset.
                    unsafe { SecItemCopyMatching(query, &raw mut found) }
                })
            },
        )
    }
}

#[cfg(test)]
mod tests;

//! The planned iOS device keys: `ios.device` and every `ios.device.<attribute>` key are
//! refused until the scheduler books devices.

use kbf_caps::{FromPlatformError, NodeCaps, Request, RequestError, property_name};

fn from(props: &[(&str, &str)]) -> Result<Request, FromPlatformError> {
    Request::from_platform(props.iter().copied())
}

/// Catches: a device key ignored as an unknown property, so `ios.device=1` matches every
/// node, Linux included, and a device test runs with no device; an attribute key let
/// through, whether one the design names (`ios.device.os_version`) or one it does not
/// (`ios.device.os`); a key read only in its exact spelling; and a refusal that does not
/// name the key or the design that plans it.
#[test]
fn ios_device_keys_are_refused_in_any_case() {
    for (sent, key) in [
        ("ios.device", "ios.device"),
        ("IOS.Device", "ios.device"),
        ("ios.device.class", "ios.device.class"),
        ("ios.device.product_type", "ios.device.product_type"),
        ("ios.device.os_version", "ios.device.os_version"),
        ("iOS.Device.OS_Build", "ios.device.os_build"),
        ("ios.device.os", "ios.device.os"),
        ("ios.device.", "ios.device."),
    ] {
        assert_eq!(property_name(sent).as_deref(), Some(key), "{sent}");
        let err = from(&[("OSFamily", "darwin"), (sent, "1")]).expect_err(sent);
        assert!(
            matches!(err, FromPlatformError::Invalid(_)),
            "{sent}: {err:?}"
        );
        let message = err.to_string();
        assert!(message.contains(&format!("{key:?}")), "{sent}: {message}");
        assert!(
            message.contains("ios-devices.md#55-rollout-order"),
            "{sent}: {message}"
        );
        assert_eq!(
            err,
            FromPlatformError::Invalid(RequestError::IosDevice(key.to_owned()))
        );
        assert_eq!(
            Request::parse([(key, "1")]),
            Err(RequestError::IosDevice(key.to_owned()))
        );
    }
}

/// Catches: the refusal applied to a node report too. A newer daemon's report may carry
/// `ios.device` entries; refused, it would stop the node registering. Skipped, as every
/// report entry the server does not know is, it matches nothing.
#[test]
fn a_report_with_ios_device_entries_is_read_and_the_entries_skipped() {
    let device = "class=iPhone,id=00008110-0001,os_build=23A341,os_version=26.0";
    let with = NodeCaps::from_report([("arch", "arm64"), ("os", "macos"), ("ios.device", device)]);
    let without = NodeCaps::from_report([("arch", "arm64"), ("os", "macos")]);
    assert_eq!(with, without);
    assert!(with.is_ok(), "{with:?}");
}

/// Catches: the refusal matched on too short a prefix (`ios.device` with no `.` after
/// it, `ios.`, `ios`), so a property that only begins like a device key is refused
/// instead of left alone, as every property kbf does not read is.
#[test]
fn names_that_only_begin_like_a_device_key_are_left_alone() {
    for sent in [
        "ios.devices",
        "ios.deviceclass",
        "ios.simulator",
        "iosx",
        "ios",
    ] {
        assert_eq!(property_name(sent), None, "{sent}");
        assert_eq!(from(&[(sent, "1")]), Ok(Request::default()), "{sent}");
    }
}

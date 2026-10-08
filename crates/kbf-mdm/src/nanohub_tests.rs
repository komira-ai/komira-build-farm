use super::*;
use crate::fake_nanohub::start;

const KEY: &str = "the-one-key";
const MAC: Device<'static> = Device {
    serial: "C02SERIAL1",
    enrollment: "UDID-1",
};

#[tokio::test]
async fn enforce_posts_one_kbf_declaration_in_the_macs_own_set() {
    // Catches: a mapping that puts the enforcement in a shared set (every Mac would
    // update at once) or under a non-kbf identifier.
    let fake = start(KEY).await;
    let hub = NanoHub::new(&format!("{}/", fake.url), KEY.into());
    let enforcement = Enforcement {
        target_os_version: "27.1".into(),
        target_build_version: "27B5".into(),
        target_local_date_time: "2026-10-09T02:00:00".into(),
    };
    hub.enforce(MAC, &enforcement).await.unwrap();
    assert_eq!(
        fake.requests(),
        [
            "PUT /api/v1/ddm/declarations",
            "PUT /api/v1/ddm/set-declarations/kbf.set.C02SERIAL1?declaration=kbf.osupdate.C02SERIAL1",
            "PUT /api/v1/ddm/enrollment-sets/UDID-1?set=kbf.set.C02SERIAL1",
        ]
    );
    let declaration: serde_json::Value = serde_json::from_str(&fake.body(0)).unwrap();
    assert_eq!(
        declaration,
        serde_json::json!({
            "Type": "com.apple.configuration.softwareupdate.enforcement.specific",
            "Identifier": "kbf.osupdate.C02SERIAL1",
            "Payload": {
                "TargetOSVersion": "27.1",
                "TargetBuildVersion": "27B5",
                "TargetLocalDateTime": "2026-10-09T02:00:00",
            },
        })
    );
}

#[tokio::test]
async fn subscribe_asks_for_every_status_item_the_gate_reads() {
    let fake = start(KEY).await;
    let hub = NanoHub::new(&fake.url, KEY.into());
    hub.subscribe(MAC).await.unwrap();
    let declaration: serde_json::Value = serde_json::from_str(&fake.body(0)).unwrap();
    assert_eq!(declaration["Identifier"], SUBSCRIPTION);
    let names: Vec<_> = declaration["Payload"]["StatusItems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, STATUS_ITEMS);
    assert_eq!(
        fake.requests()[1],
        "PUT /api/v1/ddm/set-declarations/kbf.set.C02SERIAL1?declaration=kbf.status-subscriptions"
    );
}

#[tokio::test]
async fn withdraw_leaves_the_set_then_deletes_and_tolerates_a_missing_one() {
    let fake = start(KEY).await;
    let hub = NanoHub::new(&fake.url, KEY.into());
    let id = KbfDeclaration::enforcement("C02SERIAL1");
    hub.withdraw("C02SERIAL1", &id).await.unwrap();
    fake.hub.lock().unwrap().missing = true;
    hub.withdraw("C02SERIAL1", &id).await.unwrap();
    assert_eq!(
        fake.requests(),
        [
            "DELETE /api/v1/ddm/set-declarations/kbf.set.C02SERIAL1?declaration=kbf.osupdate.C02SERIAL1",
            "DELETE /api/v1/ddm/declarations/kbf.osupdate.C02SERIAL1",
        ]
        .repeat(2)
    );
}

#[tokio::test]
async fn status_reads_the_items_and_the_newest_report_time() {
    let fake = start(KEY).await;
    fake.hub.lock().unwrap().status_values = serde_json::json!({
        "UDID-1": [
            {"path": ".StatusItems.softwareupdate.failure-reason", "value": "", "timestamp": "2026-10-08T10:00:00Z"},
            {"path": ".StatusItems.device.operating-system.build-version", "value": "27B5", "timestamp": "2026-10-08T11:00:00Z"},
            {"path": "odd", "value": "x"},
        ],
    });
    let hub = NanoHub::new(&fake.url, KEY.into());
    let status = hub.status(MAC).await.unwrap();
    assert_eq!(status.last_seen, parse_rfc3339("2026-10-08T11:00:00Z"));
    assert_eq!(
        status.items["device.operating-system.build-version"],
        "27B5"
    );
    assert_eq!(status.items["softwareupdate.failure-reason"], "");
    assert_eq!(status.items["odd"], "x");
    // A Mac that never reported.
    let other = Device {
        serial: "C02OTHER",
        enrollment: "UDID-2",
    };
    assert_eq!(hub.status(other).await.unwrap(), DeviceStatus::default());
    fake.hub.lock().unwrap().status_values = serde_json::json!(["not", "a", "map"]);
    let e = hub.status(MAC).await.unwrap_err();
    assert!(e.0.starts_with("/api/v1/ddm/status-values/UDID-1: "), "{e}");
}

#[tokio::test]
async fn declarations_lists_identifiers() {
    let fake = start(KEY).await;
    fake.hub.lock().unwrap().declarations = serde_json::json!(["kbf.osupdate.A", "com.example.x"]);
    let hub = NanoHub::new(&fake.url, KEY.into());
    assert_eq!(
        hub.declarations().await.unwrap(),
        ["kbf.osupdate.A", "com.example.x"]
    );
    // KMFDDM is Go: an empty list arrives as null.
    fake.hub.lock().unwrap().declarations = serde_json::Value::Null;
    assert!(hub.declarations().await.unwrap().is_empty());
    fake.hub.lock().unwrap().declarations = serde_json::json!({"not": "a list"});
    let e = hub.declarations().await.unwrap_err();
    assert!(e.0.starts_with("/api/v1/ddm/declarations: "), "{e}");
}

#[tokio::test]
async fn the_fake_answers_other_paths_with_no_content() {
    // The fake's catch-all, as NanoHUB answers a write it accepted.
    let fake = start(KEY).await;
    let response = reqwest::Client::new()
        .get(format!("{}/api/v1/other", fake.url))
        .basic_auth("nanohub", Some(KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_response_cut_short_is_an_error() {
    // A server that promises ten bytes and sends two.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await.unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nab")
            .await
            .unwrap();
    });
    let hub = NanoHub::new(&url, KEY.into());
    let e = hub.declarations().await.unwrap_err();
    assert!(e.0.starts_with("GET /api/v1/ddm/declarations: "), "{e}");
}

#[tokio::test]
async fn profile_and_erase_are_queued_as_mdm_commands() {
    let fake = start(KEY).await;
    let hub = NanoHub::new(&fake.url, KEY.into());
    hub.install_profile(MAC, b"<profile & bytes>")
        .await
        .unwrap();
    hub.erase(MAC).await.unwrap();
    assert_eq!(
        fake.requests(),
        ["PUT /api/v1/nanomdm/enqueue/UDID-1"].repeat(2)
    );
    let install = fake.body(0);
    assert!(
        install.contains("<string>InstallProfile</string>"),
        "{install}"
    );
    assert!(install.contains(&format!(
        "<data>{}</data>",
        STANDARD.encode(b"<profile & bytes>")
    )));
    let erase = fake.body(1);
    assert!(erase.contains("<string>EraseDevice</string>"), "{erase}");
    assert!(!erase.contains("Payload"));
    let uuid = erase
        .split("<key>CommandUUID</key><string>")
        .nth(1)
        .unwrap();
    assert_eq!(uuid.find('<'), Some(36));
    assert_eq!(&uuid[14..15], "4");
}

#[tokio::test]
async fn a_wrong_key_or_a_failing_hub_is_an_error() {
    let fake = start(KEY).await;
    let wrong = NanoHub::new(&fake.url, "guess".into());
    let e = wrong.erase(MAC).await.unwrap_err();
    assert!(e.0.contains("HTTP 401"), "{e}");
    fake.hub.lock().unwrap().fail = true;
    let hub = NanoHub::new(&fake.url, KEY.into());
    let e = hub.declarations().await.unwrap_err();
    assert_eq!(
        e.0,
        "GET /api/v1/ddm/declarations: HTTP 500 Internal Server Error: boom"
    );
    // Nothing listening.
    let gone = NanoHub::new("http://127.0.0.1:9", KEY.into());
    assert!(
        gone.erase(MAC)
            .await
            .unwrap_err()
            .0
            .starts_with("PUT /api/v1/nanomdm/enqueue/UDID-1: ")
    );
}

#[test]
fn plist_text_is_escaped() {
    let plist = String::from_utf8(command_plist("A<&>", None, "u")).unwrap();
    assert!(plist.contains("<string>A&lt;&amp;&gt;</string>"));
}

use serde_json::json;
use tonic::Code;
use tonic::codegen::http;

use super::*;

fn call() -> http::request::Parts {
    http::Request::new(()).into_parts().0
}

fn parse(v: &Value) -> Policy {
    Policy::from_json(&v.to_string()).unwrap_or_else(|e| panic!("{v}: {e}"))
}

/// The path and reason of a refused policy.
fn refused(v: &Value) -> (String, String) {
    match Policy::from_json(&v.to_string()) {
        Err(PolicyError::Invalid { path, why }) => (path, why),
        Err(e) => panic!("{v}: refused, but not as invalid: {e}"),
        Ok(_) => panic!("{v}: accepted"),
    }
}

async fn allowed(a: &dyn Authorizer, instance: &str) -> bool {
    let md = AuthenticationMetadata::default();
    matches!(a.authorize(&md, &[instance]).await.as_slice(), [Ok(())])
}

const SLOTS: [&str; 6] = [
    "capabilitiesAuthorizer",
    "contentAddressableStorage.getAuthorizer",
    "contentAddressableStorage.putAuthorizer",
    "contentAddressableStorage.findMissingAuthorizer",
    "actionCache.getAuthorizer",
    "executeAuthorizer",
];

/// The authorizer the file key `key` (one of [`SLOTS`]) configures.
fn slot<'a>(a: &'a Authorizers, key: &str) -> &'a dyn Authorizer {
    match key {
        "capabilitiesAuthorizer" => &*a.capabilities,
        "contentAddressableStorage.getAuthorizer" => &*a.cas_get,
        "contentAddressableStorage.putAuthorizer" => &*a.cas_put,
        "contentAddressableStorage.findMissingAuthorizer" => &*a.cas_find_missing,
        "actionCache.getAuthorizer" => &*a.ac_get,
        _ => &*a.execute,
    }
}

/// Catches: a slot read from the wrong key (CAS `get` into `put`, say), a slot left
/// at allow when the file names it, and a slot the file does not name that is not
/// left at allow.
#[tokio::test]
async fn each_key_configures_its_own_authorizer() {
    for key in SLOTS {
        let mut file = Map::new();
        file.insert("authenticationPolicy".into(), json!({"allow": {}}));
        let deny = json!({"deny": {}});
        match key.split_once('.') {
            Some((outer, inner)) => {
                let mut nested = Map::new();
                nested.insert(inner.into(), deny);
                file.insert(outer.into(), Value::Object(nested));
            }
            None => {
                file.insert(key.into(), deny);
            }
        }
        let policy = parse(&Value::Object(file));
        for other in SLOTS {
            let allows = allowed(slot(&policy.authorizers, other), "x").await;
            assert_eq!(
                allows,
                other != key,
                "{key} denied; {other} allows: {allows}"
            );
        }
    }
}

/// Catches: prefixes not passed through, and an `allow` authorizer that refuses.
#[tokio::test]
async fn instance_name_prefixes_and_allow_parse() {
    let policy = parse(&json!({
        "authenticationPolicy": {"allow": {}},
        "executeAuthorizer": {"instanceNamePrefix": {"allowedInstanceNamePrefixes": ["ci", "a/b"]}},
        "capabilitiesAuthorizer": {"allow": {}},
    }));
    let execute = &*policy.authorizers.execute;
    assert!(allowed(execute, "ci/main").await);
    assert!(allowed(execute, "a/b").await);
    assert!(!allowed(execute, "cid").await);
    assert!(!allowed(execute, "a").await);
    assert!(allowed(&*policy.authorizers.capabilities, "anything").await);
}

/// Catches: an `allow` whose metadata is not the file's, an `any` whose order is
/// lost, a single-child `all` that changes the answer, and an empty `any` that
/// accepts.
#[tokio::test]
async fn authentication_policies_parse_into_their_authenticators() {
    let policy = parse(&json!({"authenticationPolicy": {"any": {"policies": [
        {"deny": "no token"},
        {"all": {"policies": [{"allow": {"public": {"user": "ci"}, "private": {"k": 1}}}]}},
        {"allow": {"public": "other"}},
    ]}}}));
    let md = policy
        .authenticator
        .authenticate(&call())
        .await
        .expect("accepted");
    assert_eq!(md.public(), Some(&json!({"user": "ci"})));
    assert_eq!(md.private(), Some(&json!({"k": 1})));

    let policy = parse(&json!({"authenticationPolicy": {"all": {"policies": [
        {"allow": {"public": {"a": 1}}},
        {"allow": {"public": {"b": 2}}},
    ]}}}));
    let md = policy
        .authenticator
        .authenticate(&call())
        .await
        .expect("accepted");
    assert_eq!(md.public(), Some(&json!({"a": 1, "b": 2})));

    let policy = parse(&json!({"authenticationPolicy": {"any": {"policies": [{"deny": "solo"}]}}}));
    let e = policy
        .authenticator
        .authenticate(&call())
        .await
        .expect_err("denied");
    assert_eq!((e.code(), e.message()), (Code::Unauthenticated, "solo"));

    let policy = parse(&json!({"authenticationPolicy": {"any": {"policies": []}}}));
    let e = policy
        .authenticator
        .authenticate(&call())
        .await
        .expect_err("denied");
    assert_eq!(e.code(), Code::Unauthenticated);
}

/// Catches: the allow-all policy refusing anything, or carrying metadata.
#[tokio::test]
async fn allow_all_accepts_with_empty_metadata() {
    let policy = Policy::allow_all();
    let md = policy
        .authenticator
        .authenticate(&call())
        .await
        .expect("accepted");
    assert_eq!(*md, AuthenticationMetadata::default());
    assert!(allowed(&*policy.authorizers.execute, "").await);
    assert_eq!(format!("{policy:?}"), "Policy { .. }");
}

/// Catches: each way a file can be wrong being accepted (half-read), or refused with
/// a path that does not point at the mistake.
#[test]
fn a_wrong_file_is_refused_at_the_mistake() {
    let allow = json!({"allow": {}});
    let cases = [
        (json!([]), "$", "expected an object, found an array"),
        (json!({}), "$", "`authenticationPolicy` is required"),
        (
            json!({"authenticationPolicy": allow, "extra": 1}),
            "$.extra",
            "unknown key",
        ),
        (
            json!({"authenticationPolicy": {}}),
            "$.authenticationPolicy",
            "names 0 variants",
        ),
        (
            json!({"authenticationPolicy": {"allow": {}, "deny": "x"}}),
            "$.authenticationPolicy",
            "names 2 variants",
        ),
        (
            json!({"authenticationPolicy": {"jwt": {}}}),
            "$.authenticationPolicy.jwt",
            "`jwt` is not supported yet",
        ),
        (
            json!({"authenticationPolicy": {"tlsClientCertificate": {}}}),
            "$.authenticationPolicy.tlsClientCertificate",
            "not supported yet",
        ),
        (
            json!({"authenticationPolicy": {"open": {}}}),
            "$.authenticationPolicy.open",
            "unknown variant",
        ),
        (
            json!({"authenticationPolicy": {"allow": {"tracingAttributes": []}}}),
            "$.authenticationPolicy.allow.tracingAttributes",
            "not supported yet",
        ),
        (
            json!({"authenticationPolicy": {"allow": {"user": "x"}}}),
            "$.authenticationPolicy.allow.user",
            "unknown key; the keys here are public, private",
        ),
        (
            json!({"authenticationPolicy": {"deny": {}}}),
            "$.authenticationPolicy.deny",
            "a string",
        ),
        (
            json!({"authenticationPolicy": {"any": {"policies": [allow, {"deny": 1}]}}}),
            "$.authenticationPolicy.any.policies[1].deny",
            "a string",
        ),
        (
            json!({"authenticationPolicy": {"any": {}}}),
            "$.authenticationPolicy.any.policies",
            "`policies` is required",
        ),
        (
            json!({"authenticationPolicy": {"all": {"policies": {}}}}),
            "$.authenticationPolicy.all.policies",
            "expected an array",
        ),
        (
            json!({"authenticationPolicy": {"all": {"policies": []}}}),
            "$.authenticationPolicy.all.policies",
            "at least one policy",
        ),
        (
            json!({"authenticationPolicy": allow, "actionCache": {"putAuthorizer": allow}}),
            "$.actionCache.putAuthorizer",
            "clients never write the action cache",
        ),
        (
            json!({"authenticationPolicy": allow, "actionCache": {"findMissingAuthorizer": allow}}),
            "$.actionCache.findMissingAuthorizer",
            "unknown key",
        ),
        (
            json!({"authenticationPolicy": allow, "contentAddressableStorage": {"get": allow}}),
            "$.contentAddressableStorage.get",
            "unknown key",
        ),
        (
            json!({"authenticationPolicy": allow, "executeAuthorizer": {"remote": {}}}),
            "$.executeAuthorizer.remote",
            "`remote` is not supported yet",
        ),
        (
            json!({"authenticationPolicy": allow, "executeAuthorizer": {"jmespathExpression": "true"}}),
            "$.executeAuthorizer.jmespathExpression",
            "not supported yet",
        ),
        (
            json!({"authenticationPolicy": allow, "executeAuthorizer": {"deny": {"x": 1}}}),
            "$.executeAuthorizer.deny.x",
            "this object takes none",
        ),
        (
            json!({"authenticationPolicy": allow, "executeAuthorizer": {"allow": true}}),
            "$.executeAuthorizer.allow",
            "expected an object, found a boolean",
        ),
        (
            json!({"authenticationPolicy": allow, "executeAuthorizer": {"instanceNamePrefix": {}}}),
            "$.executeAuthorizer.instanceNamePrefix.allowedInstanceNamePrefixes",
            "is required",
        ),
        (
            json!({"authenticationPolicy": allow, "executeAuthorizer":
                {"instanceNamePrefix": {"allowedInstanceNamePrefixes": "ci"}}}),
            "$.executeAuthorizer.instanceNamePrefix.allowedInstanceNamePrefixes",
            "expected an array, found a string",
        ),
        (
            json!({"authenticationPolicy": allow, "executeAuthorizer":
                {"instanceNamePrefix": {"allowedInstanceNamePrefixes": ["ci", null]}}}),
            "$.executeAuthorizer.instanceNamePrefix.allowedInstanceNamePrefixes[1]",
            "expected a string, found null",
        ),
        (
            json!({"authenticationPolicy": allow, "executeAuthorizer":
                {"instanceNamePrefix": {"allowedInstanceNamePrefixes": ["ci/"]}}}),
            "$.executeAuthorizer.instanceNamePrefix.allowedInstanceNamePrefixes",
            "empty path component",
        ),
        (
            json!({"authenticationPolicy": allow, "capabilitiesAuthorizer": 7}),
            "$.capabilitiesAuthorizer",
            "found a number",
        ),
    ];
    for (file, path, why) in cases {
        let (got_path, got_why) = refused(&file);
        assert_eq!(got_path, path, "{file}: {got_why}");
        assert!(got_why.contains(why), "{file}: {got_why:?} lacks {why:?}");
    }
    let e = Policy::from_json("{").expect_err("not JSON");
    assert!(e.to_string().starts_with("not JSON: "), "{e}");
    let e = refused(&json!({"authenticationPolicy": {}}));
    assert_eq!(
        PolicyError::Invalid {
            path: e.0,
            why: e.1
        }
        .to_string(),
        "$.authenticationPolicy: names 0 variants; exactly one of allow, deny, any, all is needed"
    );
}

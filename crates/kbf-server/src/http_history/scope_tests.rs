//! The authorizer's answers for one request. Each test names the defect it catches.

use std::sync::Mutex;

use kbf_auth::{AuthenticationMetadata, Authorizer, BoxFuture, Policy};
use tonic::Status;

use super::Scope;
use crate::http_invocations::InvocationReaders;

/// An authorizer that allows names starting with `ok`, refuses `deny*` and fails `broken*`,
/// records each call's names, and answers for only the first `answers` names when it is set.
struct Recording {
    calls: Mutex<Vec<Vec<String>>>,
    answers: Option<usize>,
}

impl Authorizer for Recording {
    fn authorize<'a>(
        &'a self,
        _metadata: &'a AuthenticationMetadata,
        names: &'a [&'a str],
    ) -> BoxFuture<'a, Vec<Result<(), Status>>> {
        self.calls
            .lock()
            .expect("not poisoned")
            .push(names.iter().map(|n| (*n).to_owned()).collect());
        let mut out: Vec<Result<(), Status>> = names
            .iter()
            .map(|n| match n {
                n if n.starts_with("ok") => Ok(()),
                n if n.starts_with("broken") => Err(Status::unavailable("down")),
                _ => Err(Status::permission_denied("no")),
            })
            .collect();
        out.truncate(self.answers.unwrap_or(usize::MAX));
        Box::pin(std::future::ready(out))
    }
}

fn readers(authorizer: std::sync::Arc<Recording>) -> InvocationReaders {
    let policy = Policy::from_json(r#"{"authenticationPolicy": {"allow": {}}}"#).expect("a policy");
    InvocationReaders {
        authenticator: policy.authenticator,
        authorizer,
        blobs: false,
    }
}

/// Catches: an authorizer asked once per row or once per call of the route (a remote one
/// waits on every ask), a name asked twice (in one list or in two asks), a name nobody
/// asked about that is allowed (a route that forgets to ask answers everything), a refusal
/// that is lost, and a reader that gets fewer answers than names and lets the rest through.
#[tokio::test]
async fn each_name_is_asked_once_and_an_unasked_name_is_refused() {
    let recording = std::sync::Arc::new(Recording {
        calls: Mutex::new(Vec::new()),
        answers: None,
    });
    let readers = readers(recording.clone());
    let caller = AuthenticationMetadata::new(None, None);
    let mut scope = Scope::new(&readers, &caller);
    scope.ask(&["ok1", "deny1", "ok1", "broken1"]).await;
    scope.ask(&["ok1", "ok2"]).await;
    scope.ask(&[]).await;
    scope.ask(&["deny1"]).await;
    assert_eq!(
        *recording.calls.lock().expect("not poisoned"),
        [vec!["broken1", "deny1", "ok1"], vec!["ok2"]]
    );
    assert!(scope.allows("ok1") && scope.allows("ok2"));
    assert!(!scope.allows("deny1") && !scope.allows("broken1"));
    assert!(!scope.allows("never-asked"));
    assert!(scope.refusal("ok1").is_none());
    assert!(scope.refusal("never-asked").is_none());
    assert_eq!(
        scope.refusal("deny1").map(Status::code),
        Some(tonic::Code::PermissionDenied)
    );
    assert_eq!(
        scope.refusal("broken1").map(Status::code),
        Some(tonic::Code::Unavailable)
    );
    let names: Vec<String> = ["deny1", "ok2", "ok1", "x"].map(str::to_owned).into();
    assert_eq!(scope.allowed(&names), ["ok2", "ok1"]);
}

/// Catches: an authorizer that answers for fewer names than it was asked about, whose
/// unanswered names are allowed.
#[tokio::test]
async fn a_name_the_authorizer_gave_no_answer_for_is_refused() {
    let recording = std::sync::Arc::new(Recording {
        calls: Mutex::new(Vec::new()),
        answers: Some(1),
    });
    let readers = readers(recording);
    let caller = AuthenticationMetadata::new(None, None);
    let mut scope = Scope::new(&readers, &caller);
    scope.ask(&["ok1", "ok2"]).await;
    assert!(scope.allows("ok1"));
    assert!(!scope.allows("ok2"));
    assert_eq!(
        scope.refusal("ok2").map(Status::code),
        Some(tonic::Code::Internal)
    );
}

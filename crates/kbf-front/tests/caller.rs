//! Execute and the [`Caller`] an authenticating layer puts into a request: the QoS
//! the work is submitted at.

mod common;

use std::sync::{Arc, Mutex, PoisonError};

use common::{Blob, Farm};
use kbf_front::{Caller, Dispatch, ExecutionService, Stage, Submission, Ticket, closing};
use kbf_proto::reapi::execution_server::Execution;
use kbf_proto::reapi::{Action, Command, Directory, ExecuteRequest};
use kbf_types::Qos;
use tokio::sync::watch;
use tonic::{Request, Status};

/// A dispatch that records the QoS of each submission.
#[derive(Default)]
struct Recorded(Mutex<Vec<Qos>>);

impl Dispatch for Recorded {
    fn submit(&self, submission: Submission) -> Result<Ticket, Status> {
        let mut qos = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        qos.push(submission.request.qos);
        let (_, stage) = watch::channel(Stage::Queued);
        Ok(Ticket {
            name: format!("operations/{}", qos.len()),
            action: submission.request.key.action,
            stage,
        })
    }

    fn wait(&self, _: &str) -> Option<Ticket> {
        None
    }
}

/// Catches: Execute ignoring the request's [`Caller`] (every call at `ci`), or taking
/// a QoS other than the caller's; and a call without one not submitted at `ci`.
#[tokio::test]
async fn execute_submits_at_the_callers_qos() {
    let farm = Farm::start().await;
    let root = Blob::of(&Directory::default());
    let command = Blob::of(&Command {
        arguments: vec!["build".to_owned()],
        ..Default::default()
    });
    let action = Blob::of(&Action {
        command_digest: Some(command.proto.clone()),
        input_root_digest: Some(root.proto.clone()),
        ..Default::default()
    });
    farm.upload(&[&action, &command, &root]).await;
    let dispatch = Arc::new(Recorded::default());
    let service =
        ExecutionService::new(Arc::clone(&farm.cache), Arc::clone(&dispatch), closing().1);
    let execute = |caller: Option<Caller>| {
        let mut request = Request::new(ExecuteRequest {
            instance_name: "main".to_owned(),
            action_digest: Some(action.proto.clone()),
            ..Default::default()
        });
        if let Some(caller) = caller {
            request.extensions_mut().insert(caller);
        }
        request
    };
    for qos in [Qos::Interactive, Qos::Batch] {
        let caller = Caller {
            principal: Arc::from("dev"),
            qos,
        };
        service
            .execute(execute(Some(caller)))
            .await
            .expect("Execute");
    }
    service.execute(execute(None)).await.expect("Execute");
    let submitted = dispatch
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    assert_eq!(submitted, [Qos::Interactive, Qos::Batch, Qos::Ci]);
}

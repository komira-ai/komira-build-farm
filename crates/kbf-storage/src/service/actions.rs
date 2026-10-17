//! `kbf.storage.v1.ActionStore`: the action cache over [`crate::Storage`].

use kbf_meta::{ActionRecord, Closure, Role};
use kbf_proto::storage::action_store_server::ActionStore;
use kbf_proto::storage::{
    CommitRequest, CommitResponse, Entry, GetRequest, GetResponse, PrepareRequest, PrepareResponse,
    Record, Writer, prepare_response,
};
use tonic::{Request, Response, Status};

use super::{Endpoint, StorageService, catch_up, wire};
use crate::StorageError;

/// The record a `Commit` carries, as the store takes it.
fn record_of(record: Option<&Record>) -> Result<ActionRecord, Status> {
    let record = record.ok_or_else(|| Status::invalid_argument("a record is required"))?;
    Ok(ActionRecord {
        result: wire::digest(record.result.as_ref())?,
        closure: record
            .closure
            .iter()
            .map(|d| wire::digest(Some(d)))
            .collect::<Result<Closure, _>>()?,
    })
}

#[tonic::async_trait]
impl ActionStore for StorageService {
    async fn get(&self, request: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        self.admission.check(&request, Endpoint::ActionGet)?;
        let request = request.into_inner();
        self.check_map(request.map_generation).await?;
        let action = wire::digest(request.action.as_ref())?;
        let catching_up = self.catching_up();
        let entry = self.storage.action_result(&action).await?;
        if let Some(status) = catch_up::entry_miss(catching_up, entry.is_some(), &action) {
            return Err(status);
        }
        Ok(Response::new(GetResponse {
            entry: entry.map(|(digest, result)| Entry {
                result_digest: Some(wire::to_proto(&digest)),
                result: Some(result),
            }),
        }))
    }

    async fn prepare(
        &self,
        request: Request<PrepareRequest>,
    ) -> Result<Response<PrepareResponse>, Status> {
        self.admission.check(&request, Endpoint::ActionPrepare)?;
        let request = request.into_inner();
        self.check_map(request.map_generation).await?;
        let result = request
            .result
            .ok_or_else(|| Status::invalid_argument("a result is required"))?;
        let catching_up = self.catching_up();
        let outcome = match self.storage.prepare_action_result(&result).await {
            Ok(record) => prepare_response::Outcome::Record(Record {
                result: Some(wire::to_proto(&record.result)),
                closure: record.closure.iter().map(wire::to_proto).collect(),
            }),
            Err(StorageError::ActionWrite(refused)) => {
                if let Some(status) = catch_up::retryable(catching_up, &refused) {
                    return Err(status);
                }
                prepare_response::Outcome::Refused(wire::refusal_of(&refused))
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Response::new(PrepareResponse {
            outcome: Some(outcome),
        }))
    }

    async fn commit(
        &self,
        request: Request<CommitRequest>,
    ) -> Result<Response<CommitResponse>, Status> {
        self.admission.check(&request, Endpoint::ActionCommit)?;
        let _open = self.open_writes.begin();
        let request = request.into_inner();
        self.check_map(request.map_generation).await?;
        let role = match Writer::try_from(request.writer) {
            Ok(Writer::Daemon) => Role::Daemon,
            Ok(Writer::Client) => Role::Client,
            _ => return Err(Status::invalid_argument("a writer is required")),
        };
        let action = wire::digest(request.action.as_ref())?;
        let record = record_of(request.record.as_ref())?;
        let catching_up = self.catching_up();
        match self
            .storage
            .commit_action_record(role, action, record)
            .await
        {
            Ok(()) => Ok(Response::new(CommitResponse { refused: None })),
            Err(StorageError::ActionWrite(refused)) => {
                match catch_up::retryable(catching_up, &refused) {
                    Some(status) => Err(status),
                    None => Ok(Response::new(CommitResponse {
                        refused: Some(wire::refusal_of(&refused)),
                    })),
                }
            }
            Err(e) => Err(e.into()),
        }
    }
}

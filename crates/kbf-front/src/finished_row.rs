//! A [`Finished`] as the bytes of a state store row: a tag byte, then an `ExecuteResponse`.
//!
//! The tag is explicit because the response alone cannot tell a timed-out partial result
//! from a failed one that carries a result. The response is the wire form the caller is
//! sent, so a process that only reads the row (an old frontend finishing the streams it
//! kept open) answers from the row without knowing how the result was made.

use kbf_proto::reapi::{ActionResult, ExecuteResponse};
use prost::Message;

use crate::execution::Finished;
use crate::wire;

const RAN: u8 = 0;
const FAILED: u8 = 1;
const TIMED_OUT: u8 = 2;

impl Finished {
    /// The row bytes of this result: see the module doc.
    #[must_use]
    pub fn to_row(&self) -> Vec<u8> {
        let (tag, response) = match self {
            Finished::Ran(result) => (
                RAN,
                ExecuteResponse {
                    result: Some((**result).clone()),
                    status: Some(wire::rpc_ok()),
                    ..ExecuteResponse::default()
                },
            ),
            Finished::Failed(status) => (
                FAILED,
                ExecuteResponse {
                    status: Some(status.clone()),
                    ..ExecuteResponse::default()
                },
            ),
            Finished::TimedOut { status, partial } => (
                TIMED_OUT,
                ExecuteResponse {
                    result: Some((**partial).clone()),
                    status: Some(status.clone()),
                    ..ExecuteResponse::default()
                },
            ),
        };
        let mut row = vec![tag];
        response
            .encode(&mut row)
            .expect("a Vec grows to hold the message");
        row
    }

    /// The result a row holds.
    ///
    /// # Errors
    /// What is wrong with the bytes: an unknown tag, a response that does not decode, or
    /// one without the field its tag needs.
    pub fn from_row(row: &[u8]) -> Result<Finished, String> {
        let (tag, body) = row.split_first().ok_or("the row is empty")?;
        let response = ExecuteResponse::decode(body).map_err(|e| e.to_string())?;
        let status = response.status.ok_or("the row has no status");
        let result =
            |result: Option<ActionResult>| result.map(Box::new).ok_or("the row has no result");
        Ok(match *tag {
            RAN => Finished::Ran(result(response.result)?),
            FAILED => Finished::Failed(status?),
            TIMED_OUT => Finished::TimedOut {
                status: status?,
                partial: result(response.result)?,
            },
            other => return Err(format!("unknown result tag {other}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use kbf_proto::google::rpc;

    use super::*;

    fn status(code: i32, message: &str) -> rpc::Status {
        rpc::Status {
            code,
            message: message.to_owned(),
            details: Vec::new(),
        }
    }

    fn result(exit_code: i32, stdout: &[u8]) -> ActionResult {
        ActionResult {
            exit_code,
            stdout_raw: stdout.to_vec(),
            ..ActionResult::default()
        }
    }

    // Catches a codec that loses the exit code or the stdout of a run, or that confuses
    // the three variants: a timed-out partial result and a failed status must come back
    // as what they were, not as each other.
    #[test]
    fn every_variant_comes_back_as_it_went_in() {
        let all = [
            Finished::Ran(Box::new(result(3, b"out"))),
            Finished::Failed(status(9, "refused")),
            Finished::TimedOut {
                status: status(4, "the action timed out"),
                partial: Box::new(result(0, b"partial")),
            },
        ];
        for finished in all {
            assert_eq!(Finished::from_row(&finished.to_row()), Ok(finished));
        }
    }

    // Catches a row read without its checks: an empty row, an unknown tag, bytes that are
    // no message, and a timed-out row whose partial result is missing are each an error,
    // never a result invented for the caller.
    #[test]
    fn a_row_that_is_not_a_result_is_an_error() {
        assert!(Finished::from_row(&[]).is_err());
        assert!(Finished::from_row(&[9]).unwrap_err().contains("tag 9"));
        assert!(Finished::from_row(&[RAN, 0xff, 0xff]).is_err());
        let no_result = ExecuteResponse {
            status: Some(status(4, "late")),
            ..ExecuteResponse::default()
        };
        let mut row = vec![TIMED_OUT];
        no_result.encode(&mut row).unwrap();
        assert!(Finished::from_row(&row).unwrap_err().contains("no result"));
        let mut row = vec![FAILED];
        ExecuteResponse::default().encode(&mut row).unwrap();
        assert!(Finished::from_row(&row).unwrap_err().contains("no status"));
    }
}

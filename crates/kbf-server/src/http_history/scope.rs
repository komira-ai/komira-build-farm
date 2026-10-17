//! Which instances a caller may read, asked once per request.
//!
//! A cross-build route answers only what the caller's `invocations.getAuthorizer` allows:
//! each distinct instance a request would touch is put to the authorizer in one call, and
//! every row read is checked against the answers. An instance nobody asked about is not
//! allowed, so a route that forgets to ask answers nothing, not everything.

use std::collections::BTreeMap;

use kbf_auth::AuthenticationMetadata;
use tonic::{Code, Status};

use crate::http_invocations::InvocationReaders;

/// The authorizer's answers for one request.
pub(super) struct Scope<'a> {
    readers: &'a InvocationReaders,
    caller: &'a AuthenticationMetadata,
    decided: BTreeMap<String, Result<(), Status>>,
}

impl<'a> Scope<'a> {
    pub(super) fn new(readers: &'a InvocationReaders, caller: &'a AuthenticationMetadata) -> Self {
        Self {
            readers,
            caller,
            decided: BTreeMap::new(),
        }
    }

    /// Asks the authorizer, in one call, about each of `names` it has not been asked
    /// about. An authorizer that answers for fewer names than it was asked about refuses
    /// the rest.
    pub(super) async fn ask(&mut self, names: &[&str]) {
        let mut asking: Vec<&str> = names
            .iter()
            .copied()
            .filter(|n| !self.decided.contains_key(*n))
            .collect();
        asking.sort_unstable();
        asking.dedup();
        if asking.is_empty() {
            return;
        }
        let mut answers = self
            .readers
            .authorizer
            .authorize(self.caller, &asking)
            .await
            .into_iter();
        for name in asking {
            let answer = answers
                .next()
                .unwrap_or_else(|| Err(Status::internal("the authorizer gave no answer")));
            if let Err(e) = &answer {
                // A refusal is the answer; an authorizer that failed is worth a line.
                if e.code() == Code::PermissionDenied {
                    tracing::debug!(instance = name, "history read refused");
                } else {
                    tracing::warn!(instance = name, error = %e.message(), "history read not authorized");
                }
            }
            self.decided.insert(name.to_owned(), answer);
        }
    }

    /// Whether `name` was asked about and allowed.
    pub(super) fn allows(&self, name: &str) -> bool {
        matches!(self.decided.get(name), Some(Ok(())))
    }

    /// The refusal of `name`, if it was asked about and not allowed.
    pub(super) fn refusal(&self, name: &str) -> Option<&Status> {
        self.decided.get(name)?.as_ref().err()
    }

    /// The names of `names` that are allowed, in the order given.
    pub(super) fn allowed<'n>(&self, names: &'n [String]) -> Vec<&'n str> {
        names
            .iter()
            .map(String::as_str)
            .filter(|n| self.allows(n))
            .collect()
    }
}

#[cfg(test)]
#[path = "scope_tests.rs"]
mod tests;

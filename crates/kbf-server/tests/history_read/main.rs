//! Invocations and runs read back from history (`docs/design/ui-data.md` 5.5, 6 and 8,
//! phase U3): a build page the recorder no longer holds is answered from the chunks the
//! history writer put in the object store, authorized on every instance the invocation
//! used, and `GET /v1/runs/{id}` names one attempt live or durable. Servers are started
//! one after another over the same object store and state store, as a restart leaves
//! them. One test binary, so that every function the reads run is measured as one copy:
//! `read` is what goes right, `edges` what goes wrong (stores that fail, rows that change).

mod edges;
mod flog;
#[path = "../pages/mod.rs"]
mod pages;
mod read;
mod world;

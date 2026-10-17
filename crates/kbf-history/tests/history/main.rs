//! The history writer and reader over the in-memory object store and the state store
//! fake (and SQLite where it is cheap). One test binary, so every generic function is
//! measured as one copy.

mod index;
mod list;
mod reader;
mod rows_support;
mod scoped;
mod sqlite;
mod status;
mod support;
mod today;
mod writer;
mod writer_bounds;
mod writer_rows;

//! Integration tests that run in hosted CI, including the workspace layering test
//! in `tests/layering.rs`, and the repository lints applied by `tests/repo_lints.rs`:
//! the workflow lint in `tests/workflows/` (a test-only module, so its YAML parser
//! stays a dev-dependency) and [`hygiene`] for public text.

pub mod hygiene;

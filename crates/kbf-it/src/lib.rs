//! Integration tests that run in hosted CI, including the workspace layering test
//! in `tests/layering.rs`, and the repository lints applied by `tests/repo_lints.rs`:
//! [`workflows`] for GitHub Actions files and [`hygiene`] for public text.

pub mod hygiene;
pub mod workflows;

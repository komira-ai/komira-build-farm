//! The cross-build read routes over history (`docs/design/ui-data.md` 4.3, 6.2 and 8, phase
//! U3): the invocation list, the shape catalog and the tests, answered from index rows after
//! a restart, scoped to the instances a caller may read. Servers are started one after
//! another over the same stores, as a restart leaves them. One test binary, so that every
//! function the routes run is measured as one copy: `list` and `shapes` are what goes
//! right, `edges` what goes wrong or is refused.

mod edges;
mod kit;
mod list;
#[path = "../pages/mod.rs"]
mod pages;
mod shapes;
#[path = "../history_read/world.rs"]
mod world;

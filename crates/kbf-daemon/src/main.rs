//! `kbf-daemon`: the worker daemon. It will hold the stream to the server, report its
//! node, manage leases, run actions through a container driver and upload outputs.
//!
//! Today it prints its name and version and exits 0.

fn main() {
    println!("kbf-daemon {}", env!("CARGO_PKG_VERSION"));
}

//! `kbf-server`: the farm server. It will wire the front end, metadata, scheduler,
//! replicated log and storage together; it holds no logic of its own.
//!
//! Today it prints its name and version and exits 0.

fn main() {
    println!("kbf-server {}", env!("CARGO_PKG_VERSION"));
}

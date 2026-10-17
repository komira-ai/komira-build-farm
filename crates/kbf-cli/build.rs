//! Embeds the Cargo profile the binary is built with as `KBF_BUILD_PROFILE` (`debug` or
//! `release`, as Cargo's `PROFILE` says), so `kbf --version --json` can say whether a
//! binary is a release build (`src/version.rs`).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let profile = std::env::var("PROFILE").expect("Cargo sets PROFILE for a build script");
    println!("cargo:rustc-env=KBF_BUILD_PROFILE={profile}");
}

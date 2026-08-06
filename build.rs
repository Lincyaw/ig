//! Records the target triple so `ig upgrade` can name its own release asset.
//!
//! The assets are named after the same triple the release workflow builds for,
//! so taking it from the compiler is what keeps the two from drifting: a binary
//! always asks for the artifact it was itself built as.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=IG_TARGET={target}");
}

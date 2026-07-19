//! Capture the build target triple so [`release::target_triple`] can name the
//! GitHub-Releases archive for THIS platform at runtime (e.g.
//! `x86_64-apple-darwin`). `TARGET` is set by cargo for every build.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=MODELSTAT_TARGET_TRIPLE={target}");
}

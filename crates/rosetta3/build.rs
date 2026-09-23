//! Where this host keeps the libraries the runtime links against by name.
//!
//! The runtime links `libcurl` directly, by name, so that a guest's calls land
//! in the same library it would have used itself. On Linux and macOS that
//! library is on the linker's default path. On FreeBSD it is not: anything
//! outside the base system lives under `LOCALBASE` - `/usr/local` unless the
//! installation says otherwise - and neither the compiler driver nor the
//! linker looks there on its own.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("freebsd") {
        return;
    }
    println!("cargo::rerun-if-env-changed=LOCALBASE");
    let base = std::env::var("LOCALBASE").unwrap_or_else(|_| "/usr/local".into());
    println!("cargo::rustc-link-search=native={base}/lib");
}

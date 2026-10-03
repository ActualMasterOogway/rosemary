fn main() {
    println!("cargo:rerun-if-changed=src/fortify.c");

    cc::Build::new()
        .file("src/fortify.c")
        .compile("libdarwin.a");
}

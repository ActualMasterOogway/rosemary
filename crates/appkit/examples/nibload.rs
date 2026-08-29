//! Instantiate a nib and print what could not be built, which is the queue for
//! the milestone that adds windows.
fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: nibload <nib>");
        std::process::exit(2);
    };
    appkit::bootstrap();
    let owner = objc::object::create_instance(
        objc::class::lookup(c"NSApplication").expect("NSApplication"),
        0,
    );
    let path = std::path::PathBuf::from(path);
    match appkit::nib::load(&path, owner) {
        Ok(loaded) => {
            println!("built {} object(s)", loaded.objects.len());
            loaded.report(&path);
        }
        Err(e) => {
            eprintln!("{}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

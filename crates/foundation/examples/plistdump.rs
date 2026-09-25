fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: plistdump <file>");
        std::process::exit(2);
    };
    let bytes = std::fs::read(&path).expect("readable");
    match foundation::plist::parse(&bytes) {
        Ok(value) => println!("{value:#?}"),
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    }
}

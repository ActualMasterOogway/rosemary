use foundation::plist::Value;
use foundation::unarchiver::{Archive, Uid};

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: nibdump <archive>");
        std::process::exit(2);
    };
    let bytes = std::fs::read(&path).expect("readable");
    let archive = match Archive::parse(&bytes) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    };
    println!("root: {:?}, {} entries", archive.root(), archive.len());
    for index in 0..archive.len() {
        let uid = Uid(index);
        let class = archive.class_name(uid).unwrap_or("-");
        match archive.value(uid) {
            Some(Value::Dictionary(d)) => {
                let keys: Vec<&str> = d.keys().map(String::as_str).collect();
                println!("{index:>4} {class:<28} {}", keys.join(" "));
            }
            Some(other) => println!("{index:>4} {class:<28} {other:?}"),
            None => unreachable!("the index came from the table"),
        }
    }
}

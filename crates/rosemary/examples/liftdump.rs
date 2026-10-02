use microlift::arch::x86_64::X86_64;
use microlift::backend::ArtifactBackend;
use microlift::core::lift_to_code;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (bytes, va) = match args.first().map(String::as_str) {
        Some("--file") => {
            let path = args
                .get(1)
                .expect("usage: liftdump --file <raw> <va> [len]");
            let va = number(args.get(2).expect("a guest va"));
            let len = args.get(3).map(|v| number(v) as usize);
            let all = std::fs::read(path).expect("read the code");
            let bytes = match len {
                Some(n) => all[..n.min(all.len())].to_vec(),
                None => all,
            };
            (bytes, va)
        }
        Some(hex) => {
            let bytes = hex
                .as_bytes()
                .chunks(2)
                .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).expect("hex"))
                .collect();
            (bytes, args.get(1).map_or(0x1_0000_2000, |v| number(v)))
        }
        None => {
            eprintln!("usage: liftdump <hex-bytes> [guest-va] | --file <raw> <va> [len]");
            std::process::exit(2);
        }
    };

    // The one definition of what a run lifts with, not a copy of it: a dump taken
    // under some other configuration answers a question nobody asked.
    let level = rosemary::jit::opt_level();
    let config = rosemary::program::lifter_config_base();

    let lifted = lift_to_code::<X86_64>(&bytes, va, config).expect("lift");
    let (code, map) =
        ArtifactBackend::compile_lifted_to_raw_with_map(lifted, None, Some(level.flags()))
            .expect("compile");

    println!(
        "; {} guest byte(s) at {va:#x} -> {} host byte(s), opt {}",
        bytes.len(),
        code.len(),
        level.as_str(),
    );
    print!(
        "{}",
        ArtifactBackend::disassemble_annotated(&code, None, &map).expect("disassemble")
    );
}

/// A decimal or `0x`-prefixed hexadecimal argument.
fn number(raw: &str) -> u64 {
    match raw.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => raw.parse(),
    }
    .expect("a number")
}

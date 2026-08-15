//! What microlift *emits* for a stretch of guest code, as arm64.
//!
//! A profile names a hot guest instruction and stops there. Whether that
//! instruction is hot because the guest runs it a lot or because its
//! translation is bad is a different question, and the only thing that answers
//! it is the host code — which lives in an anonymous mapping, in a process that
//! is busy, and is gone when the run ends.
//!
//! This lifts raw guest bytes with the same [`LifterConfig`] and the same
//! optimisation level [`crate::cache`] uses, compiles them, and prints the
//! result annotated by the guest instruction each host instruction came from.
//! Nothing is executed.
//!
//! ```sh
//! cargo run --release --example liftdump -- <hex-bytes> [guest-va]
//! cargo run --release --example liftdump -- --file <raw> <guest-va> [length]
//! ```
//!
//! Two caveats on reading the output, both of which matter:
//!
//! - The **prologue, the entry dispatch and the epilogue** appear under
//!   `[no source]`, and in a short region they are most of it. They are also
//!   real: a region entry pays them, and the profile's "inside a region, with no
//!   guest instruction" bucket is largely them.
//! - The raw-object path compiles with the shared flags but **not** the host ISA
//!   tuning ([`rosetta3::jit::OptLevel::isa_settings`]), so a lowering that
//!   depends on `has_lse` or `has_dotprod` is shown in its baseline form.

use microlift::arch::x86_64::X86_64;
use microlift::backend::ArtifactBackend;
use microlift::core::{LifterConfig, VmResumptionPoints, lift_to_code};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (bytes, va) = match args.first().map(String::as_str) {
        Some("--file") => {
            let path = args.get(1).expect("usage: liftdump --file <raw> <va> [len]");
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

    // The lifter is configured the way `cache::lift` configures it, because a
    // dump of code compiled some other way answers a question nobody asked.
    let level = rosetta3::jit::opt_level();
    let config = LifterConfig {
        resumption: VmResumptionPoints::ExitOnly,
        strict_resume: true,
        use_vector_xmm: level.host_specific(),
        // Including the link probe, which is a run-time cost like any other and
        // the one a dump would otherwise not show. `ROSETTA3_NO_BLOCK_LINK=1`
        // takes it out here exactly as it does in a run, which is how the two
        // shapes of an exit stub can be read side by side.
        block_link: rosetta3::jit::block_linking().then(rosetta3::thread::link_config),
        ..LifterConfig::default()
    };

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

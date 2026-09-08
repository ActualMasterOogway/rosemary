//! Inspect embedded metallibs without logging shader source or requiring a GPU.
use metal_translate::{
    air::{AirInput, parse_air},
    metallib_air,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os().nth(1).ok_or("expected an input file")?;
    let bytes = std::fs::read(path)?;
    let limit = std::env::args()
        .nth(2)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(10usize);
    for (ordinal, offset) in bytes
        .windows(4)
        .enumerate()
        .filter_map(|(i, w)| (w == b"MTLB").then_some(i))
        .take(limit)
        .enumerate()
    {
        let result =
            metallib_air(&bytes[offset..]).and_then(|air| parse_air(AirInput::Bitcode(air)));
        match result {
            Ok(module) => println!(
                "{ordinal} at {offset}: imported {:?}",
                module.entries[0].stage
            ),
            Err(error) => println!("{ordinal} at {offset}: {error}"),
        }
    }
    Ok(())
}

//! How many link rows one entry into a region costs, for a given guest.
use rosetta3::Program;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: resume_points <mach-o path> [sample size]");
        std::process::exit(2);
    };
    let sample: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(4000);

    let program = match Program::load(&path, &[]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    };

    let starts: Vec<u64> = program.functions.iter().map(|f| f.start).collect();
    println!("{path}: {} function start(s)", starts.len());

    // Spread over the whole image rather than taken from the front, so a
    // hand-written prologue section cannot stand in for the program.
    let step = (starts.len() / sample).max(1);
    let mut counts: Vec<usize> = Vec::new();
    let mut bytes = 0u64;
    for &va in starts.iter().step_by(step) {
        let Some((region, _)) = program.lift_region(va) else {
            continue;
        };
        let mut declared = program.function_starts_in(region.range.clone());
        declared.push(va);
        declared.sort_unstable();
        declared.dedup();
        bytes += region.range.end - region.range.start;
        counts.push(declared.len());
    }
    if counts.is_empty() {
        println!("no region could be selected");
        return;
    }
    counts.sort_unstable();
    let n = counts.len();
    let total: usize = counts.iter().sum();
    let at = |p: f64| counts[((n as f64 * p) as usize).min(n - 1)];
    println!(
        "{n} region(s) sampled, {:.0} byte(s) each on average",
        bytes as f64 / n as f64
    );
    println!(
        "rows per publish: mean {:.1}  p50 {}  p90 {}  p99 {}  max {}",
        total as f64 / n as f64,
        at(0.50),
        at(0.90),
        at(0.99),
        counts[n - 1]
    );
    // What one entry costs a direct-mapped table of the size a thread carries.
    println!(
        "one entry rewrites {:.2}% of an 8192-row table on average, {:.2}% at p99",
        100.0 * (total as f64 / n as f64) / 8192.0,
        100.0 * at(0.99) as f64 / 8192.0,
    );
}

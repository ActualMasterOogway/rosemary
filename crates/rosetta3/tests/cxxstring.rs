//! `std::string` across the seam: the guest's layout, not the host's.
//!
//! The bug these exist for is recorded in `shims::cxxstring` — libc++ lays
//! `basic_string` out differently on x86-64 and arm64 macOS, so an import that
//! was described *correctly* still handed the object to code reading different
//! fields out of it.
//!
//! The assertion that matters is [`the_same_binary_agrees_with_itself`]: the
//! identical x86-64 executable, run through this runtime and run directly (under
//! Rosetta 2), must print the same bytes. That is the only oracle worth having
//! here, because the specification of `std::string` is what libc++ does with it,
//! and it is falsifiable in the exact way the original bug was not — the fixture
//! prints the *object's own bytes*, so a layout that disagrees shows up as a
//! diff rather than as a crash three functions later.

mod common;

use common::{build_cpp, run_guest, run_native};

/// A C++ program that reaches the out-of-line half of `basic_string`.
///
/// Built at `-O0` deliberately: the optimiser inlines the short-string paths,
/// and the entry points this is about are precisely the ones that survive as
/// *imports*. Every operation in `steamclient.dylib`'s import list is here.
///
/// It prints three things per step, and each fails differently:
///
/// - **the characters**, which a wrong buffer pointer breaks;
/// - **`size` and `capacity`**, read by the guest's own inlined accessors, which
///   a wrong size or capacity word breaks;
/// - **the twenty-four bytes of the object itself**, which is the only one of
///   the three that catches a representation that is self-consistent and still
///   not libc++'s — a short string stored long, or a capacity that lost its low
///   bit to the long/short flag.
///
/// The crossings from short to long and back are what the sizes are chosen
/// around: 22 is the last short string, 23 the first long one, and the growth
/// steps land on 25, 31 and 47.
const STRING_CPP: &str = r#"
#include <cstdio>
#include <cstring>
#include <string>

// Prints the object's own representation, deliberately leaving out the two
// things that legitimately differ between any two runs of any program: the heap
// address, and the bytes past the terminator, which libc++ never initializes.
// What is left is exactly the layout — the long/short flag, the size and
// capacity words where the guest's own inlined accessors read them, and, for a
// short string, the characters in the object.
static void dump(const char *tag, const std::string &s) {
    const unsigned char *p = (const unsigned char *)&s;
    int is_long = p[0] & 1;
    printf("%-12s %-3zu %-4zu %d [%s]", tag, s.size(), s.capacity(), is_long, s.c_str());
    if (is_long) {
        unsigned long long cap, size;
        memcpy(&cap, p, 8);
        memcpy(&size, p + 8, 8);
        printf(" cap=%016llx size=%016llx", cap, size);
    } else {
        for (size_t i = 0; i <= s.size() + 1; i++) printf(" %02x", p[i]);
    }
    printf("\n");
}

int main() {
    // __init(const char *, size_t), short and long.
    std::string a("google/protobuf");
    dump("init-short", a);
    std::string b("0123456789012345678901234567890123456789");
    dump("init-long", b);

    // The short/long boundary, from both sides.
    std::string edge(22, 'e');
    dump("edge-22", edge);
    std::string over(23, 'o');
    dump("edge-23", over);

    // push_back across the boundary, which is what calls __grow_by.
    std::string grow;
    for (int i = 0; i < 60; i++) {
        grow.push_back('a' + (i % 26));
        if (i == 21 || i == 22 || i == 24 || i == 30 || i == 46 || i == 47) dump("grow", grow);
    }

    // append, in each of its four spellings.
    std::string ap("head");
    ap.append("-tail");
    dump("append-cstr", ap);
    ap.append("-counted-and-more", 8);
    dump("append-n", ap);
    ap.append(b, 3, 5);
    dump("append-str", ap);
    ap.append(9, 'z');
    dump("append-fill", ap);

    // assign, both spellings, growing and then shrinking in place.
    std::string as("x");
    as.assign("a rather long replacement that will not fit inside the object");
    dump("assign-cstr", as);
    // Shrinking must not reallocate: the pointer the guest already holds has to
    // stay valid, so this records whether it did.
    const char *held = as.c_str();
    as.assign("short", 5);
    dump("assign-n", as);
    printf("stable=%d\n", held == as.c_str());

    // Editing.
    std::string ed("0123456789");
    ed.erase(2, 3);
    dump("erase", ed);
    ed.insert(3, "INS");
    dump("insert", ed);
    ed.replace(1, 4, "REPLACEMENT");
    dump("replace", ed);
    ed.resize(4, '.');
    dump("resize-down", ed);
    ed.resize(30, '.');
    dump("resize-up", ed);
    ed.reserve(100);
    dump("reserve", ed);

    // Copy construction, assignment, and the substring constructor.
    std::string c1(b);
    dump("copy-long", c1);
    std::string c2(a);
    dump("copy-short", c2);
    std::string sub(b, 4, 30);
    dump("substr", sub);
    std::string sub2(a, 2, 4);
    dump("substr-short", sub2);
    c2 = b;
    dump("assign-str", c2);

    // Inspection.
    printf("at=%c%c find=%zu %zu rfind=%zu %zu\n", a.at(0), a.at(6),
           a.find('/'), a.find('Q'), a.rfind('o'), a.rfind('Q'));
    printf("cmp=%d %d %d %d\n", a.compare(0, 6, "google", 6),
           a.compare(0, 6, "googlf", 6), a.compare(0, 6, "goo", 3),
           a.compare(0, 2, "google", 6));

    // The two that return a string by value.
    std::string plus = "prefix-" + a;
    dump("operator+", plus);
    dump("to_string", std::to_string(1234567890u));
    dump("to_string-i", std::to_string(-42));

    // wstring: the same code with four-byte characters, so the min_cap of 5 and
    // the data offset of 4 are exercised rather than assumed.
    std::wstring w(4, L'w');
    printf("w-short %zu %zu\n", w.size(), w.capacity());
    std::wstring w2(9, L'w');
    printf("w-long %zu %zu\n", w2.size(), w2.capacity());
    w2.push_back(L'!');
    w2.append(L"abcdef", 6);
    w2.resize(40, L'.');
    printf("w-grown %zu %zu %d\n", w2.size(), w2.capacity(), (int)w2[39]);
    std::wstring w3(w2);
    printf("w-copy %zu %d\n", w3.size(), (int)w3[0]);
    return 0;
}
"#;

fn fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_cpp("cxxstring", STRING_CPP))
        .clone()
}

/// The gate. One x86-64 binary, two ways of running it, byte-identical output.
///
/// Comparing against the *same binary* run natively, rather than against
/// expected text written here, is what makes this a statement about libc++
/// rather than about our reading of libc++. When Rosetta 2 is unavailable the
/// test skips: an absent oracle is not a failure.
#[test]
fn the_same_binary_agrees_with_itself() {
    let Some(path) = fixture() else {
        skip!("no x86_64 C++ toolchain");
    };
    let Some((native_status, native)) = run_native(&path) else {
        skip!("cannot run an x86_64 binary natively");
    };
    let (status, stdout, stderr) = run_guest(&path, &[]);
    assert_eq!(status, native_status, "exit status differs\n{stderr}");
    assert_eq!(
        stdout, native,
        "output differs.\n--- rosetta3 ---\n{stdout}\n--- native ---\n{native}\n\
         --- stderr ---\n{stderr}"
    );
}

/// The same program with every import serviced at a VM exit rather than inlined.
///
/// `--slow` is the project's standing check that "fast is an optimisation of
/// slow"; here it also pins down something specific to this module, which is
/// that a string entry point must *never* be promoted to a fast import. Being
/// promoted would mean the lifter calling the host's libc++ directly, which is
/// the bug this whole module is about, so the two runs agreeing is the
/// observable form of `is_shimmed` being consulted.
#[test]
fn the_slow_path_agrees_too() {
    let Some(path) = fixture() else {
        skip!("no x86_64 C++ toolchain");
    };
    let (status, fast, fast_err) = run_guest(&path, &[]);
    let (slow_status, slow, slow_err) = common::run_guest_with(&["--slow"], &path, &[]);
    assert_eq!(status, slow_status, "exit status differs\n{fast_err}{slow_err}");
    assert_eq!(fast, slow, "--slow differs\n{fast_err}{slow_err}");
}

/// No string operation may be reported `Unknown`, and none may be `Fast`.
///
/// The audit question from M2, asked of the surface this module owns. `Fast`
/// would mean the lifter inlining a call into the host's libc++ — the original
/// bug — so it is as much a failure here as `Unknown` is.
#[test]
fn every_string_import_is_serviced_at_an_exit() {
    let Some(path) = fixture() else {
        skip!("no x86_64 C++ toolchain");
    };
    let program = rosetta3::Program::load(&path, &[]).expect("load");
    let mut seen = 0;
    for (symbol, class) in program.classify_imports() {
        if !symbol.contains("basic_string") && !symbol.contains("to_string") {
            continue;
        }
        seen += 1;
        assert_eq!(
            class,
            rosetta3::abi::Classification::Slow,
            "{symbol} is {class:?}, and every string entry point must be shimmed"
        );
    }
    assert!(seen > 0, "the fixture imported no string entry points");
}

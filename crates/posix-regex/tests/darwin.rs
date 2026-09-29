use posix_regex::{compile::Error, PosixRegexBuilder};

fn matches(pattern: &[u8], extended: bool, input: &[u8]) -> Option<Vec<Option<(usize, usize)>>> {
    let regex = PosixRegexBuilder::new(pattern)
        .with_default_classes()
        .extended(extended)
        .compile()
        .unwrap();
    regex
        .matches(input, Some(1))
        .into_iter()
        .next()
        .map(|x| x.into_vec())
}

#[test]
fn basic_escaped_pipe_is_literal() {
    assert_eq!(matches(br"a\|b", false, b"a|b").unwrap()[0], Some((0, 3)));
    assert!(matches(br"a\|b", false, b"a").is_none());
}

#[test]
fn basic_backreference_and_extended_alternation() {
    assert_eq!(
        matches(br"\(ab\)\1", false, b"abab").unwrap()[0],
        Some((0, 4))
    );
    assert_eq!(matches(b"a|ab", true, b"abc").unwrap()[0], Some((0, 2)));
}

#[test]
fn reports_invalid_group_and_range() {
    assert_eq!(
        PosixRegexBuilder::new(b"a(").extended(true).compile().err(),
        Some(Error::Expected(b')', None))
    );
    assert_eq!(
        PosixRegexBuilder::new(b"[z-a]")
            .extended(true)
            .compile()
            .err(),
        Some(Error::InvalidCharacterRange)
    );
}

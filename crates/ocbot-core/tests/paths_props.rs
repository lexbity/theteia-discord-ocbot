//! AC-9 (pure half): lexical containment, name sanitization, and the
//! dependency-free date conversion (spec FR-35/P11). The fs adapter's
//! `realpath` re-check is covered separately in `ocbot`'s `paths_fs.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use std::path::Path;

use ocbot_core::paths::{content_tag8, dir_hash, is_inside, sanitize_name, yyyymmdd};

#[test]
fn containment_accepts_descendants_and_rejects_escapes() {
    let root = Path::new("/srv/work");
    let inside = [
        "/srv/work",
        "/srv/work/",
        "/srv/work/a",
        "/srv/work/a/b.txt",
        "/srv/work/a/../b.txt",
        "/srv/work/./a/b.txt",
        "/srv/work/a/b/../../b.txt",
        "/srv/work/a/../../work/b.txt",
    ];
    for path in inside {
        assert!(is_inside(root, Path::new(path)), "should be inside: {path}");
    }

    let outside = [
        "/srv",
        "/srv/worker",
        "/srv/work2",
        "/srv/work/..",
        "/srv/work/../etc/passwd",
        "/srv/work/../../etc",
        "/etc",
        "/srv/work/a/../../../etc",
    ];
    for path in outside {
        assert!(
            !is_inside(root, Path::new(path)),
            "should be outside: {path}"
        );
    }
}

#[test]
fn containment_handles_traversal_and_odd_inputs() {
    let root = Path::new("/");
    assert!(is_inside(root, Path::new("/anywhere")));
    assert!(is_inside(root, Path::new("/a/../../..")), "clamped at root");
    let empty = Path::new("");
    // An empty candidate normalizes to empty, which never starts with a root.
    assert!(!is_inside(Path::new("/srv"), empty));
}

#[test]
fn sanitize_strips_separators_and_hostile_characters() {
    let cases = [
        ("../evil.txt", "evil.txt"),
        ("..\\evil.txt", "evil.txt"),
        ("/etc/passwd", "passwd"),
        ("a b c.txt", "a_b_c.txt"),
        ("weird\nname.md", "weird_name.md"),
        ("..", "file"),
        (".", "file"),
        ("", "file"),
        ("/", "file"),
        (".bashrc", "bashrc"),
        ("  ", "file"),
    ];
    for (input, expected) in cases {
        assert_eq!(sanitize_name(input), expected, "input {input:?}");
    }
}

#[test]
fn sanitize_caps_length_and_never_keeps_a_dotdot() {
    let long = "a".repeat(500);
    let sanitized = sanitize_name(&long);
    assert_eq!(sanitized.len(), ocbot_core::paths::NAME_MAX);
    // The result is always a single component: no separators, and never the
    // literal parent directory.
    let encoded = sanitize_name("..%2f..%2fetc");
    assert!(!encoded.contains('/'));
    assert!(!encoded.contains('\\'));
    assert_ne!(encoded, "..");
}

#[test]
fn content_tag_is_stable_and_short() {
    let tag = content_tag8(b"hello");
    assert_eq!(tag.len(), 8);
    assert_eq!(tag, content_tag8(b"hello"));
    assert_ne!(tag, content_tag8(b"world"));
    // Exact FNV-1a values pin the mixing (kills naive ^/| confusion mutants).
    assert_eq!(tag, "80aabd0b");
    assert_eq!(content_tag8(b"a"), "8601ec8c");
    // The dir hash (used in `v1:release:<hash>`) is pinned exactly too.
    assert_eq!(dir_hash("/home/lex/work"), "86203c2712de3b85");
}

#[test]
fn date_conversion_matches_known_days() {
    assert_eq!(yyyymmdd(0), "19700101");
    assert_eq!(yyyymmdd(86_399), "19700101");
    assert_eq!(yyyymmdd(86_400), "19700102");
    // 2023-11-14T22:13:20Z
    assert_eq!(yyyymmdd(1_700_000_000), "20231114");
    // 2024-02-29 (leap day) begins at this unix second.
    assert_eq!(yyyymmdd(1_709_164_800), "20240229");
    assert_eq!(yyyymmdd(1_719_792_000), "20240701");
    // Pre-epoch and far-future dates exercise the era arithmetic.
    assert_eq!(yyyymmdd(-86_400), "19691231");
    assert_eq!(yyyymmdd(-2_208_988_800), "19000101");
    assert_eq!(yyyymmdd(4_133_894_400), "21001231");
    assert_eq!(yyyymmdd(7_258_118_400), "22000101");
}

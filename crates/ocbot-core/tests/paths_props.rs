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

// SPEC-005: Project-centric remote control tests

use ocbot_core::paths::project_display_name;
use ocbot_core::ports::{
    default_plans_dirs, default_readme_file, PlanInfo, ProjectConfig, ProjectDetails, ProjectInfo,
};
use std::path::PathBuf;

#[test]
fn project_display_name_uses_last_two_normal_components() {
    use std::path::Path;
    assert_eq!(
        project_display_name(Path::new("/home/lex/project-alpha")),
        "lex/project-alpha"
    );
    assert_eq!(
        project_display_name(Path::new("/usr/local/bin")),
        "local/bin"
    );
    assert_eq!(
        project_display_name(Path::new("/home/user/my-project")),
        "user/my-project"
    );
}

#[test]
fn project_display_name_falls_back_to_single_component() {
    use std::path::Path;
    assert_eq!(project_display_name(Path::new("/project")), "project");
    assert_eq!(project_display_name(Path::new("project")), "project");
}

#[test]
fn project_display_name_handles_root() {
    use std::path::Path;
    assert_eq!(project_display_name(Path::new("/")), "unknown");
}

#[test]
fn project_display_name_handles_relative_paths() {
    use std::path::Path;
    assert_eq!(project_display_name(Path::new("./project")), "project");
    assert_eq!(project_display_name(Path::new("./a/b")), "a/b");
}

#[test]
fn project_display_name_handles_trailing_slash() {
    use std::path::Path;
    assert_eq!(
        project_display_name(Path::new("/home/lex/project/")),
        "lex/project"
    );
}

#[test]
fn project_display_name_handles_nested_paths() {
    use std::path::Path;
    assert_eq!(project_display_name(Path::new("/a/b/c/d")), "c/d");
}

#[test]
fn project_info_structure() {
    let info = ProjectInfo {
        name: "lex/project-alpha".to_owned(),
        dir: PathBuf::from("/home/lex/project-alpha"),
    };
    assert_eq!(info.name, "lex/project-alpha");
    assert_eq!(info.dir, PathBuf::from("/home/lex/project-alpha"));
}

#[test]
fn plan_info_structure() {
    let plan = PlanInfo {
        name: "spec-auth.md".to_owned(),
        path: PathBuf::from("/home/lex/project-alpha/devdocs/plans/spec-auth.md"),
    };
    assert_eq!(plan.name, "spec-auth.md");
    assert_eq!(
        plan.path,
        PathBuf::from("/home/lex/project-alpha/devdocs/plans/spec-auth.md")
    );
}

#[test]
fn project_details_structure() {
    let details = ProjectDetails {
        name: "lex/project-alpha".to_owned(),
        readme_preview: Some("# Project Alpha\n\nA test project".to_owned()),
        plans: vec![
            PlanInfo {
                name: "plan1.md".to_owned(),
                path: PathBuf::from("/path/to/plan1.md"),
            },
            PlanInfo {
                name: "plan2.md".to_owned(),
                path: PathBuf::from("/path/to/plan2.md"),
            },
        ],
    };
    assert_eq!(details.name, "lex/project-alpha");
    assert_eq!(
        details.readme_preview,
        Some("# Project Alpha\n\nA test project".to_owned())
    );
    assert_eq!(details.plans.len(), 2);
    assert_eq!(details.plans[0].name, "plan1.md");
    assert_eq!(details.plans[1].name, "plan2.md");
}

#[test]
fn project_config_defaults() {
    let config = ProjectConfig::default();
    assert_eq!(config.plans_dirs, default_plans_dirs());
    assert_eq!(config.readme_file, default_readme_file());
}

#[test]
fn project_config_custom_values() {
    let config = ProjectConfig {
        plans_dirs: vec!["custom/plans".to_owned(), "docs/plans".to_owned()],
        readme_file: "CUSTOM.md".to_owned(),
    };
    assert_eq!(config.plans_dirs, vec!["custom/plans", "docs/plans"]);
    assert_eq!(config.readme_file, "CUSTOM.md");
}

#[test]
fn default_plans_dirs_returns_expected() {
    assert_eq!(default_plans_dirs(), vec!["devdocs/plans"]);
}

#[test]
fn default_readme_file_returns_expected() {
    assert_eq!(default_readme_file(), "README.md");
}

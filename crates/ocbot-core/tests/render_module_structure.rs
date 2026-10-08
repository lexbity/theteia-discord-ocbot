//! Structural guard for the decomposed render module (SPEC-002 §4.3,
//! FR-10/AC-5/AC-6).
//!
//! The audit's H-3 was a 1262-line god module. This test pins the decomposition:
//! every concern lives in its own file, and no file is allowed to grow back into
//! an unreviewable monolith.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use std::path::Path;

/// `mod.rs` is the facade plus the shared spec types; the rest are concerns.
const SUBMODULES: &[&str] = &[
    "mod.rs",
    "report.rs",
    "gate.rs",
    "browser.rs",
    "redact.rs",
    "label.rs",
];

/// The ceiling mandated by SPEC-002 AC-6: no render file may reach it.
const MAX_LINES: usize = 500;

#[test]
fn render_module_structure() {
    let render = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/render");
    assert!(
        render.is_dir(),
        "render must be a directory module, found no {}",
        render.display()
    );

    for file in SUBMODULES {
        let path = render.join(file);
        assert!(path.is_file(), "missing render submodule {file}");
        let source = std::fs::read_to_string(&path).expect("render submodule is readable");
        let lines = source.lines().count();
        assert!(
            lines < MAX_LINES,
            "{file} is {lines} lines (>= {MAX_LINES}); split it by concern"
        );
    }
}

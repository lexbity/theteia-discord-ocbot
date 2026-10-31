//! Pure path utilities used by the git-truth handlers (spec P7) and the
//! SPEC-005 project surfaces.
//!
//! This module is deliberately I/O-free: the fs adapter owns `realpath` and the
//! *real* containment re-check (Phase 6, FR-35). Here we define the pure,
//! testable pieces: a branch-name slug, a stable short hash for `custom_id`s
//! (D-6), a lexical containment predicate, attachment-name sanitization, and a
//! dependency-free civil-date conversion for `INBOX_DIR/<yyyymmdd>/`.

use std::path::{Component, Path, PathBuf};

/// The maximum slug length (spec P7: `[a-z0-9-]{1,40}`).
pub const SLUG_MAX: usize = 40;
/// The maximum sanitized attachment file-name length.
pub const NAME_MAX: usize = 80;

/// Turn an arbitrary operator label into a branch-safe slug.
///
/// The contract is exactly `[a-z0-9-]{1,40}`: ASCII alphanumerics are kept
/// (lowercased), every other run of characters becomes a single `-`, leading
/// and trailing dashes are trimmed, and the result is truncated. An input with
/// no usable characters yields `task` (never empty — a branch name is required).
pub fn slug(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut pending_dash = false;
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
        if out.len() >= SLUG_MAX {
            break;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "task".to_owned()
    } else {
        trimmed.chars().take(SLUG_MAX).collect()
    }
}

/// A stable 64-bit FNV-1a hash of an identifier string, rendered as 16 hex
/// characters.
///
/// Used in component IDs when carrying a path or project/plan name would exceed
/// platform limits or expose unnecessary details. This is not authorization:
/// the handler resolves the token against current project state before acting.
pub fn dir_hash(dir: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in dir.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Build a short opaque identifier for a project used in interactive component
/// IDs. The token is stable across restarts and does not expose the project name.
/// It is an identifier, not an authorization credential; callers must still
/// resolve it against the configured project set.
pub fn project_action_token(project_name: &str) -> String {
    dir_hash(&format!("project\0{project_name}"))
}

/// Whether `raw` has the shape of a token minted by [`dir_hash`]: exactly 16
/// ASCII hex characters.
///
/// This is a cheap grammar check for `custom_id` segments, not authorization —
/// the handler that consumes the token still resolves it against the current
/// project set before acting (SPEC-005).
#[must_use]
pub fn is_action_token(raw: &str) -> bool {
    raw.len() == 16 && raw.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Build a short opaque identifier for a plan scoped to one project.
///
/// Scoping the token to both names prevents two projects with an identically
/// named plan from sharing the same component identifier.
pub fn plan_action_token(project_name: &str, plan_name: &str) -> String {
    dir_hash(&format!("plan\0{project_name}\0{plan_name}"))
}

/// Lexical containment: does `candidate` live at or under `root`?
///
/// This is the pure half of FR-35. `..` is resolved lexically and can never
/// escape the root; the fs adapter additionally `realpath`s both sides so a
/// symlink swap between the check and the use is still caught at use time.
pub fn is_inside(root: &Path, candidate: &Path) -> bool {
    normalize_lexical(candidate).starts_with(normalize_lexical(root))
}

/// Resolve `.`/`..` components lexically without touching the filesystem.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Sanitize an operator-supplied file name for `INBOX_DIR`.
///
/// The result is a single path component: directory separators are dropped,
/// every character outside `[A-Za-z0-9._-]` becomes `_`, leading/trailing dots
/// and underscores are trimmed (so `..` and dotfiles cannot survive), and the
/// name is capped at [`NAME_MAX`]. Empty/`.`/`..` inputs become `file`.
pub fn sanitize_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut out = String::with_capacity(base.len());
    for ch in base.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches(['.', '_']);
    let limited: String = trimmed.chars().take(NAME_MAX).collect();
    // `trim_matches` can never leave a leading/trailing dot, so the only
    // degenerate outcome is empty (`..`, `.`, separators, empty input).
    if limited.is_empty() {
        "file".to_owned()
    } else {
        limited
    }
}

/// A short (~8 hex char) content discriminator for attachment file names.
///
/// This is FNV-1a truncated, **not** a cryptographic digest: it only breaks
/// ties between same-named uploads in one day bucket, and a collision costs one
/// overwritten file, never a correctness guarantee.
pub fn content_tag8(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", hash & 0xffff_ffff)
}

/// Format a unix timestamp as `yyyymmdd` (UTC) for the inbox date bucket.
///
/// Uses Howard Hinnant's civil-from-days algorithm, so the core needs no date
/// dependency and the conversion is pure and independently testable.
pub fn yyyymmdd(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}{month:02}{day:02}")
}

/// Days since 1970-01-01 → (year, month, day) in the proleptic Gregorian
/// calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// Derive an opaque project name from a directory path.
///
/// Rule: Use the last 2 **normal** path components (per SPEC-005 Q4).
/// Non-normal components (`RootDir`, `Prefix`, `CurDir`, `ParentDir`) are skipped.
///
/// # Examples
///
/// - `/home/lex/project-alpha` → `lex/project-alpha`
/// - `/project` → `project` (fallback to last 1 if only 1 normal component)
/// - `/home/lex` → `home/lex`
/// - `/` → `unknown`
/// - `./project` → `project`
/// - `C:\Users\lex\project` → `lex\project` (Windows)
pub fn project_display_name(dir: &Path) -> String {
    // Collect only Normal components as OsStr references
    let normal_components: Vec<_> = dir
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();

    match normal_components.len() {
        0 => "unknown".to_owned(),
        1 => normal_components[0].to_string_lossy().into_owned(),
        _ => {
            // SAFETY: We have at least 2 components due to the match guard
            let last = normal_components[normal_components.len() - 1].to_string_lossy();
            let second_last = normal_components[normal_components.len() - 2].to_string_lossy();
            // Use the OS-specific separator
            format!("{}{}{}", second_last, std::path::MAIN_SEPARATOR, last)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SLUG_MAX, dir_hash, is_action_token, plan_action_token, project_action_token,
        project_display_name, slug, yyyymmdd,
    };

    #[test]
    fn slug_is_branch_safe() {
        assert_eq!(slug("Fix the Login Bug!"), "fix-the-login-bug");
        assert_eq!(slug("  multiple   spaces  "), "multiple-spaces");
        assert_eq!(slug("a/b/c"), "a-b-c");
        assert_eq!(slug("--"), "task");
        assert_eq!(slug(""), "task");
        assert!(slug("x").len() <= SLUG_MAX);
    }

    #[test]
    fn slug_injection_is_neutralized() {
        // The classic shell-injection label can never reach argv (the git
        // adapter is argv-only) but must still be a boring branch name.
        let hostile = slug("; rm -rf / #");
        assert!(
            hostile
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "slug chars: {hostile:?}"
        );
        assert_eq!(hostile, "rm-rf");
    }

    #[test]
    fn slug_truncates_to_the_cap() {
        let long = "a".repeat(100);
        assert_eq!(slug(&long).len(), SLUG_MAX);
    }

    #[test]
    fn civil_date_conversion_handles_negative_eras_and_gregorian_leap_rules() {
        // Year zero is a leap year in the proleptic Gregorian calendar; these
        // dates cross the algorithm's negative-era boundary around 0000-03-01.
        assert_eq!(yyyymmdd(-62_167_219_200), "00000101");
        assert_eq!(yyyymmdd(-62_162_121_600), "00000229");
        assert_eq!(yyyymmdd(-62_162_035_200), "00000301");
        assert_eq!(yyyymmdd(-62_135_596_800), "00010101");

        // 1900 is not a leap year, while 2000 is divisible by 400.
        assert_eq!(yyyymmdd(-2_208_988_800), "19000101");
        assert_eq!(yyyymmdd(951_782_400), "20000229");
        // The proleptic range pins the era-crossing branches of the civil
        // algorithm: the year_of_era correction terms (the leap-hundred and
        // leap-40000 day counts) only bite below year 0, and an off-by-sign
        // there is otherwise invisible to every modern date.
        assert_eq!(yyyymmdd(-62_193_571_200), "-0010302");
    }

    #[test]
    fn dir_hash_is_stable_and_short() {
        let a = dir_hash("/home/lex/work");
        assert_eq!(a.len(), 16);
        assert_eq!(a, dir_hash("/home/lex/work"));
        assert_ne!(a, dir_hash("/home/lex/other"));
    }

    #[test]
    fn project_action_tokens_are_short_stable_and_scoped() {
        let project = project_action_token("user/project");
        assert_eq!(project.len(), 16);
        assert_eq!(project, project_action_token("user/project"));
        assert_ne!(project, project_action_token("user/other"));

        let plan = plan_action_token("user/project", "roadmap.md");
        assert_eq!(plan.len(), 16);
        assert_ne!(plan, plan_action_token("user/other", "roadmap.md"));
        assert_ne!(plan, plan_action_token("user/project", "other.md"));

        // Both tokens are exactly what the grammar check accepts.
        assert!(is_action_token(&project));
        assert!(is_action_token(&plan));
    }

    #[test]
    fn the_action_token_grammar_accepts_only_hash_shaped_ids() {
        assert!(is_action_token("0123456789abcdef"));
        assert!(is_action_token("ABCDEF0123456789"));
        // Wrong length, non-hex bytes, and non-ASCII are all rejected. These
        // checks are grammar-only; handlers still resolve tokens against the
        // live project set before acting.
        assert!(!is_action_token(""));
        assert!(!is_action_token("0123456789abcde"));
        assert!(!is_action_token("0123456789abcdef0"));
        assert!(!is_action_token("0123456789abcdeg"));
        assert!(!is_action_token("/home/lex/project"));
        assert!(!is_action_token("０１２３４５６７８９ａｂｃｄｅｆ"));
    }

    #[test]
    fn project_display_name_uses_last_two_components() {
        use std::path::Path;
        // Unix-style paths
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
    fn project_display_name_falls_back_to_last_component() {
        use std::path::Path;
        // Single normal component
        assert_eq!(project_display_name(Path::new("/project")), "project");
        assert_eq!(project_display_name(Path::new("project")), "project");
    }

    #[test]
    fn project_display_name_handles_root() {
        use std::path::Path;
        // Root directory has no normal components
        assert_eq!(project_display_name(Path::new("/")), "unknown");
    }

    #[test]
    fn project_display_name_handles_single_segment_paths() {
        use std::path::Path;
        // Relative paths with CurDir
        assert_eq!(
            project_display_name(Path::new("project-alpha")),
            "project-alpha"
        );
        assert_eq!(project_display_name(Path::new("./project")), "project");
    }

    #[test]
    fn project_display_name_handles_windows_paths() {
        use std::path::Path;
        // On Unix, backslashes in paths are treated as part of the filename,
        // so "C:\\Users\\lex\\project" is a single component "C:\Users\lex\project"
        // This test verifies behavior on the current platform
        if cfg!(windows) {
            assert_eq!(
                project_display_name(Path::new("C:\\Users\\lex\\project")),
                "lex\\project"
            );
        } else {
            // On Unix, the entire string is one normal component
            assert_eq!(
                project_display_name(Path::new("C:\\Users\\lex\\project")),
                "C:\\Users\\lex\\project"
            );
        }
    }

    #[test]
    fn project_display_name_handles_nested_paths() {
        use std::path::Path;
        assert_eq!(project_display_name(Path::new("/a/b/c/d")), "c/d");
    }

    #[test]
    fn project_display_name_handles_trailing_slash() {
        use std::path::Path;
        // Trailing slash should not affect normal components
        assert_eq!(
            project_display_name(Path::new("/home/lex/project/")),
            "lex/project"
        );
    }
}

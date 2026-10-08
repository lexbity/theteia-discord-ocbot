//! Pure path utilities used by the git-truth handlers (spec P7).
//!
//! This module is deliberately I/O-free: the fs adapter owns `realpath` and
//! containment re-checks (Phase 6, FR-35). Here we only need a deterministic
//! branch-name slug and a stable short hash for `custom_id`s that must fit in
//! Discord's 100-byte cap without holding UI state in memory (D-6).

/// The maximum slug length (spec P7: `[a-z0-9-]{1,40}`).
pub const SLUG_MAX: usize = 40;

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

/// A stable 64-bit FNV-1a hash of a directory path, rendered as 16 hex chars.
///
/// Used in `v1:release:<dir_hash>` so a button can name a directory without
/// carrying it (or any UI state) in the `custom_id` (D-6). The mapping is
/// resolved at click time by hashing each held-lock directory and comparing.
pub fn dir_hash(dir: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in dir.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::{SLUG_MAX, dir_hash, slug};

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
    fn dir_hash_is_stable_and_short() {
        let a = dir_hash("/home/lex/work");
        assert_eq!(a.len(), 16);
        assert_eq!(a, dir_hash("/home/lex/work"));
        assert_ne!(a, dir_hash("/home/lex/other"));
    }
}

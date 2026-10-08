//! The filesystem adapter (spec P11, FR-34/FR-35/FR-36): the only code that
//! turns a path into a real, canonical location. Every use re-runs `realpath`
//! and the containment check (TOCTOU), so a symlink swapped between the browse
//! and the send is caught at the moment of use, not just when it was listed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use ocbot_core::FsError;
use ocbot_core::paths::{content_tag8, is_inside, sanitize_name, yyyymmdd};
use ocbot_core::ports::{Clock, DirEntry, FsPort};

/// A real filesystem adapter rooted at `roots` (browse) with an `inbox`
/// directory for uploaded attachments.
pub struct FsAdapter {
    roots: Vec<PathBuf>,
    inbox: PathBuf,
    allow_freeform: bool,
    clock: Arc<dyn Clock>,
}

impl FsAdapter {
    /// Build the adapter: `roots` are canonicalized once (they must exist);
    /// `inbox` is created on demand.
    pub fn new(
        roots: Vec<PathBuf>,
        inbox: PathBuf,
        allow_freeform: bool,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, FsError> {
        let mut canonical_roots = Vec::with_capacity(roots.len());
        for root in roots {
            let canonical = root
                .canonicalize()
                .map_err(|error| FsError::NotFound(format!("{}: {error}", root.display())))?;
            canonical_roots.push(canonical);
        }
        Ok(Self {
            roots: canonical_roots,
            inbox,
            allow_freeform,
            clock,
        })
    }

    /// Whether `path` (already canonical) is inside one of the roots, or
    /// containment is disabled.
    fn contained(&self, canonical: &Path) -> bool {
        self.allow_freeform || self.roots.iter().any(|root| is_inside(root, canonical))
    }
}

#[async_trait]
impl FsPort for FsAdapter {
    async fn resolve(&self, path: &Path) -> Result<PathBuf, FsError> {
        let canonical = path
            .canonicalize()
            .map_err(|_| FsError::NotFound(path.display().to_string()))?;
        if self.contained(&canonical) {
            Ok(canonical)
        } else {
            Err(FsError::OutsideRoots {
                path: path.display().to_string(),
            })
        }
    }

    async fn read_text(&self, path: &Path) -> Result<String, FsError> {
        let resolved = self.resolve(path).await?;
        std::fs::read_to_string(&resolved).map_err(|error| FsError::Io(error.to_string()))
    }

    async fn list(&self, path: &Path, show_hidden: bool) -> Result<Vec<DirEntry>, FsError> {
        let resolved = self.resolve(path).await?;
        let read = std::fs::read_dir(&resolved).map_err(|error| FsError::Io(error.to_string()))?;
        let mut entries = Vec::new();
        for entry in read {
            let entry = entry.map_err(|error| FsError::Io(error.to_string()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|error| FsError::Io(error.to_string()))?;
            entries.push(DirEntry {
                name,
                path: entry.path().to_string_lossy().into_owned(),
                is_dir: file_type.is_dir(),
            });
        }
        // Directories first, then case-insensitive name (stable, deterministic).
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    async fn save_attachment(&self, filename: &str, bytes: &[u8]) -> Result<PathBuf, FsError> {
        let bucket = self.inbox.join(yyyymmdd(self.clock.now()));
        std::fs::create_dir_all(&bucket).map_err(|error| FsError::Io(error.to_string()))?;
        let name = format!("{}-{}", content_tag8(bytes), sanitize_name(filename));
        let path = bucket.join(name);
        std::fs::write(&path, bytes).map_err(|error| FsError::Io(error.to_string()))?;
        Ok(path)
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn allow_freeform(&self) -> bool {
        self.allow_freeform
    }
}

#[cfg(test)]
mod tests {
    use super::FsAdapter;
    use ocbot_core::ports::FsPort;
    use std::path::PathBuf;
    use std::sync::Arc;

    struct FixedClock(i64);
    impl ocbot_core::ports::Clock for FixedClock {
        fn now(&self) -> i64 {
            self.0
        }
    }

    #[tokio::test]
    async fn resolve_rejects_paths_outside_the_roots() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(root.path().join("a.txt"), "hi").expect("write");
        std::fs::write(outside.path().join("b.txt"), "hi").expect("write");
        let adapter = FsAdapter::new(
            vec![root.path().to_path_buf()],
            root.path().join("inbox"),
            false,
            Arc::new(FixedClock(0)),
        )
        .expect("adapter");

        assert!(adapter.resolve(&root.path().join("a.txt")).await.is_ok());
        let error = adapter
            .resolve(&outside.path().join("b.txt"))
            .await
            .expect_err("outside");
        assert!(matches!(error, ocbot_core::FsError::OutsideRoots { .. }));
    }

    #[tokio::test]
    async fn list_hides_dotfiles_and_orders_dirs_first() {
        let root = tempfile::tempdir().expect("root");
        std::fs::create_dir(root.path().join("dir")).expect("mkdir");
        std::fs::write(root.path().join("b.txt"), "b").expect("write");
        std::fs::write(root.path().join(".hidden"), "h").expect("write");
        let adapter = FsAdapter::new(
            vec![root.path().to_path_buf()],
            root.path().join("inbox"),
            false,
            Arc::new(FixedClock(0)),
        )
        .expect("adapter");

        let entries = adapter.list(root.path(), false).await.expect("list");
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["dir", "b.txt"]);
        let with_hidden = adapter.list(root.path(), true).await.expect("list");
        assert!(with_hidden.iter().any(|entry| entry.name == ".hidden"));
    }

    #[tokio::test]
    async fn attachment_path_is_sanitized_and_dated() {
        let root = tempfile::tempdir().expect("root");
        let adapter = FsAdapter::new(
            vec![root.path().to_path_buf()],
            root.path().join("inbox"),
            false,
            Arc::new(FixedClock(1_700_000_000)),
        )
        .expect("adapter");
        let path: PathBuf = adapter
            .save_attachment("../evil\n.txt", b"payload")
            .await
            .expect("save");
        let as_text = path.to_string_lossy();
        assert!(as_text.contains("20231114"), "date bucket: {as_text}");
        assert!(!as_text.contains(".."), "separators dropped: {as_text}");
        assert!(
            !as_text.contains('\n'),
            "control chars sanitized: {as_text}"
        );
        assert!(as_text.ends_with(".txt"), "{as_text}");
        assert!(path.exists());
    }
}

//! Project adapters for SPEC-005: project-centric remote control.
//!
//! This module provides the concrete implementations of [`ProjectPort`] and
//! [`ProjectConfigPort`] for filesystem-based project discovery and metadata
//! retrieval.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ocbot_core::error::{ProjectConfigError, ProjectError};
use ocbot_core::paths::project_display_name;
use ocbot_core::ports::{PlanInfo, ProjectConfig, ProjectConfigPort, ProjectDetails, ProjectInfo, ProjectPort};

/// Adapter for loading per-project `ocbot.toml` configuration.
pub struct TomlProjectConfigAdapter;

impl ProjectConfigPort for TomlProjectConfigAdapter {
    fn load_config(&self, dir: &Path) -> Result<Option<ProjectConfig>, ProjectConfigError> {
        let config_path = dir.join("ocbot.toml");

        if !config_path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&config_path)
            .map_err(|e| ProjectConfigError::IoError(config_path.display().to_string(), e.to_string()))?;

        let config: ProjectConfig = toml::from_str(&content)
            .map_err(|e| ProjectConfigError::ParseError(config_path.display().to_string(), e.to_string()))?;

        Ok(Some(config))
    }
}

/// Filesystem adapter for project operations (SPEC-005).
pub struct FsProjectAdapter {
    projects_root: PathBuf,
    default_plans_dirs: Vec<String>,
    default_readme_files: Vec<String>,
    config_adapter: Arc<dyn ProjectConfigPort>,
}

impl FsProjectAdapter {
    /// Create a new filesystem project adapter.
    ///
    /// # Arguments
    ///
    /// * `projects_root` - Root directory containing all projects
    /// * `default_plans_dirs` - Default directories to search for plans
    /// * `default_readme_files` - Default filenames to look for as README
    /// * `config_adapter` - Adapter for loading per-project config
    pub fn new(
        projects_root: PathBuf,
        default_plans_dirs: Vec<String>,
        default_readme_files: Vec<String>,
        config_adapter: Arc<dyn ProjectConfigPort>,
    ) -> Self {
        Self {
            projects_root,
            default_plans_dirs,
            default_readme_files,
            config_adapter,
        }
    }

    /// Find and read the README file from a project directory.
    /// Returns the first 5 lines if found.
    fn find_readme_preview(
        project_dir: &Path,
        primary_readme: &str,
        fallbacks: &[String],
    ) -> Result<Option<String>, std::io::Error> {
        // Try primary first
        let primary_path = project_dir.join(primary_readme);
        if primary_path.exists() {
            let content = std::fs::read_to_string(&primary_path)?;
            return Ok(Some(take_first_n_lines(&content, 5)));
        }

        // Try fallbacks
        for fallback in fallbacks {
            let path = project_dir.join(fallback);
            if path.exists() {
                let content = std::fs::read_to_string(&path)?;
                return Ok(Some(take_first_n_lines(&content, 5)));
            }
        }

        Ok(None)
    }

    /// Scan a directory for plan files (*.md).
    fn scan_plans(plans_dir: &Path) -> Result<Vec<PlanInfo>, std::io::Error> {
        let mut plans = Vec::new();

        if !plans_dir.exists() {
            return Ok(plans);
        }

        for entry in std::fs::read_dir(plans_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().is_some_and(|ext| ext == "md") {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                plans.push(PlanInfo { name, path });
            }
        }

        // Sort alphabetically by name
        plans.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(plans)
    }

    /// Collect all plans from multiple plan directories.
    /// Deduplicates by plan name (first occurrence wins).
    fn collect_all_plans(
        project_dir: &Path,
        plans_dirs: &[String],
    ) -> Result<Vec<PlanInfo>, std::io::Error> {
        let mut all_plans = Vec::new();
        let mut seen_names = HashSet::new();

        for plans_dir in plans_dirs {
            let full_plans_dir = project_dir.join(plans_dir);
            let plans = Self::scan_plans(&full_plans_dir)?;

            for plan in plans {
                if seen_names.insert(plan.name.clone()) {
                    all_plans.push(plan);
                }
            }
        }

        Ok(all_plans)
    }

}

impl ProjectPort for FsProjectAdapter {
    fn list_projects(&self) -> Result<Vec<ProjectInfo>, ProjectError> {
        let mut projects = Vec::new();

        let entries = std::fs::read_dir(&self.projects_root)
            .map_err(|e| ProjectError::IoError(self.projects_root.display().to_string(), e.to_string()))?;

        for entry in entries {
            let entry = entry
                .map_err(|e| ProjectError::IoError(self.projects_root.display().to_string(), e.to_string()))?;

            if entry
                .file_type()
                .map_err(|e| ProjectError::IoError(entry.path().display().to_string(), e.to_string()))?
                .is_dir()
            {
                let display_name = project_display_name(&entry.path());
                projects.push(ProjectInfo {
                    name: display_name,
                    dir: entry.path(),
                });
            }
        }

        if projects.is_empty() {
            return Err(ProjectError::NoProjects(self.projects_root.display().to_string()));
        }

        // Sort alphabetically by display name
        projects.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(projects)
    }

    fn get_project_info(&self, project_name: &str) -> Result<ProjectDetails, ProjectError> {
        // Resolve project_name to full path
        let project_dir = self.get_project_dir(project_name)?;

        // Load project config
        let project_config = self
            .config_adapter
            .load_config(&project_dir)
            .map_err(|e| {
                ProjectError::IoError(project_dir.display().to_string(), e.to_string())
            })?;

        let config = project_config.unwrap_or_default();

        // Determine which plans_dirs and readme_file to use
        let plans_dirs = if config.plans_dirs.is_empty() {
            self.default_plans_dirs.clone()
        } else {
            config.plans_dirs.clone()
        };

        let readme_file = if config.readme_file.is_empty() {
            self.default_readme_files[0].clone()
        } else {
            config.readme_file.clone()
        };

        // Find and read README preview
        let readme_preview = Self::find_readme_preview(&project_dir, &readme_file, &self.default_readme_files)
            .map_err(|e| {
                ProjectError::ReadError(project_dir.display().to_string(), e.to_string())
            })?;

        // Collect all plans from all plan directories
        let plans = Self::collect_all_plans(&project_dir, &plans_dirs)
            .map_err(|e| {
                ProjectError::ReadError(project_dir.display().to_string(), e.to_string())
            })?;

        Ok(ProjectDetails {
            name: project_name.to_owned(),
            readme_preview,
            plans,
        })
    }

    fn get_readme(&self, project_name: &str) -> Result<Option<String>, ProjectError> {
        let project_dir = self.get_project_dir(project_name)?;

        // Load project config to get custom readme_file
        let project_config = self
            .config_adapter
            .load_config(&project_dir)
            .map_err(|e| {
                ProjectError::IoError(project_dir.display().to_string(), e.to_string())
            })?;

        let config = project_config.unwrap_or_default();
        let readme_file = if config.readme_file.is_empty() {
            self.default_readme_files[0].clone()
        } else {
            config.readme_file.clone()
        };

        // Try primary readme file
        let readme_path = project_dir.join(&readme_file);
        if readme_path.exists() {
            return std::fs::read_to_string(&readme_path)
                .map(Some)
                .map_err(|e| {
                    ProjectError::ReadError(readme_path.display().to_string(), e.to_string())
                });
        }

        // Try all default readme files
        for default in &self.default_readme_files {
            let path = project_dir.join(default);
            if path.exists() {
                return std::fs::read_to_string(&path)
                    .map(Some)
                    .map_err(|e| {
                        ProjectError::ReadError(path.display().to_string(), e.to_string())
                    });
            }
        }

        Ok(None)
    }

    fn get_plan_content(&self, project_name: &str, plan_name: &str) -> Result<String, ProjectError> {
        let plan_path = self.get_plan_path(project_name, plan_name)?;

        std::fs::read_to_string(&plan_path)
            .map_err(|e| ProjectError::ReadError(plan_path.display().to_string(), e.to_string()))
    }

    fn get_project_dir(&self, project_name: &str) -> Result<PathBuf, ProjectError> {
        let projects = match self.list_projects() {
            Ok(projects) => projects,
            Err(ProjectError::NoProjects(_)) => {
                // No projects exist at all, so any project name is not found
                return Err(ProjectError::NotFound(project_name.to_owned()));
            }
            Err(e) => return Err(e),
        };

        projects
            .into_iter()
            .find(|p| p.name == project_name)
            .map(|p| p.dir.clone())
            .ok_or_else(|| ProjectError::NotFound(project_name.to_owned()))
    }

    fn get_plan_path(&self, project_name: &str, plan_name: &str) -> Result<PathBuf, ProjectError> {
        let project_dir = self.get_project_dir(project_name)?;

        // Load project config to get custom plans_dirs
        let project_config = self
            .config_adapter
            .load_config(&project_dir)
            .map_err(|e| {
                ProjectError::IoError(project_dir.display().to_string(), e.to_string())
            })?;

        let config = project_config.unwrap_or_default();
        let plans_dirs = if config.plans_dirs.is_empty() {
            self.default_plans_dirs.clone()
        } else {
            config.plans_dirs.clone()
        };

        // Search for the plan in all plan directories
        for plans_dir in &plans_dirs {
            let full_plans_dir = project_dir.join(plans_dir);
            let plan_path = full_plans_dir.join(plan_name);

            if plan_path.exists() {
                return Ok(plan_path);
            }
        }

        Err(ProjectError::NotFound(format!(
            "{project_name}/{plan_name}"
        )))
    }
}

/// Take first N lines from text.
fn take_first_n_lines(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    // Mock config adapter for testing
    struct MockConfigAdapter;

    impl ProjectConfigPort for MockConfigAdapter {
        fn load_config(&self, _dir: &Path) -> Result<Option<ProjectConfig>, ProjectConfigError> {
            Ok(None)
        }
    }

    #[test]
    fn test_list_projects_empty() {
        let temp_dir = tempdir().unwrap();
        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            ocbot_core::ports::default_plans_dirs(),
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        let result = adapter.list_projects();
        assert!(matches!(result, Err(ProjectError::NoProjects(_))));
    }

    #[test]
    fn test_list_projects_single() {
        let temp_dir = tempdir().unwrap();
        let project_dir = temp_dir.path().join("test-project");
        fs::create_dir(&project_dir).unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            ocbot_core::ports::default_plans_dirs(),
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        let projects = adapter.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        // The display name includes the temp dir prefix, so we check it contains the project name
        assert!(projects[0].name.contains("test-project"));
    }

    #[test]
    fn test_list_projects_multiple() {
        let temp_dir = tempdir().unwrap();
        let project1 = temp_dir.path().join("alpha");
        let project2 = temp_dir.path().join("beta");
        let project3 = temp_dir.path().join("gamma");

        fs::create_dir(&project1).unwrap();
        fs::create_dir(&project2).unwrap();
        fs::create_dir(&project3).unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            ocbot_core::ports::default_plans_dirs(),
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        let projects = adapter.list_projects().unwrap();
        assert_eq!(projects.len(), 3);
        // Sorted alphabetically
        assert!(projects[0].name.contains("alpha"));
        assert!(projects[1].name.contains("beta"));
        assert!(projects[2].name.contains("gamma"));
    }

    #[test]
    fn test_list_projects_nested_not_included() {
        let temp_dir = tempdir().unwrap();
        let project_dir = temp_dir.path().join("test-project");
        let nested_dir = project_dir.join("nested");

        fs::create_dir(&project_dir).unwrap();
        fs::create_dir(&nested_dir).unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            ocbot_core::ports::default_plans_dirs(),
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        let projects = adapter.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        assert!(projects[0].name.contains("test-project"));
    }

    #[test]
    fn test_get_project_dir() {
        let temp_dir = tempdir().unwrap();
        let project_dir = temp_dir.path().join("my-project");
        fs::create_dir(&project_dir).unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            ocbot_core::ports::default_plans_dirs(),
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        // List projects to get the actual display name
        let projects = adapter.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        let display_name = &projects[0].name;

        let result = adapter.get_project_dir(display_name);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), project_dir);
    }

    #[test]
    fn test_get_project_dir_not_found() {
        let temp_dir = tempdir().unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            ocbot_core::ports::default_plans_dirs(),
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        let result = adapter.get_project_dir("nonexistent");
        assert!(matches!(result, Err(ProjectError::NotFound(_))));
    }

    #[test]
    fn test_get_project_info_no_readme_no_plans() {
        let temp_dir = tempdir().unwrap();
        let project_dir = temp_dir.path().join("empty-project");
        fs::create_dir(&project_dir).unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            ocbot_core::ports::default_plans_dirs(),
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        let projects = adapter.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        let display_name = &projects[0].name;
        let info = adapter.get_project_info(display_name).unwrap();
        assert!(info.name.contains("empty-project"));
        assert!(info.readme_preview.is_none());
        assert!(info.plans.is_empty());
    }

    #[test]
    fn test_get_project_info_with_readme() {
        let temp_dir = tempdir().unwrap();
        let project_dir = temp_dir.path().join("with-readme");
        fs::create_dir(&project_dir).unwrap();

        let readme_path = project_dir.join("README.md");
        fs::write(&readme_path, "# Test Project\n\nDescription here\n\nMore text").unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            ocbot_core::ports::default_plans_dirs(),
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        let projects = adapter.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        let display_name = &projects[0].name;
        let info = adapter.get_project_info(display_name).unwrap();
        assert!(info.name.contains("with-readme"));
        assert!(info.readme_preview.is_some());
        // Should have first 5 lines (or fewer if not available)
        let preview = info.readme_preview.unwrap();
        assert!(preview.contains("# Test Project"));
        assert!(preview.contains("Description here"));
    }

    #[test]
    fn test_get_project_info_with_plans() {
        let temp_dir = tempdir().unwrap();
        let project_dir = temp_dir.path().join("with-plans");
        let devdocs_dir = project_dir.join("devdocs");
        let plans_dir = devdocs_dir.join("plans");

        fs::create_dir(&project_dir).unwrap();
        fs::create_dir(&devdocs_dir).unwrap();
        fs::create_dir(&plans_dir).unwrap();

        fs::write(plans_dir.join("plan1.md"), "# Plan 1").unwrap();
        fs::write(plans_dir.join("plan2.md"), "# Plan 2").unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            ocbot_core::ports::default_plans_dirs(),
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        let projects = adapter.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        let display_name = &projects[0].name;
        let info = adapter.get_project_info(display_name).unwrap();
        assert!(info.name.contains("with-plans"));
        assert_eq!(info.plans.len(), 2);
        assert_eq!(info.plans[0].name, "plan1.md");
        assert_eq!(info.plans[1].name, "plan2.md");
    }

    #[test]
    fn test_get_readme() {
        let temp_dir = tempdir().unwrap();
        let project_dir = temp_dir.path().join("readme-test");
        fs::create_dir(&project_dir).unwrap();

        let readme_path = project_dir.join("README.md");
        let readme_content = "# Full README\n\nThis is the full content";
        fs::write(&readme_path, readme_content).unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            ocbot_core::ports::default_plans_dirs(),
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        let projects = adapter.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        let display_name = &projects[0].name;
        let result = adapter.get_readme(display_name).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), readme_content);
    }

    #[test]
    fn test_get_readme_fallback() {
        let temp_dir = tempdir().unwrap();
        let project_dir = temp_dir.path().join("fallback-test");
        fs::create_dir(&project_dir).unwrap();

        // Create a non-default README
        let readme_path = project_dir.join("README.txt");
        fs::write(&readme_path, "Text README").unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            ocbot_core::ports::default_plans_dirs(),
            vec!["README.txt".to_owned(), "README.md".to_owned()],
            Arc::new(MockConfigAdapter),
        );

        let projects = adapter.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        let display_name = &projects[0].name;
        let result = adapter.get_readme(display_name).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "Text README");
    }

    #[test]
    fn test_get_plan_content() {
        let temp_dir = tempdir().unwrap();
        let project_dir = temp_dir.path().join("plan-test");
        let devdocs_dir = project_dir.join("devdocs");
        let plans_dir = devdocs_dir.join("plans");

        fs::create_dir(&project_dir).unwrap();
        fs::create_dir(&devdocs_dir).unwrap();
        fs::create_dir(&plans_dir).unwrap();

        let plan_path = plans_dir.join("test-plan.md");
        let plan_content = "# Test Plan\n\nContent here";
        fs::write(&plan_path, plan_content).unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            ocbot_core::ports::default_plans_dirs(),
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        let projects = adapter.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        let display_name = &projects[0].name;
        let result = adapter
            .get_plan_content(display_name, "test-plan.md")
            .unwrap();
        assert_eq!(result, plan_content);
    }

    #[test]
    fn test_get_plan_path() {
        let temp_dir = tempdir().unwrap();
        let project_dir = temp_dir.path().join("path-test");
        let devdocs_dir = project_dir.join("devdocs");
        let plans_dir = devdocs_dir.join("plans");

        fs::create_dir(&project_dir).unwrap();
        fs::create_dir(&devdocs_dir).unwrap();
        fs::create_dir(&plans_dir).unwrap();

        let plan_path = plans_dir.join("my-plan.md");
        fs::write(&plan_path, "content").unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            ocbot_core::ports::default_plans_dirs(),
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        let projects = adapter.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        let display_name = &projects[0].name;
        let result = adapter.get_plan_path(display_name, "my-plan.md").unwrap();
        assert_eq!(result, plan_path);
    }

    #[test]
    fn test_get_plan_path_not_found() {
        let temp_dir = tempdir().unwrap();
        let project_dir = temp_dir.path().join("empty");
        fs::create_dir(&project_dir).unwrap();

        // Create the default plans directory structure but don't add any plans
        let devdocs_dir = project_dir.join("devdocs");
        let plans_dir = devdocs_dir.join("plans");
        fs::create_dir(&devdocs_dir).unwrap();
        fs::create_dir(&plans_dir).unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            ocbot_core::ports::default_plans_dirs(),
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        let projects = adapter.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        let display_name = &projects[0].name;
        let result = adapter.get_plan_path(display_name, "nonexistent.md");
        assert!(matches!(result, Err(ProjectError::NotFound(_))));
    }

    #[test]
    fn test_scan_plans_empty_dir() {
        let temp_dir = tempdir().unwrap();
        let plans_dir = temp_dir.path().join("plans");
        fs::create_dir(&plans_dir).unwrap();

        let plans = FsProjectAdapter::scan_plans(&plans_dir).unwrap();
        assert!(plans.is_empty());
    }

    #[test]
    fn test_scan_plans_with_md_files() {
        let temp_dir = tempdir().unwrap();
        let plans_dir = temp_dir.path().join("plans");
        fs::create_dir(&plans_dir).unwrap();

        fs::write(plans_dir.join("a.md"), "").unwrap();
        fs::write(plans_dir.join("b.md"), "").unwrap();
        fs::write(plans_dir.join("c.txt"), "").unwrap(); // Not .md, should be skipped

        let plans = FsProjectAdapter::scan_plans(&plans_dir).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].name, "a.md");
        assert_eq!(plans[1].name, "b.md");
    }

    #[test]
    fn test_scan_plans_sorted() {
        let temp_dir = tempdir().unwrap();
        let plans_dir = temp_dir.path().join("plans");
        fs::create_dir(&plans_dir).unwrap();

        fs::write(plans_dir.join("z.md"), "").unwrap();
        fs::write(plans_dir.join("a.md"), "").unwrap();
        fs::write(plans_dir.join("m.md"), "").unwrap();

        let plans = FsProjectAdapter::scan_plans(&plans_dir).unwrap();
        assert_eq!(plans.len(), 3);
        assert_eq!(plans[0].name, "a.md");
        assert_eq!(plans[1].name, "m.md");
        assert_eq!(plans[2].name, "z.md");
    }

    #[test]
    fn test_collect_all_plans_multiple_dirs() {
        let temp_dir = tempdir().unwrap();
        // Create a proper project structure
        let project_dir = temp_dir.path().join("project");
        let devdocs_dir = project_dir.join("devdocs");
        let plans_dir1 = devdocs_dir.join("plans");
        let plans_dir2 = project_dir.join(".plans");

        fs::create_dir(&project_dir).unwrap();
        fs::create_dir(&devdocs_dir).unwrap();
        fs::create_dir(&plans_dir1).unwrap();
        fs::create_dir(&plans_dir2).unwrap();

        fs::write(plans_dir1.join("a.md"), "").unwrap();
        fs::write(plans_dir2.join("b.md"), "").unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            vec!["devdocs/plans".to_owned(), ".plans".to_owned()],
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        let projects = adapter.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        let display_name = &projects[0].name;
        let project_dir_resolved = adapter.get_project_dir(display_name).unwrap();

        let plans = FsProjectAdapter::collect_all_plans(&project_dir_resolved, &["devdocs/plans".to_owned(), ".plans".to_owned()])
            .unwrap();
        assert_eq!(plans.len(), 2);
    }

    #[test]
    fn test_collect_all_plans_deduplicates() {
        let temp_dir = tempdir().unwrap();
        // Create a proper project structure
        let project_dir = temp_dir.path().join("project");
        let devdocs_dir = project_dir.join("devdocs");
        let plans_dir1 = devdocs_dir.join("plans");
        let plans_dir2 = project_dir.join(".plans");

        fs::create_dir(&project_dir).unwrap();
        fs::create_dir(&devdocs_dir).unwrap();
        fs::create_dir(&plans_dir1).unwrap();
        fs::create_dir(&plans_dir2).unwrap();

        // Same plan name in both directories
        fs::write(plans_dir1.join("duplicate.md"), "first").unwrap();
        fs::write(plans_dir2.join("duplicate.md"), "second").unwrap();

        let adapter = FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            vec!["devdocs/plans".to_owned(), ".plans".to_owned()],
            ocbot_core::ports::default_readme_files(),
            Arc::new(MockConfigAdapter),
        );

        let projects = adapter.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        let display_name = &projects[0].name;
        let project_dir_resolved = adapter.get_project_dir(display_name).unwrap();

        let plans = FsProjectAdapter::collect_all_plans(&project_dir_resolved, &["devdocs/plans".to_owned(), ".plans".to_owned()])
            .unwrap();

        // Should only have one "duplicate.md" (first occurrence)
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].name, "duplicate.md");
    }

    #[test]
    fn test_take_first_n_lines() {
        let text = "line1\nline2\nline3\nline4\nline5\nline6";
        assert_eq!(take_first_n_lines(text, 3), "line1\nline2\nline3");
    }

    #[test]
    fn test_take_first_n_lines_more_than_available() {
        let text = "line1\nline2";
        assert_eq!(take_first_n_lines(text, 5), "line1\nline2");
    }

    #[test]
    fn test_take_first_n_lines_empty() {
        assert_eq!(take_first_n_lines("", 5), "");
    }
}

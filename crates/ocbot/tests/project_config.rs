//! SPEC-005 phase 14: the per-project `ocbot.toml` contract against the
//! *real* TOML adapter — precedence, dedup, README fallback and error
//! classes. The inline adapter tests stub the config port; nothing before
//! this file exercised the shipped `TomlProjectConfigAdapter`, which is the
//! entire point of the spec's §Per-Project Configuration.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;

use ocbot::adapters::projects::{FsProjectAdapter, TomlProjectConfigAdapter};
use ocbot_core::error::{ProjectConfigError, ProjectError};
use ocbot_core::ports::{
    ProjectConfig, ProjectConfigPort, ProjectPort, default_plans_dirs, default_readme_files,
};

/// A project root shaped like the spec's §Directory Layout example.
struct Root {
    adapter: FsProjectAdapter,
    name: String,
    dir: std::path::PathBuf,
    temp: tempfile::TempDir,
}

/// Build a root with one project; `config` is the optional `ocbot.toml`
/// body, `plans` maps a relative plan path to its content.
fn make_root(config: Option<&str>, plans: &[(&str, &str)], readmes: &[(&str, &str)]) -> Root {
    let temp = tempfile::tempdir().expect("temp root");
    let dir = temp.path().join("alpha");
    std::fs::create_dir_all(&dir).expect("project dir");
    if let Some(body) = config {
        std::fs::write(dir.join("ocbot.toml"), body).expect("ocbot.toml");
    }
    for (path, body) in plans {
        let plan = dir.join(path);
        std::fs::create_dir_all(plan.parent().expect("plan dir")).expect("mkdir");
        std::fs::write(plan, body).expect("plan");
    }
    for (name, body) in readmes {
        std::fs::write(dir.join(name), body).expect("readme");
    }
    let adapter = FsProjectAdapter::new(
        temp.path().to_path_buf(),
        default_plans_dirs(),
        default_readme_files(),
        Arc::new(TomlProjectConfigAdapter),
    );
    let name = adapter
        .list_projects()
        .expect("projects")
        .into_iter()
        .next()
        .expect("alpha")
        .name;
    Root {
        adapter,
        name,
        dir,
        temp,
    }
}

#[test]
fn absent_ocbot_toml_reads_as_no_project_config() {
    let temp = tempfile::tempdir().expect("temp");
    let loaded = TomlProjectConfigAdapter
        .load_config(temp.path())
        .expect("absent config is not an error");
    assert!(loaded.is_none(), "no file, no config");
}

#[test]
fn spec_shaped_config_parses_with_defaults_for_omitted_fields() {
    // The spec's exact shape: a [ocbot] table with plans_dirs, readme_file
    // omitted -> the default README filename applies.
    let temp = tempfile::tempdir().expect("temp");
    std::fs::write(
        temp.path().join("ocbot.toml"),
        "[ocbot]\nplans_dirs = [\"devdocs/plans\", \".plans\", \"docs/plans\"]\n",
    )
    .expect("write");
    let config = TomlProjectConfigAdapter
        .load_config(temp.path())
        .expect("spec-shaped config parses")
        .expect("present");
    assert_eq!(
        config.plans_dirs,
        vec!["devdocs/plans", ".plans", "docs/plans"]
    );
    assert_eq!(config.readme_file, "README.md", "omitted -> default");
}

#[test]
fn an_empty_table_or_empty_file_falls_back_to_serde_defaults() {
    let temp = tempfile::tempdir().expect("temp");
    std::fs::write(temp.path().join("ocbot.toml"), "[ocbot]\n").expect("write");
    let config = TomlProjectConfigAdapter
        .load_config(temp.path())
        .expect("parses")
        .expect("present");
    assert_eq!(config.plans_dirs, default_plans_dirs());
    assert_eq!(config.readme_file, "README.md");
}

#[test]
fn unknown_keys_and_garbage_are_parse_errors_not_silent_passes() {
    // deny_unknown_fields: a typo'd or foreign key must refuse loudly.
    let temp = tempfile::tempdir().expect("temp");
    std::fs::write(
        temp.path().join("ocbot.toml"),
        "[ocbot]\nplans_dirss = [\"typo\"]\n",
    )
    .expect("write");
    let error = TomlProjectConfigAdapter
        .load_config(temp.path())
        .expect_err("unknown key refused");
    assert!(
        matches!(error, ProjectConfigError::ParseError(..)),
        "{error:?}"
    );

    std::fs::write(temp.path().join("ocbot.toml"), "this is = not = toml").expect("write");
    let error = TomlProjectConfigAdapter
        .load_config(temp.path())
        .expect_err("garbage refused");
    assert!(
        matches!(error, ProjectConfigError::ParseError(..)),
        "{error:?}"
    );
}

#[test]
fn configured_plans_dirs_take_precedence_and_first_dir_wins_duplicates() {
    // SPEC-005 Q5: dirs searched in order, first match wins. roadmap.md
    // exists in both configured dirs with different bytes: the listing shows
    // it once, resolving to the FIRST directory's file.
    let root = make_root(
        Some("[ocbot]\nplans_dirs = [\"devdocs/plans\", \".plans\"]\n"),
        &[
            ("devdocs/plans/roadmap.md", "from devdocs"),
            (".plans/roadmap.md", "from dotplans"),
            (".plans/extra.md", "only in .plans"),
        ],
        &[("README.md", "# Alpha\n")],
    );

    let details = root.adapter.get_project_info(&root.name).expect("details");
    let names: Vec<&str> = details
        .plans
        .iter()
        .map(|plan| plan.name.as_str())
        .collect();
    // Collection keeps directory order (dir1's plans first, each dir sorted);
    // display sorting is the renderer's concern.
    assert_eq!(names, ["roadmap.md", "extra.md"], "dedup keeps one roadmap");

    let roadmap = details
        .plans
        .iter()
        .find(|plan| plan.name == "roadmap.md")
        .expect("roadmap");
    assert!(
        roadmap.path.ends_with("devdocs/plans/roadmap.md"),
        "first configured dir wins: {:?}",
        roadmap.path
    );
    assert_eq!(
        root.adapter
            .get_plan_content(&root.name, "roadmap.md")
            .expect("content"),
        "from devdocs"
    );
    assert_eq!(
        root.adapter
            .get_plan_content(&root.name, "extra.md")
            .expect("second-dir content"),
        "only in .plans"
    );
    assert_eq!(
        root.adapter
            .get_plan_path(&root.name, "roadmap.md")
            .expect("resolved"),
        root.dir.join("devdocs/plans/roadmap.md")
    );
}

#[test]
fn unconfigured_project_uses_the_global_default_plans_dir() {
    let root = make_root(
        None,
        &[
            ("devdocs/plans/spec.md", "# Spec\n"),
            (".plans/hidden.md", "not scanned"),
        ],
        &[("README.md", "# Alpha\n")],
    );
    let details = root.adapter.get_project_info(&root.name).expect("details");
    let names: Vec<&str> = details
        .plans
        .iter()
        .map(|plan| plan.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["spec.md"],
        "default config scans devdocs/plans only"
    );
    // A plan outside the searched dirs is not resolvable.
    let error = root
        .adapter
        .get_plan_path(&root.name, "hidden.md")
        .expect_err("unscanned plan");
    assert!(
        matches!(error, ProjectError::NotFound(ref text)
            if text.contains("hidden.md") && text.contains(&root.name)),
        "{error:?}"
    );
}

#[test]
fn readme_file_override_resolves_then_falls_back_to_the_default_list() {
    // Override present on disk: it wins over README.md.
    let override_hit = make_root(
        Some("[ocbot]\nreadme_file = \"CHANGES.txt\"\n"),
        &[],
        &[("CHANGES.txt", "# Changes\n"), ("README.md", "# Readme\n")],
    );
    assert_eq!(
        override_hit
            .adapter
            .get_readme(&override_hit.name)
            .expect("readme"),
        Some("# Changes\n".to_owned())
    );
    let preview = override_hit
        .adapter
        .get_project_info(&override_hit.name)
        .expect("details")
        .readme_preview
        .expect("preview from the overridden file");
    assert_eq!(preview, "# Changes");

    // Override names a missing file: the default list still rescues it.
    let override_missing = make_root(
        Some("[ocbot]\nreadme_file = \"CHANGES.txt\"\n"),
        &[],
        &[("README.txt", "# Txt\n")],
    );
    assert_eq!(
        override_missing
            .adapter
            .get_readme(&override_missing.name)
            .expect("readme"),
        Some("# Txt\n".to_owned()),
        "fallback walks the global default list"
    );

    // No candidate anywhere: None, not an error.
    let none = make_root(Some("[ocbot]\nreadme_file = \"NOPE.txt\"\n"), &[], &[]);
    assert_eq!(none.adapter.get_readme(&none.name).expect("readme"), None);
    let details = none.adapter.get_project_info(&none.name).expect("details");
    assert!(details.readme_preview.is_none());
    assert!(details.plans.is_empty());
}

#[test]
fn list_projects_skips_plain_files_and_sorts_by_display_name() {
    let temp = tempfile::tempdir().expect("temp");
    std::fs::write(temp.path().join("stray.txt"), "not a project").expect("stray");
    for name in ["zeta", "alpha"] {
        std::fs::create_dir_all(temp.path().join(name)).expect("dir");
    }
    let adapter = FsProjectAdapter::new(
        temp.path().to_path_buf(),
        default_plans_dirs(),
        default_readme_files(),
        Arc::new(TomlProjectConfigAdapter),
    );
    let names: Vec<String> = adapter
        .list_projects()
        .expect("projects")
        .into_iter()
        .map(|project| project.name)
        .collect();
    assert_eq!(names.len(), 2, "the plain file is not a project: {names:?}");
    assert_eq!(
        names[0],
        format!(
            "{}/alpha",
            temp.path().file_name().expect("name").to_string_lossy()
        )
    );
    assert!(names[1].ends_with("/zeta"), "{names:?}");
}

#[test]
fn a_failing_config_port_surfaces_as_io_error_on_every_reader() {
    struct BrokenConfig;
    impl ProjectConfigPort for BrokenConfig {
        fn load_config(&self, _dir: &Path) -> Result<Option<ProjectConfig>, ProjectConfigError> {
            Err(ProjectConfigError::ParseError(
                "test".to_owned(),
                "boom".to_owned(),
            ))
        }
    }
    let temp = tempfile::tempdir().expect("temp");
    let dir = temp.path().join("alpha");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("README.md"), "# Alpha\n").expect("readme");
    std::fs::create_dir_all(dir.join("devdocs/plans")).expect("plans dir");
    std::fs::write(dir.join("devdocs/plans/roadmap.md"), "# R\n").expect("plan");
    let adapter = FsProjectAdapter::new(
        temp.path().to_path_buf(),
        default_plans_dirs(),
        default_readme_files(),
        Arc::new(BrokenConfig),
    );
    let name = adapter
        .list_projects()
        .expect("projects")
        .into_iter()
        .next()
        .expect("alpha")
        .name;

    for result in [
        adapter.get_project_info(&name).map(|_| ()),
        adapter.get_readme(&name).map(|_| ()),
        adapter.get_plan_path(&name, "roadmap.md").map(|_| ()),
        adapter.get_plan_content(&name, "roadmap.md").map(|_| ()),
    ] {
        let error = result.expect_err("config failure must propagate");
        // The mapped message is the ProjectConfigError *Display* — this is
        // the text the operator-facing handler forwards verbatim.
        let ProjectError::IoError(_, message) = &error else {
            panic!("expected IoError mapping, got {error:?}");
        };
        assert_eq!(
            message,
            &ProjectConfigError::ParseError("test".to_owned(), "boom".to_owned()).to_string(),
            "the forwarded text is exactly the ProjectConfigError Display"
        );
    }
}

#[test]
fn unreadable_config_file_surfaces_as_io_error() {
    // An ocbot.toml that exists but cannot be read (here: a directory) is the
    // adapter's IoError class, distinct from the parse-error class.
    let temp = tempfile::tempdir().expect("temp");
    std::fs::create_dir(temp.path().join("ocbot.toml")).expect("obstructing dir");
    let error = TomlProjectConfigAdapter
        .load_config(temp.path())
        .expect_err("unreadable config");
    assert!(
        matches!(error, ProjectConfigError::IoError(..)),
        "{error:?}"
    );
}

#[test]
fn explicit_empty_config_fields_fall_back_to_the_global_defaults() {
    // serde only applies field defaults to MISSING keys: an explicitly empty
    // `plans_dirs = []` / `readme_file = ""` must still fall back to the
    // global configuration, not scan the empty string as a filename.
    let root = make_root(
        Some("[ocbot]\nplans_dirs = []\nreadme_file = \"\"\n"),
        &[("devdocs/plans/spec.md", "# Spec\n")],
        &[("README.md", "# Alpha\nline2\n")],
    );
    let details = root.adapter.get_project_info(&root.name).expect("details");
    let names: Vec<&str> = details
        .plans
        .iter()
        .map(|plan| plan.name.as_str())
        .collect();
    assert_eq!(names, ["spec.md"], "empty list -> global default dirs");
    assert_eq!(details.readme_preview.as_deref(), Some("# Alpha\nline2"));
    assert_eq!(
        root.adapter.get_readme(&root.name).expect("readme"),
        Some("# Alpha\nline2\n".to_owned()),
        "empty filename -> default list"
    );
    assert!(
        root.adapter
            .get_plan_path(&root.name, "spec.md")
            .expect("resolvable")
            .ends_with("devdocs/plans/spec.md")
    );
}

#[test]
fn preview_falls_back_through_the_default_readme_list() {
    // get_project_info's preview path (not just get_readme) walks the
    // defaults when the configured filename is missing.
    let root = make_root(
        Some("[ocbot]\nreadme_file = \"MISSING.txt\"\n"),
        &[],
        &[("README.txt", "# Txt\n")],
    );
    let details = root.adapter.get_project_info(&root.name).expect("details");
    assert_eq!(details.readme_preview.as_deref(), Some("# Txt"));
}

#[test]
fn obstructing_readme_is_a_read_error_not_a_quiet_fallback() {
    // exists()+unreadable must surface ReadError (both in the primary slot and
    // mid-fallback-list), never silently pretend the file is absent.
    let root = make_root(None, &[], &[]);
    std::fs::create_dir(root.dir.join("README.md")).expect("obstruct README");
    let error = root
        .adapter
        .get_project_info(&root.name)
        .expect_err("primary README obstructed");
    assert!(matches!(error, ProjectError::ReadError(..)), "{error:?}");
    let error = root
        .adapter
        .get_readme(&root.name)
        .expect_err("same via get_readme");
    assert!(matches!(error, ProjectError::ReadError(..)), "{error:?}");

    // README.txt now obstructs the fallback loop (primary config name absent):
    let root2 = make_root(Some("[ocbot]\nreadme_file = \"NONE.txt\"\n"), &[], &[]);
    std::fs::create_dir(root2.dir.join("README.md")).expect("obstruct default slot");
    let error = root2
        .adapter
        .get_readme(&root2.name)
        .expect_err("fallback slot obstructed");
    assert!(matches!(error, ProjectError::ReadError(..)), "{error:?}");
}

#[test]
fn unreadable_plans_directory_surfaces_as_read_error() {
    let root = make_root(None, &[("devdocs/plans/spec.md", "# S\n")], &[]);
    let plans = root.dir.join("devdocs/plans");
    std::fs::set_permissions(&plans, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let result = root.adapter.get_project_info(&root.name);
    std::fs::set_permissions(&plans, std::fs::Permissions::from_mode(0o755)).expect("restore");
    let error = result.expect_err("unreadable plans dir must not pass silently");
    assert!(matches!(error, ProjectError::ReadError(..)), "{error:?}");
}

#[test]
fn removed_root_propagates_io_error_through_the_resolvers() {
    let root = make_root(None, &[], &[]);
    std::fs::remove_dir_all(root.temp.path()).expect("remove root");
    for error in [
        root.adapter.list_projects().err(),
        root.adapter.get_project_dir("anything").err(),
        root.adapter.get_readme("anything").err(),
        root.adapter.get_plan_content("anything", "x.md").err(),
    ]
    .into_iter()
    .flatten()
    {
        assert!(
            matches!(error, ProjectError::IoError(..)),
            "root-gone class must propagate: {error:?}"
        );
    }
}

#[test]
fn empty_root_maps_every_name_to_not_found() {
    // list_projects raises NoProjects on an empty-but-present root; the
    // name-resolvers translate that into NotFound for the caller.
    let temp = tempfile::tempdir().expect("temp");
    std::fs::write(temp.path().join("stray.txt"), "not a project").expect("stray");
    let adapter = FsProjectAdapter::new(
        temp.path().to_path_buf(),
        default_plans_dirs(),
        default_readme_files(),
        Arc::new(TomlProjectConfigAdapter),
    );
    assert!(matches!(
        adapter.list_projects(),
        Err(ProjectError::NoProjects(..))
    ));
    assert!(matches!(
        adapter.get_project_dir("nobody/project"),
        Err(ProjectError::NotFound(name)) if name == "nobody/project"
    ));
}

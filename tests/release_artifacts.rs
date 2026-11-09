use assert_cmd::Command;
use hyperliquid_cli::command_registry::CommandRegistry;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn repo_file(path: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(root.join(path)).unwrap_or_else(|err| panic!("failed to read {path}: {err}"))
}

#[test]
fn oss_release_health_files_are_present() {
    for path in [
        "LICENSE",
        "CONTRIBUTING.md",
        ".github/PULL_REQUEST_TEMPLATE.md",
        "scripts/pre-release-check.sh",
    ] {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(
            root.join(path).exists(),
            "missing OSS release health file {path}"
        );
    }
}

#[test]
fn release_workflow_publishes_the_assets_the_updater_downloads_with_checksums() {
    let release_workflow = repo_file(".github/workflows/release.yml");
    let updater = repo_file("src/update/release.rs");
    let install_script = repo_file("install.sh");

    for asset in [
        "hyperliquid-linux-x86_64.tar.gz",
        "hyperliquid-linux-aarch64.tar.gz",
        "hyperliquid-macos-x86_64.tar.gz",
        "hyperliquid-macos-aarch64.tar.gz",
    ] {
        assert!(
            release_workflow.contains(asset),
            "release.yml does not build {asset}"
        );
        assert!(
            updater.contains(asset),
            "the self-updater does not map {asset}"
        );
    }
    assert!(
        release_workflow.contains("sha256sum"),
        "release.yml publishes no checksums"
    );
    assert!(
        install_script.contains("sha256_check") && install_script.contains("${asset}.sha256"),
        "install.sh does not verify the published checksum"
    );
}

#[test]
fn public_docs_do_not_reference_local_user_paths() {
    for path in ["README.md", "AGENTS.md"] {
        let text = repo_file(path);
        assert!(
            !text.contains("/Users/studio"),
            "{path} should not contain local workstation paths"
        );
    }
}

#[test]
fn ci_and_task_gates_name_only_existing_test_targets() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut targets = BTreeSet::new();
    for path in [".github/workflows/ci.yml", "Taskfile.yml"] {
        let text = repo_file(path);
        let mut words = text.split_whitespace();
        while let Some(word) = words.next() {
            if word == "--test"
                && let Some(target) = words.next()
            {
                assert!(
                    root.join("tests").join(format!("{target}.rs")).exists(),
                    "{path} runs `--test {target}` but tests/{target}.rs does not exist"
                );
                targets.insert(target.to_string());
            }
        }
    }
    assert!(
        targets.contains("contract_fixtures"),
        "no CI or Taskfile gate runs the contract fixtures: {targets:?}"
    );
}

#[test]
fn top_level_help_lists_every_registered_command_group() {
    let output = Command::cargo_bin("hyperliquid")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let help = String::from_utf8(output).unwrap();
    let listed = help
        .lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .filter_map(|line| line.split_whitespace().next())
        .collect::<BTreeSet<_>>();

    let missing = CommandRegistry::get()
        .commands()
        .iter()
        .map(|command| command.command_path[0].as_str())
        .filter(|name| !listed.contains(name))
        .collect::<BTreeSet<_>>();
    assert!(missing.is_empty(), "not in `--help` Commands: {missing:?}");
}

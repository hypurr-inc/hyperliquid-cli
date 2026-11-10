//! What a release has to deliver to the people and programs that fetch it.

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use hyperliquid_cli::commands;

fn repo_file(path: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(root.join(path)).unwrap_or_else(|err| panic!("failed to read {path}: {err}"))
}

/// The archive names inside the quotes of `source`.
fn quoted_archives(source: &str) -> BTreeSet<String> {
    source
        .split('"')
        .filter(|text| text.starts_with("hyperliquid-") && text.ends_with(".tar.gz"))
        .map(str::to_string)
        .collect()
}

/// The self-updater downloads a release asset by name: one the release
/// workflow does not publish is an update that fails with a 404 for everyone
/// on that platform.
#[test]
fn every_asset_the_updater_downloads_is_published_by_the_release_workflow() {
    let workflow = repo_file(".github/workflows/release.yml");
    let published: BTreeSet<&str> = workflow
        .lines()
        .filter_map(|line| line.trim().strip_prefix("asset: "))
        .collect();
    let downloaded = quoted_archives(&repo_file("src/update/release.rs"));

    assert!(!downloaded.is_empty(), "the updater names no archive");
    for asset in &downloaded {
        assert!(
            published.contains(asset.as_str()),
            "the updater downloads {asset}, which release.yml does not publish: {published:?}"
        );
    }
}

/// `install.sh` refuses an archive that does not match the checksum published
/// beside it, so the workflow must publish one for every asset.
#[test]
fn published_assets_carry_the_checksum_the_installer_verifies() {
    let workflow = repo_file(".github/workflows/release.yml");
    let installer = repo_file("install.sh");

    assert!(
        workflow.contains("\"$asset.sha256\""),
        "release.yml publishes no `<asset>.sha256`"
    );
    assert!(
        installer.contains("${asset}.sha256") && installer.contains("sha256_check"),
        "install.sh does not verify the published checksum"
    );
}

#[test]
fn top_level_help_lists_every_registered_command_group() {
    let output = support::hermetic_command()
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

    let missing = commands::registry()
        .commands()
        .iter()
        .map(|command| command.command_path[0].as_str())
        .filter(|name| !listed.contains(name))
        .collect::<BTreeSet<_>>();
    assert!(missing.is_empty(), "not in `--help` Commands: {missing:?}");
}

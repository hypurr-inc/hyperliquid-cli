mod support;

use predicates::prelude::*;
use support::{hermetic_command, run_json};

#[test]
fn every_format_is_accepted_by_its_long_and_short_flag() {
    for flag in ["--format", "-f"] {
        for format in ["pretty", "table", "json"] {
            hermetic_command()
                .args([flag, format, "--help"])
                .assert()
                .success();
        }
    }
}

#[test]
fn the_json_flag_enables_json_mode() {
    let parsed = run_json(hermetic_command().args(["--json", "--version"]));

    assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn only_a_command_that_changes_state_carries_a_notes_block() {
    hermetic_command()
        .args(["orders", "create", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Notes:"))
        .stdout(predicate::str::contains("Risk:"))
        .stdout(predicate::str::contains("Dry-run:"))
        .stdout(predicate::str::contains("Confirmation:"));

    hermetic_command()
        .args(["mids", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Notes:").not());
}

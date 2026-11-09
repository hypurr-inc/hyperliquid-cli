//! A reader that went away (`| head`, a dropped pipe) is the end of the output,
//! never a panic and never a hidden failure: a run that succeeds still exits 0
//! and a run that fails still exits with its own code. The rule lives in
//! `output::sink`; these runs close one of the CLI's two pipes before it writes.

mod support;

use std::process::{Output, Stdio};

use assert_cmd::Command;
use support::{IsolatedHome, std_command};

/// Which of the CLI's pipes nobody reads.
#[derive(Clone, Copy)]
enum Closed {
    Stdout,
    Stderr,
}

/// Run `command` with stdout and stderr piped and the read end of `closed`
/// dropped before the CLI can write to it (starting the process takes far
/// longer than this).
fn run_with_a_closed_pipe(command: &Command, closed: Closed) -> Output {
    let mut child = std_command(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    match closed {
        Closed::Stdout => drop(child.stdout.take()),
        Closed::Stderr => drop(child.stderr.take()),
    }
    child.wait_with_output().unwrap()
}

/// Neither a panic nor the bug report a panic prints.
fn assert_no_panic(stderr: &[u8]) {
    let stderr = String::from_utf8_lossy(stderr);
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    assert!(!stderr.contains("Unexpected error"), "stderr: {stderr}");
}

#[test]
fn an_answer_for_a_closed_stdout_ends_the_run_normally() {
    let home = IsolatedHome::new();
    let mut command = home.json_command();
    command.arg("schema");

    let output = run_with_a_closed_pipe(&command, Closed::Stdout);

    assert_eq!(output.status.code(), Some(0));
    assert_no_panic(&output.stderr);
}

#[test]
fn a_failure_for_a_closed_stdout_keeps_its_exit_code() {
    let home = IsolatedHome::new();
    let mut command = home.json_command();
    command.args(["schema", "no-such-command"]);

    let output = run_with_a_closed_pipe(&command, Closed::Stdout);

    assert_eq!(
        output.status.code(),
        Some(13),
        "the error envelope had nowhere to go, and the exit code still says what failed"
    );
    assert_no_panic(&output.stderr);
}

#[test]
fn a_failure_for_a_closed_stderr_keeps_its_exit_code() {
    let home = IsolatedHome::new();
    let mut command = home.command();
    command.args(["schema", "no-such-command"]);

    let output = run_with_a_closed_pipe(&command, Closed::Stderr);

    assert_eq!(output.status.code(), Some(13));
    assert!(output.stdout.is_empty(), "a human error goes to stderr");
}

/// The config file's permission advice is the first thing a run says on stderr,
/// before its answer.
#[cfg(unix)]
fn home_with_a_loose_config() -> IsolatedHome {
    use std::os::unix::fs::PermissionsExt;

    let home = IsolatedHome::new();
    std::fs::create_dir_all(home.config_dir()).unwrap();
    std::fs::write(home.config_file(), "{}").unwrap();
    std::fs::set_permissions(home.config_file(), std::fs::Permissions::from_mode(0o644)).unwrap();
    home
}

#[cfg(unix)]
#[test]
fn a_notice_for_a_closed_stderr_does_not_stop_the_answer() {
    let home = home_with_a_loose_config();
    let mut command = home.command();
    command.args(["account", "ls"]);

    let open = command.output().unwrap();
    assert!(
        String::from_utf8_lossy(&open.stderr).contains("readable by other users"),
        "the notice is said before the answer"
    );
    let answer = String::from_utf8_lossy(&open.stdout).into_owned();
    assert!(answer.starts_with("ID"), "{answer}");

    let closed = run_with_a_closed_pipe(&command, Closed::Stderr);

    assert_eq!(closed.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&closed.stdout),
        answer,
        "the answer is printed whether or not the notice was heard"
    );
}

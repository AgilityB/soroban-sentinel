//! Integration tests for the `sentinel scan --format junit` output.

use assert_cmd::Command;
use predicates::prelude::*;

/// Checks that `--format junit` is listed in the scan help text.
#[test]
fn cli_junit_help_lists_junit_in_formats() {
    let mut cmd = Command::cargo_bin("sentinel").unwrap();
    cmd.args(["scan", "--help"]);
    cmd.assert()
        .code(0)
        .stdout(predicate::str::contains("junit"));
}

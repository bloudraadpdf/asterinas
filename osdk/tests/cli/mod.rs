// SPDX-License-Identifier: MPL-2.0

use std::fs;

use crate::util::*;

#[test]
fn cli_help_message() {
    let output = cargo_osdk(["-h"]).output().unwrap();
    assert_success(&output);
    assert_stdout_contains_msg(&output, "cargo osdk <COMMAND>");
}

#[test]
fn cli_new_help_message() {
    let output = cargo_osdk(["new", "-h"]).output().unwrap();
    assert_success(&output);
    assert_stdout_contains_msg(&output, "cargo osdk new [OPTIONS] <name>");
}

#[test]
fn cli_build_help_message() {
    let output = cargo_osdk(["build", "-h"]).output().unwrap();
    assert_success(&output);
    assert_stdout_contains_msg(&output, "cargo osdk build [OPTIONS]");
}

#[test]
fn cli_build_rejects_invalid_lockfiles_without_panicking() {
    let work = tempfile::tempdir_in(".").unwrap();
    let work = work.path().canonicalize().unwrap();
    let output = cargo_osdk(["new", "--kernel", "lockfile-kernel"])
        .current_dir(&work)
        .output()
        .unwrap();
    assert_success(&output);
    let kernel = work.join("lockfile-kernel");
    fs::create_dir(kernel.join("lock-directory")).unwrap();
    for path in ["missing.lock", "lock-directory"] {
        let output = cargo_osdk(["build", "--lockfile", path])
            .current_dir(&kernel)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("lockfile"), "{error}");
        assert!(!error.contains("panicked"), "{error}");
    }
}

#[test]
fn cli_build_rejects_a_stale_kernel_lockfile_without_changing_it() {
    let work = tempfile::tempdir_in(".").unwrap();
    let work = work.path().canonicalize().unwrap();
    let output = cargo_osdk(["new", "--kernel", "locked-kernel"])
        .current_dir(&work)
        .output()
        .unwrap();
    assert_success(&output);
    let kernel = work.join("locked-kernel");
    edit_config_files(&kernel);
    let contents = b"version = 4\n";
    fs::write(kernel.join("pinned.lock"), contents).unwrap();
    let output = cargo_osdk(["build", "--lockfile", "pinned.lock"])
        .current_dir(&kernel)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("--locked"), "{error}");
    assert!(!error.contains("panicked"), "{error}");
    assert_eq!(fs::read(kernel.join("pinned.lock")).unwrap(), contents);
}

#[test]
fn cli_run_help_message() {
    let output = cargo_osdk(["run", "-h"]).output().unwrap();
    assert_success(&output);
    assert_stdout_contains_msg(&output, "cargo osdk run [OPTIONS]");
}

#[test]
fn cli_test_help_message() {
    let output = cargo_osdk(["test", "-h"]).output().unwrap();
    assert_success(&output);
    assert_stdout_contains_msg(&output, "cargo osdk test [OPTIONS] [TESTNAME]");
}

#[test]
fn cli_check_help_message() {
    let output = cargo_osdk(["check", "-h"]).output().unwrap();
    assert_success(&output);
    assert_stdout_contains_msg(&output, "cargo osdk check");
}

#[test]
fn cli_clippy_help_message() {
    let output = cargo_osdk(["clippy", "-h"]).output().unwrap();
    assert_success(&output);
    assert_stdout_contains_msg(&output, "cargo osdk clippy");
}

#[test]
fn cli_new_crate_with_hyphen() {
    let output = cargo_osdk(["new", "--kernel", "my-first-os"])
        .output()
        .unwrap();
    assert_success(&output);
    assert!(fs::metadata("my-first-os").is_ok());
    let _ = fs::remove_dir_all("my-first-os");
}

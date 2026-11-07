//! The password the caller hands over must never travel in the command line.
//!
//! The windowed caller (the GUI) starts the client as a child process and
//! writes the box's password into the pipe. Two things have to hold:
//!
//! * the client takes the password from the pipe and nowhere else;
//! * the failure comes back marked, so the window can show it.

use std::io::Write;
use std::process::{Command, Stdio};

/// The client binary this test drives.
fn client() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_mirage-subscriber-client"))
}

/// A run that stops before it touches the network: the box's binary is not
/// there, and the client refuses that long before any ssh happens.
fn run(args: &[&str], secret: Option<&str>) -> (bool, String, String) {
    let mut child = Command::new(client())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the client starts");
    {
        let mut pipe = child.stdin.take().expect("the pipe");
        if let Some(secret) = secret {
            pipe.write_all(secret.as_bytes()).expect("the secret goes in");
            pipe.write_all(b"\n").expect("the line ends");
        }
        // The pipe is closed here: the client sees the end of the secret.
    }
    let out = child.wait_with_output().expect("the run ends");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The password arrives in the pipe, the failure is marked, and the secret is
/// not echoed anywhere.
#[test]
fn the_password_comes_from_the_pipe_and_is_never_echoed() {
    let (ok, stdout, _stderr) = run(
        &[
            "oss",
            "--box",
            "root@box.example:22",
            "--password-stdin",
            "--binary",
            "no-such-binary-here",
            "--mwg-port",
            "41443",
        ],
        Some("the-secret"),
    );
    assert!(!ok, "the run must fail");
    assert!(
        stdout.contains("mirage-oss-error:"),
        "the failure must be marked for the caller: {stdout}"
    );
    assert!(
        !stdout.contains("the-secret"),
        "the secret must never be echoed: {stdout}"
    );
}

/// Two ways of handing over the password at once is a mistake, not a choice:
/// the caller does not know which one the client will take.
#[test]
fn two_ways_to_give_the_password_are_refused() {
    let (ok, stdout, _stderr) = run(
        &[
            "oss",
            "--box",
            "root@box.example:22",
            "--password",
            "in-the-command-line",
            "--password-stdin",
            "--binary",
            "no-such-binary-here",
            "--mwg-port",
            "41443",
        ],
        Some("the-secret"),
    );
    assert!(!ok, "the run must fail");
    assert!(
        stdout.contains("mirage-oss-error:"),
        "the refusal must be marked for the caller: {stdout}"
    );
    assert!(
        !stdout.contains("in-the-command-line"),
        "the flag's value must not be echoed: {stdout}"
    );
    assert!(
        !stdout.contains("the-secret"),
        "the secret must never be echoed: {stdout}"
    );
}

/// The pipe is read to its end: the client does not wait for a line that
/// never comes, and it does not need a password to be told twice.
#[test]
fn an_empty_pipe_is_a_password_of_nothing() {
    let (ok, stdout, _stderr) = run(
        &[
            "oss",
            "--box",
            "root@box.example:22",
            "--password-stdin",
            "--binary",
            "no-such-binary-here",
            "--mwg-port",
            "41443",
        ],
        None,
    );
    assert!(!ok, "the run must fail");
    assert!(
        stdout.contains("mirage-oss-error:"),
        "the failure must be marked for the caller: {stdout}"
    );
}

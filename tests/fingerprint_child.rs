//! SESSION-2, in the real binary: the hidden fingerprint child (`clck --clck-fingerprint-child`)
//! never reports a verified finger unless a PAM transaction succeeded. These run the built `clck`
//! exactly as the locker spawns it, with an empty environment, and need no reader, no fprintd, and
//! no root: every case here must end WITHOUT the success marker.

use std::process::{Command, Stdio};

const CHILD_ARG: &str = "--clck-fingerprint-child";
const MARKER: &[u8] = b"clck-fingerprint-verified\n";

fn child(args: &[&str]) -> (Option<i32>, Vec<u8>) {
    let output = Command::new(env!("CARGO_BIN_EXE_clck"))
        .arg(CHILD_ARG)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("run clck");
    (output.status.code(), output.stdout)
}

fn me() -> String {
    std::process::id().to_string()
}

#[test]
fn malformed_arguments_end_in_a_setup_failure() {
    for args in [
        vec![],
        vec!["clck-fingerprint"],
        vec!["clck-fingerprint", "user", &me()],
        vec!["clck-fingerprint", "user", &me(), "0", "extra"],
        vec!["clck-fingerprint", "", &me(), "0"],
    ] {
        let (code, stdout) = child(&args);
        assert_eq!(code, Some(13), "{args:?}");
        assert!(stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn a_child_whose_parent_is_not_the_locker_refuses_to_run() {
    // The parent pid on the command line is not this process: the child must not authenticate.
    let (code, stdout) = child(&["other", "nobody", "1", "0"]);
    assert_eq!(code, Some(13));
    assert!(stdout.is_empty());
}

#[test]
fn a_missing_or_malformed_service_never_reaches_pam() {
    for service in ["clck-test-absent-service", "../other", "Other", ""] {
        let (code, stdout) = child(&[service, "nobody", &me(), "0"]);
        assert_eq!(code, Some(13), "{service:?}");
        assert!(stdout.is_empty(), "{service:?}");
    }
}

#[test]
fn a_real_pam_transaction_that_denies_is_never_verified() {
    // `/etc/pam.d/other` denies on every mainstream distribution (pam_deny); where it does not
    // exist the child refuses before PAM. Either way: no marker, no status 0. Deliberately not a
    // password stack such as `login`, whose pam_faillock would count a failure for a real user.
    let (code, stdout) = child(&["other", "nobody", &me(), "0"]);
    assert_ne!(code, Some(0));
    assert!(code.is_some(), "the child crashed instead of exiting");
    assert!(stdout.is_empty() && stdout != MARKER);
}

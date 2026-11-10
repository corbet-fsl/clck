//! SESSION-2, in the real binary: the hidden fingerprint child (`clck --clck-fingerprint-child`)
//! never reports a verified finger unless a PAM transaction succeeded. These run the built `clck`
//! exactly as the locker spawns it, with an empty environment, and need no reader, no fprintd and
//! no root. None of them reaches PAM: an authentication attempt against a real service of the host
//! running the tests is never made (it could count a failed login in pam_faillock).

use std::process::{Command, Stdio};

const CHILD_ARG: &str = "--clck-fingerprint-child";
const MARKER: &[u8] = b"clck-fingerprint-verified\n";
const EXIT_SETUP: i32 = 13;

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
        assert_eq!(code, Some(EXIT_SETUP), "{args:?}");
        assert!(stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn a_child_whose_parent_is_not_the_locker_refuses_to_run() {
    // The parent pid on the command line is not this process: the child must not authenticate.
    let (code, stdout) = child(&["clck-test-absent-service", "nobody", "1", "0"]);
    assert_eq!(code, Some(EXIT_SETUP));
    assert!(stdout.is_empty());
}

#[test]
fn stock_missing_or_malformed_services_are_refused_before_pam() {
    for service in [
        // Present on most hosts, and refused by name: never a stock service.
        "other",
        "system-services",
        "system-auth",
        "login",
        "sudo",
        // Dedicated name, but no such file here.
        "clck-test-absent-service",
        "clck-../other",
        "clck-Fingerprint",
        "",
    ] {
        let (code, stdout) = child(&[service, "nobody", &me(), "0"]);
        assert_eq!(code, Some(EXIT_SETUP), "{service:?}");
        assert!(stdout.is_empty() && stdout != MARKER, "{service:?}");
    }
}

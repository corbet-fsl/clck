//! PAM authentication on a dedicated worker thread, so the blocking PAM stack never stalls the
//! render loop (the dashboard/clock keep ticking during a verify). Fail-closed: only a correct
//! password unlocks; every error path stays locked.
//!
//! Privilege model: clck runs entirely as the logged-in user, UNPRIVILEGED. `pam_unix` execs
//! the setuid `unix_chkpwd` to read `/etc/shadow` — that privilege lives in the audited pam helper,
//! never in clck. clck must never be setuid.

use crate::diagnostics::Diagnostics;
use std::ffi::{CStr, CString};
use std::sync::mpsc;
use std::time::Instant;
use zeroize::Zeroizing;

/// Verdict sent worker -> loop. Never carries the password.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthOutcome {
    Unlocked,
    Wrong,
    MaxTries,
    Error,
}

impl AuthOutcome {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Unlocked => "unlocked",
            Self::Wrong => "wrong",
            Self::MaxTries => "max_tries",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthStage {
    Context,
    Authenticate,
}

impl AuthStage {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::Authenticate => "authenticate",
        }
    }
}

pub(crate) struct AuthAttempt {
    pub(crate) id: u64,
    pub(crate) password: Zeroizing<String>,
}

pub(crate) struct AuthResult {
    pub(crate) id: u64,
    pub(crate) outcome: AuthOutcome,
}

/// A `pam_client::ConversationHandler` that answers PAM's password prompts straight out of a
/// zeroizing buffer and nothing else (`AUTH-1`).
///
/// `pam_client::conv_mock::Conversation` (the crate's built-in mock handler) copies the plaintext
/// into a plain, non-zeroizing `String` internally — fine for its intended use (tests), wrong for a
/// lock screen that holds a real login password. This handler holds exactly one `Zeroizing<String>`
/// and never clones it; the single unavoidable exception is the `CString` each prompt answer
/// returns, because that is the type `libpam`'s C API requires — pam-client hands it across the FFI
/// boundary and it is freed there, outside anything we control. That one conversion is the "one
/// CString libpam requires" the crate's contract makes unavoidable; nothing else is copied.
struct PasswordConversation {
    password: Zeroizing<String>,
}

impl pam_client::ConversationHandler for PasswordConversation {
    /// The normal password prompt (`PAM_PROMPT_ECHO_OFF`, characters not echoed).
    fn prompt_echo_off(&mut self, _prompt: &CStr) -> Result<CString, pam_client::ErrorCode> {
        CString::new(self.password.as_bytes()).map_err(|_| pam_client::ErrorCode::CONV_ERR)
    }

    /// Fallback only: some PAM stacks issue an echoed prompt (`PAM_PROMPT_ECHO_ON`) instead. A
    /// plain `pam_unix` password check never does, but answering it the same way (rather than
    /// refusing) keeps a legitimate, differently-configured stack working without ever showing or
    /// echoing anything ourselves.
    fn prompt_echo_on(&mut self, _prompt: &CStr) -> Result<CString, pam_client::ErrorCode> {
        CString::new(self.password.as_bytes()).map_err(|_| pam_client::ErrorCode::CONV_ERR)
    }

    /// `PAM_TEXT_INFO` — ignored. PAM module chatter never reaches the lock screen or a log
    /// (`AUTH-1`: the only thing this handler ever says back is the password itself, to PAM).
    fn text_info(&mut self, _msg: &CStr) {}

    /// `PAM_ERROR_MSG` — ignored, same reasoning as `text_info`.
    fn error_msg(&mut self, _msg: &CStr) {}

    /// Anything asking a yes/no question is unexpected for a password check — fail closed rather
    /// than guess an answer.
    fn radio_prompt(&mut self, _prompt: &CStr) -> Result<bool, pam_client::ErrorCode> {
        Err(pam_client::ErrorCode::CONV_ERR)
    }

    /// Binary protocol exchange is unexpected here too — fail closed.
    fn binary_prompt(
        &mut self,
        _type: u8,
        _data: &[u8],
    ) -> Result<(u8, Vec<u8>), pam_client::ErrorCode> {
        Err(pam_client::ErrorCode::CONV_ERR)
    }
}

/// One PAM attempt, built + run entirely on the worker thread (pam `Context` is `!Send`).
fn authenticate_once(
    service: &str,
    user: &str,
    password: Zeroizing<String>,
) -> (AuthOutcome, AuthStage, Option<pam_client::ErrorCode>) {
    use pam_client::{Context, ErrorCode, Flag};

    let conv = PasswordConversation { password };
    let mut ctx = match Context::new(service, Some(user), conv) {
        Ok(c) => c,
        Err(e) => {
            // Service missing / init failed -> FAIL CLOSED (AUTH-3). Preserve the code for the
            // credential-blind debug event so a configuration error is distinguishable from a
            // wrong secret without ever logging the secret or PAM's prompt text.
            return (AuthOutcome::Error, AuthStage::Context, Some(e.code()));
        }
    };
    // A screen locker re-opens an already-valid, already-logged-in session, so we gate ONLY on the
    // authentication phase (the password) and do NOT call pam_acct_mgmt. This matches swaylock and
    // most lockers: the account phase (pam_faillock's account check, expiry, etc.) is not a screen
    // unlock's concern, and requiring it wrongly rejects a correct password on real PAM stacks
    // (observed: acct_mgmt returning AUTH_ERR after prior auth failures). SESSION-1 still holds:
    // only a correct password reaches AuthOutcome::Unlocked.
    match ctx.authenticate(Flag::NONE) {
        Ok(()) => (AuthOutcome::Unlocked, AuthStage::Authenticate, None),
        Err(e) => {
            let code = e.code();
            let outcome = match code {
                ErrorCode::MAXTRIES => AuthOutcome::MaxTries,
                ErrorCode::AUTH_ERR
                | ErrorCode::USER_UNKNOWN
                | ErrorCode::CRED_INSUFFICIENT
                | ErrorCode::AUTHINFO_UNAVAIL => AuthOutcome::Wrong,
                _ => AuthOutcome::Error, // abort / conversation error -> FAIL CLOSED
            };
            (outcome, AuthStage::Authenticate, Some(code))
        }
    }
    // `ctx` (and the `PasswordConversation` it owns) drops here: the `Zeroizing<String>` wipes
    // itself, and this is the only long-lived copy of the plaintext there ever was.
}

fn emit_auth_diagnostic(
    service: &str,
    (outcome, stage, code): (AuthOutcome, AuthStage, Option<pam_client::ErrorCode>),
) {
    match code {
        Some(code) => eprintln!(
            "clck[diag]: service={service:?} outcome={} stage={} code={code:?}",
            outcome.label(),
            stage.label()
        ),
        None => eprintln!(
            "clck[diag]: service={service:?} outcome={} stage={} code=SUCCESS",
            outcome.label(),
            stage.label()
        ),
    }
}

/// One-shot auth check for `clck --check-auth`: runs the exact live unlock path and prints only
/// the configured service, outcome, failing stage, and symbolic PAM code.
pub fn diagnose(service: &str, user: &str, password: String) {
    let result = authenticate_once(service, user, Zeroizing::new(password));
    emit_auth_diagnostic(service, result);
}

/// Spawns the persistent auth worker. Keeping it persistent lets `pam_faillock` counters and
/// backoff accumulate coherently across attempts. Passwords arrive on `attempts`; verdicts go out
/// on `results` (a calloop channel, which wakes the event loop cleanly).
pub fn spawn(
    service: String,
    user: String,
    attempts: mpsc::Receiver<AuthAttempt>,
    results: calloop::channel::Sender<AuthResult>,
    diagnostics: Diagnostics,
) {
    std::thread::spawn(move || {
        while let Ok(attempt) = attempts.recv() {
            diagnostics.pam_started(attempt.id, &service, &user);
            let started = Instant::now();
            let (outcome, stage, code) = authenticate_once(&service, &user, attempt.password);
            diagnostics.pam_finished(attempt.id, outcome, stage, code, started.elapsed());
            let done = matches!(outcome, AuthOutcome::Unlocked);
            let _ = results.send(AuthResult {
                id: attempt.id,
                outcome,
            });
            if done {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_outcomes_have_stable_credential_blind_labels() {
        assert_eq!(AuthOutcome::Unlocked.label(), "unlocked");
        assert_eq!(AuthOutcome::Wrong.label(), "wrong");
        assert_eq!(AuthOutcome::MaxTries.label(), "max_tries");
        assert_eq!(AuthOutcome::Error.label(), "error");
    }

    fn function_source<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
        source
            .split_once(start)
            .unwrap_or_else(|| panic!("missing function start: {start}"))
            .1
            .split_once(end)
            .unwrap_or_else(|| panic!("missing function end: {end}"))
            .0
    }

    #[test]
    fn check_auth_cannot_log_credential_metadata() {
        let production = include_str!("auth.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production auth source");
        let authenticate = function_source(
            production,
            "fn authenticate_once",
            "fn emit_auth_diagnostic",
        );
        let emitter = function_source(production, "fn emit_auth_diagnostic", "/// One-shot auth");
        let diagnose = function_source(production, "pub fn diagnose", "/// Spawns");

        assert!(!emitter.contains("password"));
        for forbidden in [
            "eprintln!",
            "println!",
            "dbg!",
            "format!",
            "write!",
            "writeln!",
            "tracing::",
            "log::",
            ".len(",
            "pw_len",
        ] {
            for source in [authenticate, diagnose] {
                assert!(
                    !source.contains(forbidden),
                    "check-auth can expose credential metadata through `{forbidden}`"
                );
            }
        }
    }

    #[test]
    fn check_auth_reuses_the_live_authentication_primitive() {
        let production = include_str!("auth.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production auth source");
        let diagnose = function_source(production, "pub fn diagnose", "/// Spawns");

        assert_eq!(diagnose.matches("authenticate_once(").count(), 1);
        for duplicated_pam_call in ["Context::new(", ".authenticate(", ".acct_mgmt("] {
            assert!(
                !diagnose.contains(duplicated_pam_call),
                "check-auth duplicates live PAM semantics through `{duplicated_pam_call}`"
            );
        }
        assert_eq!(production.matches("Context::new(").count(), 1);
        assert_eq!(production.matches(".authenticate(").count(), 1);
        assert_eq!(production.matches(".acct_mgmt(").count(), 0);
    }
}

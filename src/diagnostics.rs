//! Structured, credential-blind diagnostics for one locker process.
//!
//! This module deliberately exposes named methods rather than a general-purpose logging API. Its
//! callers can report lifecycle, display, and PAM verdict metadata, but no credential-bearing type
//! is accepted anywhere in this interface. Debug output goes to stderr, which the seated session's
//! systemd user unit already captures in journald.

use crate::auth::{AuthOutcome, AuthStage};
use crate::content::OutputRole;
use pam_client::ErrorCode;
use std::fmt::Arguments;
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct Diagnostics {
    enabled: bool,
    instance: u32,
}

impl Diagnostics {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            instance: std::process::id(),
        }
    }

    fn emit(&self, fields: Arguments<'_>) {
        if self.enabled {
            eprintln!("clck[debug]: instance={} {fields}", self.instance);
        }
    }

    pub(crate) fn startup(&self) {
        self.emit(format_args!("event=start"));
    }

    pub(crate) fn wayland_connected(&self) {
        self.emit(format_args!("event=wayland_connected"));
    }

    pub(crate) fn lock_requested(&self) {
        self.emit(format_args!("event=lock_requested"));
    }

    pub(crate) fn lock_acquired(&self, outputs: usize) {
        self.emit(format_args!("event=lock_acquired outputs={outputs}"));
    }

    pub(crate) fn daemon_ready(&self, notified: bool) {
        self.emit(format_args!(
            "event=daemon_ready parent_notification={}",
            if notified { "ok" } else { "error" }
        ));
    }

    pub(crate) fn surface_configured(
        &self,
        output: &str,
        role: OutputRole,
        width: u32,
        height: u32,
        scale: u32,
    ) {
        self.emit(format_args!(
            "event=surface_configured output={output:?} role={role:?} width={width} height={height} scale={scale}"
        ));
    }

    pub(crate) fn output_surface_added(
        &self,
        output: &str,
        role: OutputRole,
        reason: &'static str,
    ) {
        self.emit(format_args!(
            "event=output_surface_added output={output:?} role={role:?} reason={reason}"
        ));
    }

    pub(crate) fn output_surface_updated(&self, output: &str, role: OutputRole, scale: u32) {
        self.emit(format_args!(
            "event=output_surface_updated output={output:?} role={role:?} scale={scale}"
        ));
    }

    pub(crate) fn output_surface_removed(&self, output: &str, role: OutputRole) {
        self.emit(format_args!(
            "event=output_surface_removed output={output:?} role={role:?}"
        ));
    }

    pub(crate) fn keyboard_capability(&self, present: bool) {
        self.emit(format_args!(
            "event=keyboard_capability state={}",
            if present { "present" } else { "removed" }
        ));
    }

    pub(crate) fn keyboard_focus(&self, entered: bool, output: Option<(&str, OutputRole)>) {
        let state = if entered { "entered" } else { "left" };
        match output {
            Some((name, role)) => self.emit(format_args!(
                "event=keyboard_focus state={state} output={name:?} role={role:?}"
            )),
            None => self.emit(format_args!(
                "event=keyboard_focus state={state} output=unknown"
            )),
        }
    }

    pub(crate) fn submit_ignored(&self, reason: &'static str) {
        self.emit(format_args!("event=auth_submit_ignored reason={reason}"));
    }

    pub(crate) fn auth_submitted(&self, attempt: u64) {
        self.emit(format_args!("event=auth_submitted attempt={attempt}"));
    }

    pub(crate) fn auth_delivery_failed(&self, attempt: u64) {
        self.emit(format_args!(
            "event=auth_delivery_failed attempt={attempt} reason=worker_unavailable"
        ));
    }

    pub(crate) fn pam_started(&self, attempt: u64, service: &str, user: &str) {
        self.emit(format_args!(
            "event=pam_started attempt={attempt} service={service:?} user={user:?}"
        ));
    }

    pub(crate) fn pam_finished(
        &self,
        attempt: u64,
        outcome: AuthOutcome,
        stage: AuthStage,
        code: Option<ErrorCode>,
        elapsed: Duration,
    ) {
        match code {
            Some(code) => self.emit(format_args!(
                "event=pam_finished attempt={attempt} outcome={} stage={} code={code:?} elapsed_ms={}",
                outcome.label(),
                stage.label(),
                elapsed.as_millis()
            )),
            None => self.emit(format_args!(
                "event=pam_finished attempt={attempt} outcome={} stage={} code=SUCCESS elapsed_ms={}",
                outcome.label(),
                stage.label(),
                elapsed.as_millis()
            )),
        }
    }

    pub(crate) fn auth_result_received(&self, attempt: u64, outcome: AuthOutcome) {
        self.emit(format_args!(
            "event=auth_result_received attempt={attempt} outcome={}",
            outcome.label()
        ));
    }

    pub(crate) fn auth_failed_ui(&self, attempt: u64, backoff: Duration) {
        self.emit(format_args!(
            "event=auth_failed_ui attempt={attempt} backoff_ms={}",
            backoff.as_millis()
        ));
    }

    pub(crate) fn unlock_requested(&self, attempt: u64) {
        self.emit(format_args!("event=unlock_requested attempt={attempt}"));
    }

    pub(crate) fn unlock_flushed(&self, ok: bool) {
        self.emit(format_args!(
            "event=unlock_flushed status={}",
            if ok { "ok" } else { "error" }
        ));
    }

    pub(crate) fn authenticated_exit(&self, attempt: u64) {
        self.emit(format_args!(
            "event=process_exit reason=authenticated attempt={attempt}"
        ));
    }

    pub(crate) fn lock_finished(&self) {
        self.emit(format_args!(
            "event=process_exit reason=compositor_finished_or_rejected"
        ));
    }

    pub(crate) fn sigusr1(&self) {
        self.emit(format_args!("event=sigusr1 action=repaint"));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn diagnostics_interface_has_no_credential_bearing_inputs() {
        let production = include_str!("diagnostics.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production diagnostics source");

        for forbidden in [
            "Zeroizing",
            "PasswordConversation",
            "AuthAttempt",
            "KeyEvent",
            "utf8",
        ] {
            assert!(
                !production.contains(forbidden),
                "diagnostics accepts credential-bearing `{forbidden}` data"
            );
        }
    }
}

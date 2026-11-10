//! The optional fingerprint unlock path (`SESSION-2`).
//!
//! Off unless the host configures `fingerprint_pam_service`. When it is set, clck runs at most one
//! PAM transaction against that service at a time, in a CHILD PROCESS: a hidden re-exec of the
//! clck binary itself (`/proc/self/exe` with [`CHILD_ARG`]). Two reasons for a process rather than
//! a thread:
//!
//! - `pam_fprintd` blocks for up to its own `timeout=` waiting for a finger, and a blocked PAM
//!   call cannot be cancelled from another thread. A child can be killed at any moment, and
//!   fprintd releases the reader as soon as the killed client's bus connection drops.
//! - The PAM module's code (and libsystemd's D-Bus client) never runs inside the process that holds
//!   the session lock and the password buffer. A crash there costs one fingerprint attempt, never
//!   the locker (`LOCK-2`).
//!
//! The password path is untouched and always works; this path can only ADD a PAM-gated way in.
//! Three parts, each testable on its own:
//!
//! - [`Arm`]: the pure arming policy -- when to start an attempt, when to kill one, and which
//!   result counts (only the attempt that is in flight right now, by generation).
//! - the parent side ([`child_command`], [`spawn`], [`collect`], [`FingerprintPath`]): spawn with an
//!   empty environment and only stdio, kill, reap, classify an exit.
//! - [`child_main`]: the child -- one PAM transaction that never answers a prompt, and the success
//!   signal is BOTH an exact marker on stdout AND exit status 0.

use crate::diagnostics::Diagnostics;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// argv[1] of the hidden re-exec. Not a user-facing flag: `--help` does not list it.
pub const CHILD_ARG: &str = "--clck-fingerprint-child";

/// What the child writes to stdout, and nothing else, after PAM returned success. The parent
/// unlocks only on this exact byte string AND exit status 0 (`classify`), so no other way for the
/// child to end with status 0 -- a `--help` fall-through, a library calling `exit(0)` -- can be read
/// as a verified finger.
pub(crate) const MARKER: &[u8] = b"clck-fingerprint-verified\n";

/// Where PAM reads its per-service policy. A configured fingerprint service must be a file here;
/// see [`service_problem`].
pub(crate) const PAM_DIR: &str = "/etc/pam.d";
/// Every fingerprint service name starts with this. The name comes from the user's own config
/// file, so it may only ever select a service the host made FOR this purpose -- never a stock one
/// such as `system-services` (`auth sufficient pam_permit.so` on Arch) or `system-auth` (which
/// would count every silent attempt as a failed login in pam_faillock).
pub(crate) const SERVICE_PREFIX: &str = "clck-";

// Child exit statuses. Only `EXIT_VERIFIED` together with `MARKER` can unlock.
pub(crate) const EXIT_VERIFIED: i32 = 0;
pub(crate) const EXIT_NO_MATCH: i32 = 10;
pub(crate) const EXIT_UNAVAILABLE: i32 = 11;
pub(crate) const EXIT_PAM_ERROR: i32 = 12;
pub(crate) const EXIT_SETUP: i32 = 13;

/// The parent's own hard limit for one attempt. Longer than the `timeout=30` the host's
/// `pam_fprintd` line carries plus D-Bus activation and the reader claim, so a healthy attempt ends
/// by itself first; this only reaps an attempt that hangs.
pub(crate) const ATTEMPT_DEADLINE: Duration = Duration::from_secs(40);
/// Minimum gap between one child ending (or being killed) and the next one starting. fprintd
/// releases a vanished client's claim asynchronously; claiming again within that window fails with
/// "already claimed".
pub(crate) const SETTLE: Duration = Duration::from_millis(1500);
/// A finger that does not match counts; after this many in one lock the fingerprint path stays off
/// until the next lock (the password still works). The same cap phones use against repeated
/// presentation attacks.
pub(crate) const MAX_NO_MATCH: u32 = 5;
/// An attempt that ends without a verdict this fast means the reader, fprintd, or an enrolled print
/// is not there (`pam_fprintd` answers AUTHINFO_UNAVAIL at once). After this many in a row the path
/// stays off for this lock instead of spawning a process on every key press.
pub(crate) const MAX_QUICK_FAILURES: u32 = 3;
pub(crate) const QUICK: Duration = Duration::from_secs(5);
/// Attempts killed at [`ATTEMPT_DEADLINE`] (fprintd or the reader stopped answering). After this
/// many in one lock the path stays off instead of holding a hung reader again on every key press.
pub(crate) const MAX_HUNG: u32 = 3;
/// A jump of CLOCK_BOOTTIME against CLOCK_MONOTONIC larger than this between two 1 s ticks means
/// the machine was suspended in between.
pub(crate) const RESUME_JUMP: Duration = Duration::from_secs(2);

/// Something that should (re)arm the reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// Key or pointer input on a lock surface. Arms only when no attempt is in flight: an attempt
    /// already listening needs no second one, and input during it must not extend the window.
    Input,
    /// A display came back -- an output appeared after the lock was taken (the lid opened), or the
    /// machine resumed from suspend. Arms now when idle, otherwise right after the attempt in flight
    /// ends, because the reader may have been reset underneath it.
    Wake,
}

impl Trigger {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Wake => "wake",
        }
    }
}

/// How one child ended, as the parent reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChildExit {
    /// Exit status 0 and exactly [`MARKER`] on stdout: PAM accepted the finger.
    Verified,
    /// A finger was read and rejected: PAM_MAXTRIES, which `pam_fprintd max-tries=1` returns for
    /// one finger that does not match.
    NoMatch,
    /// No reader, no fprintd, no enrolled print, the reader busy, or pam_fprintd's own timeout.
    Unavailable,
    /// Everything else: a PAM or setup error (PAM_AUTH_ERR included: pam_fprintd returns it when
    /// fprintd refuses to start a verify), a signal, status 0 without the exact marker.
    Failed,
}

impl ChildExit {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::NoMatch => "no_match",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    None,
    /// Start the attempt with this generation.
    Spawn(u64),
    /// Kill the attempt with this generation (its deadline passed); its result no longer counts.
    Kill(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The attempt in flight verified: unlock exactly as a correct password does.
    Unlock,
    /// The attempt in flight ended without a verdict; stay locked.
    Continue,
    /// Not the attempt in flight (killed, superseded, or unknown): ignored, whatever it says.
    Stale,
}

impl Verdict {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Unlock => "unlock",
            Self::Continue => "stay_locked",
            Self::Stale => "ignored_stale",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Disarmed {
    NoMatchLimit,
    Unavailable,
    Hung,
}

impl Disarmed {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NoMatchLimit => "no_match_limit",
            Self::Unavailable => "unavailable",
            Self::Hung => "unresponsive",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct InFlight {
    generation: u64,
    started: Instant,
    deadline: Instant,
}

/// The arming policy for one lock. Pure: it never touches a process, a clock, or the compositor,
/// so every ordering of events can be tested directly.
#[derive(Debug, Default)]
pub(crate) struct Arm {
    /// Set once the compositor confirmed the lock. Nothing arms before that.
    active: bool,
    want: bool,
    in_flight: Option<InFlight>,
    /// The generation of the newest attempt ever started; generations are never reused.
    generation: u64,
    not_before: Option<Instant>,
    no_match: u32,
    quick_failures: u32,
    hung: u32,
    disarmed: Option<Disarmed>,
}

impl Arm {
    /// The compositor confirmed the session lock: arm the first attempt.
    pub(crate) fn on_locked(&mut self) {
        self.active = true;
        self.want = true;
    }

    pub(crate) fn trigger(&mut self, trigger: Trigger) {
        if !self.active || self.disarmed.is_some() {
            return;
        }
        match trigger {
            Trigger::Input if self.in_flight.is_some() => {}
            Trigger::Input | Trigger::Wake => self.want = true,
        }
    }

    /// What to do now. Call after every event and on the 1 s tick.
    pub(crate) fn poll(&mut self, now: Instant) -> Action {
        if !self.active {
            return Action::None;
        }
        if let Some(flight) = self.in_flight {
            if now >= flight.deadline || self.disarmed.is_some() {
                self.in_flight = None;
                self.not_before = Some(now + SETTLE);
                if now >= flight.deadline {
                    self.hung += 1;
                    if self.hung >= MAX_HUNG && self.disarmed.is_none() {
                        self.disarmed = Some(Disarmed::Hung);
                    }
                }
                return Action::Kill(flight.generation);
            }
            return Action::None;
        }
        if self.disarmed.is_some() || !self.want {
            return Action::None;
        }
        if self.not_before.is_some_and(|t| now < t) {
            return Action::None;
        }
        self.want = false;
        self.generation += 1;
        self.in_flight = Some(InFlight {
            generation: self.generation,
            started: now,
            deadline: now + ATTEMPT_DEADLINE,
        });
        Action::Spawn(self.generation)
    }

    /// The child of `generation` ended with `exit`. Only the attempt in flight can yield `Unlock`.
    pub(crate) fn finished(&mut self, generation: u64, exit: ChildExit, now: Instant) -> Verdict {
        let flight = match self.in_flight {
            Some(flight) if flight.generation == generation => flight,
            _ => return Verdict::Stale,
        };
        self.in_flight = None;
        self.not_before = Some(now + SETTLE);
        match exit {
            ChildExit::Verified => Verdict::Unlock,
            ChildExit::NoMatch => {
                self.quick_failures = 0;
                self.no_match += 1;
                if self.no_match >= MAX_NO_MATCH {
                    self.disarmed = Some(Disarmed::NoMatchLimit);
                }
                Verdict::Continue
            }
            ChildExit::Unavailable | ChildExit::Failed => {
                if now.saturating_duration_since(flight.started) < QUICK {
                    self.quick_failures += 1;
                    if self.quick_failures >= MAX_QUICK_FAILURES {
                        self.disarmed = Some(Disarmed::Unavailable);
                    }
                } else {
                    self.quick_failures = 0;
                }
                Verdict::Continue
            }
        }
    }

    pub(crate) fn disarmed(&self) -> Option<Disarmed> {
        self.disarmed
    }

    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> Option<u64> {
        self.in_flight.map(|flight| flight.generation)
    }
}

/// Why a configured fingerprint service is refused, or `None` when it may be used. A refused
/// service leaves the locker password-only; it never falls back to another service (`AUTH-3`).
///
/// Whatever the service's `auth` stack returns as success WITHOUT a prompt unlocks the moment the
/// lock is taken (the first attempt starts then, and the conversation answers no prompt). So the
/// service must be one the host made for exactly this:
/// - a dedicated name: [`SERVICE_PREFIX`] plus lowercase ASCII (Linux-PAM lowercases the name before
///   it opens `/etc/pam.d/<name>`, so only a lowercase name makes the file checked here the file PAM
///   reads), and not the password service;
/// - a file in `/etc/pam.d` that this process can open (Linux-PAM silently uses `/etc/pam.d/other`
///   for a missing or unreadable service), owned by `owner` (root), writable by nobody else, and
///   with at least one `auth` line (otherwise PAM takes `other`'s auth stack).
///
/// `owner` is a parameter only so tests can use files they own.
pub(crate) fn service_problem(
    service: &str,
    password_service: Option<&str>,
    pam_dir: &Path,
    owner: u32,
) -> Option<&'static str> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Some(rest) = service.strip_prefix(SERVICE_PREFIX) else {
        return Some("not_dedicated");
    };
    if rest.is_empty()
        || !rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b))
    {
        return Some("invalid_name");
    }
    if password_service.is_some_and(|password| password.to_ascii_lowercase() == service) {
        return Some("same_as_password_service");
    }
    let mut file = match std::fs::File::open(pam_dir.join(service)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Some("missing"),
        Err(_) => return Some("unreadable"),
    };
    // fstat of the descriptor just opened: the checks below are about exactly the bytes PAM reads.
    let Ok(meta) = file.metadata() else {
        return Some("unreadable");
    };
    if !meta.is_file() {
        return Some("missing");
    }
    if meta.uid() != owner {
        return Some("not_root_owned");
    }
    if meta.permissions().mode() & 0o022 != 0 {
        return Some("writable_by_others");
    }
    let mut text = String::new();
    if (&mut file)
        .take(64 * 1024)
        .read_to_string(&mut text)
        .is_err()
    {
        return Some("unreadable");
    }
    let has_auth = text
        .lines()
        .any(|line| matches!(line.split_whitespace().next(), Some("auth") | Some("-auth")));
    if !has_auth {
        return Some("no_auth_line");
    }
    None
}

/// The child's command line: `/proc/self/exe CHILD_ARG <service> <user> <parent pid> <0|1>`, with
/// an EMPTY environment, stdin from /dev/null, stdout a pipe (the marker), and stderr the parent's
/// (journald) only in debug mode. `program` is a parameter only so tests can substitute a stand-in.
pub(crate) fn child_command(program: &Path, service: &str, user: &str, debug: bool) -> Command {
    let mut command = Command::new(program);
    command
        .arg0("clck-fingerprint")
        .arg(CHILD_ARG)
        .arg(service)
        .arg(user)
        .arg(std::process::id().to_string())
        .arg(if debug { "1" } else { "0" })
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if debug {
            Stdio::inherit()
        } else {
            Stdio::null()
        });
    command
}

/// Spawn `command` and make its stdout pipe non-blocking, so reading the marker after the child
/// exited can never stall the event loop (`AUTH-2`), even if something else still holds the pipe.
pub(crate) fn spawn(mut command: Command) -> io::Result<Child> {
    let mut child = command.spawn()?;
    let nonblocking = child
        .stdout
        .as_ref()
        .ok_or_else(|| io::Error::other("child stdout is not a pipe"))
        .and_then(|stdout| set_nonblocking(stdout.as_raw_fd()));
    if let Err(error) = nonblocking {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(child)
}

fn set_nonblocking(fd: i32) -> io::Result<()> {
    // SAFETY: `fd` is the child stdout pipe this process owns; F_GETFL/F_SETFL only change its
    // status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A pidfd for `child`, readable once it exits. `child` is not reaped yet, so the pid still names
/// exactly this process.
pub(crate) fn pidfd(child: &Child) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open takes a pid and flags and returns a new descriptor (O_CLOEXEC is implied).
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a non-negative return is a new descriptor owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// Reap `child` if it has exited and classify it. `Ok(None)` while it still runs.
pub(crate) fn collect(child: &mut Child) -> io::Result<Option<ChildExit>> {
    let Some(status) = child.try_wait()? else {
        return Ok(None);
    };
    let mut out = [0_u8; MARKER.len() + 1];
    let mut filled = 0;
    if let Some(stdout) = child.stdout.as_mut() {
        while filled < out.len() {
            match stdout.read(&mut out[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }
    Ok(Some(classify(status, &out[..filled])))
}

/// The ONLY way to `ChildExit::Verified`: exit status 0 and stdout exactly [`MARKER`].
pub(crate) fn classify(status: ExitStatus, stdout: &[u8]) -> ChildExit {
    match status.code() {
        Some(EXIT_VERIFIED) if stdout == MARKER => ChildExit::Verified,
        Some(EXIT_NO_MATCH) => ChildExit::NoMatch,
        Some(EXIT_UNAVAILABLE) => ChildExit::Unavailable,
        _ => ChildExit::Failed,
    }
}

/// The child's exit status for a PAM error. Never `EXIT_VERIFIED`.
pub(crate) fn exit_code_for(code: pam_client::ErrorCode) -> i32 {
    use pam_client::ErrorCode;
    match code {
        ErrorCode::MAXTRIES => EXIT_NO_MATCH,
        ErrorCode::AUTHINFO_UNAVAIL
        | ErrorCode::USER_UNKNOWN
        | ErrorCode::CRED_INSUFFICIENT
        | ErrorCode::CRED_UNAVAIL => EXIT_UNAVAILABLE,
        _ => EXIT_PAM_ERROR,
    }
}

/// CLOCK_BOOTTIME minus CLOCK_MONOTONIC in nanoseconds: grows by exactly the time spent
/// suspended. Signed, because a time namespace (a container) may offset the two clocks so that
/// BOOTTIME reads below MONOTONIC; only the change between two readings matters.
pub(crate) fn suspend_offset() -> Option<i128> {
    fn read(clock: libc::clockid_t) -> Option<i128> {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid out-pointer for one timespec.
        if unsafe { libc::clock_gettime(clock, &mut ts) } != 0 {
            return None;
        }
        Some(i128::from(ts.tv_sec) * 1_000_000_000 + i128::from(ts.tv_nsec))
    }
    Some(read(libc::CLOCK_BOOTTIME)? - read(libc::CLOCK_MONOTONIC)?)
}

/// Whether the machine was suspended between two readings of [`suspend_offset`].
pub(crate) fn resumed(previous: Option<i128>, current: Option<i128>) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => current - previous > RESUME_JUMP.as_nanos() as i128,
        _ => false,
    }
}

struct Running {
    generation: u64,
    child: Child,
}

/// The parent's whole fingerprint state for one lock: the policy, the child in flight, and killed
/// children that still have to be reaped.
pub(crate) struct FingerprintPath {
    service: String,
    user: String,
    debug: bool,
    /// Always `/proc/self/exe` (the running clck, even if its file was replaced by an upgrade);
    /// a field only so tests can run stand-in children through the real code.
    program: PathBuf,
    arm: Arm,
    running: Option<Running>,
    reaping: Vec<Child>,
    suspend_offset: Option<i128>,
}

impl FingerprintPath {
    pub(crate) fn new(service: String, user: String, debug: bool) -> Self {
        Self {
            service,
            user,
            debug,
            program: PathBuf::from("/proc/self/exe"),
            arm: Arm::default(),
            running: None,
            reaping: Vec::new(),
            suspend_offset: suspend_offset(),
        }
    }

    pub(crate) fn on_locked(&mut self) {
        self.arm.on_locked();
    }

    /// Returns whether this trigger armed an attempt that was not already wanted -- the caller
    /// then steps the path at once. Pointer motion arrives in floods; most of it changes nothing.
    pub(crate) fn trigger(&mut self, trigger: Trigger, diagnostics: &Diagnostics) -> bool {
        let before = self.arm.want;
        self.arm.trigger(trigger);
        let armed = self.arm.want && !before;
        if armed {
            diagnostics.fingerprint_trigger(trigger);
        }
        armed
    }

    /// The 1 s tick: notice a resume from suspend.
    pub(crate) fn tick(&mut self, diagnostics: &Diagnostics) {
        let current = suspend_offset();
        if resumed(self.suspend_offset, current) {
            self.trigger(Trigger::Wake, diagnostics);
        }
        self.suspend_offset = current;
    }

    /// Reap, classify, kill, and spawn as the policy says. Returns the generation that verified,
    /// if any; the caller then unlocks exactly as for a correct password. `watch` registers a pidfd
    /// with the event loop so an exit is seen at once rather than on the next tick.
    pub(crate) fn step(
        &mut self,
        diagnostics: &Diagnostics,
        watch: &mut dyn FnMut(OwnedFd),
    ) -> Option<u64> {
        self.step_at(Instant::now(), diagnostics, watch)
    }

    fn step_at(
        &mut self,
        now: Instant,
        diagnostics: &Diagnostics,
        watch: &mut dyn FnMut(OwnedFd),
    ) -> Option<u64> {
        self.reaping
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));

        if let Some(running) = self.running.as_mut() {
            let collected = collect(&mut running.child);
            let exit = match collected {
                Ok(None) => None,
                Ok(Some(exit)) => Some(exit),
                Err(_) => {
                    let _ = running.child.kill();
                    Some(ChildExit::Failed)
                }
            };
            if let Some(exit) = exit {
                let mut finished = self.running.take().expect("running child");
                if exit == ChildExit::Failed {
                    // Already reaped by `collect` unless it errored; a killed one goes to reaping.
                    if let Ok(None) = finished.child.try_wait() {
                        self.reaping.push(finished.child);
                    }
                }
                let verdict = self.arm.finished(finished.generation, exit, now);
                diagnostics.fingerprint_finished(finished.generation, exit, verdict);
                if verdict == Verdict::Unlock {
                    return Some(finished.generation);
                }
                if let Some(reason) = self.arm.disarmed() {
                    diagnostics.fingerprint_disarmed(reason);
                }
            }
        }

        match self.arm.poll(now) {
            Action::None => {}
            Action::Kill(generation) => {
                if let Some(mut running) = self.running.take() {
                    if running.generation == generation {
                        let _ = running.child.kill();
                        diagnostics.fingerprint_killed(generation, "deadline");
                        self.reaping.push(running.child);
                        if let Some(reason) = self.arm.disarmed() {
                            diagnostics.fingerprint_disarmed(reason);
                        }
                    } else {
                        self.running = Some(running);
                    }
                }
            }
            Action::Spawn(generation) => {
                let command = child_command(&self.program, &self.service, &self.user, self.debug);
                match spawn(command) {
                    Ok(child) => {
                        diagnostics.fingerprint_spawned(generation);
                        match pidfd(&child) {
                            Ok(fd) => watch(fd),
                            Err(_) => diagnostics.fingerprint_pidfd_unavailable(generation),
                        }
                        self.running = Some(Running { generation, child });
                    }
                    Err(_) => {
                        let verdict = self.arm.finished(generation, ChildExit::Failed, now);
                        diagnostics.fingerprint_spawn_failed(generation);
                        debug_assert_ne!(verdict, Verdict::Unlock);
                    }
                }
            }
        }
        None
    }

    /// Kill every child now (the process is about to exit). PR_SET_PDEATHSIG in the child is the
    /// backstop for exits that never reach here.
    pub(crate) fn kill_all(&mut self) {
        if let Some(running) = self.running.as_mut() {
            let _ = running.child.kill();
        }
    }
}

/// A conversation that answers NOTHING. `pam_fprintd` only sends informational messages; any
/// module that asks for a secret gets PAM_CONV_ERR, so a misconfigured service that falls back to
/// a password prompt fails closed instead of being handed an empty answer.
pub(crate) struct NoSecretConversation;

impl pam_client::ConversationHandler for NoSecretConversation {
    fn prompt_echo_off(&mut self, _prompt: &CStr) -> Result<CString, pam_client::ErrorCode> {
        Err(pam_client::ErrorCode::CONV_ERR)
    }

    fn prompt_echo_on(&mut self, _prompt: &CStr) -> Result<CString, pam_client::ErrorCode> {
        Err(pam_client::ErrorCode::CONV_ERR)
    }

    fn text_info(&mut self, _msg: &CStr) {}

    fn error_msg(&mut self, _msg: &CStr) {}

    fn radio_prompt(&mut self, _prompt: &CStr) -> Result<bool, pam_client::ErrorCode> {
        Err(pam_client::ErrorCode::CONV_ERR)
    }

    fn binary_prompt(
        &mut self,
        _type: u8,
        _data: &[u8],
    ) -> Result<(u8, Vec<u8>), pam_client::ErrorCode> {
        Err(pam_client::ErrorCode::CONV_ERR)
    }
}

/// Run the child if this process was started as one; return otherwise. Call first in `main`.
pub fn run_child_if_requested() {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).map(OsString::as_os_str) == Some(OsStr::new(CHILD_ARG)) {
        child_main(&args);
    }
}

/// Close every descriptor above stderr. Everything clck opens is O_CLOEXEC already; this makes the
/// child's fd table not depend on that (or on what a library consumer's binary opened).
pub(crate) fn close_inherited_fds() -> io::Result<()> {
    // SAFETY: close_range(3, ~0, 0) closes descriptors this process owns and touches no memory.
    if unsafe { libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 0_u32) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn child_exit(code: i32, debug: bool, stage: &str) -> ! {
    if debug {
        eprintln!("clck[debug]: fingerprint_child event=exit stage={stage} status={code}");
    }
    std::process::exit(code)
}

/// The child: one PAM transaction against the fingerprint service, then exit. Never returns.
fn child_main(args: &[OsString]) -> ! {
    // Exec'd through /proc/self/exe the kernel names this process `exe`; name it for ps/journald.
    // SAFETY: PR_SET_NAME reads a NUL-terminated string of at most 16 bytes.
    unsafe { libc::prctl(libc::PR_SET_NAME, c"clck-fp".as_ptr(), 0, 0, 0) };
    // Die with the locker. Set first; the parent pid check below closes the window in which the
    // parent could have died before this line.
    // SAFETY: prctl(PR_SET_PDEATHSIG) only sets this process's parent-death signal.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) };
    // `pkill -USR1 clck` is aimed at the locker (COMPAT-1); it must not cost a pending attempt.
    // SAFETY: SIG_IGN is always a valid disposition.
    unsafe { libc::signal(libc::SIGUSR1, libc::SIG_IGN) };
    if close_inherited_fds().is_err() {
        child_exit(EXIT_SETUP, false, "fds");
    }
    let text = |index: usize| args.get(index).and_then(|arg| arg.to_str());
    let (Some(service), Some(user), Some(parent), Some(debug)) =
        (text(2), text(3), text(4), text(5))
    else {
        child_exit(EXIT_SETUP, false, "args");
    };
    let debug = debug == "1";
    if args.len() != 6 || user.is_empty() {
        child_exit(EXIT_SETUP, debug, "args");
    }
    // SAFETY: getppid has no preconditions.
    if parent.parse::<libc::pid_t>().ok() != Some(unsafe { libc::getppid() }) {
        child_exit(EXIT_SETUP, debug, "parent");
    }
    if service_problem(service, None, Path::new(PAM_DIR), 0).is_some() {
        child_exit(EXIT_SETUP, debug, "service");
    }

    let mut ctx = match pam_client::Context::new(service, Some(user), NoSecretConversation) {
        Ok(ctx) => ctx,
        Err(_) => child_exit(EXIT_PAM_ERROR, debug, "context"),
    };
    // Authentication only, like the password path: a lock screen resumes a session, it does not
    // open one (no acct_mgmt, no setcred, no session).
    match ctx.authenticate(pam_client::Flag::NONE) {
        Ok(()) => {
            drop(ctx);
            let mut stdout = io::stdout().lock();
            if stdout
                .write_all(MARKER)
                .and_then(|()| stdout.flush())
                .is_err()
            {
                child_exit(EXIT_SETUP, debug, "marker");
            }
            child_exit(EXIT_VERIFIED, debug, "authenticate")
        }
        Err(error) => child_exit(exit_code_for(error.code()), debug, "authenticate"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::{Mutex, MutexGuard};

    /// Tests that write an executable or start a process hold this. Without it, a fork in another
    /// test thread can inherit a script's still-open write descriptor and the exec of that script
    /// fails with ETXTBSY (seen as intermittent spawn failures under parallel `cargo test`).
    static SPAWN: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        SPAWN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn locked_arm() -> (Arm, Instant) {
        let mut arm = Arm::default();
        arm.on_locked();
        (arm, Instant::now())
    }

    #[test]
    fn nothing_arms_before_the_compositor_confirms_the_lock() {
        let mut arm = Arm::default();
        let now = Instant::now();
        arm.trigger(Trigger::Input);
        arm.trigger(Trigger::Wake);
        assert_eq!(arm.poll(now), Action::None);
        assert_eq!(arm.finished(1, ChildExit::Verified, now), Verdict::Stale);
    }

    #[test]
    fn the_lock_arms_one_attempt_and_only_that_attempt_can_unlock() {
        let (mut arm, now) = locked_arm();
        assert_eq!(arm.poll(now), Action::Spawn(1));
        assert_eq!(arm.poll(now), Action::None, "one attempt at a time");
        assert_eq!(arm.finished(2, ChildExit::Verified, now), Verdict::Stale);
        assert_eq!(arm.finished(0, ChildExit::Verified, now), Verdict::Stale);
        assert_eq!(
            arm.in_flight(),
            Some(1),
            "a stale result leaves the attempt alone"
        );
        assert_eq!(arm.finished(1, ChildExit::Verified, now), Verdict::Unlock);
    }

    #[test]
    fn a_result_arriving_twice_counts_once() {
        let (mut arm, now) = locked_arm();
        assert_eq!(arm.poll(now), Action::Spawn(1));
        assert_eq!(arm.finished(1, ChildExit::NoMatch, now), Verdict::Continue);
        assert_eq!(arm.finished(1, ChildExit::Verified, now), Verdict::Stale);
    }

    #[test]
    fn a_killed_attempt_can_never_unlock_afterwards() {
        let (mut arm, now) = locked_arm();
        assert_eq!(arm.poll(now), Action::Spawn(1));
        let late = now + ATTEMPT_DEADLINE;
        assert_eq!(arm.poll(late), Action::Kill(1));
        assert_eq!(arm.finished(1, ChildExit::Verified, late), Verdict::Stale);
        // And the next attempt gets a new generation, so the old one cannot be mistaken for it.
        arm.trigger(Trigger::Input);
        assert_eq!(arm.poll(late), Action::None, "settle after a kill");
        assert_eq!(arm.poll(late + SETTLE), Action::Spawn(2));
        assert_eq!(
            arm.finished(1, ChildExit::Verified, late + SETTLE),
            Verdict::Stale
        );
        assert_eq!(
            arm.finished(2, ChildExit::Verified, late + SETTLE),
            Verdict::Unlock
        );
    }

    #[test]
    fn input_rearms_only_when_idle_and_after_the_settle_gap() {
        let (mut arm, now) = locked_arm();
        assert_eq!(arm.poll(now), Action::Spawn(1));
        arm.trigger(Trigger::Input); // during the attempt: no second window queued
        let end = now + Duration::from_secs(30);
        assert_eq!(
            arm.finished(1, ChildExit::Unavailable, end),
            Verdict::Continue
        );
        assert_eq!(arm.poll(end + SETTLE), Action::None, "idle until new input");
        arm.trigger(Trigger::Input);
        assert_eq!(
            arm.poll(end + Duration::from_millis(100)),
            Action::None,
            "settling"
        );
        assert_eq!(arm.poll(end + SETTLE), Action::Spawn(2));
    }

    #[test]
    fn a_wake_during_an_attempt_rearms_right_after_it() {
        let (mut arm, now) = locked_arm();
        assert_eq!(arm.poll(now), Action::Spawn(1));
        arm.trigger(Trigger::Wake);
        assert_eq!(arm.poll(now), Action::None);
        let end = now + Duration::from_secs(10);
        assert_eq!(
            arm.finished(1, ChildExit::Unavailable, end),
            Verdict::Continue
        );
        assert_eq!(arm.poll(end + SETTLE), Action::Spawn(2));
    }

    #[test]
    fn five_rejected_fingers_disarm_the_path_for_this_lock() {
        let (mut arm, mut now) = locked_arm();
        for generation in 1..=MAX_NO_MATCH as u64 {
            arm.trigger(Trigger::Input);
            assert_eq!(arm.poll(now), Action::Spawn(generation));
            assert_eq!(
                arm.finished(generation, ChildExit::NoMatch, now),
                Verdict::Continue
            );
            now += SETTLE;
        }
        assert_eq!(arm.disarmed(), Some(Disarmed::NoMatchLimit));
        arm.trigger(Trigger::Input);
        arm.trigger(Trigger::Wake);
        assert_eq!(arm.poll(now + Duration::from_secs(600)), Action::None);
    }

    #[test]
    fn a_missing_reader_or_print_stops_respawning_after_three_quick_failures() {
        let (mut arm, mut now) = locked_arm();
        for (generation, exit) in [
            (1, ChildExit::Unavailable),
            (2, ChildExit::Failed),
            (3, ChildExit::Unavailable),
        ] {
            arm.trigger(Trigger::Input);
            assert_eq!(arm.poll(now), Action::Spawn(generation));
            assert_eq!(
                arm.finished(generation, exit, now + Duration::from_millis(50)),
                Verdict::Continue
            );
            now += SETTLE + Duration::from_millis(50);
        }
        assert_eq!(arm.disarmed(), Some(Disarmed::Unavailable));
        arm.trigger(Trigger::Input);
        assert_eq!(arm.poll(now + Duration::from_secs(60)), Action::None);
    }

    #[test]
    fn a_full_timeout_is_normal_and_resets_the_quick_failure_streak() {
        let (mut arm, mut now) = locked_arm();
        for generation in 1..=6_u64 {
            arm.trigger(Trigger::Input);
            assert_eq!(arm.poll(now), Action::Spawn(generation));
            let took = if generation % 2 == 0 {
                Duration::from_secs(30) // pam_fprintd's own timeout: nobody touched the reader
            } else {
                Duration::from_millis(10)
            };
            arm.finished(generation, ChildExit::Unavailable, now + took);
            now += took + SETTLE;
        }
        assert_eq!(arm.disarmed(), None);
    }

    #[test]
    fn generations_are_never_reused() {
        let (mut arm, mut now) = locked_arm();
        let mut seen = Vec::new();
        for round in 0..4 {
            arm.trigger(Trigger::Wake);
            match arm.poll(now) {
                Action::Spawn(generation) => {
                    seen.push(generation);
                    if round % 2 == 0 {
                        now += ATTEMPT_DEADLINE;
                        assert_eq!(arm.poll(now), Action::Kill(generation));
                    } else {
                        now += Duration::from_secs(30);
                        arm.finished(generation, ChildExit::Unavailable, now);
                    }
                    now += SETTLE;
                }
                other => panic!("expected a spawn, got {other:?}"),
            }
        }
        assert_eq!(seen, vec![1, 2, 3, 4]);
    }

    #[test]
    fn attempts_that_hang_until_the_deadline_disarm_the_path_after_three() {
        let (mut arm, mut now) = locked_arm();
        for generation in 1..=MAX_HUNG as u64 {
            arm.trigger(Trigger::Input);
            assert_eq!(arm.poll(now), Action::Spawn(generation));
            now += ATTEMPT_DEADLINE;
            assert_eq!(arm.poll(now), Action::Kill(generation));
            now += SETTLE;
        }
        assert_eq!(arm.disarmed(), Some(Disarmed::Hung));
        arm.trigger(Trigger::Input);
        arm.trigger(Trigger::Wake);
        assert_eq!(arm.poll(now + Duration::from_secs(600)), Action::None);
    }

    #[test]
    fn only_status_zero_with_the_exact_marker_is_verified() {
        let exited = |code: i32| ExitStatus::from_raw(code << 8);
        assert_eq!(classify(exited(0), MARKER), ChildExit::Verified);
        assert_eq!(classify(exited(0), b""), ChildExit::Failed);
        assert_eq!(
            classify(exited(0), b"clck-fingerprint-verified"),
            ChildExit::Failed
        );
        let mut longer = MARKER.to_vec();
        longer.push(b'x');
        assert_eq!(classify(exited(0), &longer), ChildExit::Failed);
        assert_eq!(classify(exited(EXIT_NO_MATCH), MARKER), ChildExit::NoMatch);
        assert_eq!(
            classify(exited(EXIT_UNAVAILABLE), MARKER),
            ChildExit::Unavailable
        );
        assert_eq!(classify(exited(EXIT_PAM_ERROR), MARKER), ChildExit::Failed);
        assert_eq!(classify(exited(EXIT_SETUP), b""), ChildExit::Failed);
        assert_eq!(classify(exited(101), MARKER), ChildExit::Failed, "a panic");
        // Killed by a signal (raw wait status = the signal number): never verified.
        assert_eq!(
            classify(ExitStatus::from_raw(libc::SIGKILL), MARKER),
            ChildExit::Failed
        );
        assert_eq!(
            classify(ExitStatus::from_raw(libc::SIGSEGV), MARKER),
            ChildExit::Failed
        );
    }

    #[test]
    fn pam_errors_map_to_non_success_statuses() {
        use pam_client::ErrorCode;
        // pam_fprintd max-tries=1: one finger that does not match.
        assert_eq!(exit_code_for(ErrorCode::MAXTRIES), EXIT_NO_MATCH);
        // pam_fprintd: fprintd refused to start the verify (polkit, reader in use) -- not a finger.
        assert_eq!(exit_code_for(ErrorCode::AUTH_ERR), EXIT_PAM_ERROR);
        // pam_fprintd: timeout, no fprintd, no reader, no enrolled print, claim refused.
        assert_eq!(exit_code_for(ErrorCode::AUTHINFO_UNAVAIL), EXIT_UNAVAILABLE);
        assert_eq!(exit_code_for(ErrorCode::USER_UNKNOWN), EXIT_UNAVAILABLE);
        for code in [
            ErrorCode::CONV_ERR,
            ErrorCode::ABORT,
            ErrorCode::SERVICE_ERR,
            ErrorCode::SYSTEM_ERR,
            ErrorCode::MODULE_UNKNOWN,
            ErrorCode::INCOMPLETE,
            ErrorCode::PERM_DENIED,
        ] {
            assert_eq!(exit_code_for(code), EXIT_PAM_ERROR, "{code:?}");
        }
    }

    #[test]
    fn the_child_conversation_never_answers_a_prompt() {
        use pam_client::ConversationHandler;
        let mut conv = NoSecretConversation;
        let prompt = CString::new("Password: ").unwrap();
        assert!(conv.prompt_echo_off(&prompt).is_err());
        assert!(conv.prompt_echo_on(&prompt).is_err());
        assert!(conv.radio_prompt(&prompt).is_err());
        assert!(conv.binary_prompt(0, b"").is_err());
    }

    #[test]
    fn only_a_dedicated_root_style_service_with_an_auth_line_is_accepted() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("clck-fp-pamd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, text: &str, mode: u32| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let good = "# comment\nauth required pam_fprintd.so max-tries=1 timeout=30\n";
        write("clck-fingerprint", good, 0o644);
        write("clck-dash-auth", "-auth required pam_fprintd.so\n", 0o644);
        write(
            "clck-no-auth",
            "# auth only in a comment\naccount required pam_permit.so\n",
            0o644,
        );
        write("clck-group-writable", good, 0o664);
        write("clck-other-writable", good, 0o646);
        write("clck-unreadable", good, 0o000);
        // Stock services exist under their own names; the name rule alone keeps them out.
        write("system-services", "auth sufficient pam_permit.so\n", 0o644);
        write("other", "auth required pam_deny.so\n", 0o644);
        std::fs::create_dir_all(dir.join("clck-a-directory")).unwrap();
        symlink(dir.join("clck-fingerprint"), dir.join("clck-link")).unwrap();

        // `owner` is this test's own uid: the files are ours, as /etc/pam.d's are root's.
        // SAFETY: getuid has no preconditions.
        let me = unsafe { libc::getuid() };
        let check = |name: &str, password: Option<&str>| service_problem(name, password, &dir, me);

        assert_eq!(check("clck-fingerprint", Some("swaylock")), None);
        assert_eq!(check("clck-fingerprint", None), None);
        assert_eq!(check("clck-dash-auth", None), None);
        assert_eq!(
            check("clck-link", None),
            None,
            "a link to a good file is that file"
        );

        for stock in [
            "system-services",
            "system-auth",
            "login",
            "sudo",
            "other",
            "swaylock",
        ] {
            assert_eq!(check(stock, None), Some("not_dedicated"), "{stock}");
        }
        assert_eq!(check("", None), Some("not_dedicated"));
        assert_eq!(check("clck-", None), Some("invalid_name"));
        assert_eq!(check("clck-../other", None), Some("invalid_name"));
        assert_eq!(check("clck-Fingerprint", None), Some("invalid_name"));
        assert_eq!(check("clck-finger print", None), Some("invalid_name"));
        assert_eq!(
            check("clck-fingerprint", Some("clck-fingerprint")),
            Some("same_as_password_service")
        );
        assert_eq!(
            check("clck-fingerprint", Some("Clck-Fingerprint")),
            Some("same_as_password_service"),
            "PAM lowercases service names"
        );
        assert_eq!(
            check("clck-absent", None),
            Some("missing"),
            "never PAM's `other`"
        );
        assert_eq!(check("clck-a-directory", None), Some("missing"));
        assert_eq!(check("clck-no-auth", None), Some("no_auth_line"));
        assert_eq!(
            check("clck-group-writable", None),
            Some("writable_by_others")
        );
        assert_eq!(
            check("clck-other-writable", None),
            Some("writable_by_others")
        );
        if me != 0 {
            // root reads a mode-000 file anyway
            assert_eq!(check("clck-unreadable", None), Some("unreadable"));
        }
        assert_eq!(
            service_problem("clck-fingerprint", None, &dir, me.wrapping_add(1)),
            Some("not_root_owned")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_suspend_is_seen_as_a_boottime_jump() {
        let second = 1_000_000_000_i128;
        for base in [100 * second, -7 * second] {
            assert!(!resumed(None, Some(base)));
            assert!(!resumed(Some(base), None));
            assert!(!resumed(Some(base), Some(base + 30_000_000)));
            assert!(resumed(Some(base), Some(base + 60 * second)));
        }
        let a = suspend_offset().expect("both clocks readable");
        let b = suspend_offset().expect("both clocks readable");
        assert!(
            !resumed(Some(a), Some(b)),
            "no suspend between two adjacent readings"
        );
    }

    #[test]
    fn the_child_gets_an_empty_environment_and_no_stdin() {
        let _serial = serial();
        // The production builder, run against a stand-in program that reports what it received.
        let dir = std::env::temp_dir().join(format!("clck-fp-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let probe = dir.join("probe");
        std::fs::write(
            &probe,
            "#!/bin/sh\nif read -r line; then printf 'stdin=readable\\0'; fi\nexec cat /proc/self/environ\n",
        )
        .unwrap();
        std::fs::set_permissions(&probe, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();

        let builder = child_command(&probe, "svc", "user", false);
        let args: Vec<_> = builder.get_args().collect();
        assert_eq!(
            args,
            [
                CHILD_ARG,
                "svc",
                "user",
                std::process::id().to_string().as_str(),
                "0"
            ]
        );
        let output = child_command(&probe, "svc", "user", false)
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(output.status.success());
        // Only what the stand-in shell itself creates may appear; nothing of this process's own
        // environment (HOME, PATH, WAYLAND_DISPLAY, DBUS_SESSION_BUS_ADDRESS, ...) reaches it.
        for entry in output.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let entry = String::from_utf8_lossy(entry);
            let name = entry.split('=').next().unwrap();
            assert!(
                ["PWD", "OLDPWD", "SHLVL", "_"].contains(&name),
                "the child inherited `{name}`"
            );
        }
    }

    #[test]
    fn a_stand_in_child_is_collected_and_classified_through_the_real_path() {
        let _serial = serial();
        fn run(script: &str) -> ChildExit {
            let mut command = Command::new("/bin/sh");
            command
                .arg("-c")
                .arg(script)
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let mut child = spawn(command).unwrap();
            let fd = pidfd(&child).unwrap();
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            assert_eq!(
                unsafe { libc::poll(&mut poll, 1, 10_000) },
                1,
                "pidfd never fired"
            );
            collect(&mut child).unwrap().expect("exited")
        }
        assert_eq!(
            run("printf 'clck-fingerprint-verified\\n'; exit 0"),
            ChildExit::Verified
        );
        assert_eq!(run("exit 0"), ChildExit::Failed, "status 0 alone");
        assert_eq!(
            run("printf 'clck-fingerprint-verified\\n'; exit 10"),
            ChildExit::NoMatch
        );
        assert_eq!(run("exit 11"), ChildExit::Unavailable);
        assert_eq!(
            run("printf 'clck-fingerprint-verified\\n'; kill -9 $$"),
            ChildExit::Failed,
            "killed after writing the marker"
        );
        assert_eq!(
            run("printf 'clck-fingerprint-verified\\nmore'; exit 0"),
            ChildExit::Failed
        );
        // A grandchild that keeps the pipe open must not stall the collection.
        assert_eq!(run("sleep 30 & exit 11"), ChildExit::Unavailable);
    }

    #[test]
    fn a_killed_child_is_reaped_and_reports_a_signal() {
        let _serial = serial();
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("printf 'clck-fingerprint-verified\\n'; exec sleep 60")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = spawn(command).unwrap();
        assert_eq!(collect(&mut child).unwrap(), None, "still running");
        child.kill().unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert_eq!(classify(status, MARKER), ChildExit::Failed);
    }

    // The fd hygiene of the child, exercised in a real process: this test re-runs its own binary
    // with a descriptor deliberately left WITHOUT close-on-exec, and the re-run calls
    // `close_inherited_fds` and reports what is still open.
    #[test]
    fn the_child_closes_every_inherited_descriptor_above_stderr() {
        const MODE: &str = "CLCK_FP_FD_PROBE";
        if std::env::var_os(MODE).is_some() {
            let before = std::fs::read_dir("/proc/self/fd").unwrap().count();
            close_inherited_fds().unwrap();
            let mut open: Vec<i32> = std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
                .collect();
            open.sort();
            let open: Vec<String> = open.iter().map(i32::to_string).collect();
            println!("open-fds-before={before} open-fds={}", open.join(","));
            return;
        }
        let _serial = serial();
        let mut fds = [-1; 2];
        // SAFETY: two valid slots; plain pipe() leaves both ends inheritable on purpose.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fingerprint::tests::the_child_closes_every_inherited_descriptor_above_stderr",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(MODE, "1")
            .output()
            .unwrap();
        // SAFETY: the two descriptors created above.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        // libtest prints its own "test ... " prefix on the same line.
        let line = stdout
            .lines()
            .find_map(|line| line.find("open-fds-before=").map(|at| &line[at..]))
            .unwrap_or_else(|| panic!("probe did not report: {stdout}"));
        let (before, after) = line.split_once(' ').unwrap();
        let before: usize = before["open-fds-before=".len()..].parse().unwrap();
        assert!(
            before >= 6,
            "the probe did not inherit the test pipe: {line}"
        );
        // 0, 1, 2 and the one descriptor read_dir holds while it lists /proc/self/fd.
        let open: Vec<i32> = after["open-fds=".len()..]
            .split(|c: char| c == ',' || c.is_whitespace())
            .take_while(|fd| !fd.is_empty())
            .map(|fd| fd.parse().unwrap())
            .collect();
        assert_eq!(open[..3], [0, 1, 2], "{line}");
        assert_eq!(open.len(), 4, "inherited descriptors survived: {line}");
    }

    /// A `FingerprintPath` whose children are a stand-in shell script, for driving the real
    /// parent-side code (spawn, pidfd, collect, kill, reap) without PAM or a reader.
    struct StandIn {
        _serial: MutexGuard<'static, ()>,
        dir: PathBuf,
        path: FingerprintPath,
        watched: Vec<OwnedFd>,
        diagnostics: Diagnostics,
    }

    impl StandIn {
        fn new(name: &str, script: &str) -> Self {
            let serial = serial();
            let dir =
                std::env::temp_dir().join(format!("clck-fp-standin-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let program = dir.join("child");
            std::fs::write(&program, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(
                &program,
                std::os::unix::fs::PermissionsExt::from_mode(0o700),
            )
            .unwrap();
            let mut path = FingerprintPath::new("svc".into(), "user".into(), false);
            path.program = program;
            path.on_locked();
            Self {
                _serial: serial,
                dir,
                path,
                watched: Vec::new(),
                diagnostics: Diagnostics::new(false),
            }
        }

        fn step(&mut self, now: Instant) -> Option<u64> {
            let watched = &mut self.watched;
            self.path
                .step_at(now, &self.diagnostics, &mut |fd| watched.push(fd))
        }

        /// Block until the newest child's pidfd says it exited.
        fn wait_exit(&self) {
            let fd = self
                .watched
                .last()
                .expect("a child was spawned and watched");
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            assert_eq!(
                unsafe { libc::poll(&mut poll, 1, 10_000) },
                1,
                "child never exited"
            );
        }
    }

    impl Drop for StandIn {
        fn drop(&mut self) {
            self.path.kill_all();
            for child in self.path.reaping.iter_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            if let Some(running) = self.path.running.as_mut() {
                let _ = running.child.wait();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_verified_child_unlocks_through_the_real_parent_path() {
        let mut fp = StandIn::new("verified", "printf 'clck-fingerprint-verified\\n'; exit 0");
        let t0 = Instant::now();
        assert_eq!(fp.step(t0), None, "spawned, still running");
        assert_eq!(fp.watched.len(), 1, "the pidfd is handed to the event loop");
        fp.wait_exit();
        assert_eq!(fp.step(t0), Some(1));
    }

    #[test]
    fn status_zero_without_the_marker_does_not_unlock() {
        let mut fp = StandIn::new("zero", "exit 0");
        let t0 = Instant::now();
        assert_eq!(fp.step(t0), None);
        fp.wait_exit();
        assert_eq!(fp.step(t0), None);
        assert_eq!(fp.path.arm.in_flight(), None, "the attempt ended");
        assert!(fp.path.running.is_none());
    }

    #[test]
    fn a_hung_child_is_killed_at_the_deadline_and_its_late_marker_never_counts() {
        // Writes the marker at once, then hangs: only exit status 0 together with the marker can
        // unlock, and the deadline kill turns the status into a signal.
        let mut fp = StandIn::new(
            "hung",
            "printf 'clck-fingerprint-verified\\n'; exec sleep 60",
        );
        let t0 = Instant::now();
        assert_eq!(fp.step(t0), None);
        assert_eq!(fp.step(t0 + ATTEMPT_DEADLINE), None, "killed, not unlocked");
        assert!(fp.path.running.is_none());
        assert_eq!(fp.path.reaping.len(), 1, "killed child awaits reaping");
        fp.wait_exit();
        assert_eq!(fp.step(t0 + ATTEMPT_DEADLINE), None);
        assert!(fp.path.reaping.is_empty(), "reaped on the next step");
    }

    #[test]
    fn a_missing_reader_stops_after_three_quick_attempts_without_unlocking() {
        // pam_fprintd's answer when fprintd, the reader, or an enrolled print is absent.
        let mut fp = StandIn::new("absent", "exit 11");
        let mut now = Instant::now();
        for _ in 0..MAX_QUICK_FAILURES {
            fp.path.trigger(Trigger::Input, &fp.diagnostics);
            assert_eq!(fp.step(now), None);
            fp.wait_exit();
            assert_eq!(fp.step(now), None);
            now += SETTLE;
        }
        assert_eq!(fp.path.arm.disarmed(), Some(Disarmed::Unavailable));
        fp.path.trigger(Trigger::Input, &fp.diagnostics);
        assert_eq!(fp.step(now + Duration::from_secs(60)), None);
        assert_eq!(
            fp.watched.len(),
            MAX_QUICK_FAILURES as usize,
            "no further child"
        );
    }

    #[test]
    fn a_child_that_cannot_be_started_fails_closed() {
        let mut fp = StandIn::new("nostart", "exit 0");
        fp.path.program = fp.dir.join("does-not-exist");
        let mut now = Instant::now();
        for _ in 0..MAX_QUICK_FAILURES {
            fp.path.trigger(Trigger::Input, &fp.diagnostics);
            assert_eq!(fp.step(now), None);
            now += SETTLE;
        }
        assert!(fp.watched.is_empty());
        assert_eq!(fp.path.arm.disarmed(), Some(Disarmed::Unavailable));
    }

    #[test]
    fn kill_all_ends_the_attempt_in_flight() {
        let mut fp = StandIn::new("killall", "exec sleep 60");
        assert_eq!(fp.step(Instant::now()), None);
        fp.path.kill_all();
        fp.wait_exit();
        let running = fp.path.running.as_mut().expect("still tracked");
        assert_eq!(running.child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    // SESSION-2: the success exit is reachable from exactly one place, right after the marker.
    #[test]
    fn the_child_exits_verified_only_after_writing_the_marker() {
        let production = include_str!("fingerprint.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("production source");
        let child = production
            .split("fn child_main(")
            .nth(1)
            .expect("child_main");
        assert_eq!(child.matches("EXIT_VERIFIED").count(), 1);
        let marker_write = child.find("write_all(MARKER)").expect("marker write");
        let verified_exit = child.find("EXIT_VERIFIED").expect("verified exit");
        assert!(marker_write < verified_exit);
        assert_eq!(production.matches(".authenticate(").count(), 1);
        assert_eq!(production.matches(".acct_mgmt(").count(), 0);
        assert_eq!(production.matches("std::process::exit(").count(), 1);

        // The parent reports a verified generation from exactly one place, on Verdict::Unlock.
        let step = production
            .split("fn step_at(")
            .nth(1)
            .expect("step_at")
            .split("\n    pub(crate) fn kill_all")
            .next()
            .unwrap();
        assert_eq!(step.matches("return Some(").count(), 1);
        let unlock = step
            .find("if verdict == Verdict::Unlock")
            .expect("unlock check");
        assert!(unlock < step.find("return Some(").unwrap());
        // `ChildExit::Verified` is CONSTRUCTED on exactly one line, the one that checks the marker;
        // everywhere else it may only be matched on.
        let constructed: Vec<&str> = production
            .lines()
            .map(str::trim)
            .filter(|line| line.contains("ChildExit::Verified") && !line.starts_with("///"))
            .filter(|line| !line.starts_with("ChildExit::Verified =>"))
            .collect();
        assert_eq!(constructed.len(), 1, "{constructed:?}");
        assert!(constructed[0].contains("Some(EXIT_VERIFIED) if stdout == MARKER =>"));
    }
}

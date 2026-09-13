//! The content-agnostic locker: acquires the `ext-session-lock`, creates one surface per output,
//! classifies each output's role, routes keyboard input to a password buffer, and unlocks the
//! whole session on a correct PAM password. Fail-closed. A `Session` output always paints the
//! (pluggable, via `builder().session(..)`) lock-screen [`KioskContent`]; a `Kiosk` output paints
//! the latest frame from the kiosk display socket (`crate::socket`, run on its own thread here) or
//! the same built-in clock whenever the socket has nothing current for it.

use crate::auth::{self, AuthAttempt, AuthOutcome, AuthResult};
use crate::content::{AuthState, AuthView, Frame, KioskContent, OutputRole};
use crate::daemon::{self, ForkRole, ReadyNotifier};
use crate::diagnostics::Diagnostics;
use crate::lockscreen::ClockLockScreen;
use crate::socket::{self, SocketState};
use calloop::{
    channel::{channel, Event as ChanEvent},
    ping::make_ping,
    timer::{TimeoutAction, Timer},
    EventLoop,
};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_registry, delegate_seat,
    delegate_session_lock, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        Capability, SeatHandler, SeatState,
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard::WlKeyboard, wl_output, wl_seat::WlSeat, wl_shm, wl_surface},
    Connection, QueueHandle,
};
use zeroize::Zeroizing;

/// Per-host configuration. Values are supplied by the caller (from a config file in the default
/// binary, or in code by a library consumer) — the framework itself hardcodes no policy.
pub struct Config {
    /// Connector names (e.g. `"DP-3"`) that carry live kiosk content. Every other output is a
    /// `Session` lock screen — the safe default.
    pub kiosk_outputs: Vec<String>,
    /// PAM service name (`/etc/pam.d/<name>`). Fail-closed if missing.
    pub pam_service: String,
    /// Username to authenticate; `None` = the current `$USER`.
    pub username: Option<String>,
    /// Path to the kiosk display Unix socket. `None` (the default) resolves to
    /// `$XDG_RUNTIME_DIR/nixlock.sock` at startup; if that env var is also unset the socket server
    /// is skipped entirely (the kiosk output then always shows the built-in clock) -- a display
    /// feature must never block or weaken the lock itself. See `crate::socket` for the wire
    /// protocol a client streams frames over.
    pub socket_path: Option<PathBuf>,
    /// Emit credential-blind lifecycle and PAM result events to stderr. The seated session's
    /// systemd unit captures stderr in journald. No password content, length, key symbol, PAM
    /// prompt, or PAM message is ever included.
    pub debug: bool,
}

#[non_exhaustive]
#[derive(Debug)]
pub enum LockError {
    /// The compositor does not advertise `ext-session-lock-v1`; refuse loudly, never half-lock.
    NoSessionLockProtocol,
    Wayland(String),
    /// The `-f` parent could not create its child or the child exited before acquiring the lock.
    Daemonize(String),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::NoSessionLockProtocol => write!(f, "compositor lacks ext-session-lock-v1"),
            LockError::Wayland(s) => write!(f, "wayland: {s}"),
            LockError::Daemonize(s) => write!(f, "daemonize: {s}"),
        }
    }
}
impl std::error::Error for LockError {}

/// Set by the `SIGUSR1` handler, drained by the 1s repaint tick.
static SIGUSR1_SEEN: AtomicBool = AtomicBool::new(false);

/// `SIGUSR1`'s DEFAULT DISPOSITION IS TERMINATE, and that is why this exists.
///
/// The fleet's swayidle line ends `unlock 'pkill -USR1 clck'`, and `COMPAT-1` promises the
/// signal reaches a process still named `clck`. With no handler installed that signal killed
/// the locker outright -- and by `LOCK-2` the compositor then keeps every output locked with no
/// client left to accept a password. That is exactly the "lockout that bricks the session"
/// `SESSION-1` forbids, reachable by the one signal the desk is wired to send.
///
/// The handler stores a flag and returns. Nothing else: a signal handler may only call
/// async-signal-safe functions, and an atomic store is one while locking a mutex or touching the
/// wayland connection is not. The 1s repaint tick drains it, so delivery costs at most one tick.
extern "C" fn on_sigusr1(_sig: libc::c_int) {
    SIGUSR1_SEEN.store(true, Ordering::Relaxed);
}

/// Install the handler. Returns false if the kernel refused, which is logged and never fatal --
/// failing to lock the screen because a signal disposition could not be set would be a far worse
/// outcome than the signal staying lethal.
fn install_sigusr1_handler() -> bool {
    // SAFETY: `on_sigusr1` is `extern "C"`, does nothing but an atomic store, and the handler is
    // installed once before the event loop runs.
    unsafe {
        libc::signal(libc::SIGUSR1, on_sigusr1 as *const () as libc::sighandler_t) != libc::SIG_ERR
    }
}

/// The entrypoint. Locks now: the built-in [`ClockLockScreen`] on `Session` outputs, and on
/// `Kiosk` outputs whatever the kiosk socket (`Config::socket_path`) is streaming — or that same
/// clock, until a frame arrives or after the streaming client disconnects. Blocks until the
/// correct password unlocks the whole session.
pub fn run(config: Config) -> Result<(), LockError> {
    run_inner(config, Box::new(ClockLockScreen::new()), None)
}

/// Fork before creating any worker threads, then return in the parent only after the child has
/// acquired the compositor's session lock. This is the swaylock-compatible `-f` contract used by
/// swayidle: a successful hook means the session is already secured, while the child continues to
/// own the lock and authenticate the user.
pub fn run_daemonized(config: Config) -> Result<(), LockError> {
    match daemon::fork().map_err(|e| LockError::Daemonize(e.to_string()))? {
        ForkRole::Parent(waiter) => waiter
            .wait()
            .map_err(|e| LockError::Daemonize(e.to_string())),
        ForkRole::Child(ready) => {
            let result = run_inner(config, Box::new(ClockLockScreen::new()), Some(ready));
            if let Err(error) = result {
                eprintln!("clck: fatal: {error}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
    }
}

/// Full control — override the `Session` lock screen (the `Kiosk` fallback is always the shipped
/// clock; the socket is the only way to change what a `Kiosk` output actually shows).
pub fn builder(config: Config) -> Builder {
    Builder {
        config,
        session: None,
    }
}

pub struct Builder {
    config: Config,
    session: Option<Box<dyn KioskContent>>,
}
impl Builder {
    pub fn session(mut self, c: impl KioskContent) -> Self {
        self.session = Some(Box::new(c));
        self
    }
    pub fn run(self) -> Result<(), LockError> {
        let session = self
            .session
            .unwrap_or_else(|| Box::new(ClockLockScreen::new()));
        run_inner(self.config, session, None)
    }
}

fn run_inner(
    config: Config,
    session_content: Box<dyn KioskContent>,
    daemon_ready: Option<ReadyNotifier>,
) -> Result<(), LockError> {
    let diagnostics = Diagnostics::new(config.debug);
    diagnostics.startup();
    // Before anything else, so a signal arriving during startup cannot kill a locker that has
    // already told the compositor to lock.
    if !install_sigusr1_handler() {
        eprintln!("clck: could not install the SIGUSR1 handler; `pkill -USR1 clck` stays lethal");
    }
    let username = config
        .username
        .clone()
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "root".to_string());
    eprintln!(
        "clck: kiosk_outputs={:?} pam={} user={username}",
        config.kiosk_outputs, config.pam_service
    );

    let conn = Connection::connect_to_env().map_err(|e| LockError::Wayland(e.to_string()))?;
    diagnostics.wayland_connected();
    let (globals, mut event_queue) =
        registry_queue_init(&conn).map_err(|e| LockError::Wayland(e.to_string()))?;
    let qh: QueueHandle<Locker> = event_queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).map_err(|e| LockError::Wayland(e.to_string()))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| LockError::Wayland(e.to_string()))?;
    let output_state = OutputState::new(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);
    let session_lock_state = SessionLockState::new(&globals, &qh);

    let (attempt_tx, attempt_rx) = mpsc::channel::<AuthAttempt>();
    let (result_tx, result_ch) = channel::<AuthResult>();
    auth::spawn(
        config.pam_service.clone(),
        username,
        attempt_rx,
        result_tx,
        diagnostics.clone(),
    );

    // Resolve the kiosk display socket now, but do not touch its filesystem path until the
    // compositor confirms this process owns the session lock. A second `clck -f` can be
    // rejected because another locker is already active; starting the socket before that verdict
    // would unlink the active locker's listener and strand its dashboard client (DISPLAY-3).
    let socket_state = SocketState::new();
    let socket_path = socket::resolve_path(config.socket_path.as_deref());
    if socket_path.is_none() {
        eprintln!(
            "clck: socket: no XDG_RUNTIME_DIR and no socket_path configured; kiosk socket disabled"
        );
    }

    let mut locker = Locker {
        registry_state: RegistryState::new(&globals),
        output_state,
        seat_state,
        compositor,
        shm,
        session_lock_state,
        conn: conn.clone(),
        lock: None,
        pool: None,
        surfaces: Vec::new(),
        kiosk_outputs: config.kiosk_outputs,
        kiosk_fallback: ClockLockScreen::new(),
        session_content,
        socket: socket_state,
        socket_path,
        keyboard: None,
        password: Zeroizing::new(String::new()),
        caps_lock: false,
        failed: false,
        verifying: false,
        attempt_tx,
        fail_count: 0,
        locked_until: None,
        next_attempt_id: 1,
        diagnostics,
        daemon_ready,
    };

    event_queue
        .roundtrip(&mut locker)
        .map_err(|e| LockError::Wayland(e.to_string()))?;
    event_queue
        .roundtrip(&mut locker)
        .map_err(|e| LockError::Wayland(e.to_string()))?;

    let lock = locker
        .session_lock_state
        .lock(&qh)
        .map_err(|_| LockError::NoSessionLockProtocol)?;
    locker.diagnostics.lock_requested();
    locker.lock = Some(lock);

    let mut event_loop: EventLoop<Locker> =
        EventLoop::try_new().map_err(|e| LockError::Wayland(e.to_string()))?;
    let handle = event_loop.handle();
    WaylandSource::new(conn, event_queue)
        .insert(handle.clone())
        .map_err(|e| LockError::Wayland(e.to_string()))?;
    handle
        .insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, l| {
            // Sweep an expired wrong-password backoff here rather than a dedicated one-shot timer:
            // this 1s tick already runs unconditionally, so checking `Instant::now()` against
            // `locked_until` needs no extra `LoopHandle` stashed on `Locker`.
            if l.locked_until.is_some_and(|until| Instant::now() >= until) {
                l.locked_until = None;
            }
            // COMPAT-1: re-show the indicator on SIGUSR1. Deliberately touches NO auth state --
            // not the typed buffer, not the failed flag, not the backoff -- because SESSION-1
            // requires every non-PAM input path to be incapable of affecting the gate. The repaint
            // below is the entire effect.
            if SIGUSR1_SEEN.swap(false, Ordering::Relaxed) {
                eprintln!("clck: SIGUSR1: re-showing the password indicator");
                l.diagnostics.sigusr1();
            }
            l.redraw_all();
            TimeoutAction::ToDuration(Duration::from_secs(1))
        })
        .map_err(|e| LockError::Wayland(e.to_string()))?;
    handle
        .insert_source(result_ch, |ev, _, l| {
            if let ChanEvent::Msg(outcome) = ev {
                l.on_auth(outcome);
            }
        })
        .map_err(|e| LockError::Wayland(e.to_string()))?;

    // Repaint promptly when a new socket frame lands, rather than waiting for the 1s tick above.
    // Not load-bearing (the tick alone keeps the kiosk output eventually-consistent), just nicer
    // latency — so a failure to wire it is logged, not fatal.
    match make_ping() {
        Ok((ping, ping_source)) => {
            locker.socket.set_ping(ping);
            handle
                .insert_source(ping_source, |_, _, l| l.redraw_all())
                .map_err(|e| LockError::Wayland(e.to_string()))?;
        }
        Err(e) => eprintln!("clck: socket: repaint ping unavailable ({e}); 1s tick still applies"),
    }

    event_loop
        .run(Duration::from_secs(1), &mut locker, |_| {})
        .map_err(|e| LockError::Wayland(e.to_string()))?;
    Ok(())
}

struct Entry {
    output: wl_output::WlOutput,
    surface: SessionLockSurface,
    role: OutputRole,
    name: String,
    width: u32,
    height: u32,
    /// The output's scale factor, forwarded into the socket HELLO for `Kiosk`-role entries so a
    /// client can render at the right density. `1` when the compositor never reports one.
    scale: u32,
}

struct Locker {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    compositor: CompositorState,
    shm: Shm,
    session_lock_state: SessionLockState,
    conn: Connection,
    lock: Option<SessionLock>,
    pool: Option<SlotPool>,
    surfaces: Vec<Entry>,
    kiosk_outputs: Vec<String>,
    /// The `Kiosk` role's fallback content — always the shipped clock (no longer pluggable per
    /// output; see `content.rs`'s header). Painted whenever the socket has no valid current frame.
    kiosk_fallback: ClockLockScreen,
    session_content: Box<dyn KioskContent>,
    /// The kiosk display socket's shared state (`crate::socket`) — the latest streamed frame, the
    /// kiosk output's current geometry (written here on `configure`), and the repaint ping.
    socket: Arc<SocketState>,
    /// Resolved during startup and consumed only after `SessionLockHandler::locked` confirms this
    /// process owns the compositor lock. Until then, the existing socket path belongs to the
    /// already-active locker and must not be unlinked (DISPLAY-3).
    socket_path: Option<PathBuf>,
    keyboard: Option<WlKeyboard>,
    password: Zeroizing<String>,
    caps_lock: bool,
    failed: bool,
    verifying: bool,
    attempt_tx: mpsc::Sender<AuthAttempt>,
    /// Consecutive `Wrong`/`MaxTries`/`Error` verdicts since the last (never-happened, since a
    /// success exits) reset. Drives the backoff delay below.
    fail_count: u32,
    /// Set on a failed verdict to `now + backoff`; while `Some` and still in the future, new
    /// attempts are refused (`submit`/`press_key`) so a wrong password cannot be retried faster
    /// than the delay. Cleared once expiry is observed (checked lazily, and swept by the existing
    /// 1s repaint timer) — never a lockout (`SESSION-1`: always retryable, just not instantly).
    locked_until: Option<Instant>,
    next_attempt_id: u64,
    diagnostics: Diagnostics,
    /// Present only for `-f`; consumed once the compositor confirms the session lock is held.
    daemon_ready: Option<ReadyNotifier>,
}

impl Locker {
    fn output_details(&self, output: &wl_output::WlOutput) -> (String, OutputRole, u32) {
        let info = self.output_state.info(output);
        let name = info
            .as_ref()
            .and_then(|i| i.name.clone())
            .unwrap_or_default();
        let role = crate::content::role_for(&self.kiosk_outputs, &name);
        let scale = info
            .as_ref()
            .map(|i| i.scale_factor.max(1) as u32)
            .unwrap_or(1);
        (name, role, scale)
    }

    fn refresh_kiosk_geometry(&self) {
        let geometry = self
            .surfaces
            .iter()
            .find(|entry| entry.role == OutputRole::Kiosk && entry.width > 0 && entry.height > 0)
            .map(|entry| (entry.width, entry.height, entry.scale))
            .unwrap_or((0, 0, 1));
        self.socket.set_geometry(geometry.0, geometry.1, geometry.2);
    }

    fn add_output_surface(
        &mut self,
        qh: &QueueHandle<Self>,
        session_lock: &SessionLock,
        output: wl_output::WlOutput,
        reason: &'static str,
    ) {
        if self.surfaces.iter().any(|entry| entry.output == output) {
            return;
        }

        let (name, role, scale) = self.output_details(&output);
        let surface = self.compositor.create_surface(qh);
        let lock_surface = session_lock.create_lock_surface(surface, &output, qh);
        eprintln!("clck:   '{name}' -> {role:?} ({reason})");
        self.diagnostics.output_surface_added(&name, role, reason);
        self.surfaces.push(Entry {
            output,
            surface: lock_surface,
            role,
            name,
            width: 0,
            height: 0,
            scale,
        });
    }

    fn update_output_surface(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let Some(idx) = self
            .surfaces
            .iter()
            .position(|entry| entry.output == output)
        else {
            let Some(session_lock) = self.lock.clone().filter(SessionLock::is_locked) else {
                return;
            };
            self.add_output_surface(qh, &session_lock, output, "update-after-lock");
            return;
        };

        let (name, role, scale) = self.output_details(&output);
        let entry = &mut self.surfaces[idx];
        if entry.name == name && entry.role == role && entry.scale == scale {
            return;
        }

        entry.name = name;
        entry.role = role;
        entry.scale = scale;
        self.diagnostics
            .output_surface_updated(&entry.name, entry.role, entry.scale);
        self.refresh_kiosk_geometry();
        self.render_surface(idx);
    }

    fn remove_output_surface(&mut self, output: &wl_output::WlOutput) {
        let Some(idx) = self
            .surfaces
            .iter()
            .position(|entry| &entry.output == output)
        else {
            return;
        };
        let entry = self.surfaces.remove(idx);
        eprintln!("clck: output removed: '{}' ({:?})", entry.name, entry.role);
        self.diagnostics
            .output_surface_removed(&entry.name, entry.role);
        self.refresh_kiosk_geometry();
    }

    fn auth_view(&self) -> AuthView {
        let state = if self.verifying {
            AuthState::Verifying
        } else if self.failed {
            AuthState::Failed
        } else {
            AuthState::Idle(self.password.chars().count())
        };
        AuthView {
            state,
            caps_lock: self.caps_lock,
        }
    }

    fn render_surface(&mut self, idx: usize) {
        let (wl, role, name, w, h) = {
            let e = &self.surfaces[idx];
            (
                e.surface.wl_surface().clone(),
                e.role,
                e.name.clone(),
                e.width,
                e.height,
            )
        };
        if w == 0 || h == 0 {
            return;
        }
        let av = self.auth_view();
        let expect = (w * h * 4) as usize;
        // A `Kiosk` output blits the latest socket frame when one exists and its geometry matches
        // this surface exactly; otherwise (no client yet, a disconnect, or a stale/mismatched
        // size) it falls back to the built-in clock, same as `Session` — DISPLAY-1.
        let socket_frame = if role == OutputRole::Kiosk {
            let latest = self.socket.latest.lock().unwrap();
            latest
                .as_ref()
                .filter(|f| f.width == w && f.height == h)
                .map(|f| f.rgba.clone())
        } else {
            None
        };
        let rgba = match socket_frame {
            Some(rgba) => rgba,
            None => {
                let frame = Frame {
                    role,
                    output_name: &name,
                    width: w,
                    height: h,
                    auth: av,
                };
                match role {
                    OutputRole::Kiosk => self.kiosk_fallback.paint(&frame),
                    OutputRole::Session => self.session_content.paint(&frame),
                }
            }
        };
        // KIOSK-2 / FRAME-1: a wrong-sized buffer is never blitted; fall back to the lock screen.
        let rgba = if rgba.len() == expect {
            rgba
        } else {
            eprintln!(
                "clck: {name} content returned {} bytes (want {expect}); lock screen",
                rgba.len()
            );
            let frame = Frame {
                role: OutputRole::Session,
                output_name: &name,
                width: w,
                height: h,
                auth: av,
            };
            self.session_content.paint(&frame)
        };
        if rgba.len() != expect {
            return; // even the fallback disagreed; skip rather than blit garbage
        }
        if self.pool.is_none() {
            self.pool = Some(SlotPool::new(expect, &self.shm).expect("pool"));
        }
        let pool = self.pool.as_mut().unwrap();
        let (buffer, canvas) = pool
            .create_buffer(w as i32, h as i32, w as i32 * 4, wl_shm::Format::Argb8888)
            .expect("buffer");
        for (dst, s) in canvas
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(rgba.as_chunks::<4>().0)
        {
            dst[0] = s[2];
            dst[1] = s[1];
            dst[2] = s[0];
            dst[3] = s[3];
        }
        buffer.attach_to(&wl).expect("attach");
        wl.damage_buffer(0, 0, w as i32, h as i32);
        wl.commit();
    }

    fn redraw_all(&mut self) {
        for idx in 0..self.surfaces.len() {
            self.render_surface(idx);
        }
    }

    fn redraw_locks(&mut self) {
        for idx in 0..self.surfaces.len() {
            if self.surfaces[idx].role == OutputRole::Session {
                self.render_surface(idx);
            }
        }
    }

    /// `true` while a wrong-password backoff delay is still running. Checked lazily against
    /// `Instant::now()` — no dedicated expiry callback needed, since a `false` result here is
    /// exactly as good as an eagerly-cleared `None` (see the repaint timer for the sweep that keeps
    /// `locked_until` from growing stale forever).
    fn backoff_active(&self) -> bool {
        self.locked_until
            .is_some_and(|until| Instant::now() < until)
    }

    fn submit(&mut self) {
        if self.verifying {
            self.diagnostics.submit_ignored("verification_in_flight");
            return;
        }
        if self.backoff_active() {
            self.diagnostics.submit_ignored("backoff_active");
            return;
        }
        if self.password.is_empty() {
            self.diagnostics.submit_ignored("empty_input");
            return;
        }
        let attempt_id = self.next_attempt_id;
        self.next_attempt_id = self.next_attempt_id.saturating_add(1);
        let pw = std::mem::take(&mut self.password);
        self.verifying = true;
        self.failed = false;
        self.redraw_locks();
        self.diagnostics.auth_submitted(attempt_id);
        if self
            .attempt_tx
            .send(AuthAttempt {
                id: attempt_id,
                password: pw,
            })
            .is_err()
        {
            self.diagnostics.auth_delivery_failed(attempt_id);
            self.verifying = false;
            self.failed = true;
            self.redraw_locks();
        }
    }

    fn on_auth(&mut self, result: AuthResult) {
        let AuthResult { id, outcome } = result;
        self.diagnostics.auth_result_received(id, outcome);
        self.verifying = false;
        match outcome {
            AuthOutcome::Unlocked => {
                self.diagnostics.unlock_requested(id);
                if let Some(lock) = self.lock.take() {
                    lock.unlock();
                }
                let flushed = self.conn.flush(); // push unlock_and_destroy before exit
                self.diagnostics.unlock_flushed(flushed.is_ok());
                if let Err(e) = flushed {
                    eprintln!("clck: unlock flush failed: {e}");
                }
                self.diagnostics.authenticated_exit(id);
                std::process::exit(0);
            }
            AuthOutcome::Wrong | AuthOutcome::MaxTries | AuthOutcome::Error => {
                self.failed = true;
                self.password.clear();
                // Growing backoff before the NEXT attempt is accepted (SESSION-1: always
                // retryable, just not instantly) -- 1s,2s,3s,4s,5s, capped at 5s.
                self.fail_count = self.fail_count.saturating_add(1);
                let delay = Duration::from_secs(self.fail_count.min(5) as u64);
                self.locked_until = Some(Instant::now() + delay);
                self.diagnostics.auth_failed_ui(id, delay);
                self.redraw_locks();
            }
        }
    }
}

impl SessionLockHandler for Locker {
    fn locked(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, session_lock: SessionLock) {
        // Create Session (lock-screen) outputs FIRST so keyboard focus lands on a lock screen,
        // never on a kiosk output (KIOSK-1: kiosk surfaces are input-inert).
        let mut ordered: Vec<wl_output::WlOutput> = self.output_state.outputs().collect();
        ordered.sort_by_key(|output| self.output_details(output).1 == OutputRole::Kiosk);
        eprintln!("clck: locked; {} output(s)", ordered.len());
        self.diagnostics.lock_acquired(ordered.len());

        // Only a compositor-confirmed lock owner may publish the kiosk socket. In particular, a
        // duplicate swayidle invocation is rejected through `finished` without reaching here, so
        // it cannot unlink and replace the active locker's listener (DISPLAY-3).
        if let Some(path) = self.socket_path.take() {
            socket::spawn(path, Arc::clone(&self.socket));
        }

        for output in ordered {
            self.add_output_surface(qh, &session_lock, output, "initial-lock");
        }
        if let Some(ready) = self.daemon_ready.take() {
            let notified = ready.notify().is_ok();
            self.diagnostics.daemon_ready(notified);
            if !notified {
                eprintln!("clck: daemon parent disappeared before lock readiness was reported");
            }
        }
    }

    fn finished(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _lock: SessionLock) {
        eprintln!("clck: lock finished/rejected");
        self.diagnostics.lock_finished();
        std::process::exit(1);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        if let Some(idx) = self
            .surfaces
            .iter()
            .position(|e| e.surface.wl_surface() == surface.wl_surface())
        {
            self.surfaces[idx].width = w;
            self.surfaces[idx].height = h;
            self.diagnostics.surface_configured(
                &self.surfaces[idx].name,
                self.surfaces[idx].role,
                w,
                h,
                self.surfaces[idx].scale,
            );
            // The one (v1) kiosk output's geometry is what a connecting socket client's HELLO
            // reports, and what an already-connected client's frames are validated against — keep
            // it current on every configure, including a later resize or hot-unplug.
            self.refresh_kiosk_geometry();
            self.render_surface(idx);
        }
    }
}

impl SeatHandler for Locker {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            self.diagnostics.keyboard_capability(true);
            match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(kb) => self.keyboard = Some(kb),
                Err(e) => eprintln!("clck: get_keyboard failed: {e}"),
            }
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Keyboard {
            self.diagnostics.keyboard_capability(false);
            if let Some(kb) = self.keyboard.take() {
                kb.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl KeyboardHandler for Locker {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        let output = self
            .surfaces
            .iter()
            .find(|entry| entry.surface.wl_surface() == surface)
            .map(|entry| (entry.name.as_str(), entry.role));
        self.diagnostics.keyboard_focus(true, output);
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        let output = self
            .surfaces
            .iter()
            .find(|entry| entry.surface.wl_surface() == surface)
            .map(|entry| (entry.name.as_str(), entry.role));
        self.diagnostics.keyboard_focus(false, output);
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self.verifying || self.backoff_active() {
            return; // one attempt in flight (AUTH-2), or cooling down after a wrong one
        }
        match event.keysym {
            Keysym::Return | Keysym::KP_Enter => self.submit(),
            Keysym::BackSpace => {
                self.password.pop();
                self.failed = false;
                self.redraw_locks();
            }
            Keysym::Escape => {
                self.password.clear();
                self.failed = false;
                self.redraw_locks();
            }
            _ => {
                if let Some(txt) = event.utf8 {
                    if !txt.is_empty() && !txt.chars().any(|c| c.is_control()) {
                        self.password.push_str(&txt);
                        self.failed = false;
                        self.redraw_locks();
                    }
                }
            }
        }
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn repeat_key(
        &mut self,
        c: &Connection,
        q: &QueueHandle<Self>,
        k: &WlKeyboard,
        s: u32,
        e: KeyEvent,
    ) {
        self.press_key(c, q, k, s, e);
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        mods: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        if mods.caps_lock != self.caps_lock {
            self.caps_lock = mods.caps_lock;
            self.redraw_locks();
        }
    }
}

impl CompositorHandler for Locker {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}
impl OutputHandler for Locker {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let Some(session_lock) = self.lock.clone().filter(SessionLock::is_locked) else {
            // Initial output discovery finishes before lock acquisition. `locked` snapshots those
            // outputs after the compositor confirms ownership; only later advertisements belong
            // to the hotplug path here (LOCK-3).
            return;
        };
        self.add_output_surface(qh, &session_lock, output, "hotplug");
    }

    fn update_output(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.update_output_surface(qh, output);
    }

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.remove_output_surface(&output);
    }
}
impl ShmHandler for Locker {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
impl ProvidesRegistryState for Locker {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}
delegate_compositor!(Locker);
delegate_output!(Locker);
delegate_shm!(Locker);
delegate_seat!(Locker);
delegate_keyboard!(Locker);
delegate_session_lock!(Locker);
delegate_registry!(Locker);

#[cfg(test)]
mod tests {
    use super::*;

    // Reaching the end of this test IS the assertion. SIGUSR1's default disposition terminates the
    // process, so without the handler this kills the whole test binary rather than failing one
    // case -- which is precisely what `pkill -USR1 clck` did to the live locker, leaving the
    // compositor holding a lock with no client to unlock it (LOCK-2 + SESSION-1).
    #[test]
    fn sigusr1_is_survivable_and_observed() {
        assert!(
            install_sigusr1_handler(),
            "kernel refused the SIGUSR1 handler"
        );
        SIGUSR1_SEEN.store(false, Ordering::Relaxed);

        // SAFETY: raise() on the calling thread, with our handler installed above.
        assert_eq!(unsafe { libc::raise(libc::SIGUSR1) }, 0);

        assert!(
            SIGUSR1_SEEN.swap(false, Ordering::Relaxed),
            "the handler ran but did not record the signal, so the tick can never see it"
        );
    }

    // SESSION-1: every input path that is not PAM has to be provably incapable of reaching the
    // gate. Asserted against the handler's own source so that GIVING it auth state to touch fails
    // the build -- the same guard the kiosk socket carries for DISPLAY-2.
    #[test]
    fn the_sigusr1_handler_touches_nothing_but_its_own_flag() {
        let source = include_str!("locker.rs");
        let body = source
            .split("extern \"C\" fn on_sigusr1")
            .nth(1)
            .expect("handler not found")
            .split("\n}")
            .next()
            .unwrap();

        for forbidden in [
            "unlock",
            "password",
            "pam",
            "auth",
            "failed",
            "locked_until",
        ] {
            assert!(
                !body.to_lowercase().contains(forbidden),
                "the SIGUSR1 handler references `{forbidden}`: a signal must not be able to reach \
                 authentication state (BEHAVIORS.md SESSION-1)"
            );
        }
    }

    // DISPLAY-3: a duplicate locker is rejected asynchronously, so anything done in `run_inner`
    // happens too early to claim ownership of the kiosk socket. Keep the actual start inside the
    // compositor's confirmed `locked` callback; `finished` must be able to exit without ever
    // unlinking the active locker's socket path.
    #[test]
    fn the_kiosk_socket_starts_only_after_the_session_lock_is_acquired() {
        let source = include_str!("locker.rs");
        let startup = source
            .split("fn run_inner(")
            .nth(1)
            .expect("run_inner not found")
            .split("struct Entry")
            .next()
            .unwrap();
        assert!(
            !startup.contains("socket::spawn("),
            "run_inner starts the kiosk socket before the compositor accepts the session lock"
        );

        let locked = source
            .split("impl SessionLockHandler for Locker")
            .nth(1)
            .expect("SessionLockHandler implementation not found")
            .split("fn finished(")
            .next()
            .unwrap();
        assert!(
            locked.contains("socket::spawn("),
            "the confirmed locked callback never starts the kiosk socket"
        );
    }

    // LOCK-3: this source-level guard complements checks/headless-output-hotplug.sh's real
    // compositor exercise. It keeps all three OutputHandler callbacks wired to the lifecycle
    // helpers so a later cleanup cannot silently restore the zero-output lockout.
    #[test]
    fn output_callbacks_maintain_lock_surfaces_after_acquisition() {
        let source = include_str!("locker.rs");
        let handler = source
            .split("impl OutputHandler for Locker")
            .nth(1)
            .expect("OutputHandler implementation not found")
            .split("impl ShmHandler for Locker")
            .next()
            .unwrap();

        for required in [
            "self.add_output_surface(",
            "self.update_output_surface(",
            "self.remove_output_surface(",
        ] {
            assert!(
                handler.contains(required),
                "OutputHandler lost `{required}`, so a locked-session hotplug can become invisible"
            );
        }
    }
}

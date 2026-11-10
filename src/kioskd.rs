//! `clck kioskd`: the kiosk without a lock. A small separate process that shows the kiosk content (the latest frame a client
//! streams, else the built-in clock) on the configured kiosk outputs through an OVERLAY layer surface while `kiosk on` is the
//! state, and removes it on `kiosk off`. It decides nothing: whoever wants the kiosk shown sends the command
//! (`crate::control`).
//!
//! What it deliberately is not: a second locker. It holds no session lock, takes no keyboard (keyboard interactivity none,
//! no seat bound at all), takes no pointer (an empty input region: events pass through to what is below), and has no
//! connection to the authentication code: `control.rs` pins by test that neither this file nor the protocol mentions it.
//! When the session is locked the compositor shows only the lock surfaces, so this overlay is simply not visible then;
//! `kiosk off` while locked just removes the overlay. Outputs not named in `kiosk_outputs` never get a surface (the same
//! exact-name rule as the locker's kiosk role).
//!
//! Frames: its OWN live socket (`live_socket`, default `$XDG_RUNTIME_DIR/clck-live.sock`), never the locker's, so the locker's
//! rule that only the accepted locker publishes ITS socket is untouched. The wire is the same content-blind one
//! (`crate::socket`): a client such as cksk connects, is told the kiosk output's geometry and streams frames; the latest valid
//! one is kept and painted while the kiosk is on. A client may stay connected for good: it costs nothing here until `on`.
//!
//! Wakeups: the control socket and the compositor's events; a one-second clock repaint only while the kiosk is on.

use crate::content::{role_for, AuthState, AuthView, Frame, KioskContent, OutputRole};
use crate::control::{self, Command, Reply, MAX_LINE};
use crate::lockscreen::ClockLockScreen;
use crate::socket::{self, SocketState};
use calloop::{
    generic::Generic,
    ping::make_ping,
    timer::{TimeoutAction, Timer},
    EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

pub struct Config {
    /// Connector names that carry the kiosk. Empty: `kiosk on` is refused (nothing to show it on).
    pub kiosk_outputs: Vec<String>,
    /// Where `clck kiosk on|off|status` connects. `None`: `$XDG_RUNTIME_DIR/clck-control.sock`.
    pub control_socket: Option<PathBuf>,
    /// Where a frame client connects. `None`: `$XDG_RUNTIME_DIR/clck-live.sock`.
    pub live_socket: Option<PathBuf>,
    /// The LOCKER's kiosk socket path, only to refuse a live socket that names the same file.
    pub locker_socket: Option<PathBuf>,
}

#[derive(Debug)]
pub enum Error {
    Wayland(String),
    Control(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Wayland(s) => write!(f, "wayland: {s}"),
            Error::Control(s) => write!(f, "control socket: {s}"),
        }
    }
}
impl std::error::Error for Error {}

struct Entry {
    output: wl_output::WlOutput,
    layer: LayerSurface,
    name: String,
    width: u32,
    height: u32,
}

struct Kiosk {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    shm: Shm,
    layer_shell: LayerShell,
    pool: Option<SlotPool>,
    on: bool,
    entries: Vec<Entry>,
    kiosk_outputs: Vec<String>,
    clock: ClockLockScreen,
    socket: Arc<SocketState>,
    handle: Option<LoopHandle<'static, Kiosk>>,
    tick: Option<RegistrationToken>,
}

/// `$XDG_RUNTIME_DIR/<name>`.
fn runtime_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join(name))
}

/// Bind the control socket: its parent must exist, a stale socket nobody answers on is replaced, a live one is refused,
/// anything that is not a socket is never touched. Mode 0600.
pub fn bind_control(path: &Path) -> std::io::Result<UnixListener> {
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if !m.file_type().is_socket() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        if UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("{} is served by another kioskd", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    l.set_nonblocking(true)?;
    Ok(l)
}

/// Read one request line from a connection, bounded in size and in time. `None`: nothing usable arrived.
fn read_request(s: &mut UnixStream) -> Option<String> {
    s.set_read_timeout(Some(Duration::from_millis(250))).ok()?;
    s.set_write_timeout(Some(Duration::from_millis(250))).ok()?;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while buf.len() <= MAX_LINE {
        match s.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => buf.push(byte[0]),
            Err(_) => return None,
        }
    }
    String::from_utf8(buf).ok()
}

impl Kiosk {
    fn kiosk_names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }

    fn output_name(&self, output: &wl_output::WlOutput) -> String {
        self.output_state
            .info(output)
            .and_then(|i| i.name)
            .unwrap_or_default()
    }

    /// The geometry a frame client is told, from the first kiosk output the compositor has described, so a client can
    /// connect and stream while the kiosk is still off.
    fn publish_geometry(&self) {
        let g = self
            .output_state
            .outputs()
            .filter_map(|o| self.output_state.info(&o))
            .find(|i| {
                i.name
                    .as_deref()
                    .is_some_and(|n| role_for(&self.kiosk_outputs, n) == OutputRole::Kiosk)
            })
            .and_then(|i| {
                i.logical_size.map(|(w, h)| {
                    (
                        w.max(0) as u32,
                        h.max(0) as u32,
                        i.scale_factor.max(1) as u32,
                    )
                })
            })
            .unwrap_or((0, 0, 1));
        self.socket.set_geometry(g.0, g.1, g.2);
    }

    fn add_surface(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if self.entries.iter().any(|e| e.output == output) {
            return;
        }
        let name = self.output_name(&output);
        if role_for(&self.kiosk_outputs, &name) != OutputRole::Kiosk {
            return;
        }
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("clck-kiosk"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, 0);
        // Over everything but pushing nothing aside, taking neither keys nor clicks.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        match Region::new(&self.compositor) {
            Ok(empty) => layer.wl_surface().set_input_region(Some(empty.wl_region())),
            Err(e) => eprintln!("clck kioskd: no empty input region ({e})"),
        }
        layer.commit();
        eprintln!("clck kioskd: '{name}': kiosk shown");
        self.entries.push(Entry {
            output,
            layer,
            name,
            width: 0,
            height: 0,
        });
    }

    fn remove_surfaces(&mut self) {
        for e in self.entries.drain(..) {
            eprintln!("clck kioskd: '{}': kiosk removed", e.name);
        }
    }

    fn set_on(&mut self, on: bool, qh: &QueueHandle<Self>) {
        if on == self.on {
            return;
        }
        self.on = on;
        if on {
            let outputs: Vec<_> = self.output_state.outputs().collect();
            for o in outputs {
                self.add_surface(qh, o);
            }
            self.start_tick();
        } else {
            self.stop_tick();
            self.remove_surfaces();
        }
    }

    /// The clock repaints once a second, only while the kiosk is shown.
    fn start_tick(&mut self) {
        if self.tick.is_some() {
            return;
        }
        let Some(handle) = self.handle.clone() else {
            return;
        };
        self.tick = handle
            .insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, k| {
                k.redraw_all();
                TimeoutAction::ToDuration(Duration::from_secs(1))
            })
            .ok();
    }

    fn stop_tick(&mut self) {
        if let (Some(t), Some(h)) = (self.tick.take(), self.handle.as_ref()) {
            h.remove(t);
        }
    }

    fn handle_command(&mut self, c: Command, qh: &QueueHandle<Self>) -> Reply {
        match c {
            Command::Kiosk(true) if self.kiosk_outputs.is_empty() => {
                Reply::err("no kiosk output is configured")
            }
            Command::Kiosk(on) => {
                self.set_on(on, qh);
                Reply::state(self.on, Some(self.kiosk_names()))
            }
            Command::Status => Reply::state(self.on, Some(self.kiosk_names())),
        }
    }

    fn serve(&mut self, listener: &UnixListener, qh: &QueueHandle<Self>) {
        while let Ok((mut s, _)) = listener.accept() {
            let reply = match read_request(&mut s) {
                None => Reply::err("no request"),
                Some(line) => match control::parse_line(&line) {
                    Ok(c) => self.handle_command(c, qh),
                    Err(e) => Reply::err(e.to_string()),
                },
            };
            let _ = s.write_all(reply.line().as_bytes());
        }
    }

    fn draw(&mut self, idx: usize) {
        let (wl, name, w, h) = {
            let e = &self.entries[idx];
            (
                e.layer.wl_surface().clone(),
                e.name.clone(),
                e.width,
                e.height,
            )
        };
        if w == 0 || h == 0 {
            return;
        }
        let expect = (w * h * 4) as usize;
        let streamed = self.socket.latest.lock().ok().and_then(|l| {
            l.as_ref()
                .filter(|f| f.width == w && f.height == h)
                .map(|f| f.rgba.clone())
        });
        let rgba = streamed.filter(|b| b.len() == expect).unwrap_or_else(|| {
            self.clock.paint(&Frame {
                role: OutputRole::Kiosk,
                output_name: &name,
                width: w,
                height: h,
                auth: AuthView {
                    state: AuthState::Idle(0),
                    caps_lock: false,
                },
            })
        });
        if rgba.len() != expect {
            return; // never blit a wrong-sized buffer
        }
        if self.pool.is_none() {
            self.pool = SlotPool::new(expect, &self.shm).ok();
        }
        let Some(pool) = self.pool.as_mut() else {
            return;
        };
        let Ok((buffer, canvas)) =
            pool.create_buffer(w as i32, h as i32, w as i32 * 4, wl_shm::Format::Argb8888)
        else {
            return;
        };
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
        if buffer.attach_to(&wl).is_ok() {
            wl.damage_buffer(0, 0, w as i32, h as i32);
            wl.commit();
        }
    }

    fn redraw_all(&mut self) {
        for idx in 0..self.entries.len() {
            self.draw(idx);
        }
    }
}

/// Run until killed.
pub fn run(config: Config) -> Result<(), Error> {
    let control_path = config
        .control_socket
        .clone()
        .or_else(|| runtime_path("clck-control.sock"))
        .ok_or_else(|| Error::Control("no XDG_RUNTIME_DIR and no control_socket_path".into()))?;
    let live_path = config
        .live_socket
        .clone()
        .or_else(|| runtime_path("clck-live.sock"));
    if live_path.is_some() && live_path == config.locker_socket {
        return Err(Error::Control(
            "the live socket must not be the locker's kiosk socket".into(),
        ));
    }
    eprintln!(
        "clck kioskd: kiosk_outputs={:?} control={}",
        config.kiosk_outputs,
        control_path.display()
    );

    let conn = Connection::connect_to_env().map_err(|e| Error::Wayland(e.to_string()))?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).map_err(|e| Error::Wayland(e.to_string()))?;
    let qh: QueueHandle<Kiosk> = event_queue.handle();
    let compositor =
        CompositorState::bind(&globals, &qh).map_err(|e| Error::Wayland(e.to_string()))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| Error::Wayland(e.to_string()))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|e| Error::Wayland(format!("the compositor has no wlr-layer-shell: {e}")))?;
    let output_state = OutputState::new(&globals, &qh);

    let socket = SocketState::new();
    match live_path {
        Some(p) => {
            eprintln!("clck kioskd: live frames on {}", p.display());
            socket::spawn(p, socket.clone());
        }
        None => eprintln!("clck kioskd: no live socket (no XDG_RUNTIME_DIR): the clock only"),
    }

    let mut kiosk = Kiosk {
        registry_state: RegistryState::new(&globals),
        output_state,
        compositor,
        shm,
        layer_shell,
        pool: None,
        on: false,
        entries: Vec::new(),
        kiosk_outputs: config.kiosk_outputs,
        clock: ClockLockScreen::new(),
        socket,
        handle: None,
        tick: None,
    };
    event_queue
        .roundtrip(&mut kiosk)
        .map_err(|e| Error::Wayland(e.to_string()))?;
    event_queue
        .roundtrip(&mut kiosk)
        .map_err(|e| Error::Wayland(e.to_string()))?;
    kiosk.publish_geometry();

    // Advertised last: a control socket exists only once this daemon can act on a command (a daemon that failed to reach
    // the compositor leaves no socket behind for `kiosk status` to be refused on).
    let listener = bind_control(&control_path)
        .map_err(|e| Error::Control(format!("{}: {e}", control_path.display())))?;

    let mut event_loop: EventLoop<'static, Kiosk> =
        EventLoop::try_new().map_err(|e| Error::Wayland(e.to_string()))?;
    let handle = event_loop.handle();
    kiosk.handle = Some(handle.clone());
    WaylandSource::new(conn, event_queue)
        .insert(handle.clone())
        .map_err(|e| Error::Wayland(e.to_string()))?;

    let qh2 = qh.clone();
    let listener = Arc::new(listener);
    let l2 = listener.clone();
    handle
        .insert_source(
            Generic::new(ListenerFd(listener), Interest::READ, Mode::Level),
            move |_, _, k| {
                k.serve(&l2, &qh2);
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| Error::Control(e.to_string()))?;

    // A new frame repaints at once (only matters while the kiosk is shown).
    match make_ping() {
        Ok((ping, source)) => {
            kiosk.socket.set_ping(ping);
            handle
                .insert_source(source, |_, _, k| {
                    if k.on {
                        k.redraw_all();
                    }
                })
                .map_err(|e| Error::Wayland(e.to_string()))?;
        }
        Err(e) => {
            eprintln!("clck kioskd: repaint ping unavailable ({e}); the clock tick still applies")
        }
    }

    event_loop
        .run(None, &mut kiosk, |_| {})
        .map_err(|e| Error::Wayland(e.to_string()))
}

/// The control listener as a pollable fd that keeps the listener alive.
struct ListenerFd(Arc<UnixListener>);
impl std::os::fd::AsFd for ListenerFd {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&*self.0)
    }
}

impl CompositorHandler for Kiosk {
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

impl OutputHandler for Kiosk {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.publish_geometry();
        if self.on {
            self.add_surface(qh, output);
        }
    }
    fn update_output(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.publish_geometry();
        // A name that arrives after the output (the compositor describes it in pieces) makes it eligible now.
        if self.on {
            self.add_surface(qh, output);
        }
    }
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.entries.retain(|e| e.output != output);
        self.publish_geometry();
    }
}

impl LayerShellHandler for Kiosk {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        self.entries
            .retain(|e| e.layer.wl_surface() != layer.wl_surface());
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let Some(idx) = self
            .entries
            .iter()
            .position(|e| e.layer.wl_surface() == layer.wl_surface())
        else {
            return;
        };
        let (w, h) = configure.new_size;
        self.entries[idx].width = w;
        self.entries[idx].height = h;
        self.draw(idx);
    }
}

impl ShmHandler for Kiosk {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Kiosk {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(Kiosk);
delegate_output!(Kiosk);
delegate_shm!(Kiosk);
delegate_layer!(Kiosk);
delegate_registry!(Kiosk);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    struct Dir(PathBuf);
    impl Dir {
        fn new() -> Dir {
            let d = std::env::temp_dir().join(format!(
                "clck-kioskd-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&d).unwrap();
            Dir(d)
        }
        fn sock(&self) -> PathBuf {
            self.0.join("control.sock")
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_control_socket_is_0600() {
        let d = Dir::new();
        let _l = bind_control(&d.sock()).unwrap();
        let mode = std::fs::metadata(d.sock()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_live_socket_is_refused_a_stale_one_replaced_a_file_never_touched() {
        let d = Dir::new();
        let live = bind_control(&d.sock()).unwrap();
        assert_eq!(
            bind_control(&d.sock()).err().unwrap().kind(),
            std::io::ErrorKind::AddrInUse
        );
        drop(live); // the path stays, nobody answers
        assert!(d.sock().exists());
        let _again = bind_control(&d.sock()).unwrap();

        let f = Dir::new();
        std::fs::write(f.sock(), b"precious").unwrap();
        assert_eq!(
            bind_control(&f.sock()).err().unwrap().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(f.sock()).unwrap(), b"precious");
    }

    #[test]
    fn a_request_is_bounded_in_size_and_time() {
        let d = Dir::new();
        let l = bind_control(&d.sock()).unwrap();
        l.set_nonblocking(false).unwrap();
        // a request with no newline and far too long: read stops at the cap, the line is refused by the parser
        let path = d.sock();
        let h = std::thread::spawn(move || {
            let mut c = UnixStream::connect(path).unwrap();
            c.write_all(&vec![b'x'; 10_000]).unwrap();
        });
        let (mut s, _) = l.accept().unwrap();
        let line = read_request(&mut s).unwrap();
        assert!(line.len() <= MAX_LINE + 1);
        assert!(control::parse_line(&line).is_err());
        h.join().unwrap();
        // a client that connects and says nothing is let go after the timeout
        let path = d.sock();
        let _silent = UnixStream::connect(path).unwrap();
        let (mut s, _) = l.accept().unwrap();
        let t = std::time::Instant::now();
        assert!(read_request(&mut s).is_none());
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_closed_connection_without_a_newline_still_yields_its_text() {
        let d = Dir::new();
        let l = bind_control(&d.sock()).unwrap();
        l.set_nonblocking(false).unwrap();
        let path = d.sock();
        let h = std::thread::spawn(move || {
            let mut c = UnixStream::connect(path).unwrap();
            c.write_all(br#"{"cmd":"status"}"#).unwrap();
        });
        let (mut s, _) = l.accept().unwrap();
        h.join().unwrap();
        let line = read_request(&mut s).unwrap();
        assert_eq!(control::parse_line(&line), Ok(Command::Status));
    }

    #[test]
    fn the_live_socket_may_not_be_the_lockers() {
        let p = PathBuf::from("/tmp/same.sock");
        let e = run(Config {
            kiosk_outputs: vec![],
            control_socket: Some(PathBuf::from("/nonexistent-dir/c.sock")),
            live_socket: Some(p.clone()),
            locker_socket: Some(p),
        })
        .err()
        .unwrap();
        assert!(e.to_string().contains("must not be the locker"), "{e}");
    }
}

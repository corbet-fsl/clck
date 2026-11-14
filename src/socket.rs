//! The kiosk frame SOCKET SERVER: a content-BLIND Unix-domain frame receiver. One client at a time (cksk, or anything
//! that writes Netpbm PAM images) streams frames in; the render paths (`locker.rs`, `kioskd.rs`) paint the latest one on
//! the KIOSK output and fall back to clck's own built-in clock while no frame has arrived or once the client is gone.
//!
//! FRAME INPUT, Netpbm PAM over a Unix socket (README "Frame input"; BEHAVIORS.md DISPLAY-1, DISPLAY-2, DISPLAY-4):
//!   - the server writes NOTHING on the socket: no greeting, no reply. A client connects and writes images.
//!   - the stream is a sequence of PAM (`P7`) images, each a header and then exactly `WIDTH * HEIGHT * DEPTH` raster
//!     bytes. `crate::netpbm` holds what is accepted: `TUPLTYPE RGB_ALPHA` with `DEPTH 4` (straight alpha) or
//!     `TUPLTYPE RGB` with `DEPTH 3`, `MAXVAL 255`, a header of at most 1 KiB, `WIDTH` and `HEIGHT` each `1..=8192`.
//!     Anything else closes the connection: a byte stream cannot be resynchronized after an image it does not understand.
//!   - the peer must be this process's own user (`SO_PEERCRED`); any other is closed at once, before it can replace the
//!     current client.
//!   - once an image's first byte has arrived, the whole image must arrive within `FRAME_DEADLINE` (10 s), else the
//!     connection is closed. Between images a client may stay idle for as long as it likes.
//!   - an accepted image of any size is kept; the render path shows it 1:1 when it matches the kiosk output and scales it
//!     to fit otherwise, aspect preserved, centred on black (`LatestFrame::fit`).
//!   - while there is no kiosk output, images are read and discarded.
//!   - a new connection replaces the previous one (one client at a time, the latest wins); a disconnect clears the frame
//!     and the kiosk goes back to the clock.
//!
//! DISPLAY-ONLY, PERMANENTLY (BEHAVIORS.md DISPLAY-2): this module only ever turns received bytes into pixels for one
//! output. It has no path to `Locker`'s password buffer, auth state, or the session-lock unlock call -- SESSION-1 stays
//! the only gate, entirely untouched by anything here. A reverse channel (input events client-ward, so a streamed
//! dashboard could itself be interactive) is a plausible future addition -- deliberately NOT built here; it would need its
//! own careful accounting against KIOSK-1 (kiosk surfaces are input-inert) before it could exist at all.
use crate::netpbm;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// An image whose first byte has arrived must be complete within this long, or the connection is closed.
pub const FRAME_DEADLINE: Duration = Duration::from_secs(10);

/// The most recently fully-received frame, as premultiplied RGBA8 at the size the client sent -- or `None` while no client
/// has sent one, after the streaming client disconnects, or while there is no kiosk output.
pub struct LatestFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl LatestFrame {
    /// This frame as premultiplied RGBA8 for a `width` x `height` surface: the frame itself when the sizes match (1:1),
    /// otherwise scaled to fit, aspect preserved, centred on black. `None` only for a surface without area.
    pub fn fit(&self, width: u32, height: u32) -> Option<Vec<u8>> {
        if (self.width, self.height) == (width, height) {
            return Some(self.rgba.clone());
        }
        let src = tiny_skia::PixmapRef::from_bytes(&self.rgba, self.width, self.height)?;
        let mut dst = tiny_skia::Pixmap::new(width, height)?;
        dst.fill(tiny_skia::Color::BLACK);
        let scale = (width as f32 / self.width as f32).min(height as f32 / self.height as f32);
        let (w, h) = (self.width as f32 * scale, self.height as f32 * scale);
        // Whole-pixel offsets: the fitted image starts on a pixel edge.
        let x = ((width as f32 - w) / 2.0).floor();
        let y = ((height as f32 - h) / 2.0).floor();
        dst.draw_pixmap(
            0,
            0,
            src,
            &tiny_skia::PixmapPaint {
                quality: tiny_skia::FilterQuality::Bilinear,
                ..Default::default()
            },
            tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, x, y),
            None,
        );
        Some(dst.take())
    }
}

/// Shared state the render path (`locker.rs`, `kioskd.rs`) reads and this module's accept/read threads write.
pub struct SocketState {
    pub latest: Mutex<Option<LatestFrame>>,
    /// Whether a kiosk output exists to show a frame on. While it does not -- `kiosk_outputs` is empty, or the compositor
    /// has not described or configured the kiosk output yet, or it was unplugged -- received frames are discarded.
    /// Read and written only while holding `latest`, so a frame is never stored after the output went away.
    kiosk_output: AtomicBool,
    /// Set once the caller's calloop event loop exists, so a newly-accepted frame can wake a repaint immediately instead
    /// of waiting for the 1s tick. `None` early at startup is fine -- the periodic tick still repaints eventually.
    ping: Mutex<Option<calloop::ping::Ping>>,
    /// Monotonic id of the current connection; a superseded (older) connection's handler thread checks this and exits
    /// rather than going on storing frames nobody will read.
    active_gen: AtomicU64,
    /// A clone of the currently-active stream, kept ONLY so a new connection can `shutdown()` the previous one --
    /// unblocking its handler thread out of a pending `read` (one client at a time: a new connection replaces the old).
    active_stream: Mutex<Option<UnixStream>>,
}

impl SocketState {
    pub fn new() -> Arc<Self> {
        Arc::new(SocketState {
            latest: Mutex::new(None),
            kiosk_output: AtomicBool::new(false),
            ping: Mutex::new(None),
            active_gen: AtomicU64::new(0),
            active_stream: Mutex::new(None),
        })
    }

    /// Called by the render path whenever its kiosk outputs change: `true` while at least one exists to paint on.
    /// Losing the last one drops the held frame; frames are then discarded until one is back.
    pub fn set_kiosk_output(&self, present: bool) {
        let mut latest = self.latest.lock().unwrap();
        self.kiosk_output.store(present, Ordering::SeqCst);
        if !present {
            *latest = None;
        }
    }

    /// Called once the caller's `EventLoop` and its ping source exist.
    pub fn set_ping(&self, ping: calloop::ping::Ping) {
        *self.ping.lock().unwrap() = Some(ping);
    }
}

/// Resolve the socket path: the configured value, else `$XDG_RUNTIME_DIR/clck.sock`. `None` when neither is available
/// -- the caller then skips the socket server entirely. A display feature must never block or weaken the lock itself
/// (DISPLAY-2), so a missing/misconfigured socket is a silent-degrade-to-clock, never a startup failure.
pub fn resolve_path(configured: Option<&Path>) -> Option<PathBuf> {
    configured.map(Path::to_path_buf).or_else(|| {
        std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|d| !d.is_empty())
            .map(|d| PathBuf::from(d).join("clck.sock"))
    })
}

/// Starts the listener thread. Best-effort: a bind failure is logged and the kiosk stays on the default clock forever
/// (never a reason to fail the lock itself -- see `resolve_path` above).
pub fn spawn(path: PathBuf, state: Arc<SocketState>) {
    std::thread::spawn(move || {
        // Unlink a stale socket from a previous, uncleanly-exited run before binding -- otherwise `bind` fails with
        // AddrInUse against a path nothing is actually listening on any more.
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "clck: socket: bind {} failed: {e}; kiosk socket disabled, default clock only",
                    path.display()
                );
                return;
            }
        };
        eprintln!("clck: socket: listening on {}", path.display());
        for stream in listener.incoming() {
            match stream {
                Ok(s) => accept_one(s, &state),
                Err(e) => eprintln!("clck: socket: accept failed: {e}"),
            }
        }
    });
}

/// The peer's user id, from the kernel (`SO_PEERCRED`), never from anything the peer sends.
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    let cred = rustix::net::sockopt::socket_peercred(stream).map_err(std::io::Error::from)?;
    Ok(cred.uid.as_raw())
}

fn own_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn accept_one(stream: UnixStream, state: &Arc<SocketState>) {
    // The same-user rule comes first: a refused peer is closed (dropped) before it can replace the current client.
    match peer_uid(&stream) {
        Ok(uid) if uid == own_uid() => {}
        Ok(uid) => {
            eprintln!("clck: socket: refused a client of uid {uid} (only this user may stream)");
            return;
        }
        Err(e) => {
            eprintln!("clck: socket: peer credentials unavailable ({e}); closed");
            return;
        }
    }

    // One client at a time: this connection becomes the new active one, and whatever was active before it gets shut
    // down so its handler thread unblocks out of its read and exits cleanly.
    let my_gen = state.active_gen.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(old) = state.active_stream.lock().unwrap().take() {
        let _ = old.shutdown(Shutdown::Both);
    }
    eprintln!("clck: socket: client connected");

    let reader = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("clck: socket: stream clone failed: {e}");
            return;
        }
    };
    *state.active_stream.lock().unwrap() = Some(stream);

    let state = Arc::clone(state);
    std::thread::spawn(move || read_frames(reader, my_gen, state, FRAME_DEADLINE));
}

/// The stream as a reader with an optional deadline: while `until` is set, every read waits at most until then and a
/// read past it fails with `TimedOut`. One deadline for the whole image, not per read, so a peer that trickles a byte
/// at a time is let go at the deadline too.
struct Deadline {
    stream: UnixStream,
    until: Option<Instant>,
}

impl Read for Deadline {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let stalled =
            || std::io::Error::new(ErrorKind::TimedOut, "the image was not complete in time");
        let wait = match self.until {
            None => None,
            Some(until) => match until.checked_duration_since(Instant::now()) {
                Some(left) if !left.is_zero() => Some(left),
                _ => return Err(stalled()),
            },
        };
        self.stream.set_read_timeout(wait)?;
        match self.stream.read(buf) {
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Err(stalled())
            }
            other => other,
        }
    }
}

fn read_frames(stream: UnixStream, my_gen: u64, state: Arc<SocketState>, deadline: Duration) {
    let current = || state.active_gen.load(Ordering::SeqCst) == my_gen;
    let mut reader = BufReader::with_capacity(
        64 * 1024,
        Deadline {
            stream,
            until: None,
        },
    );
    let mut logged_size = None;
    let mut discarding = false;
    loop {
        if !current() {
            break; // superseded by a newer connection; that thread already owns the display
        }
        // Between images there is no deadline: a client may stay connected and quiet for good.
        reader.get_mut().until = None;
        match reader.fill_buf() {
            Ok([]) => break, // a clean end between images
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("clck: socket: read failed: {e}");
                break;
            }
        }
        // The image has started: all of it must arrive within the deadline.
        reader.get_mut().until = Some(Instant::now() + deadline);
        let image = netpbm::read_header(&mut reader)
            .and_then(|h| netpbm::read_raster(&mut reader, &h).map(|rgba| (h, rgba)));
        let (header, rgba) = match image {
            Ok(image) => image,
            Err(e) => {
                eprintln!("clck: socket: {e}; closing");
                break; // no reliable way to resync the byte stream past an image it could not take
            }
        };
        let mut latest = state.latest.lock().unwrap();
        if !current() {
            break;
        }
        if !state.kiosk_output.load(Ordering::SeqCst) {
            if !discarding {
                eprintln!("clck: socket: no kiosk output; frames are discarded until there is one");
                discarding = true;
            }
            continue;
        }
        discarding = false;
        let size = (header.width, header.height);
        if logged_size != Some(size) {
            eprintln!("clck: socket: frame {}x{} accepted", size.0, size.1);
            logged_size = Some(size);
        }
        *latest = Some(LatestFrame {
            width: header.width,
            height: header.height,
            rgba,
        });
        drop(latest);
        if let Some(ping) = state.ping.lock().unwrap().as_ref() {
            ping.ping();
        }
    }
    // Only the still-current generation clears the frame back to the default clock -- a superseded connection's late
    // exit must never stomp the NEW connection's already-live frame.
    if current() {
        *state.latest.lock().unwrap() = None;
        eprintln!("clck: socket: client disconnected; kiosk falls back to the default clock");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Drive `read_frames` exactly as production does -- on its own thread, against a real `UnixStream` pair -- with the
    /// given image deadline. Returns the writable client end and a join handle.
    fn serve_with(
        state: &Arc<SocketState>,
        deadline: Duration,
    ) -> (UnixStream, std::thread::JoinHandle<()>) {
        let (client, server) = UnixStream::pair().unwrap();
        let my_gen = state.active_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let st = Arc::clone(state);
        let h = std::thread::spawn(move || read_frames(server, my_gen, st, deadline));
        (client, h)
    }

    fn serve(state: &Arc<SocketState>) -> (UnixStream, std::thread::JoinHandle<()>) {
        serve_with(state, FRAME_DEADLINE)
    }

    fn with_output() -> Arc<SocketState> {
        let state = SocketState::new();
        state.set_kiosk_output(true);
        state
    }

    /// One RGB_ALPHA image, every pixel the straight-alpha `px`.
    fn image(width: u32, height: u32, px: [u8; 4]) -> Vec<u8> {
        let mut v = format!(
            "P7\nWIDTH {width}\nHEIGHT {height}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
        )
        .into_bytes();
        for _ in 0..width * height {
            v.extend_from_slice(&px);
        }
        v
    }

    /// These are threaded handoffs, so the assertion is "reaches this state within a bound", never a fixed sleep.
    fn within<F: Fn() -> bool>(f: F) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn latest_px(state: &SocketState) -> Option<[u8; 4]> {
        state
            .latest
            .lock()
            .unwrap()
            .as_ref()
            .map(|f| f.rgba[0..4].try_into().unwrap())
    }

    /// Nothing becomes visible for a bounded window. A negative observation, not a proof of never -- but the accepting
    /// path lands in single-digit milliseconds, so a frame that has not appeared in 300ms was rejected, not merely slow.
    ///
    /// It has to be observed WHILE THE CLIENT IS STILL CONNECTED: disconnecting is itself defined to clear the frame.
    fn stays_empty(state: &SocketState) -> bool {
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            if state.latest.lock().unwrap().is_some() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    /// The server closed the connection: the client reads EOF, or a reset when the server left bytes of ours unread
    /// (bounded, so a server that keeps reading fails cleanly instead of hanging the test).
    fn closed_by_server(client: &mut UnixStream) -> bool {
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut sink = [0u8; 1];
        match client.read(&mut sink) {
            Ok(0) => true,
            Err(e) => e.kind() == ErrorKind::ConnectionReset,
            Ok(_) => false,
        }
    }

    // ── DISPLAY-1 ────────────────────────────────────────────────────────────────────────────

    #[test]
    fn an_image_is_accepted_and_kept_premultiplied() {
        let state = with_output();
        let (mut client, h) = serve(&state);

        client.write_all(&image(4, 2, [255, 0, 0, 128])).unwrap();
        assert!(
            within(|| latest_px(&state) == Some([128, 0, 0, 128])),
            "frame was never accepted (or not premultiplied)"
        );
        let held = state.latest.lock().unwrap();
        let f = held.as_ref().unwrap();
        assert_eq!((f.width, f.height), (4, 2));
        assert_eq!(f.rgba.len(), 4 * 2 * 4, "kept as w*h*4 premultiplied RGBA");
        drop(held);

        drop(client);
        h.join().unwrap();
    }

    #[test]
    fn an_rgb_image_is_accepted_as_opaque() {
        let state = with_output();
        let (mut client, h) = serve(&state);
        let mut v = b"P7\nWIDTH 2\nHEIGHT 1\nDEPTH 3\nMAXVAL 255\nTUPLTYPE RGB\nENDHDR\n".to_vec();
        v.extend_from_slice(&[9, 8, 7, 9, 8, 7]);
        client.write_all(&v).unwrap();
        assert!(within(|| latest_px(&state) == Some([9, 8, 7, 255])));
        drop(client);
        h.join().unwrap();
    }

    // Any accepted size is kept; fitting it to the output is the paint path's job (`fit`), so a size that differs from
    // the output's is no reason to drop it any more.
    #[test]
    fn images_of_any_size_are_kept_and_the_newest_wins() {
        let state = with_output();
        let (mut client, h) = serve(&state);
        client.write_all(&image(8, 2, [1, 1, 1, 255])).unwrap();
        assert!(within(|| latest_px(&state) == Some([1, 1, 1, 255])));
        client.write_all(&image(3, 5, [2, 2, 2, 255])).unwrap();
        assert!(within(|| latest_px(&state) == Some([2, 2, 2, 255])));
        let held = state.latest.lock().unwrap();
        assert_eq!(held.as_ref().map(|f| (f.width, f.height)), Some((3, 5)));
        drop(held);
        drop(client);
        h.join().unwrap();
    }

    // The kiosk must fall back to the built-in clock the moment the streaming client goes away -- never keep painting a
    // frozen last frame on a locked machine.
    #[test]
    fn a_disconnect_clears_the_frame_so_the_kiosk_falls_back_to_the_clock() {
        let state = with_output();
        let (mut client, h) = serve(&state);

        client
            .write_all(&image(4, 2, [0x11, 0x11, 0x11, 255]))
            .unwrap();
        assert!(within(|| latest_px(&state).is_some()));

        drop(client);
        h.join().unwrap();
        assert!(
            state.latest.lock().unwrap().is_none(),
            "stale frame survived the disconnect"
        );
    }

    #[test]
    fn no_frame_ever_sent_means_no_frame_to_show() {
        let state = with_output();
        let (client, h) = serve(&state);
        drop(client);
        h.join().unwrap();
        assert!(state.latest.lock().unwrap().is_none());
    }

    // Before the compositor has described the kiosk output (or with none configured) there is nothing to show a frame
    // on: frames are read and discarded, and the connection is kept for when there is.
    #[test]
    fn frames_without_a_kiosk_output_are_discarded_and_the_connection_kept() {
        let state = SocketState::new();
        let (mut client, h) = serve(&state);

        client.write_all(&image(4, 2, [0x44, 0, 0, 255])).unwrap();
        assert!(
            stays_empty(&state),
            "a frame was kept while there was no kiosk output"
        );

        state.set_kiosk_output(true);
        client.write_all(&image(4, 2, [0x45, 0, 0, 255])).unwrap();
        assert!(
            within(|| latest_px(&state) == Some([0x45, 0, 0, 255])),
            "the connection did not survive the discarded frame"
        );

        // Losing the kiosk output drops the held frame at once.
        state.set_kiosk_output(false);
        assert!(state.latest.lock().unwrap().is_none());

        drop(client);
        h.join().unwrap();
    }

    // ── DISPLAY-4: what closes the connection ────────────────────────────────────────────────

    #[test]
    fn a_refused_header_closes_the_connection() {
        let state = with_output();
        let (mut client, h) = serve(&state);
        client
            .write_all(b"P7\nWIDTH 1\nHEIGHT 1\nDEPTH 1\nMAXVAL 255\nTUPLTYPE GRAYSCALE\nENDHDR\n")
            .unwrap();
        assert!(closed_by_server(&mut client));
        h.join().unwrap();
        assert!(state.latest.lock().unwrap().is_none());
    }

    // The header alone has to end the connection, WITHOUT the raster ever being read: asserted as "the client observes
    // EOF while it is still connected and has sent no raster". Sending a header and checking the handler exited could
    // not fail: without the limit the handler would block reading the raster and the test would HANG.
    #[test]
    fn an_oversized_image_closes_the_connection_on_the_header_alone() {
        let state = with_output();
        let (mut client, h) = serve(&state);
        client
            .write_all(
                b"P7\nWIDTH 8193\nHEIGHT 8\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n",
            )
            .unwrap();
        assert!(
            closed_by_server(&mut client),
            "the server should have closed on the header alone; it is still reading the raster"
        );
        h.join().unwrap();
    }

    #[test]
    fn a_header_past_one_kib_closes_the_connection() {
        let state = with_output();
        let (mut client, h) = serve(&state);
        let mut v = b"P7\n".to_vec();
        while v.len() < 2048 {
            v.extend_from_slice(b"# a very long comment that never ends the header\n");
        }
        client.write_all(&v).unwrap();
        assert!(closed_by_server(&mut client));
        h.join().unwrap();
    }

    // A stream that ends mid-image cannot be resynchronized, so it is treated exactly like a disconnect -- and must not
    // leave a half-read buffer on screen.
    #[test]
    fn a_truncated_image_is_treated_as_a_disconnect() {
        let state = with_output();
        let (mut client, h) = serve(&state);
        let full = image(4, 2, [0x55, 0x55, 0x55, 255]);
        client.write_all(&full[..full.len() - 4]).unwrap();
        drop(client);
        h.join().unwrap();
        assert!(
            state.latest.lock().unwrap().is_none(),
            "a partial buffer must never be shown"
        );
    }

    // An image that has started but stops arriving closes the connection at the deadline (10 s in production; short
    // here). The client is still connected and simply sends nothing more.
    #[test]
    fn a_stalled_image_closes_the_connection_at_the_deadline() {
        let state = with_output();
        let (mut client, h) = serve_with(&state, Duration::from_millis(200));
        let full = image(4, 2, [0x66, 0, 0, 255]);
        let started = Instant::now();
        client.write_all(&full[..full.len() / 2]).unwrap();
        assert!(
            closed_by_server(&mut client),
            "a stalled image was waited on"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(200),
            "closed before the deadline"
        );
        h.join().unwrap();
        assert!(state.latest.lock().unwrap().is_none());
    }

    // One deadline for the whole image, not per read: steady progress one byte at a time is still let go at the deadline.
    #[test]
    fn a_trickling_image_is_let_go_at_one_deadline() {
        let state = with_output();
        let (client, h) = serve_with(&state, Duration::from_millis(300));
        let started = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        let (mut writer, halt) = (client.try_clone().unwrap(), Arc::clone(&stop));
        let trickle = std::thread::spawn(move || {
            for b in image(64, 64, [1, 2, 3, 255]) {
                if halt.load(Ordering::SeqCst) || writer.write_all(&[b]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        // Bounded, so a per-read timeout (which this trickle never trips) fails the test instead of hanging it.
        let let_go = within(|| h.is_finished());
        let held = started.elapsed();
        stop.store(true, Ordering::SeqCst);
        trickle.join().unwrap();
        assert!(
            let_go && held < Duration::from_secs(2),
            "one byte every 20 ms held the connection for {held:?}"
        );
        h.join().unwrap();
        drop(client);
        assert!(state.latest.lock().unwrap().is_none());
    }

    // Quiet BETWEEN images is not a stall: a client may send one frame and the next much later.
    #[test]
    fn a_quiet_client_between_images_is_kept() {
        let state = with_output();
        let (mut client, h) = serve_with(&state, Duration::from_millis(100));
        client.write_all(&image(2, 2, [7, 0, 0, 255])).unwrap();
        assert!(within(|| latest_px(&state) == Some([7, 0, 0, 255])));
        std::thread::sleep(Duration::from_millis(400));
        client.write_all(&image(2, 2, [8, 0, 0, 255])).unwrap();
        assert!(
            within(|| latest_px(&state) == Some([8, 0, 0, 255])),
            "the connection was closed while the client was quiet between images"
        );
        drop(client);
        h.join().unwrap();
    }

    // ── the listener: no greeting, same user, one client at a time ─────────────────────────────

    fn listening() -> (PathBuf, Arc<SocketState>) {
        let dir = std::env::temp_dir().join(format!(
            "clck-socket-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clck.sock");
        let state = with_output();
        spawn(path.clone(), Arc::clone(&state));
        assert!(
            within(|| UnixStream::connect(&path).is_ok()),
            "never listened"
        );
        // that probe connection was a client too; let it go before the test's own
        assert!(within(|| state.active_stream.lock().unwrap().is_some()));
        (path, state)
    }

    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn the_server_says_nothing_and_a_same_user_client_is_admitted() {
        let (path, state) = listening();
        let mut client = UnixStream::connect(&path).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut sink = [0u8; 1];
        match client.read(&mut sink) {
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            other => panic!("the server wrote or closed instead of staying silent: {other:?}"),
        }
        client.write_all(&image(2, 2, [3, 4, 5, 255])).unwrap();
        assert!(within(|| latest_px(&state) == Some([3, 4, 5, 255])));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_peer_uid_is_the_kernels_and_is_this_user_for_our_own_socket() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&a).unwrap(), own_uid());
    }

    // A new client replaces the previous one, which is shut down; the newest one's frames are the ones shown.
    #[test]
    fn the_latest_client_wins_and_the_previous_one_is_closed() {
        let (path, state) = listening();
        let mut first = UnixStream::connect(&path).unwrap();
        first.write_all(&image(2, 2, [1, 0, 0, 255])).unwrap();
        assert!(within(|| latest_px(&state) == Some([1, 0, 0, 255])));

        let mut second = UnixStream::connect(&path).unwrap();
        assert!(closed_by_server(&mut first), "the replaced client was kept");
        second.write_all(&image(2, 2, [2, 0, 0, 255])).unwrap();
        assert!(within(|| latest_px(&state) == Some([2, 0, 0, 255])));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // A superseded connection's handler exits late. It must not clear the frame the NEW connection has already
    // published, which would blink the kiosk back to the clock for no reason.
    #[test]
    fn a_superseded_connection_does_not_clear_the_newer_frame() {
        let state = with_output();

        let (client_old, h_old) = serve(&state);
        // A newer connection takes over the generation while the old handler is still alive.
        state.active_gen.fetch_add(1, Ordering::SeqCst);
        *state.latest.lock().unwrap() = Some(LatestFrame {
            width: 4,
            height: 2,
            rgba: vec![0x66; 4 * 2 * 4],
        });

        drop(client_old);
        h_old.join().unwrap();

        assert_eq!(
            latest_px(&state),
            Some([0x66; 4]),
            "the old connection's exit stomped the live frame"
        );
    }

    // ── fitting a frame to the output ────────────────────────────────────────────────────────

    fn solid(width: u32, height: u32, px: [u8; 4]) -> LatestFrame {
        LatestFrame {
            width,
            height,
            rgba: px.repeat((width * height) as usize),
        }
    }

    fn at(buf: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * width + x) * 4) as usize;
        buf[i..i + 4].try_into().unwrap()
    }

    const BLACK: [u8; 4] = [0, 0, 0, 255];
    const RED: [u8; 4] = [255, 0, 0, 255];

    #[test]
    fn a_matching_frame_is_shown_one_to_one() {
        let mut f = solid(3, 2, RED);
        f.rgba[4..8].copy_from_slice(&[1, 2, 3, 4]);
        assert_eq!(f.fit(3, 2).unwrap(), f.rgba, "byte for byte");
    }

    #[test]
    fn a_wider_frame_is_letterboxed_centred_on_black() {
        // 4x1 onto 4x4: scale 1, 4x1 band in rows 1..2 (offset floor(3/2) = 1)
        let out = solid(4, 1, RED).fit(4, 4).unwrap();
        assert_eq!(out.len(), 4 * 4 * 4);
        for x in 0..4 {
            assert_eq!(at(&out, 4, x, 0), BLACK);
            assert_eq!(at(&out, 4, x, 1), RED);
            assert_eq!(at(&out, 4, x, 2), BLACK);
            assert_eq!(at(&out, 4, x, 3), BLACK);
        }
    }

    #[test]
    fn a_taller_frame_is_pillarboxed_and_scaled_up() {
        // 1x2 onto 6x4: scale 2, a 2x4 column at x 2..4
        let out = solid(1, 2, RED).fit(6, 4).unwrap();
        for y in 0..4 {
            assert_eq!(at(&out, 6, 0, y), BLACK);
            assert_eq!(at(&out, 6, 1, y), BLACK);
            assert_eq!(at(&out, 6, 2, y), RED);
            assert_eq!(at(&out, 6, 3, y), RED);
            assert_eq!(at(&out, 6, 4, y), BLACK);
            assert_eq!(at(&out, 6, 5, y), BLACK);
        }
    }

    #[test]
    fn a_larger_frame_is_scaled_down_to_fill_the_same_aspect() {
        // 384x216 onto 192x108 (a 16:9 frame at twice the kiosk's size): no bars at all
        let out = solid(384, 216, RED).fit(192, 108).unwrap();
        assert_eq!(out.len(), 192 * 108 * 4);
        for (x, y) in [(0, 0), (191, 0), (0, 107), (191, 107), (96, 54)] {
            assert_eq!(at(&out, 192, x, y), RED, "({x},{y})");
        }
    }

    #[test]
    fn a_transparent_frame_shows_black_where_it_is_clear() {
        let out = solid(2, 2, [0, 0, 0, 0]).fit(4, 2).unwrap();
        for x in 0..4 {
            assert_eq!(at(&out, 4, x, 0), BLACK);
        }
    }

    // ── DISPLAY-2 ────────────────────────────────────────────────────────────────────────────

    // The socket is display-only, permanently. Asserted against the sources of the socket and of the image decoder it
    // feeds rather than a comment, so that ADDING a path from received bytes to the password buffer, the auth state, or
    // the unlock call fails the build instead of a review.
    //
    // Scoped to the code above `#[cfg(test)]`: these test modules necessarily say these words.
    #[test]
    fn the_socket_module_has_no_route_to_authentication_or_unlock() {
        for (name, source) in [
            ("socket.rs", include_str!("socket.rs")),
            ("netpbm.rs", include_str!("netpbm.rs")),
        ] {
            let code = source.split("#[cfg(test)]").next().unwrap();
            let stripped: String = code
                .lines()
                .map(|l| match l.find("//") {
                    Some(i) => &l[..i],
                    None => l,
                })
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();

            for forbidden in ["unlock", "password", "pam", "auth"] {
                assert!(
                    !stripped.contains(forbidden),
                    "{name} code references `{forbidden}`: the kiosk display socket must never be able to reach \
                     authentication or the unlock call (BEHAVIORS.md DISPLAY-2)"
                );
            }
        }
    }

    // ── resolve_path ─────────────────────────────────────────────────────────────────────────
    //
    // These mutate a process-global env var, so they are one test rather than three racing ones.
    #[test]
    fn resolve_path_prefers_the_configured_value_then_xdg_then_gives_up() {
        let configured = Path::new("/run/custom/kiosk.sock");

        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        assert_eq!(resolve_path(Some(configured)).unwrap(), configured);
        assert_eq!(
            resolve_path(None).unwrap(),
            PathBuf::from("/run/user/1000/clck.sock")
        );

        std::env::remove_var("XDG_RUNTIME_DIR");
        // No socket is a silent degrade to the built-in clock, never a startup failure -- a display feature must not be
        // able to stop the lock from coming up.
        assert!(resolve_path(None).is_none());
        assert_eq!(resolve_path(Some(configured)).unwrap(), configured);
    }
}

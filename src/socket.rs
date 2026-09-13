//! The kiosk display SOCKET SERVER: a content-BLIND Unix-domain frame receiver. Exactly one
//! external client at a time (`nixwatch-frames`, or anything speaking the same wire protocol)
//! streams premultiplied RGBA frames in; the render path (`locker.rs`) blits the latest one onto
//! the KIOSK output and falls back to clck's own built-in clock whenever no frame has ever
//! arrived, the frame's geometry does not match the kiosk output, or the client has disconnected.
//!
//! WIRE PROTOCOL v1 (all integers little-endian):
//!   - on connect, this server immediately writes a HELLO: 8-byte magic `b"CLCK1"`, then
//!     `width: u32`, `height: u32`, `scale: u32` -- the kiosk output's current geometry. `0,0,0`
//!     means "no kiosk output" (either none is configured on this host, or the compositor has not
//!     sent its first surface `configure` yet -- see `SocketState::geometry`'s own doc).
//!   - the client then sends repeated FRAMES: `width: u32`, `height: u32`, then exactly
//!     `width*height*4` bytes of premultiplied RGBA (row-major, top-down, stride = width*4).
//!   - a frame whose geometry does not match the kiosk output is dropped (never blitted) and the
//!     connection is kept; a genuinely truncated read (the stream itself ended mid-frame) is
//!     treated the same as a clean disconnect, since there is no way to resynchronize a byte
//!     stream without knowing how much of it to discard.
//!
//! DISPLAY-ONLY, PERMANENTLY (BEHAVIORS.md DISPLAY-2): this module only ever turns received bytes
//! into pixels for one output. It has no path to `Locker`'s password buffer, auth state, or the
//! session-lock unlock call -- SESSION-1 stays the only gate, entirely untouched by anything here.
//! A reverse channel (input events client-ward, so a streamed dashboard could itself be
//! interactive) is a plausible future addition -- deliberately NOT built here; it would need its
//! own careful accounting against KIOSK-1 (kiosk surfaces are input-inert) before it could exist
//! at all.
use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// The handshake magic is the deployed wire protocol's, kept byte-identical so today's cksk
// (and yesterday's nixwatch-frames) validate it. Never rename this to match the server:
// the magic names the WIRE, not the program.
pub const MAGIC: &[u8; 8] = b"NIXLOCK1";

/// A frame this large is never legitimate for a real display, at any DPI this framework will ever
/// run on -- guards against a garbled or hostile length prefix turning into a multi-gigabyte
/// allocation. Purely a defensive backstop; the normal rejection path is the geometry check below.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// The most recently fully-received, geometry-valid frame -- or `None` while no client has ever
/// sent one, or after the streaming client disconnects.
pub struct LatestFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Shared state the render path (`locker.rs`) reads and this module's accept/read threads write.
pub struct SocketState {
    pub latest: Mutex<Option<LatestFrame>>,
    /// Current kiosk output geometry (width, height, scale) -- `(0, 0, 0)` means "no kiosk
    /// output": either `kiosk_outputs` is empty (permanent, by construction) or the compositor
    /// has not yet sent the kiosk surface's first `configure` (a narrow startup race -- a client
    /// connecting in that window sees `0,0,0` too and should idle/retry exactly as it would for
    /// the permanent case; see the wire protocol doc above).
    geometry: Mutex<(u32, u32, u32)>,
    /// Set once the caller's calloop event loop exists, so a newly-accepted frame can wake a
    /// repaint immediately instead of waiting for the 1s tick. `None` early at startup is fine --
    /// the periodic tick still repaints eventually (see `locker.rs`'s own timer).
    ping: Mutex<Option<calloop::ping::Ping>>,
    /// Monotonic id of the current connection; a superseded (older) connection's handler thread
    /// checks this and exits rather than going on writing frames nobody will read.
    active_gen: AtomicU64,
    /// A clone of the currently-active stream, kept ONLY so a new connection can `shutdown()` the
    /// previous one -- unblocking its handler thread out of a pending `read` (one client at a
    /// time: a new connection replaces the old, per the protocol doc in `locker.rs`/README).
    active_stream: Mutex<Option<UnixStream>>,
}

impl SocketState {
    pub fn new() -> Arc<Self> {
        Arc::new(SocketState {
            latest: Mutex::new(None),
            geometry: Mutex::new((0, 0, 0)),
            ping: Mutex::new(None),
            active_gen: AtomicU64::new(0),
            active_stream: Mutex::new(None),
        })
    }

    /// Called by `locker.rs` whenever the kiosk output's surface configures (or reconfigures).
    pub fn set_geometry(&self, width: u32, height: u32, scale: u32) {
        *self.geometry.lock().unwrap() = (width, height, scale);
    }

    /// Called once the caller's `EventLoop` and its ping source exist.
    pub fn set_ping(&self, ping: calloop::ping::Ping) {
        *self.ping.lock().unwrap() = Some(ping);
    }
}

/// Resolve the socket path: the configured value, else `$XDG_RUNTIME_DIR/nixlock.sock`. `None`
/// when neither is available -- the caller then skips the socket server entirely. A display
/// feature must never block or weaken the lock itself (DISPLAY-2), so a missing/misconfigured
/// socket is a silent-degrade-to-clock, never a startup failure.
///
/// The filename keeps the previous product name on purpose: deployed cksk clients dial it
/// (with a clck.sock-first fallback), and renaming the default is a deploy step -- the wire
/// (HELLO magic, framing) is versioned separately and does not follow filenames.
pub fn resolve_path(configured: Option<&Path>) -> Option<PathBuf> {
    configured.map(Path::to_path_buf).or_else(|| {
        std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("nixlock.sock"))
    })
}

/// Starts the listener thread. Best-effort: a bind failure is logged and the kiosk stays on the
/// default clock forever (never a reason to fail the lock itself -- see `resolve_path` above).
pub fn spawn(path: PathBuf, state: Arc<SocketState>) {
    std::thread::spawn(move || {
        // Unlink a stale socket from a previous, uncleanly-exited run before binding -- otherwise
        // `bind` fails with AddrInUse against a path nothing is actually listening on any more.
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

fn accept_one(mut stream: UnixStream, state: &Arc<SocketState>) {
    // One client at a time: this connection becomes the new active one, and whatever was active
    // before it gets shut down so its handler thread unblocks out of its read and exits cleanly.
    let my_gen = state.active_gen.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(old) = state.active_stream.lock().unwrap().take() {
        let _ = old.shutdown(Shutdown::Both);
    }

    let (w, h, scale) = *state.geometry.lock().unwrap();
    if let Err(e) = write_hello(&mut stream, w, h, scale) {
        eprintln!("clck: socket: HELLO write failed: {e}");
        return;
    }
    eprintln!("clck: socket: client connected, geometry {w}x{h}");

    let reader = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("clck: socket: stream clone failed: {e}");
            return;
        }
    };
    *state.active_stream.lock().unwrap() = Some(stream);

    let state = Arc::clone(state);
    std::thread::spawn(move || read_frames(reader, my_gen, state));
}

fn write_hello(
    stream: &mut UnixStream,
    width: u32,
    height: u32,
    scale: u32,
) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(8 + 12);
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.extend_from_slice(&scale.to_le_bytes());
    stream.write_all(&buf)
}

fn read_frames(mut stream: UnixStream, my_gen: u64, state: Arc<SocketState>) {
    let mut header = [0u8; 8];
    loop {
        if state.active_gen.load(Ordering::SeqCst) != my_gen {
            break; // superseded by a newer connection; that thread already owns the display
        }
        if let Err(e) = stream.read_exact(&mut header) {
            if e.kind() != ErrorKind::UnexpectedEof {
                eprintln!("clck: socket: read frame header failed: {e}");
            }
            break; // clean EOF or a real error -- either way, the client is gone
        }
        let width = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let height = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let expect = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(4);
        if expect == 0 || expect > MAX_FRAME_BYTES {
            eprintln!("clck: socket: frame {width}x{height} rejected (implausible size); closing");
            break; // no reliable way to resync the byte stream without a valid length
        }
        let mut payload = vec![0u8; expect];
        if let Err(e) = stream.read_exact(&mut payload) {
            eprintln!("clck: socket: frame {width}x{height} truncated: {e}; closing");
            break;
        }
        let (ew, eh, _) = *state.geometry.lock().unwrap();
        if width != ew || height != eh {
            eprintln!("clck: socket: frame {width}x{height} rejected (kiosk is {ew}x{eh})");
            continue; // FRAME validation: fully read, but dropped -- never blitted, connection kept
        }
        eprintln!("clck: socket: frame {width}x{height} accepted");
        *state.latest.lock().unwrap() = Some(LatestFrame {
            width,
            height,
            rgba: payload,
        });
        if let Some(ping) = state.ping.lock().unwrap().as_ref() {
            ping.ping();
        }
    }
    // Only the still-current generation clears the frame back to the default clock -- a
    // superseded connection's late exit must never stomp the NEW connection's already-live frame.
    if state.active_gen.load(Ordering::SeqCst) == my_gen {
        *state.latest.lock().unwrap() = None;
        eprintln!("clck: socket: client disconnected; kiosk falls back to the default clock");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Drive `read_frames` exactly as production does -- on its own thread, against a real
    /// `UnixStream` pair. Returns the writable client end and a join handle.
    fn serve(state: &Arc<SocketState>) -> (UnixStream, std::thread::JoinHandle<()>) {
        let (client, server) = UnixStream::pair().unwrap();
        let my_gen = state.active_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let st = Arc::clone(state);
        let h = std::thread::spawn(move || read_frames(server, my_gen, st));
        (client, h)
    }

    fn frame(width: u32, height: u32, fill: u8) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&width.to_le_bytes());
        v.extend_from_slice(&height.to_le_bytes());
        v.resize(8 + (width as usize) * (height as usize) * 4, fill);
        v
    }

    /// These are threaded handoffs, so the assertion is "reaches this state within a bound",
    /// never a fixed sleep.
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

    fn latest_fill(state: &SocketState) -> Option<u8> {
        state.latest.lock().unwrap().as_ref().map(|f| f.rgba[0])
    }

    /// Nothing becomes visible for a bounded window. A negative observation, not a proof of never
    /// -- but the accepting path above lands in single-digit milliseconds, so a frame that has not
    /// appeared in 300ms was rejected, not merely slow.
    ///
    /// It has to be observed WHILE THE CLIENT IS STILL CONNECTED. Asserting `latest.is_none()`
    /// after a disconnect proves nothing at all, because disconnecting is itself defined to clear
    /// the frame -- a mutation that deleted the geometry check entirely passed that way.
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

    // ── DISPLAY-1 ────────────────────────────────────────────────────────────────────────────

    #[test]
    fn a_frame_matching_the_kiosk_geometry_is_accepted() {
        let state = SocketState::new();
        state.set_geometry(4, 2, 1);
        let (mut client, h) = serve(&state);

        client.write_all(&frame(4, 2, 0xAB)).unwrap();
        assert!(
            within(|| latest_fill(&state) == Some(0xAB)),
            "frame was never accepted"
        );
        let held = state.latest.lock().unwrap();
        let f = held.as_ref().unwrap();
        assert_eq!((f.width, f.height), (4, 2));
        assert_eq!(f.rgba.len(), 4 * 2 * 4, "payload must be exactly w*h*4");
        drop(held);

        drop(client);
        h.join().unwrap();
    }

    // The kiosk must fall back to the built-in clock the moment the streaming client goes away --
    // never keep painting a frozen last frame on a locked machine.
    #[test]
    fn a_disconnect_clears_the_frame_so_the_kiosk_falls_back_to_the_clock() {
        let state = SocketState::new();
        state.set_geometry(4, 2, 1);
        let (mut client, h) = serve(&state);

        client.write_all(&frame(4, 2, 0x11)).unwrap();
        assert!(within(|| latest_fill(&state) == Some(0x11)));

        drop(client);
        h.join().unwrap();
        assert!(
            state.latest.lock().unwrap().is_none(),
            "stale frame survived the disconnect"
        );
    }

    // A mismatched frame is DROPPED, not fatal: the connection must survive it, which is why the
    // second (matching) frame has to land on the same stream.
    #[test]
    fn a_wrong_geometry_frame_is_dropped_and_the_connection_is_kept() {
        let state = SocketState::new();
        state.set_geometry(4, 2, 1);
        let (mut client, h) = serve(&state);

        // Sent ALONE and checked before anything else is written: sending a good frame after it
        // and inspecting the result cannot distinguish "the bad one was dropped" from "the bad one
        // was accepted and then overwritten".
        client.write_all(&frame(8, 2, 0x22)).unwrap();
        assert!(
            stays_empty(&state),
            "a frame whose geometry does not match the kiosk output was blitted"
        );

        // The drop must not have killed the connection -- the same stream still works.
        client.write_all(&frame(4, 2, 0x33)).unwrap();
        assert!(
            within(|| latest_fill(&state) == Some(0x33)),
            "the connection did not survive a dropped frame"
        );

        drop(client);
        h.join().unwrap();
    }

    #[test]
    fn no_frame_ever_sent_means_no_frame_to_show() {
        let state = SocketState::new();
        state.set_geometry(4, 2, 1);
        let (client, h) = serve(&state);
        drop(client);
        h.join().unwrap();
        assert!(state.latest.lock().unwrap().is_none());
    }

    // A frame that arrives before the compositor has configured the kiosk surface (geometry still
    // 0,0,0) cannot match anything, so it must be dropped rather than blitted at a guessed size.
    #[test]
    fn a_frame_arriving_before_the_kiosk_has_geometry_is_dropped() {
        let state = SocketState::new();
        let (mut client, h) = serve(&state);

        client.write_all(&frame(4, 2, 0x44)).unwrap();
        assert!(
            stays_empty(&state),
            "a frame was blitted at a guessed size before the compositor configured the kiosk"
        );

        drop(client);
        h.join().unwrap();
    }

    // ── defensive length handling ────────────────────────────────────────────────────────────

    // A garbled or hostile length prefix must not become a huge allocation: the header alone has
    // to end the connection, WITHOUT the payload ever being read.
    //
    // Asserted as "the client observes EOF while it is still connected and has sent no payload".
    // The obvious alternative -- send a header and check the handler exited -- cannot fail: if the
    // cap were gone the handler would block in `read_exact` forever and the test would HANG rather
    // than fail. A read timeout turns that same mutation into a clean failure.
    //
    // Sized just past the cap on purpose. At 0xFFFF x 0xFFFF a removed cap aborts the test process
    // on a ~17 GiB reservation, which is also not a legible failure.
    #[test]
    fn an_implausibly_large_frame_closes_the_connection_without_reading_the_payload() {
        const W: u32 = 4097;
        const H: u32 = 4096;
        assert!(
            (W as usize) * (H as usize) * 4 > MAX_FRAME_BYTES,
            "this test is only meaningful above the cap"
        );

        let state = SocketState::new();
        state.set_geometry(4, 2, 1);
        let (mut client, h) = serve(&state);

        let mut header = Vec::new();
        header.extend_from_slice(&W.to_le_bytes());
        header.extend_from_slice(&H.to_le_bytes());
        client.write_all(&header).unwrap();

        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut sink = [0u8; 1];
        assert_eq!(
            client.read(&mut sink).ok(),
            Some(0),
            "the server should have closed on the header alone; it is still reading the payload"
        );

        h.join().unwrap();
        assert!(state.latest.lock().unwrap().is_none());
    }

    #[test]
    fn a_zero_sized_frame_closes_the_connection() {
        let state = SocketState::new();
        state.set_geometry(4, 2, 1);
        let (mut client, h) = serve(&state);
        let _ = client.write_all(&[0u8; 8]);
        h.join().unwrap();
        assert!(state.latest.lock().unwrap().is_none());
    }

    // A stream that ends mid-payload cannot be resynchronized, so it is treated exactly like a
    // disconnect -- and must not leave a half-read buffer on screen.
    #[test]
    fn a_truncated_frame_is_treated_as_a_disconnect() {
        let state = SocketState::new();
        state.set_geometry(4, 2, 1);
        let (mut client, h) = serve(&state);

        let full = frame(4, 2, 0x55);
        client.write_all(&full[..full.len() - 4]).unwrap();
        drop(client);

        h.join().unwrap();
        assert!(
            state.latest.lock().unwrap().is_none(),
            "a partial buffer must never be shown"
        );
    }

    // ── one client at a time ─────────────────────────────────────────────────────────────────

    // A superseded connection's handler exits late. It must not clear the frame the NEW connection
    // has already published, which would blink the kiosk back to the clock for no reason.
    #[test]
    fn a_superseded_connection_does_not_clear_the_newer_frame() {
        let state = SocketState::new();
        state.set_geometry(4, 2, 1);

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
            latest_fill(&state),
            Some(0x66),
            "the old connection's exit stomped the live frame"
        );
    }

    // ── HELLO wire format ────────────────────────────────────────────────────────────────────

    #[test]
    fn hello_is_the_magic_then_three_little_endian_u32s() {
        let (mut server, mut client) = UnixStream::pair().unwrap();
        write_hello(&mut server, 3840, 2160, 2).unwrap();
        drop(server);

        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();

        assert_eq!(got.len(), 20, "HELLO is 8 magic + 3 u32");
        assert_eq!(&got[0..8], MAGIC);
        assert_eq!(u32::from_le_bytes(got[8..12].try_into().unwrap()), 3840);
        assert_eq!(u32::from_le_bytes(got[12..16].try_into().unwrap()), 2160);
        assert_eq!(u32::from_le_bytes(got[16..20].try_into().unwrap()), 2);
    }

    // `0,0,0` is the documented "no kiosk output" answer a client must be able to distinguish.
    #[test]
    fn hello_reports_zero_geometry_when_there_is_no_kiosk_output() {
        let (mut server, mut client) = UnixStream::pair().unwrap();
        let (w, h, s) = *SocketState::new().geometry.lock().unwrap();
        write_hello(&mut server, w, h, s).unwrap();
        drop(server);

        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert_eq!(&got[8..20], &[0u8; 12]);
    }

    // ── DISPLAY-2 ────────────────────────────────────────────────────────────────────────────

    // The socket is display-only, permanently. Asserted against the module's own source rather
    // than a comment, so that ADDING a path from received bytes to the password buffer, the auth
    // state, or the unlock call fails the build instead of a review.
    //
    // Scoped to the code above `#[cfg(test)]`: this test module necessarily says these words.
    #[test]
    fn the_socket_module_has_no_route_to_authentication_or_unlock() {
        let source = include_str!("socket.rs");
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
                "socket.rs code references `{forbidden}`: the kiosk display socket must never be \
                 able to reach authentication or the unlock call (BEHAVIORS.md DISPLAY-2)"
            );
        }
    }

    // ── resolve_path ─────────────────────────────────────────────────────────────────────────
    //
    // These mutate a process-global env var, so they are one test rather than three racing ones.
    #[test]
    fn resolve_path_prefers_the_configured_value_then_xdg_then_gives_up() {
        let configured = Path::new("/run/custom/nixlock.sock");

        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        assert_eq!(resolve_path(Some(configured)).unwrap(), configured);
        assert_eq!(
            resolve_path(None).unwrap(),
            PathBuf::from("/run/user/1000/nixlock.sock")
        );

        std::env::remove_var("XDG_RUNTIME_DIR");
        // No socket is a silent degrade to the built-in clock, never a startup failure -- a
        // display feature must not be able to stop the lock from coming up.
        assert!(resolve_path(None).is_none());
        assert_eq!(resolve_path(Some(configured)).unwrap(), configured);
    }
}

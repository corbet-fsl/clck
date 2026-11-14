# clck

A Wayland session locker that keeps kiosk outputs live while locking the rest, with PAM
unlock. A kiosk output shows whatever a client (e.g. cksk) streams over the display Unix
socket -- or the built-in clock until a frame arrives. No nix required.

```console
$ cargo build --release
$ ./target/release/clck --help
```

Config: `$XDG_CONFIG_HOME/clck/config.json` (`kiosk_outputs`, `pam_service`,
`socket_path`, `debug`, optional `fingerprint_pam_service`), previous `nixlock` location as
migration fallback. `fingerprint_pam_service` names a dedicated `clck-*` PAM service, a
root-owned file such as `/etc/pam.d/clck-fingerprint` holding exactly
`auth required pam_fprintd.so max-tries=1 timeout=30`; it is tried in parallel with the password
(BEHAVIORS.md SESSION-2). Unset, clck is password-only.

## Frame input: Netpbm PAM over a Unix socket

A kiosk output shows the images a client writes to clck's Unix socket: `socket_path`, default
`$XDG_RUNTIME_DIR/clck.sock`, while the lock is held; `live_socket_path`, default
`$XDG_RUNTIME_DIR/clck-live.sock`, for `clck kioskd` (below). clck sends nothing on the socket: a
client connects and writes a stream of [Netpbm PAM](https://netpbm.sourceforge.net/doc/pam.html)
images, one after another, for as long as it likes.

- Each image is a `P7` header (`WIDTH`, `HEIGHT`, `DEPTH`, `MAXVAL`, `TUPLTYPE` lines in any order,
  `#` comment lines allowed, `ENDHDR` last), then exactly `WIDTH * HEIGHT * DEPTH` bytes, row-major,
  top-down.
- Accepted: `TUPLTYPE RGB_ALPHA` with `DEPTH 4` (straight alpha, not premultiplied) or
  `TUPLTYPE RGB` with `DEPTH 3`, both with `MAXVAL 255`.
- Limits: the header is at most 1 KiB; `WIDTH` and `HEIGHT` are each 1 to 8192; once an image's
  first byte has arrived, the whole image must arrive within 10 s. Anything else closes the
  connection.
- Same user only: a client whose user id (from the kernel, `SO_PEERCRED`) is not clck's own is
  closed at once.
- One client at a time: a new connection replaces the previous one. When the client disconnects,
  the kiosk goes back to the built-in clock.
- Scaling: an image the size of the kiosk output is shown 1:1; any other size is scaled to fit,
  aspect ratio preserved, centred on black. While there is no kiosk output yet, images are read
  and discarded.

Any PAM writer works, netpbm's own tools included; the connection must stay open while the image
should stay on screen:

```console
$ { pngtopam -alphapam board.png; sleep infinity; } | socat -u - UNIX-CONNECT:"$XDG_RUNTIME_DIR/clck-live.sock"
```

The contract is `DISPLAY-1`, `DISPLAY-2` and `DISPLAY-4` in `BEHAVIORS.md`. cksk is the client that
streams the status boards.

## The kiosk without a lock

`clck kioskd` is a separate small process (run it as a user service) that shows the kiosk content on the `kiosk_outputs`
through an overlay layer surface while the state is `on`, and nothing otherwise. The one dumb command:

```console
$ clck kiosk on      # {"ok":true,"kiosk":"on","outputs":["DP-3"]}
$ clck kiosk off
$ clck kiosk status
```

It holds no session lock, binds no keyboard or pointer, and shares no code with the authentication (`KIOSKD-1`..`5` in
`BEHAVIORS.md`). It decides nothing: whoever knows when the seat is idle or away sends the command. Frame clients (cksk)
stream to its own `live_socket_path`, never to the locker's socket.

License: FSL-1.1-ALv2, see `LICENSE.md`.

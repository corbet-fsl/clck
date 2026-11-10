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
Wire protocol: `BEHAVIORS.md` (DISPLAY-1/DISPLAY-2).

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

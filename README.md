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
migration fallback. `fingerprint_pam_service` names a dedicated PAM service (for example
`/etc/pam.d/clck-fingerprint`: `auth required pam_fprintd.so max-tries=1 timeout=30`) that is
tried in parallel with the password; unset, clck is password-only (BEHAVIORS.md SESSION-2).
Wire protocol: `BEHAVIORS.md` (DISPLAY-1/DISPLAY-2).

License: FSL-1.1-ALv2, see `LICENSE.md`.

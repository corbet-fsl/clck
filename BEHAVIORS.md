# nixlock — behaviour contract
Review this INSTEAD of the code. Every entry is decidable from outside the process: a situation, an
observable outcome, the failure it prevents, and the boundary next to it.

**Why one repo, and one sentence.** nixlock is a session locker whose outputs do not all play the
same role: designated **kiosk** outputs stay a live, CPU-rendered display (a dashboard, or the
shipped clock) with no password affordance, while every other **Session** output is a PAM lock
screen. The security is `ext-session-lock`'s (the compositor enforces the lock); nixlock's job is to
split outputs by role without ever weakening that lock. Almost every entry below is a way of saying
one thing: *a kiosk display, a dashboard failure, a locker crash, or a swaylock-shaped CLI must never
be a way past the lock.*

Ids are machine-read (`### <ID> — <title>`, id is the first token) and stable. Do not renumber.

## Locking — the compositor holds the guarantee, not us
### LOCK-1 — no `ext-session-lock` ⇒ fail loud, never a half-locked desktop
**GIVEN** a compositor that does not advertise `ext_session_lock_manager_v1`, **THEN** nixlock exits
non-zero before painting anything, names the missing protocol, and does NOT fall back to a
layer-shell / top-layer surface that merely looks like a lock.

Why: a "lock" that is just a fullscreen window is bypassable -- another surface can take focus, the
compositor can be told to close it, a crash reveals the desktop. `ext-session-lock` is the only thing
that makes the lock a compositor-*enforced* state. The most dangerous outcome is the plausible one: a
best-effort overlay that is pixel-identical to a real lock and is not one. Refusing is the safe answer.

Not: nixlock never implements its own locking via keyboard-grab + a top layer as a "better than
nothing" fallback. Nothing-that-says-so beats something-that-lies.

### LOCK-2 — the locker dying leaves the session LOCKED
**GIVEN** nixlock crashes, is killed, or its render loop panics while the session is locked, **THEN**
the compositor keeps every output locked (ext-session-lock's defined behaviour when the lock client
dies without a clean unlock) and the session is not revealed.

Why: the guarantee must not depend on the locker staying alive. The protocol hands the compositor the
fail-secure role precisely so a bug in the locker cannot open the screen -- a locker is the one
program whose *crash* must be safe. This is only true if nixlock never takes an action that would let
its own death unlock the screen.

Not: nixlock sends the compositor's unlock request on exactly one path -- a PAM success from the
password service, or (when configured) from the fingerprint service (`SESSION-1`, `SESSION-2`) --
and on no other: not a signal, not a timeout, not exit, not a panic handler, not a fingerprint
child that merely ended.

### LOCK-3 — every output advertised while locked receives a lock surface
**GIVEN** the compositor confirms the session lock while no outputs exist, or an output is added,
removed, renamed, or rescaled after lock acquisition, **THEN** nixlock creates, updates, or destroys
the corresponding lock surface immediately. A later Session output shows the password screen and a
later Kiosk output stays input-inert; neither can become an invisible locked output.

Why: lid-close can remove the internal panel before `before-sleep` acquires the lock, and dock
outputs may not return until resume. Snapshotting outputs only once at lock acquisition leaves the
compositor correctly locked but with no client surface to paint or focus, so the user sees neither a
password field nor the unlocked desktop and has no normal way to authenticate.

Not: a zero-output lock is not an error and must not unlock or delay suspend. nixlock holds the
compositor lock, watches the output lifecycle, and paints each output whenever it appears.

## Kiosk — a live display that is never an escape
### KIOSK-1 — kiosk surfaces are input-inert; keys never reach the session
**GIVEN** a kiosk output rendering a live dashboard, **THEN** keyboard and pointer input over that
output is consumed by the lock and delivered to neither the dashboard content nor any session client,
and a kiosk output presents no password field and no text entry at all.

Why: the kiosk role is a *display*, not a window. Its whole reason to exist is a live screen with no
auth affordance -- so if input reached the content it would be an interactive program running on a
locked machine, which is exactly the escape the lock exists to prevent. A dashboard on a locked box
must be look-only.

Not: the kiosk never receives input to "interact with the dashboard." It renders; it does not listen.

### KIOSK-2 — kiosk content failure ⇒ that output falls back to the lock screen
**GIVEN** a kiosk output's content panics, produces no frame, or (the out-of-process path -- see
`DISPLAY-1`) its streaming client sends nothing usable, **THEN** that output reverts to a known-safe
fallback (nixlock's own clock, or -- the deeper safety net, a paint result whose byte count still
disagrees with the surface after that -- the Session lock screen), the other outputs are unaffected,
and the session stays locked.

Why: a kiosk that fails must degrade toward MORE security (a lock screen), never toward less -- never a
blank output that could read as "monitor off," never a frozen last frame, never a torn buffer. The
failure of an optional dashboard can weaken nothing.

Not: a failed kiosk never unlocks, never reveals the desktop underneath, and never takes the rest of
the lock down with it.

## Kiosk display socket — content-blind, and never a second way in
### DISPLAY-1 — a kiosk output shows the latest streamed frame, or the default clock
**GIVEN** a `Kiosk` output, **THEN** it displays the most recently accepted frame from the kiosk
display socket if one exists and its geometry matches the output exactly, and otherwise -- no
client has ever connected, no frame has arrived yet, the geometry does not match, or the client has
disconnected -- shows nixlock's own built-in clock. Never a blank output, a frozen last frame past a
disconnect, or a torn/partial buffer.

Why: this generalizes `KIOSK-2` to a client that lives in a whole separate process nixlock does not
control at all. A dashboard process can crash, hang, or simply not have started yet; the kiosk
output's failure mode must still be "falls back to something known-safe," exactly as an in-process
content failure already had to.

Not: nixlock does not retain or replay the last frame from a client that is no longer connected --
a disconnect clears back to the clock immediately, not on some later timeout.

### DISPLAY-2 — the socket is display-only; it can never unlock or accept input
**GIVEN** any byte nixlock reads off the kiosk display socket, **THEN** it is used for exactly one
purpose -- validated, then blitted as pixels to the `Kiosk` output -- and never reaches the
password buffer, the auth state machine, or the compositor's unlock call. `SESSION-1` remains the
only gate, entirely unaware the socket exists.

Why: a second channel that can influence unlock, even indirectly, would BE the lock's weak point --
so the socket protocol is deliberately one-directional and content-blind: nixlock parses only a
width/height header to size and validate a frame, never interprets the pixels themselves.

Not: nixlock never executes, evaluates, or forwards anything a kiosk client sends. A reverse channel
(input events flowing client-ward, so a streamed dashboard could itself be interactive) is a
plausible future addition and is deliberately NOT built here -- it would need its own accounting
against `KIOSK-1` before it could exist at all.

### DISPLAY-3 — only the accepted locker may publish the kiosk socket
**GIVEN** a second nixlock process starts while the compositor is already locked by a live nixlock
process, **THEN** the compositor rejects the duplicate before it unlinks, replaces, or otherwise
touches the active locker's kiosk socket. The original socket remains reachable and its dashboard
client keeps streaming.

Why: swayidle can invoke the lock command again while a persistent locker already owns the session
lock. Unix socket cleanup is destructive: unlinking the pathname does not stop the original
listener, but makes it unreachable; when the rejected duplicate exits, clients see a dead socket
pathname and `ECONNREFUSED` even though the original process is healthy.

Not: merely requesting an `ext-session-lock` is not ownership. nixlock resolves the configured
socket path during startup but does not unlink or bind it until the compositor emits the confirmed
`locked` event. A rejected process exits through `finished` without publishing a socket.

## Kiosk without a lock — `clck kioskd`, a display that is never a second locker
### KIOSKD-1 — the lock-less kiosk shares nothing with the lock path
**GIVEN** `clck kioskd` running and any command on its control socket, **THEN** nothing it receives can reach password
verification, the fingerprint path, the session lock or the compositor's unlock call: the daemon holds no session lock, binds
no seat (no keyboard, no pointer), and its source (`src/kioskd.rs`, `src/control.rs`) names none of them (a test reads both
files). `kiosk off` removes an overlay and nothing else; it never unlocks. `kiosk on` never locks. While the session is locked
the compositor shows only the lock surfaces, so the overlay is simply not visible then.

Why: Julian's desk wants the status boards on one monitor whenever the seat is not in use, without locking the seat. That must
not become a second, weaker way to unlock (or to lock) the session. Separate process, separate code path, no shared state.

Not: the daemon never decides WHEN to show the kiosk: no presence logic, no timer policy. A sensor or the human sends the
command.

### KIOSKD-2 — only named kiosk outputs, input-inert, over the desktop but pushing nothing aside
**GIVEN** `kiosk on`, **THEN** a surface exists exactly on each output whose name equals a configured `kiosk_outputs` entry
(the locker's exact-name rule, `role_for`), on the overlay layer, anchored to all edges, exclusive zone -1, keyboard
interactivity none, with an empty input region (pointer events pass through to what is below). With no kiosk output configured
`kiosk on` is refused.

### KIOSKD-3 — a dumb, bounded, private control socket
**GIVEN** the control socket (`$XDG_RUNTIME_DIR/clck-control.sock`, mode 0600), **THEN** it accepts one JSON line per
connection: `{"cmd":"kiosk","on":true|false}` or `{"cmd":"status"}`, at most 256 bytes, read within 250 ms; everything else (an
unknown command, an unknown field, a missing `on`, invalid JSON, an oversized line) is answered with an error and changes
nothing. A live socket of another daemon is never taken over, a stale one is replaced, a file that is not a socket is never
removed.

### KIOSKD-4 — its own frame socket, never the locker's
**GIVEN** a frame client (e.g. cksk), **THEN** it streams to the daemon's OWN live socket (`live_socket_path`, default
`$XDG_RUNTIME_DIR/clck-live.sock`) over the same content-blind wire as `DISPLAY-1`/`DISPLAY-2`; the daemon refuses to start
when that path is the locker's `socket_path`, so `DISPLAY-3` (only the accepted locker publishes ITS socket) is untouched. The
latest valid frame is painted while the kiosk is on, else the built-in clock.

### KIOSKD-5 — the locker's command line is unchanged
**GIVEN** `clck` with any arguments other than a FIRST argument `kioskd` or `kiosk`, **THEN** it behaves exactly as before
(unknown words ignored, `-f`, `--check-auth`). `kioskd` and `kiosk` are dispatched before the locker's argument loop and never
reach it.

## Authentication — the credential and the conversation
### AUTH-1 — the password lives only in a zeroizing buffer
**GIVEN** a user typing a password, **THEN** the bytes land in a buffer that is zeroized on drop, are
never written to a log, file, or status, never appear in an error message, and do not survive a failed
attempt.

Why: a lock screen holds the plaintext credential every few seconds, and the one place it must never
leak is the one it is most tempted to -- a debug line, an `auth failed for "<input>"` message, a
retained "last try." The zeroizing buffer is the whole defence; an error string that interpolates the
attempt defeats it silently.

Not: nixlock keeps no password history and holds no cleartext beyond the single in-flight attempt.

### AUTH-2 — PAM runs off the render loop; the display never stalls during auth
**GIVEN** a submitted password while a kiosk dashboard (or the default clock) is animating, **THEN**
the PAM conversation runs without blocking the frame loop: the clock keeps ticking and the dashboard
keeps updating while authentication is in flight.

Why: `pam_authenticate` can block for seconds -- a slow `unix_chkpwd`, a deliberate fail delay -- and a
lock screen that freezes every visible output on each attempt looks *crashed*, which trains users to
hold the power button to get past it. A kiosk display that stutters on every keypress also stops being
the live panel it was put there to be.

Not: nixlock does not spawn an unbounded worker per keystroke; at most one auth attempt is in flight,
and the render loop owns the frame clock regardless of what auth is doing.

### AUTH-3 — an unknown or missing PAM service fails closed, and names itself
**GIVEN** a `pam_service` that names no configured PAM service on the host, **THEN** nixlock treats
every authentication as a failure -- never a success, never a fallback to `login`/`other` -- and its
diagnostic names the missing service.

Why: a lock that cannot find its own auth policy must DENY, not admit. Silently falling back to another
service authenticates against a policy nobody chose for the lock, and "it let me in because its config
was missing" is the worst possible failure for a lock screen.

Not: nixlock never guesses a service name and never substitutes a default PAM stack for the one it was
told to use.

## Session — the one gate
### SESSION-1 — only PAM success unlocks: the password service, or (when configured) the fingerprint service
**GIVEN** a password submitted on a Session output, **THEN** the session unlocks if and only if PAM
returns success for the password service; a wrong password leaves the session locked, advances a
visible failed-attempt indicator, and re-arms for the next attempt. The only other way in is a PAM
success from the configured fingerprint service (`SESSION-2`).

Why: this is the single gate, and the compositor unlock request is emitted on exactly one condition --
PAM success, from the password service or (when configured) the fingerprint service -- and nothing
adjacent to it: not a keypress count, not an elapsed time, not a received signal. Every other input
path has to be provably incapable of unlocking.

Not: no lockout that bricks the session -- a wrong password is always retryable -- and `SIGUSR1`
re-shows the indicator (`COMPAT-1`) without touching auth state. Its handler stores one flag and
returns; asserted against the handler's own source, so giving it auth state fails the build.

### SESSION-2 — the fingerprint is an optional PARALLEL path that can only add a PAM-gated way in
**GIVEN** `fingerprint_pam_service` is unset (the default), **THEN** nixlock behaves exactly as without
this entry: no child process, no pointer binding, no extra output. **GIVEN** it names a DEDICATED
service -- `clck-<lowercase>`, a root-owned file in `/etc/pam.d` that nobody else can write, that the
locker can read, with an `auth` line, and not `pam_service` -- **THEN** while locked
nixlock keeps at most one fingerprint attempt running: armed when the compositor confirms the lock,
again by key or pointer input when none is running, and by a display coming back (an output that
appears after the lock, a resume from suspend). Each attempt is a separate PAM transaction (auth
only) in a child process -- a re-exec of the locker with an empty environment, only stdio, and a
parent-death signal -- whose conversation answers no prompt. The session unlocks through the same
single unlock call as a correct password if and only if the child of the attempt IN FLIGHT exits 0
AND wrote the exact success marker; the result of any other attempt (killed at its 40 s deadline,
superseded, or reported twice) is ignored. The password field works throughout, and the lock screen
shows nothing new: no prompt, no fingerprint text, no failure indicator.

Why: a finger is convenience, not policy. Stacked into the password service it would be sequential
-- the password field dead until the reader times out, and a missing or broken reader a lockout.
In parallel, a reader that is absent, unenrolled, busy, or slow costs nothing. A child process,
unlike a thread, can be killed while `pam_fprintd` blocks, and keeps the module's code out of the
process that holds the lock and the password buffer (`LOCK-2`). Arming only at lock, input and wake
keeps the reader idle through a long locked night.

Not: never a second factor and never a replacement for the password; never sudo, login, polkit or
the keyring (nixlock uses only its own dedicated service). Never a stock service: the name comes
from the user's own config file, and whatever the service's stack accepts WITHOUT a prompt unlocks
the moment the lock is taken -- so only a `clck-` service the host created for this may be named,
and it should hold the fingerprint module alone (`auth required pam_fprintd.so max-tries=1
timeout=30`; `max-tries=1` makes one rejected finger one attempt, and the timeout must stay under the
locker's 40 s deadline). Not retried without limit: five rejected fingers, three attempts in a row
that end at once (no reader, no fprintd, no enrolled print, fprintd refusing the verify), or three
attempts killed at the deadline turn the path off until the next lock; an attempt that simply timed
out (nobody touched the reader) re-arms only on new input. Never an unlock on a child's exit status
alone, a signal, a timeout, or a missing service: a missing, malformed, unreadable, or stock service
leaves nixlock password-only and is never replaced by PAM's `other` (`AUTH-3`). `pkill -USR1` aimed
at the locker does not cancel an attempt. Not stronger than the reader: a match-on-chip sensor on
Linux has no host-verified secure channel, and someone with physical access to the machine can
attack the sensor itself; the finger is convenience for a lock screen, never a gate for anything
more sensitive.

## Diagnostics — enough evidence without credential material
### DIAG-1 — debug mode identifies the failing stage and never logs the credential
**GIVEN** debug mode is enabled and an unlock is attempted, **THEN** journald receives structured
events for submission, PAM start/finish, delivery to the Wayland event loop, UI failure/backoff or
the compositor unlock request, and process exit. Attempts are correlated by an opaque counter and
PAM failures include only their stage, symbolic result code, and duration. Fingerprint attempts
(`SESSION-2`) are reported by generation, trigger, and symbolic outcome only.

Why: a cleared password field looks the same for a wrong password, an unavailable PAM service, a
stalled event loop, and a failed Wayland flush. Those cases need separate evidence, including an
event emitted directly by the PAM worker before the render loop handles its result.

Not: diagnostics never receive or record password content or length, key symbols, PAM prompt text,
or PAM message text. Debugging must not turn the most credential-sensitive process in the session
into a credential recorder.

## Compatibility — the contract swayidle already speaks
### COMPAT-1 — swaylock-CLI compatible; `-f` daemonizes AFTER the lock is held; stays named `nixlock`
**GIVEN** swayidle invoking `nixlock -f`, **THEN** the process stays named `nixlock` (so
`pkill -USR1 nixlock` reaches it), and `SIGUSR1` is HANDLED: never fatal, and it repaints -- re-showing
the password indicator -- within one repaint tick (<=1s), touching no auth state. The fork happens
before any worker thread is created, but the parent does not return until the compositor confirms
that the child holds the session lock. A startup failure closes that readiness channel and makes the
parent fail rather than claiming an unsecured session is ready.

Why SIGUSR1 is called out as a guarantee and not a nicety: its default disposition is TERMINATE, and
the fleet's swayidle line ends `unlock 'pkill -USR1 nixlock'`. With no handler installed that signal
killed the locker, and by `LOCK-2` the compositor then holds every output locked with no client left
to take a password -- the "lockout that bricks the session" `SESSION-1` forbids, reachable by the one
signal the desk is actually wired to send.
Per-host values it cannot receive on that command line -- kiosk outputs, kiosk command, PAM service,
fingerprint PAM service -- come from `$XDG_CONFIG_HOME/nixlock/config.json`, because swayidle only ever appends `-f`.

Why: the fleet's idle daemon treats the locker as a black box it calls `<cmd> -f` and signals by
process name. Being a faithful drop-in for that contract is what lets nixlock replace swaylock with
zero swayidle changes -- and the ordering (`-f` daemonizes only after the lock is held) is the part
that matters for security, since a daemonize-then-lock locker leaves a window where the session is
already "locked" in the idle daemon's eyes but the screen is not yet covered.

Not: nixlock is not a new idle protocol or a new signalling scheme; it conforms to the existing one,
and the binary's name is part of the contract, not a cosmetic detail.

## Non-goals
- **Not a compositor.** The lock is the compositor's, via `ext-session-lock`; nixlock is the client
  (`LOCK-1`).
- **Not an idle daemon.** swayidle/hypridle own idle timeouts, suspend, and before-sleep locking;
  nixlock is only ever invoked by them.
- **Not a bar or status surface** for the unlocked desktop. The kiosk role is a display *while
  locked*. The one exception is `clck kioskd` (`KIOSKD-*`): a separate, deliberately dumb process that
  shows the same kiosk content on the kiosk outputs on command, without a lock.
- **Not an output/display-layout manager.** nixlock reads connector names; it does not arrange, scale,
  or toggle outputs.
- **Not multi-factor.** A screen lock here is a deliberate low-friction single-factor gate. The
  optional fingerprint (`SESSION-2`) is an alternative to the password, never an additional one.

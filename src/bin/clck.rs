//! The default `clck` binary: a swaylock-compatible clock locker (the shipped `ClockLockScreen`
//! on every `Session` output). A `Kiosk` output shows whatever a client streams over the kiosk
//! display Unix socket (e.g. `nixwatch-frames`) -- see `clck::socket`'s wire protocol -- and
//! the same built-in clock otherwise. Per-host values come from
//! `$XDG_CONFIG_HOME/clck/config.json`, because swayidle only ever appends `-f`.

use clck::{run, run_daemonized, Config};

use serde::Deserialize;

#[derive(serde::Deserialize, Default)]
struct FileCfg {
    #[serde(default)]
    kiosk_outputs: Vec<String>,
    #[serde(default)]
    pam_service: Option<String>,
    #[serde(default)]
    socket_path: Option<String>,
    #[serde(default)]
    debug: bool,
    #[serde(default)]
    fingerprint_pam_service: Option<String>,
    /// `clck kioskd` / `clck kiosk` only (the lock-less kiosk): where the control command connects. Lenient on purpose: a
    /// wrongly typed value is ignored, because a parse error here would drop the LOCKER's own keys (pam_service,
    /// kiosk_outputs) and lock with the wrong PAM service.
    #[serde(default, deserialize_with = "lenient_string")]
    control_socket_path: Option<String>,
    /// `clck kioskd` only: where a frame client streams (never the locker's `socket_path`). Lenient like the above.
    #[serde(default, deserialize_with = "lenient_string")]
    live_socket_path: Option<String>,
}

/// A string, or nothing for any other JSON value.
fn lenient_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) => Some(s),
        _ => None,
    })
}

fn main() {
    // First, before any argument parsing: a hidden fingerprint child (SESSION-2) never returns
    // from here, so it cannot fall through into the locker or the help text.
    clck::fingerprint_child_if_requested();
    if std::env::args().any(|a| a == "--check-auth") {
        return check_auth_mode();
    }
    // The lock-less kiosk: `clck kioskd` (the daemon) and `clck kiosk on|off|status` (its one dumb command). Dispatched
    // before the locker's argument loop, which ignores unknown words and would otherwise start a LOCK.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("kioskd") => return kioskd_mode(),
        Some("kiosk") => return kiosk_mode(&argv[1..]),
        _ => {}
    }
    let mut daemonize = false;
    let mut debug = false;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "-f" | "--daemonize" => daemonize = true,
            "--debug" => debug = true,
            "-h" | "--help" => return help(),
            "-v" | "--version" => return println!("clck {}", env!("CARGO_PKG_VERSION")),
            other => eprintln!("clck: ignoring unknown arg {other}"),
        }
    }
    let mut config = load_config();
    config.debug |= debug;
    let result = if daemonize {
        run_daemonized(config)
    } else {
        run(config)
    };
    if let Err(e) = result {
        eprintln!("clck: fatal: {e}");
        std::process::exit(1);
    }
}

fn read_file_cfg() -> FileCfg {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// `clck kioskd`: the kiosk without a lock (src/kioskd.rs). Runs until killed.
fn kioskd_mode() {
    let file = read_file_cfg();
    let config = clck::kioskd::Config {
        kiosk_outputs: file.kiosk_outputs,
        control_socket: file.control_socket_path.map(std::path::PathBuf::from),
        live_socket: file.live_socket_path.map(std::path::PathBuf::from),
        locker_socket: file.socket_path.map(std::path::PathBuf::from),
    };
    if let Err(e) = clck::kioskd::run(config) {
        eprintln!("clck kioskd: fatal: {e}");
        std::process::exit(1);
    }
}

/// `clck kiosk on|off|status`: the one dumb command (src/control.rs). Exit 0 when the daemon answered ok.
fn kiosk_mode(args: &[String]) {
    use clck::control::{default_path, send, Command};
    let cmd = match args {
        [a] if a == "on" => Command::Kiosk(true),
        [a] if a == "off" => Command::Kiosk(false),
        [a] if a == "status" => Command::Status,
        _ => {
            eprintln!("usage: clck kiosk on|off|status");
            std::process::exit(2);
        }
    };
    let file = read_file_cfg();
    let Some(path) = file
        .control_socket_path
        .map(std::path::PathBuf::from)
        .or_else(default_path)
    else {
        eprintln!("clck: no control socket path (set control_socket_path or XDG_RUNTIME_DIR)");
        std::process::exit(2);
    };
    match send(&path, cmd, std::time::Duration::from_secs(3)) {
        Ok(reply) if reply.ok => {
            println!("{}", reply.line().trim_end());
        }
        Ok(reply) => {
            eprintln!("clck: {}", reply.error.unwrap_or_else(|| "refused".into()));
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("clck: no kioskd answered on {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

/// `clck --check-auth`: read a password from stdin and run the exact PAM path verbosely, to
/// verify a host's PAM service authenticates the current user. Never echoes the password.
fn check_auth_mode() {
    use std::io::Read;
    let cfg = load_config();
    let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());
    let mut pw = String::new();
    std::io::stdin().read_to_string(&mut pw).ok();
    let pw = pw.trim_end_matches(['\n', '\r']).to_string();
    clck::check_auth(&cfg.pam_service, &user, pw);
}

fn config_path() -> String {
    let fresh = std::env::var("XDG_CONFIG_HOME")
        .map(|d| format!("{d}/clck/config.json"))
        .unwrap_or_else(|_| {
            format!(
                "{}/.config/clck/config.json",
                std::env::var("HOME").unwrap_or_default()
            )
        });
    if std::path::Path::new(&fresh).exists() {
        return fresh;
    }
    // Migration fallback: the previous nixlock location keeps working with no flag day.
    std::env::var("XDG_CONFIG_HOME")
        .map(|d| format!("{d}/nixlock/config.json"))
        .unwrap_or_else(|_| {
            format!(
                "{}/.config/nixlock/config.json",
                std::env::var("HOME").unwrap_or_default()
            )
        })
}

fn load_config() -> Config {
    let path = config_path();
    let file: FileCfg = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    Config {
        kiosk_outputs: file.kiosk_outputs,
        pam_service: std::env::var("PAM_SERVICE")
            .ok()
            .or(file.pam_service)
            // Previous product name: /etc/pam.d entries and deploy configs reference it.
            // Renaming the default is a deploy step, not a code rename.
            .unwrap_or_else(|| "nixlock".to_string()),
        username: None,
        socket_path: file.socket_path.map(std::path::PathBuf::from),
        debug: file.debug,
        fingerprint_pam_service: file.fingerprint_pam_service,
    }
}

fn help() {
    println!(
        "clck — a Wayland session locker that keeps kiosk outputs live while locking the rest.\n\
         \n\
         USAGE: clck [-f] [--debug] [-h] [-v]\n\
           -f, --daemonize   return after a child has acquired the session lock\n\
               --debug       log credential-blind lifecycle and PAM result events\n\
           -h, --help        this help\n\
           -v, --version     print version\n\
         \n\
         CONFIG: $XDG_CONFIG_HOME/clck/config.json\n\
           {{ kiosk_outputs: [..], pam_service: \"..\", socket_path: \"..\", debug: false,\n\
             fingerprint_pam_service: null }}\n\
           fingerprint_pam_service (optional): a dedicated `clck-*` PAM service, e.g.\n\
           /etc/pam.d/clck-fingerprint holding exactly\n\
             auth required pam_fprintd.so max-tries=1 timeout=30\n\
           tried in parallel with the password; unset, clck is password-only.\n\
         \n\
         The default binary shows the clock lock screen on every `Session` output. A `Kiosk`\n\
         output shows whatever a client streams over the kiosk display Unix socket (DISPLAY-1) --\n\
         socket_path, default $XDG_RUNTIME_DIR/nixlock.sock -- or the same built-in clock until a\n\
         frame arrives / after the client disconnects. That socket is DISPLAY-ONLY: it can never\n\
         unlock the session (DISPLAY-2); unlock is PAM-only, exactly as on every Session output."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // SESSION-2: a config without the key keeps every host exactly as before.
    #[test]
    fn the_fingerprint_path_is_off_unless_configured() {
        let default: FileCfg = serde_json::from_str(r#"{"pam_service":"swaylock"}"#).unwrap();
        let set: FileCfg =
            serde_json::from_str(r#"{"fingerprint_pam_service":"clck-fingerprint"}"#).unwrap();

        assert_eq!(default.fingerprint_pam_service, None);
        assert_eq!(
            set.fingerprint_pam_service.as_deref(),
            Some("clck-fingerprint")
        );
    }

    #[test]
    fn the_hidden_child_is_dispatched_before_any_other_argument() {
        let main = include_str!("clck.rs")
            .split("fn main()")
            .nth(1)
            .expect("main");
        let child = main
            .find("fingerprint_child_if_requested()")
            .expect("dispatch");
        for later in ["--check-auth", "--help", "--version", "load_config()"] {
            assert!(
                child < main.find(later).expect(later),
                "{later} is parsed first"
            );
        }
    }

    #[test]
    fn the_kiosk_words_are_dispatched_after_the_child_and_before_the_lockers_argument_loop() {
        let main = include_str!("clck.rs")
            .split("fn main()")
            .nth(1)
            .expect("main");
        let child = main.find("fingerprint_child_if_requested()").unwrap();
        let check = main.find("--check-auth").unwrap();
        let kioskd = main.find("Some(\"kioskd\")").unwrap();
        let kiosk = main.find("Some(\"kiosk\")").unwrap();
        let arg_loop = main.find("\"-f\" | \"--daemonize\"").unwrap();
        assert!(
            child < kioskd && check < kioskd,
            "the hidden child and --check-auth keep their place"
        );
        assert!(
            kioskd < arg_loop && kiosk < arg_loop,
            "never reaches the locker's loop"
        );
        // each kiosk branch returns: it cannot fall through into the lock
        assert!(main[kioskd..arg_loop].contains("return kioskd_mode()"));
        assert!(main[kiosk..arg_loop].contains("return kiosk_mode("));
    }

    #[test]
    fn a_wrongly_typed_kiosk_key_cannot_take_the_lockers_keys_with_it() {
        let f: FileCfg = serde_json::from_str(
            r#"{"pam_service":"clck","kiosk_outputs":["DP-3"],"control_socket_path":5,"live_socket_path":["x"]}"#,
        )
        .unwrap();
        assert_eq!(f.pam_service.as_deref(), Some("clck"));
        assert_eq!(f.kiosk_outputs, vec!["DP-3".to_string()]);
        assert!(f.control_socket_path.is_none() && f.live_socket_path.is_none());
        let f: FileCfg = serde_json::from_str(r#"{"live_socket_path":"/run/x.sock"}"#).unwrap();
        assert_eq!(f.live_socket_path.as_deref(), Some("/run/x.sock"));
    }

    #[test]
    fn debug_mode_is_off_by_default_and_explicit_when_enabled() {
        let default: FileCfg = serde_json::from_str("{}").unwrap();
        let enabled: FileCfg = serde_json::from_str(r#"{"debug":true}"#).unwrap();

        assert!(!default.debug);
        assert!(enabled.debug);
    }
}

//! The default `clck` binary: a swaylock-compatible clock locker (the shipped `ClockLockScreen`
//! on every `Session` output). A `Kiosk` output shows whatever a client streams over the kiosk
//! display Unix socket (e.g. `nixwatch-frames`) -- see `clck::socket`'s wire protocol -- and
//! the same built-in clock otherwise. Per-host values come from
//! `$XDG_CONFIG_HOME/clck/config.json`, because swayidle only ever appends `-f`.

use clck::{run, run_daemonized, Config};

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
}

fn main() {
    if std::env::args().any(|a| a == "--check-auth") {
        return check_auth_mode();
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
           {{ kiosk_outputs: [..], pam_service: \"..\", socket_path: \"..\", debug: false }}\n\
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

    #[test]
    fn debug_mode_is_off_by_default_and_explicit_when_enabled() {
        let default: FileCfg = serde_json::from_str("{}").unwrap();
        let enabled: FileCfg = serde_json::from_str(r#"{"debug":true}"#).unwrap();

        assert!(!default.debug);
        assert!(enabled.debug);
    }
}

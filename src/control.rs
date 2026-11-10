//! The kiosk control protocol: ONE dumb command, `kiosk on|off`, and a status read.
//!
//! It exists for `clck kioskd`, the lock-less kiosk (see `crate::kioskd`), and for nothing else: it is not a way to talk to
//! the locker. This module and `kioskd` import nothing of the authentication, the fingerprint path or the session lock
//! (`the_kiosk_side_has_no_path_to_the_lock` below pins that by reading both sources), so no byte that arrives here can
//! reach PAM or the compositor's unlock call. No presence logic lives here either: whoever decides WHEN to show the kiosk
//! (a sensor, a script, the human) just sends the command.
//!
//! Wire: one JSON object per line over a Unix socket (`$XDG_RUNTIME_DIR/clck-control.sock`, mode 0600), one request and one
//! reply per connection.
//!
//! ```text
//! {"cmd":"kiosk","on":true}    -> {"ok":true,"kiosk":"on"}
//! {"cmd":"kiosk","on":false}   -> {"ok":true,"kiosk":"off"}
//! {"cmd":"status"}             -> {"ok":true,"kiosk":"on","outputs":["DP-3"]}
//! anything else                -> {"ok":false,"error":"..."}
//! ```

use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A request longer than this is refused unread (the longest legitimate one is about 30 bytes).
pub const MAX_LINE: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Kiosk(bool),
    Status,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    cmd: String,
    #[serde(default)]
    on: Option<bool>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    TooLong,
    NotJson,
    UnknownCommand(String),
    MissingOn,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::TooLong => write!(f, "request too long"),
            ParseError::NotJson => write!(f, "not a command"),
            ParseError::UnknownCommand(c) => write!(f, "unknown command {c:?}"),
            ParseError::MissingOn => write!(f, "kiosk needs \"on\": true or false"),
        }
    }
}

/// Parse one request line. Everything not exactly one of the three commands is an error, never a guess.
pub fn parse_line(line: &str) -> Result<Command, ParseError> {
    if line.len() > MAX_LINE {
        return Err(ParseError::TooLong);
    }
    let r: Request = serde_json::from_str(line.trim()).map_err(|_| ParseError::NotJson)?;
    match (r.cmd.as_str(), r.on) {
        ("kiosk", Some(on)) => Ok(Command::Kiosk(on)),
        ("kiosk", None) => Err(ParseError::MissingOn),
        ("status", None) => Ok(Command::Status),
        (other, _) => Err(ParseError::UnknownCommand(other.to_string())),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kiosk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Reply {
    pub fn state(on: bool, outputs: Option<Vec<String>>) -> Reply {
        Reply {
            ok: true,
            kiosk: Some(if on { "on" } else { "off" }.into()),
            outputs,
            error: None,
        }
    }
    pub fn err(msg: impl Into<String>) -> Reply {
        Reply {
            ok: false,
            kiosk: None,
            outputs: None,
            error: Some(msg.into()),
        }
    }
    pub fn line(&self) -> String {
        let mut s = serde_json::to_string(self).unwrap_or_else(|_| r#"{"ok":false}"#.into());
        s.push('\n');
        s
    }
}

pub fn request_line(c: Command) -> String {
    match c {
        Command::Kiosk(on) => format!("{{\"cmd\":\"kiosk\",\"on\":{on}}}\n"),
        Command::Status => "{\"cmd\":\"status\"}\n".to_string(),
    }
}

/// `$XDG_RUNTIME_DIR/clck-control.sock`.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join("clck-control.sock"))
}

/// Send one command to a running `clck kioskd` and return its reply. Bounded: a daemon that does not answer in `wait` is an
/// error, not a hang.
pub fn send(path: &Path, c: Command, wait: Duration) -> io::Result<Reply> {
    let mut s = UnixStream::connect(path)?;
    s.set_read_timeout(Some(wait))?;
    s.set_write_timeout(Some(wait))?;
    s.write_all(request_line(c).as_bytes())?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    serde_json::from_str(line.trim())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad reply: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_commands_parse() {
        assert_eq!(
            parse_line(r#"{"cmd":"kiosk","on":true}"#),
            Ok(Command::Kiosk(true))
        );
        assert_eq!(
            parse_line("{\"cmd\":\"kiosk\",\"on\":false}\n"),
            Ok(Command::Kiosk(false))
        );
        assert_eq!(parse_line(r#"{"cmd":"status"}"#), Ok(Command::Status));
    }

    #[test]
    fn everything_else_is_an_error_never_a_guess() {
        assert_eq!(parse_line(r#"{"cmd":"kiosk"}"#), Err(ParseError::MissingOn));
        assert_eq!(
            parse_line(r#"{"cmd":"kiosk","on":"yes"}"#),
            Err(ParseError::NotJson)
        );
        assert_eq!(
            parse_line(r#"{"cmd":"status","on":true}"#),
            Err(ParseError::UnknownCommand("status".into()))
        );
        assert_eq!(parse_line("kiosk on"), Err(ParseError::NotJson));
        assert_eq!(parse_line(""), Err(ParseError::NotJson));
        assert_eq!(
            parse_line(r#"{"cmd":"unlock"}"#),
            Err(ParseError::UnknownCommand("unlock".into()))
        );
        assert_eq!(
            parse_line(r#"{"cmd":"kiosk","on":true,"extra":1}"#),
            Err(ParseError::NotJson),
            "unknown fields are refused"
        );
    }

    #[test]
    fn an_oversized_request_is_refused() {
        let long = format!(r#"{{"cmd":"kiosk","on":true,"pad":"{}"}}"#, "x".repeat(300));
        assert_eq!(parse_line(&long), Err(ParseError::TooLong));
    }

    #[test]
    fn replies_and_requests_round_trip() {
        for c in [Command::Kiosk(true), Command::Kiosk(false), Command::Status] {
            assert_eq!(parse_line(&request_line(c)), Ok(c));
        }
        let r = Reply::state(true, Some(vec!["DP-3".into()]));
        assert_eq!(
            r.line(),
            "{\"ok\":true,\"kiosk\":\"on\",\"outputs\":[\"DP-3\"]}\n"
        );
        assert_eq!(serde_json::from_str::<Reply>(r.line().trim()).unwrap(), r);
        assert_eq!(
            Reply::err("nope").line(),
            "{\"ok\":false,\"error\":\"nope\"}\n"
        );
    }

    /// The kiosk side must have no way to reach the authentication, the fingerprint path or the session lock: it neither
    /// imports them nor names the lock's calls. Read from the sources, like the locker's own structural tests.
    #[test]
    fn the_kiosk_side_has_no_path_to_the_lock() {
        for (name, src) in [
            ("control.rs", include_str!("control.rs")),
            ("kioskd.rs", include_str!("kioskd.rs")),
        ] {
            // Only the production part: the forbidden words appear in this very test.
            let prod = src.split("#[cfg(test)]").next().unwrap();
            for word in [
                "crate::auth",
                "crate::fingerprint",
                "crate::locker",
                "session_lock",
                "SessionLock",
                "unlock",
                "pam_client",
                "Zeroizing",
                "password",
            ] {
                assert!(
                    !prod
                        .lines()
                        .filter(|l| !l.trim_start().starts_with("//"))
                        .any(|l| l.contains(word)),
                    "{name} must not mention {word:?} outside comments"
                );
            }
        }
    }
}

//! Process separation for swaylock-compatible `-f` operation.
//!
//! The fork happens before the locker creates worker threads. The original process then waits on
//! a private pipe until the child has acquired the compositor's session lock. This gives swayidle
//! the ordering it expects without ever continuing Rust code in a post-fork copy of a
//! multi-threaded process.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

const READY: u8 = b'R';

pub(crate) enum ForkRole {
    Parent(ReadyWaiter),
    Child(ReadyNotifier),
}

pub(crate) struct ReadyWaiter {
    read: OwnedFd,
    child: libc::pid_t,
}

pub(crate) struct ReadyNotifier {
    write: Option<OwnedFd>,
}

pub(crate) fn fork() -> io::Result<ForkRole> {
    let (read, write) = pipe()?;

    // SAFETY: the default binary calls this while it is still single-threaded, before opening the
    // Wayland connection or starting the PAM/socket workers. Both branches immediately discard the
    // pipe end they do not own. The child calls only `setsid` before returning to ordinary Rust.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(io::Error::last_os_error());
    }
    if child > 0 {
        drop(write);
        return Ok(ForkRole::Parent(ReadyWaiter { read, child }));
    }

    drop(read);
    // SAFETY: a freshly forked child cannot be a process-group leader, so `setsid` is the standard
    // single-fork daemon separation here. Stdio deliberately stays open for journald diagnostics.
    if unsafe { libc::setsid() } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ForkRole::Child(ReadyNotifier { write: Some(write) }))
}

impl ReadyWaiter {
    pub(crate) fn wait(self) -> io::Result<()> {
        wait_for_ready(self.read, Some(self.child))
    }
}

impl ReadyNotifier {
    pub(crate) fn notify(mut self) -> io::Result<()> {
        let write = self.write.take().expect("readiness notification sent once");
        File::from(write).write_all(&[READY])
    }
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds: [RawFd; 2] = [-1, -1];
    // SAFETY: `fds` points at two valid `c_int` slots. On success, pipe2 initializes both with
    // uniquely owned descriptors, which are immediately wrapped in `OwnedFd`.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful `pipe2` returned two new, valid, uniquely owned descriptors.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn wait_for_ready(read: OwnedFd, child: Option<libc::pid_t>) -> io::Result<()> {
    let mut byte = [0_u8; 1];
    match File::from(read).read_exact(&mut byte) {
        Ok(()) if byte[0] == READY => Ok(()),
        Ok(()) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon child sent an invalid readiness marker",
        )),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            if let Some(child) = child {
                let mut status = 0;
                // SAFETY: `child` is the direct child returned by `fork`; this branch is reached
                // only after its pipe closed before readiness, so a blocking reap cannot wait on a
                // healthy long-running locker.
                let waited = unsafe { libc::waitpid(child, &mut status, 0) };
                if waited < 0 {
                    return Err(io::Error::last_os_error());
                }
                Err(io::Error::other(format!(
                    "daemon child exited before acquiring the session lock (wait status {status})"
                )))
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "readiness pipe closed before the session lock was acquired",
                ))
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_marker_round_trips() {
        let (read, write) = pipe().unwrap();
        ReadyNotifier { write: Some(write) }.notify().unwrap();
        wait_for_ready(read, None).unwrap();
    }

    #[test]
    fn closed_pipe_is_not_ready() {
        let (read, write) = pipe().unwrap();
        drop(write);
        let error = wait_for_ready(read, None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
}

//! Reading a password: from standard input (`--password-stdin`), or from the
//! terminal with echo off. Never from the command line.

use anyhow::{bail, Context, Result};
use livesync_host::secret::Secret;
use std::io::{BufRead, Read, Write};

/// All of standard input, without the trailing newline.
pub fn from_stdin() -> Result<Secret> {
    let mut s = String::new();
    std::io::stdin()
        .read_to_string(&mut s)
        .context("reading the password from stdin")?;
    let s = s.trim_end_matches(['\n', '\r']);
    if s.is_empty() {
        bail!("no password on stdin");
    }
    Ok(Secret::new(s))
}

/// Ask on the terminal twice, with echo off.
pub fn ask_new(what: &str) -> Result<Secret> {
    let a = ask(&format!("{what}: "))?;
    let b = ask("again: ")?;
    if a != b {
        bail!("the two passwords differ; nothing was changed");
    }
    if a.expose().is_empty() {
        bail!("an empty password; nothing was changed (use --no-password for a server without one)");
    }
    Ok(a)
}

/// `--password-stdin`, else the terminal.
pub fn new_password(password_stdin: bool, what: &str) -> Result<Secret> {
    if password_stdin {
        from_stdin()
    } else {
        ask_new(what)
    }
}

#[cfg(unix)]
fn ask(prompt: &str) -> Result<Secret> {
    use std::os::fd::AsRawFd;
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("no terminal to ask for the password on; give it with --password-stdin")?;
    let fd = tty.as_raw_fd();
    let mut old: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut old) } != 0 {
        bail!("cannot turn off the terminal's echo; give the password with --password-stdin");
    }
    let mut quiet = old;
    quiet.c_lflag &= !libc::ECHO;
    quiet.c_lflag |= libc::ECHONL;
    let mut out = &tty;
    out.write_all(prompt.as_bytes())?;
    out.flush()?;
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
    let mut line = String::new();
    let read = std::io::BufReader::new(&tty).read_line(&mut line);
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &old) };
    read.context("reading the password")?;
    Ok(Secret::new(line.trim_end_matches(['\n', '\r'])))
}

#[cfg(not(unix))]
fn ask(_prompt: &str) -> Result<Secret> {
    bail!("give the password with --password-stdin")
}

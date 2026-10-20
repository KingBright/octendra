use anyhow::{Context, Result, bail, ensure};
use nix::{
    fcntl::OFlag,
    sys::termios::{LocalFlags, SetArg, SpecialCharacterIndices, Termios, tcgetattr, tcsetattr},
};
use std::{
    fs::{File, OpenOptions},
    io::{ErrorKind, IsTerminal, Read, Write},
    os::unix::fs::{FileTypeExt, OpenOptionsExt},
    time::Duration,
};
use zeroize::Zeroizing;

const MAX_PASSWORD_BYTES: usize = 1024;

pub(super) struct Terminal {
    file: File,
    original: Termios,
}

impl Terminal {
    pub(super) fn open() -> Result<Self> {
        ensure!(
            std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
            "password reset requires an interactive terminal; pipes and redirected input are refused"
        );
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(
                (OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC | OFlag::O_NOCTTY).bits(),
            )
            .open("/dev/tty")
            .context("open the administrator's controlling terminal")?;
        ensure!(
            file.metadata()?.file_type().is_char_device() && file.is_terminal(),
            "password input must be a terminal"
        );
        ensure!(
            nix::unistd::tcgetpgrp(&file)? == nix::unistd::getpgrp(),
            "password reset must run in the foreground terminal"
        );
        let original = tcgetattr(&file).context("read administrator terminal settings")?;
        let terminal = Self { file, original };
        let mut hidden = terminal.original.clone();
        hidden.local_flags.remove(
            LocalFlags::ECHO
                | LocalFlags::ECHONL
                | LocalFlags::ICANON
                | LocalFlags::ISIG
                | LocalFlags::IEXTEN,
        );
        hidden.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
        hidden.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
        // Disable echo before displaying any password prompt. Control keys are
        // handled below so cancellation always unwinds and restores the terminal.
        tcsetattr(&terminal.file, SetArg::TCSAFLUSH, &hidden)
            .context("hide administrator terminal input")?;
        Ok(terminal)
    }

    pub(super) async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let mut remaining = bytes;
        while !remaining.is_empty() {
            match self.file.write(remaining) {
                Ok(written) => {
                    ensure!(written > 0, "terminal output closed");
                    remaining = &remaining[written..];
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("write administrator terminal"),
            }
        }
        Ok(())
    }

    pub(super) async fn read_line(&mut self, prompt: &str) -> Result<Zeroizing<Vec<u8>>> {
        self.write(prompt.as_bytes()).await?;
        let mut bytes = Zeroizing::new(Vec::new());
        loop {
            let mut byte = Zeroizing::new([0u8; 1]);
            // macOS rejects kqueue registration for /dev/tty. Nonblocking IO
            // with bounded yielding works on both supported POSIX targets and
            // leaves signal cancellation responsive without a detached reader.
            match self.file.read(byte.as_mut_slice()) {
                Ok(count) => {
                    ensure!(count == 1, "terminal closed; configuration unchanged");
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("read administrator terminal"),
            }
            match byte[0] {
                b'\r' | b'\n' => {
                    self.write(b"\n").await?;
                    ensure!(
                        std::str::from_utf8(&bytes).is_ok(),
                        "password input must be valid UTF-8"
                    );
                    return Ok(bytes);
                }
                3 | 4 | 26 => bail!("password reset cancelled; configuration unchanged"),
                8 | 127 => {
                    if let Some(last) = bytes.pop()
                        && last & 0xc0 == 0x80
                    {
                        while bytes.last().is_some_and(|b| b & 0xc0 == 0x80) {
                            bytes.pop();
                        }
                        bytes.pop();
                    }
                }
                value if value < 0x20 => {
                    bail!("unsupported terminal control character; configuration unchanged");
                }
                value => {
                    ensure!(
                        bytes.len() < MAX_PASSWORD_BYTES,
                        "password input exceeds 1024 bytes; configuration unchanged"
                    );
                    bytes.push(value);
                }
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = tcsetattr(&self.file, SetArg::TCSAFLUSH, &self.original);
    }
}

//! A pseudo-terminal for the host side of a shell: open, spawn a child on it as its
//! controlling terminal, async read/write on the master, resize.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::process::Stdio;

use rustix::fs::{Mode, OFlags};
use rustix::pty::OpenptFlags;
use rustix::termios::Winsize;
use tokio::io::unix::AsyncFd;

pub struct PtyMaster {
    fd: AsyncFd<OwnedFd>,
}

fn winsize(cols: u16, rows: u16) -> Winsize {
    Winsize { ws_row: rows.max(1), ws_col: cols.max(1), ws_xpixel: 0, ws_ypixel: 0 }
}

/// Open a pty pair sized `cols` x `rows`. Returns (master, slave).
pub fn open(cols: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
    rustix::pty::grantpt(&master)?;
    rustix::pty::unlockpt(&master)?;
    let name = rustix::pty::ptsname(&master, Vec::new())?;
    let slave = rustix::fs::open(name.as_c_str(), OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC, Mode::empty())?;
    rustix::termios::tcsetwinsize(&master, winsize(cols, rows))?;
    Ok((master, slave))
}

/// Make `cmd` run on the pty: slave as stdio, new session, slave as controlling terminal.
pub fn attach(cmd: &mut tokio::process::Command, slave: &OwnedFd) -> io::Result<()> {
    cmd.stdin(Stdio::from(slave.try_clone()?));
    cmd.stdout(Stdio::from(slave.try_clone()?));
    cmd.stderr(Stdio::from(slave.try_clone()?));
    // SAFETY: only async-signal-safe syscalls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::setsid()?;
            rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
            Ok(())
        });
    }
    Ok(())
}

impl PtyMaster {
    pub fn new(master: OwnedFd) -> io::Result<PtyMaster> {
        rustix::io::ioctl_fionbio(&master, true)?;
        Ok(PtyMaster { fd: AsyncFd::new(master)? })
    }

    /// Read some output. `Ok(0)` once every slave is closed (Linux reports EIO then).
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut g = self.fd.readable().await?;
            match g.try_io(|fd| rustix::io::read(fd.get_ref(), &mut *buf).map_err(io::Error::from)) {
                Ok(Ok(n)) => return Ok(n),
                Ok(Err(e)) if e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) => return Ok(0),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }

    pub async fn write_all(&self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let mut g = self.fd.writable().await?;
            match g.try_io(|fd| rustix::io::write(fd.get_ref(), data).map_err(io::Error::from)) {
                Ok(Ok(n)) => data = &data[n..],
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        rustix::termios::tcsetwinsize(self.fd.get_ref().as_fd(), winsize(cols, rows))?;
        Ok(())
    }
}

//! The guard's hands: ask logind, take and drop the lock, suspend. Behind a trait so the
//! state machine can be driven against a fake in tests.
//!
//! The lock (Linux) is taken by a `systemd-inhibit` child whose command waits on a pipe
//! from the daemon:
//!
//!   systemd-inhibit --what=handle-lid-switch:sleep --mode=block … /bin/sh -c 'echo held; exec cat'
//!
//! logind hands the lock's fd to systemd-inhibit, which keeps it while its command runs
//! (the command does not get the fd). `cat` reads the pipe whose only write end is in the
//! daemon (close-on-exec, so tmux and other children never inherit it). When the daemon
//! exits for any reason, including SIGKILL or the watchdog's SIGABRT, the kernel closes the
//! write end, `cat` sees EOF, systemd-inhibit exits and logind drops the lock. That ties
//! the lock to the daemon's life without a D-Bus library in the binary and without
//! `PR_SET_PDEATHSIG` (which fires when the spawning *thread* exits, not the process).
//! `KillMode=process` in the unit does not matter: the pipe still closes.

use std::io::Read;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::lid::{Logind, parse_busctl_bools};

pub trait Backend: Send {
    fn logind(&mut self) -> Logind;
    fn acquire(&mut self) -> Result<(), String>;
    fn release(&mut self);
    /// The lock is held right now (the holder is alive).
    fn held(&mut self) -> bool;
    fn suspend(&mut self) -> Result<(), String>;
}

const CMD_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_OUTPUT: u64 = 4096;
pub const WHY: &str = "jaunt host mode: staying reachable on mains power";

/// Run a command (argv only), wait at most `limit`, return its stdout (capped).
pub fn run_capture(argv: &[&str], limit: Duration) -> Result<String, String> {
    let (prog, args) = argv.split_first().ok_or("empty command")?;
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {prog}: {e}"))?;
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{prog} did not answer within {} s", limit.as_secs()));
            }
            Err(e) => return Err(format!("{prog}: {e}")),
        }
    };
    let mut out = String::new();
    if let Some(o) = child.stdout.take() {
        let _ = o.take(MAX_OUTPUT).read_to_string(&mut out);
    }
    if !status.success() {
        let mut err = String::new();
        if let Some(e) = child.stderr.take() {
            let _ = e.take(MAX_OUTPUT).read_to_string(&mut err);
        }
        let err = err.trim();
        return Err(if err.is_empty() { format!("{prog} failed ({status})") } else { err.to_string() });
    }
    Ok(out)
}

pub struct Systemd {
    holder: Option<(Child, ChildStdin)>,
    /// Test mode (`JAUNT_HOME`): never really suspend unless `JAUNT_ALLOW_SUSPEND=1`.
    pub test_mode: bool,
}

impl Systemd {
    pub fn new(test_mode: bool) -> Systemd {
        Systemd { holder: None, test_mode }
    }
}

impl Backend for Systemd {
    fn logind(&mut self) -> Logind {
        match run_capture(
            &[
                "busctl",
                "--system",
                "get-property",
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
                "LidClosed",
                "Docked",
                "OnExternalPower",
            ],
            CMD_TIMEOUT,
        ) {
            Ok(out) => parse_busctl_bools(&out),
            Err(e) => {
                tracing::debug!("cannot ask logind: {e}");
                Logind::default()
            }
        }
    }

    fn acquire(&mut self) -> Result<(), String> {
        self.release();
        let mut child = Command::new("systemd-inhibit")
            .args([
                "--what=handle-lid-switch:sleep",
                "--who=jaunt",
                &format!("--why={WHY}"),
                "--mode=block",
                "--no-ask-password",
                "/bin/sh",
                "-c",
                "echo held; exec cat",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run systemd-inhibit: {e}"))?;
        let stdin = child.stdin.take().ok_or("no pipe to systemd-inhibit")?;
        let mut stdout = child.stdout.take().ok_or("no pipe from systemd-inhibit")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8];
            let n = stdout.read(&mut buf).unwrap_or(0);
            let _ = tx.send(buf[..n].to_vec());
        });
        match rx.recv_timeout(CMD_TIMEOUT) {
            Ok(b) if b.starts_with(b"held") => {
                self.holder = Some((child, stdin));
                Ok(())
            }
            _ => {
                drop(stdin);
                let _ = child.kill();
                let _ = child.wait();
                let mut err = String::new();
                if let Some(e) = child.stderr.take() {
                    let _ = e.take(MAX_OUTPUT).read_to_string(&mut err);
                }
                let err = err.trim();
                Err(if err.is_empty() { "systemd-inhibit gave up without saying why".into() } else { err.to_string() })
            }
        }
    }

    fn release(&mut self) {
        if let Some((mut child, stdin)) = self.holder.take() {
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn held(&mut self) -> bool {
        match &mut self.holder {
            Some((child, _)) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn suspend(&mut self) -> Result<(), String> {
        if self.test_mode && std::env::var("JAUNT_ALLOW_SUSPEND").as_deref() != Ok("1") {
            return Err("test mode (JAUNT_HOME is set): not suspending".into());
        }
        run_capture(&["systemctl", "suspend"], Duration::from_secs(10)).map(|_| ())
    }
}

/// macOS (UNVERIFIED): `caffeinate -s -w <daemon pid>` holds "prevent system sleep" (honoured
/// only on AC) and exits by itself when the daemon dies. It does not keep a closed lid awake.
#[cfg(target_os = "macos")]
pub struct Caffeinate {
    holder: Option<Child>,
}

#[cfg(target_os = "macos")]
impl Caffeinate {
    pub fn new() -> Caffeinate {
        Caffeinate { holder: None }
    }
}

#[cfg(target_os = "macos")]
impl Backend for Caffeinate {
    fn logind(&mut self) -> Logind {
        Logind::default()
    }
    fn acquire(&mut self) -> Result<(), String> {
        self.release();
        let pid = std::process::id().to_string();
        let child = Command::new("/usr/bin/caffeinate")
            .args(["-s", "-w", &pid])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot run caffeinate: {e}"))?;
        self.holder = Some(child);
        Ok(())
    }
    fn release(&mut self) {
        if let Some(mut c) = self.holder.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
    fn held(&mut self) -> bool {
        self.holder.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }
    fn suspend(&mut self) -> Result<(), String> {
        Err("not supported on macOS".into())
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;

    #[derive(Default)]
    pub struct Fake {
        pub logind: Logind,
        pub refuse: Option<String>,
        pub holding: bool,
        /// The holder died behind our back.
        pub crash: bool,
        pub suspends: u32,
        pub acquires: u32,
        pub releases: u32,
    }

    impl Backend for Fake {
        fn logind(&mut self) -> Logind {
            self.logind
        }
        fn acquire(&mut self) -> Result<(), String> {
            self.acquires += 1;
            if let Some(e) = &self.refuse {
                return Err(e.clone());
            }
            self.holding = true;
            Ok(())
        }
        fn release(&mut self) {
            if self.holding {
                self.releases += 1;
            }
            self.holding = false;
        }
        fn held(&mut self) -> bool {
            if self.crash {
                self.holding = false;
            }
            self.holding
        }
        fn suspend(&mut self) -> Result<(), String> {
            self.suspends += 1;
            Ok(())
        }
    }
}

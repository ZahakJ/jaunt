//! jaunt: hop between your own machines.

mod awake;
mod check;
mod addrs;
mod children;
mod cli_trust;
mod config;
mod daemon;
mod events;
mod identity;
mod ipc;
mod net;
mod pair;
mod paths;
mod proto;
mod pty;
mod roster;
mod services;
mod shell;
mod systemd;
mod term;
mod trust;
mod ui;
mod uninstall;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::paths::Paths;
use crate::proto::{IpcRequest, IpcResponse};

#[derive(Parser)]
#[command(name = "jaunt", version, about = "Hop between your own machines", args_conflicts_with_subcommands = true)]
#[command(allow_external_subcommands = true)]
struct Cli {
    /// No colours (also NO_COLOR).
    #[arg(long, global = true)]
    no_color: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in the foreground (normally started for you).
    Daemon,
    /// Install and start the user service.
    Up,
    /// Stop the daemon (and disable the user service if installed).
    Down,
    /// Machines, path (direct / relay / offline), latency, sessions.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Open a session: `jaunt hop toluene[:session]` (same as `jaunt toluene`).
    Hop { target: String },
    /// Run one command on a machine: `jaunt run toluene -- uname -a`.
    Run {
        machine: String,
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// Copy files or directories; one side is machine:path (`jaunt cp notes.txt toluene:docs/`).
    Cp { src: String, dst: String },
    /// Forward a local port to a machine's loopback: `jaunt fwd toluene 8080[:9090]`. The local
    /// end listens on 127.0.0.1, so every user of this machine can use it while it runs
    /// (as with ssh -L); on a shared machine forward only what anyone here may reach.
    Fwd {
        machine: String,
        /// <port>[:<localport>]
        spec: String,
    },
    /// stdin/stdout to a machine's loopback port (default 22), for `ProxyCommand jaunt pipe %h`.
    Pipe {
        machine: String,
        #[arg(default_value_t = 22)]
        port: u16,
    },
    /// Rename this machine.
    Name { name: String },
    /// Invite a machine into the mesh: shows a code for 'jaunt join' on the new machine.
    Link {
        /// Skip comparing the sigil (scripted tests only).
        #[arg(long)]
        yes: bool,
    },
    /// Join a mesh with the code another machine's 'jaunt link' shows. Run it without the
    /// code and type the code when asked (or pipe it in): a code on the command line can be
    /// read by every other user of this machine while the join runs.
    Join {
        /// The code, e.g. 4127-tremor-kudos (spaces instead of dashes are fine). Better left
        /// out: then it is asked for.
        #[arg(num_args = 0..)]
        code: Vec<String>,
        /// Relay URL, when this machine does not know one yet.
        #[arg(long)]
        relay: Option<String>,
        /// Skip comparing the sigil (scripted tests only).
        #[arg(long)]
        yes: bool,
    },
    /// Revoke a machine: it is refused everywhere from now on and its sessions end.
    Unlink {
        name: String,
        /// Also revoke every machine it added.
        #[arg(long)]
        cascade: bool,
        /// Also end the tmux sessions that machine created, here and on every machine that
        /// confirms the revocation (what runs inside them is ended with them).
        #[arg(long)]
        kill_sessions: bool,
    },
    /// Take this machine out of the mesh and wipe its key and roster.
    Leave {
        #[arg(long)]
        yes: bool,
    },
    /// Show or set the relay(s) every machine uses.
    Relay {
        #[command(subcommand)]
        action: Option<RelayCmd>,
    },
    /// Diagnose: key, daemon, relay, tmux, peers.
    Check,
    /// Host mode: keep this laptop awake and reachable with the lid closed while on mains power.
    Host {
        #[command(subcommand)]
        action: Option<HostCmd>,
    },
    /// Forget this machine's identity and roster and start with a new key.
    Reset {
        #[arg(long)]
        yes: bool,
    },
    #[command(hide = true)]
    Debug {
        #[command(subcommand)]
        action: DebugCmd,
    },
    /// The mesh view and linking ritual on simulated data: mesh, link or ls.
    Demo {
        #[arg(default_value = "mesh")]
        what: String,
        /// Freeze at this many seconds into the story (for captures).
        #[arg(long)]
        at: Option<f64>,
        /// `demo link`: show the joining side.
        #[arg(long)]
        join: bool,
    },
    /// Remove jaunt from this machine: service, binary, config, state (asks about the key).
    Uninstall {
        /// Also delete the key and roster without asking.
        #[arg(long)]
        yes: bool,
        /// Keep the key and roster without asking.
        #[arg(long)]
        keep_key: bool,
    },
    /// Install the tmux snippet (prefix+J picker, F12 key passthrough) into jaunt's config.
    TmuxSetup,
    /// The mesh view as a picker for a tmux popup: Enter opens the hop in a new window.
    #[command(hide = true)]
    Pick,
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Subcommand)]
enum RelayCmd {
    /// Set the relay URL(s) for the whole mesh.
    Set {
        #[arg(required = true)]
        urls: Vec<String>,
        /// PEM file with a CA to trust for this relay's TLS (self-hosted CA). Per machine.
        #[arg(long)]
        ca: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum HostCmd {
    /// Hold the lid lock while on mains power and not too hot (persisted).
    On,
    /// Stop holding it; the machine sleeps on lid close as usual.
    Off,
    /// What is true right now, what closing the lid would do, and what is missing.
    Status,
}

#[derive(Subcommand)]
enum DebugCmd {
    /// Print this machine's id.
    Id,
    /// Trust a machine without pairing (tests only).
    AddPeer { id: String, name: String },
    /// Print the daemon's status snapshot and then its event stream as JSON lines.
    Events {
        #[arg(long, default_value_t = 50)]
        history: u32,
    },
}

fn init_logging(daemon: bool) {
    let default = if daemon { "warn,jaunt=info" } else { "error" };
    let filter = std::env::var("JAUNT_LOG").unwrap_or_else(|_| default.to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

fn main() {
    let cli = Cli::parse();
    ui::theme::set_no_color_flag(cli.no_color);
    let daemon = matches!(cli.cmd, Some(Cmd::Daemon));
    if daemon {
        // L2: however the daemon was started, it does not stay in (or keep busy) the
        // directory it was started from, and its PATH holds absolute directories only, so
        // no helper (tmux, systemd-inhibit, busctl, systemctl) is ever found relative to it.
        let _ = std::env::set_current_dir(ipc::daemon_dir());
        let path = ipc::clean_path(std::env::var_os("PATH").as_deref());
        // SAFETY: still single-threaded here: the async runtime is built below.
        unsafe { std::env::set_var("PATH", path) };
    }
    init_logging(daemon);
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("jaunt: cannot start the async runtime: {e}");
            std::process::exit(1);
        }
    };
    let code = rt.block_on(async {
        match dispatch(cli).await {
            Ok(code) => code,
            Err(e) => {
                term::restore();
                // Errors can carry text from another machine: no escapes reach the terminal.
                eprintln!("jaunt: {}", term::safe_text(&format!("{e:#}"), 4000, true));
                1
            }
        }
    });
    rt.shutdown_timeout(std::time::Duration::from_millis(200));
    std::process::exit(code);
}

fn split_target(t: &str) -> (&str, Option<&str>) {
    match t.split_once(':') {
        Some((m, s)) if !s.is_empty() => (m, Some(s)),
        Some((m, _)) => (m, None),
        None => (t, None),
    }
}

pub(crate) async fn hop(paths: &Paths, target: &str) -> Result<i32> {
    let (machine, session) = split_target(target);
    let session = match session {
        Some(s) => s.to_string(),
        None => Config::load(paths)?.default_session(),
    };
    if !proto::valid_session(&session) {
        bail!("'{session}' is not a valid session name (letters, digits, '-', '_', '.')");
    }
    shell::hop(paths, machine, &session).await
}

async fn dispatch(cli: Cli) -> Result<i32> {
    let paths = Paths::from_env()?;
    match cli.cmd {
        None if term::stdout_is_tty() && term::stdin_is_tty() => ui::live::run(&paths, false).await,
        None => ls(&paths, false).await,
        Some(Cmd::Pick) => ui::live::run(&paths, true).await,
        Some(Cmd::Demo { what, at, join }) => ui::demo::run(&what, at, join).await,
        Some(Cmd::Uninstall { yes, keep_key }) => uninstall::run(&paths, uninstall::check_flags(yes, keep_key)?).await,
        Some(Cmd::TmuxSetup) => ui::tmux::setup(&paths),
        Some(Cmd::Daemon) => {
            daemon::run(paths).await?;
            Ok(0)
        }
        Some(Cmd::Up) => {
            // A daemon that an earlier command started by hand holds the single-instance
            // lock, and the service would fail to start beside it: hand over to systemd.
            if paths.jaunt_home.is_none() && !systemd::unit_active() && stop_daemon(&paths).await? {
                for _ in 0..60 {
                    if ipc::lock_is_free(&paths) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                println!("stopped the daemon that was running outside systemd; the service takes over");
            }
            systemd::up(&paths)?;
            Ok(0)
        }
        Some(Cmd::Down) => down(&paths).await,
        Some(Cmd::Ls { json }) => ls(&paths, json).await,
        Some(Cmd::Hop { target }) => hop(&paths, &target).await,
        Some(Cmd::External(args)) => {
            if args.len() != 1 {
                bail!("unknown command '{}'; see jaunt --help", args.join(" "));
            }
            hop(&paths, &args[0]).await
        }
        Some(Cmd::Run { machine, argv }) => services::run(&paths, &machine, argv).await,
        Some(Cmd::Cp { src, dst }) => services::copy::cp(&paths, &src, &dst).await,
        Some(Cmd::Fwd { machine, spec }) => services::tcp::fwd(&paths, &machine, &spec).await,
        Some(Cmd::Pipe { machine, port }) => {
            if port == 0 {
                bail!("port 0 is not a port");
            }
            services::tcp::pipe(&paths, &machine, port).await
        }
        Some(Cmd::Name { name }) => {
            println!("{}", ipc::call_ok(&paths, &IpcRequest::Rename { name }).await?);
            Ok(0)
        }
        Some(Cmd::Relay { action }) => relay(&paths, action).await,
        Some(Cmd::Link { yes }) => cli_trust::link(&paths, yes).await,
        Some(Cmd::Join { code, relay, yes }) => cli_trust::join(&paths, code, relay, yes).await,
        Some(Cmd::Unlink { name, cascade, kill_sessions }) => cli_trust::unlink(&paths, name, cascade, kill_sessions).await,
        Some(Cmd::Leave { yes }) => cli_trust::leave(&paths, yes).await,
        Some(Cmd::Check) => Ok(if check::run(&paths).await? > 0 { 1 } else { 0 }),
        Some(Cmd::Reset { yes }) => reset(&paths, yes).await,
        Some(Cmd::Host { action }) => match action {
            Some(HostCmd::On) => awake::cli::set(&paths, true).await,
            Some(HostCmd::Off) => awake::cli::set(&paths, false).await,
            None | Some(HostCmd::Status) => awake::cli::status(&paths).await,
        },
        Some(Cmd::Debug { action }) => debug(&paths, action).await,
    }
}

pub(crate) async fn status(paths: &Paths) -> Result<proto::StatusSnapshot> {
    match ipc::call(paths, &IpcRequest::Status).await? {
        IpcResponse::Status(s) => Ok(s),
        IpcResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected answer from the daemon: {other:?}"),
    }
}

async fn ls(paths: &Paths, json: bool) -> Result<i32> {
    let s = status(paths).await?;
    let mut out = std::io::stdout().lock();
    if json {
        serde_json::to_writer_pretty(&mut out, &s)?;
        writeln!(out)?;
    } else if term::stdout_is_tty() {
        ui::ls_pretty(&s, &mut out)?;
    } else {
        ui::ls_plain(&s, &mut out)?;
    }
    Ok(0)
}

pub(crate) async fn stop_daemon(paths: &Paths) -> Result<bool> {
    let Some(mut s) = ipc::connect_existing(paths).await else { return Ok(false) };
    ipc::send_request(&mut s, &IpcRequest::Shutdown).await?;
    for _ in 0..50 {
        if ipc::connect_existing(paths).await.is_none() {
            return Ok(true);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    bail!("the daemon did not stop within 5 s")
}

async fn down(paths: &Paths) -> Result<i32> {
    if systemd::unit_installed(paths) {
        systemd::down_unit()?;
        // Also a daemon that was started outside the service, if one is still there.
        let _ = stop_daemon(paths).await;
        println!("jaunt service stopped and disabled");
    } else if stop_daemon(paths).await? {
        println!("jaunt daemon stopped");
    } else {
        println!("the jaunt daemon was not running");
    }
    Ok(0)
}

async fn relay(paths: &Paths, action: Option<RelayCmd>) -> Result<i32> {
    match action {
        None => {
            let s = status(paths).await?;
            if s.relays.is_empty() {
                println!("no relay configured (jaunt relay set URL)");
            }
            for r in &s.relays {
                println!("{r}\t{}", if s.relay_connected { "connected" } else { "not connected" });
            }
            Ok(0)
        }
        Some(RelayCmd::Set { urls, ca }) => {
            net::parse_relays(&urls)?;
            let mut cfg = Config::load(paths)?;
            cfg.relays = urls.clone();
            let ca_changed = match ca {
                Some(p) => {
                    let abs = std::fs::canonicalize(&p).with_context(|| format!("cannot find {}", p.display()))?;
                    let changed = cfg.relay_ca.as_ref() != Some(&abs);
                    cfg.relay_ca = Some(abs);
                    changed
                }
                None => false,
            };
            let was_running = ipc::connect_existing(paths).await.is_some();
            cfg.save(paths)?;
            println!("{}", ipc::call_ok(paths, &IpcRequest::SetRelays { relays: urls.clone() }).await?);
            if ca_changed && was_running {
                println!("the relay CA applies after the daemon restarts (jaunt down; any jaunt command starts it again)");
                return Ok(0);
            }
            if urls.is_empty() {
                return Ok(0);
            }
            // The running daemon switches to the new list at once (no restart): say whether
            // it got through.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let s = status(paths).await?;
                if s.relay_connected {
                    println!("the daemon is connected to the relay now (no restart needed)");
                    break;
                }
                if std::time::Instant::now() > deadline {
                    println!("the daemon is using the new relay list but is not connected to the relay yet; jaunt check says why");
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Ok(0)
        }
    }
}

async fn reset(paths: &Paths, yes: bool) -> Result<i32> {
    if !yes {
        if !term::stdin_is_tty() {
            bail!("reset needs --yes when not run on a terminal");
        }
        print!("This forgets this machine's key and roster; other machines must link it again. Type 'reset' to go on: ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != "reset" {
            println!("nothing changed");
            return Ok(1);
        }
    }
    // Best effort: while this machine is still a member, revoke its old key everywhere
    // reachable, so a copy of that key (a stolen disk) is worthless too.
    if ipc::connect_existing(paths).await.is_some()
        && let Ok(s) = status(paths).await
        && !s.peers.is_empty()
        && !s.revoked.iter().any(|r| r.id == s.me.id)
    {
        println!("revoking this machine's old key on the others first");
        if let Err(e) = cli_trust::announce_leave(paths).await {
            println!("could not tell the others ({e:#}); unlink this machine's old key from one of them");
        }
    }
    wipe_identity(paths).await?;
    let key = identity::create(paths)?;
    println!("new identity: {}", key.public());
    println!("link this machine again from one already in the mesh");
    Ok(0)
}

/// Stop the daemon and delete the key and roster.
pub(crate) async fn wipe_identity(paths: &Paths) -> Result<()> {
    stop_daemon(paths).await?;
    identity::remove(paths)?;
    match std::fs::remove_file(paths.roster_file()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn debug(paths: &Paths, action: DebugCmd) -> Result<i32> {
    match action {
        DebugCmd::Id => {
            let key = identity::load_or_create(paths)?;
            println!("{}", key.public());
        }
        DebugCmd::AddPeer { id, name } => {
            println!("{}", ipc::call_ok(paths, &IpcRequest::AddPeer { id, name }).await?);
        }
        DebugCmd::Events { history } => {
            let mut s = ipc::connect(paths).await?;
            proto::write_json(&mut s, proto::FrameType::Json, &IpcRequest::Subscribe { history }).await?;
            let mut out = std::io::stdout();
            while let Some(f) = proto::read_frame(&mut s).await? {
                out.write_all(&f.payload)?;
                out.write_all(b"\n")?;
                out.flush()?;
            }
        }
    }
    Ok(0)
}

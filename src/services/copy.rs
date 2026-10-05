//! `Copy { op, path }` and `jaunt cp`. After the `Reply`, the sender writes:
//! `Plan`, then per entry an `Entry` message (a `Json` frame) followed, for a file, by
//! exactly `size` bytes in `Data` frames; `Skip` for what it could not send; `Done` at the
//! end, or `Abort` to give up. For a push the host answers `Written` or `Abort`.
//! The receiver writes through `tree::Writer`, which never writes outside the destination.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use iroh::endpoint::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::tree::{self, Entry, Kind, Plan, Writer};
use crate::daemon::Daemon;
use crate::ipc;
use crate::paths::Paths;
use crate::proto::{self, CopyOp, FrameType, Reply, Service};
use crate::roster::MachineId;

const MAX_MSG: usize = 16 * 1024;
const CHUNK: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "m", rename_all = "snake_case")]
pub enum Msg {
    Plan { name: String, files: u64, bytes: u64 },
    Entry(Entry),
    Skip { path: String, reason: String },
    Done,
    Abort { reason: String },
    Written { files: u64, bytes: u64 },
}

async fn put<W: AsyncWrite + Unpin>(w: &mut W, m: &Msg) -> Result<()> {
    proto::write_json(w, FrameType::Json, m).await?;
    Ok(())
}

// ------------------------------------------------------------------------- progress

/// One line on stderr, redrawn at most 5 times a second; only when stderr is a terminal.
pub struct Progress {
    on: bool,
    total: u64,
    done: u64,
    started: Instant,
    drawn: Option<Instant>,
}

impl Progress {
    pub fn new(on: bool) -> Progress {
        Progress { on, total: 0, done: 0, started: Instant::now(), drawn: None }
    }
    fn total(&mut self, t: u64) {
        self.total = t;
        self.started = Instant::now();
    }
    fn add(&mut self, n: u64) {
        self.done += n;
        if self.on && self.drawn.is_none_or(|t| t.elapsed() >= Duration::from_millis(200)) {
            self.drawn = Some(Instant::now());
            self.draw(false);
        }
    }
    fn draw(&self, last: bool) {
        let secs = self.started.elapsed().as_secs_f64().max(0.001);
        let pct = (self.done * 100).checked_div(self.total).unwrap_or(100);
        let mut e = std::io::stderr().lock();
        let _ = write!(e, "\r\x1b[K{} of {} ({pct}%) {}/s", human(self.done), human(self.total), human((self.done as f64 / secs) as u64));
        if last {
            let _ = write!(e, " in {secs:.1} s\r\n");
        }
        let _ = e.flush();
    }
    fn finish(&self) {
        if self.on {
            self.draw(true);
        }
    }
}

pub fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}

// ------------------------------------------------------------------------- sending

pub async fn send_tree<W: AsyncWrite + Unpin>(w: &mut W, plan: &Plan, progress: &mut Progress) -> Result<()> {
    put(w, &Msg::Plan { name: plan.name.clone(), files: plan.files, bytes: plan.bytes }).await?;
    progress.total(plan.bytes);
    for s in &plan.skipped {
        put(w, &Msg::Skip { path: s.path.clone(), reason: s.reason.clone() }).await?;
    }
    let mut buf = vec![0u8; CHUNK];
    for e in &plan.entries {
        if e.kind != Kind::File {
            put(w, &Msg::Entry(e.clone())).await?;
            continue;
        }
        let src = tree::source_path(&plan.root, &e.path);
        let mut f = match tokio::fs::File::open(&src).await {
            Ok(f) => f,
            Err(err) => {
                put(w, &Msg::Skip { path: e.path.clone(), reason: format!("cannot read it: {err}") }).await?;
                continue;
            }
        };
        put(w, &Msg::Entry(e.clone())).await?;
        let mut left = e.size;
        while left > 0 {
            let want = (left.min(CHUNK as u64)) as usize;
            let n = match f.read(&mut buf[..want]).await {
                Ok(0) => Err(format!("{} got shorter while it was being copied", src.display())),
                Ok(n) => Ok(n),
                Err(e) => Err(format!("cannot read {}: {e}", src.display())),
            };
            let n = match n {
                Ok(n) => n,
                Err(reason) => {
                    put(w, &Msg::Abort { reason: reason.clone() }).await?;
                    bail!("{reason}");
                }
            };
            proto::write_frame(w, FrameType::Data, &buf[..n]).await?;
            left -= n as u64;
            progress.add(n as u64);
        }
    }
    put(w, &Msg::Done).await?;
    w.flush().await?;
    Ok(())
}

// ------------------------------------------------------------------------- receiving

/// Read a whole tree from `r` into `w`. `skip` is told about what the sender left out.
pub async fn receive_tree<R: AsyncRead + Unpin>(
    r: &mut R,
    w: &mut Writer,
    progress: &mut Progress,
    mut skip: impl FnMut(&str, &str),
) -> Result<()> {
    let mut cur: Option<tree::Incoming> = None;
    let res = receive_inner(r, w, progress, &mut skip, &mut cur).await;
    if res.is_err()
        && let Some(f) = cur.take()
    {
        let _ = std::fs::remove_file(&f.tmp);
    }
    res
}

async fn receive_inner<R: AsyncRead + Unpin>(
    r: &mut R,
    w: &mut Writer,
    progress: &mut Progress,
    skip: &mut impl FnMut(&str, &str),
    cur: &mut Option<tree::Incoming>,
) -> Result<()> {
    let first = proto::expect_frame(r, FrameType::Json).await.context("the copy did not start")?;
    match proto::parse_json::<Msg>(&first, MAX_MSG)? {
        Msg::Plan { name, bytes, .. } => {
            w.set_name(&name)?;
            progress.total(bytes);
        }
        Msg::Abort { reason } => bail!("{reason}"),
        _ => bail!("the copy did not start with a plan"),
    }
    let mut entries: u64 = 0;
    let mut seen_root = false;
    loop {
        let Some(f) = proto::read_frame(r).await? else { bail!("the copy ended before it was complete") };
        match f.kind {
            FrameType::Data => {
                let Some(inc) = cur.as_mut() else { bail!("file data arrived outside a file") };
                let n = f.payload.len() as u64;
                if n > inc.left {
                    bail!("the sender sent more data than it announced for {}", inc.dest.display());
                }
                inc.file.write_all(&f.payload).with_context(|| format!("cannot write {}", inc.dest.display()))?;
                inc.left -= n;
                progress.add(n);
                if inc.left == 0
                    && let Some(done) = cur.take()
                {
                    let size = done.file.metadata().map(|m| m.len()).unwrap_or(0);
                    w.close_file(done, size)?;
                }
            }
            FrameType::Json => {
                let msg = proto::parse_json::<Msg>(&f.payload, MAX_MSG)?;
                if cur.is_some() && !matches!(msg, Msg::Abort { .. }) {
                    bail!("a file in the copy was cut short");
                }
                match msg {
                    Msg::Entry(e) => {
                        entries += 1;
                        if entries > tree::MAX_ENTRIES {
                            bail!("the copy has more than {} entries", tree::MAX_ENTRIES);
                        }
                        if e.path.is_empty() == seen_root {
                            bail!("the copy's entries are out of order");
                        }
                        seen_root = true;
                        match e.kind {
                            Kind::Dir => w.dir(&e)?,
                            Kind::Symlink => w.symlink(&e)?,
                            Kind::File => {
                                let inc = w.open_file(&e)?;
                                if e.size == 0 {
                                    w.close_file(inc, 0)?;
                                } else {
                                    *cur = Some(inc);
                                }
                            }
                        }
                    }
                    Msg::Skip { path, reason } => skip(&path, &reason),
                    Msg::Done => {
                        if !seen_root {
                            bail!("the copy was empty");
                        }
                        w.finish()?;
                        return Ok(());
                    }
                    Msg::Abort { reason } => bail!("the sender gave up: {reason}"),
                    Msg::Plan { .. } | Msg::Written { .. } => bail!("unexpected message in the copy"),
                }
            }
            other => bail!("unexpected {other:?} frame in the copy"),
        }
    }
}

// ------------------------------------------------------------------------- host side

/// A path as the remote side names it: relative paths and `~/` are under $HOME.
fn expand_remote(path: &str, home: &Path) -> Result<PathBuf> {
    if path.contains('\0') || path.len() > tree::MAX_PATH {
        bail!("that is not a usable path");
    }
    Ok(match path {
        "" | "~" => home.to_path_buf(),
        p if p.starts_with("~/") => home.join(&p[2..]),
        p if p.starts_with('~') => bail!("'~user' paths are not supported; give the full path"),
        p => home.join(p),
    })
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub async fn serve(d: &Arc<Daemon>, _from: MachineId, op: CopyOp, path: String, mut send: SendStream, mut recv: RecvStream) -> Result<()> {
    let at = match expand_remote(&path, &home_dir()) {
        Ok(p) => p,
        Err(e) => return refuse(&mut send, format!("{e:#}")).await,
    };
    match op {
        CopyOp::Pull => {
            let plan = match tokio::task::spawn_blocking(move || tree::plan(&at)).await? {
                Ok(p) => p,
                Err(e) => return refuse(&mut send, format!("on {}: {e:#}", d.my_name())).await,
            };
            proto::write_json(&mut send, FrameType::Reply, &Reply::Ok).await?;
            send_tree(&mut send, &plan, &mut Progress::new(false)).await?;
            send.finish()?;
            let _ = tokio::time::timeout(Duration::from_secs(5), send.stopped()).await;
        }
        CopyOp::Push => {
            proto::write_json(&mut send, FrameType::Reply, &Reply::Ok).await?;
            let mut w = Writer::new(at);
            let answer = match receive_tree(&mut recv, &mut w, &mut Progress::new(false), |_, _| {}).await {
                Ok(()) => Msg::Written { files: w.files, bytes: w.bytes },
                Err(e) => Msg::Abort { reason: format!("on {}: {e:#}", d.my_name()) },
            };
            put(&mut send, &answer).await?;
            send.finish()?;
            let _ = tokio::time::timeout(Duration::from_secs(5), send.stopped()).await;
        }
    }
    Ok(())
}

async fn refuse(send: &mut SendStream, reason: String) -> Result<()> {
    proto::write_json(send, FrameType::Reply, &Reply::Refused { reason }).await?;
    send.finish()?;
    Ok(())
}

// ------------------------------------------------------------------------- client side

/// The name a pull of `rpath` should arrive under, when the path itself says it.
fn expected_name(rpath: &str) -> Option<String> {
    let last = rpath.trim_end_matches('/').rsplit('/').next()?;
    (!last.is_empty() && last != "." && last != ".." && last != "~").then(|| last.to_string())
}

/// `machine:path` (the machine part has no `/`; write `./a:b` for a local file with a colon).
pub fn split_remote(s: &str) -> Option<(&str, &str)> {
    let (m, p) = s.split_once(':')?;
    (!m.is_empty() && !m.contains('/')).then_some((m, p))
}

/// `jaunt cp <src> <dst>`: one side is `machine:path`. Directories are copied whole.
pub async fn cp(paths: &Paths, src: &str, dst: &str) -> Result<i32> {
    let progress = Progress::new(std::io::stderr().is_terminal());
    match (split_remote(src), split_remote(dst)) {
        (Some(_), Some(_)) => bail!("both sides name a machine; run the copy on one of them"),
        (None, None) => bail!("neither side names a machine (machine:path); use cp for local copies"),
        (None, Some((machine, rpath))) => push(paths, src, machine, rpath, progress).await,
        (Some((machine, rpath)), None) => pull(paths, machine, rpath, dst, progress).await,
    }
}

async fn push(paths: &Paths, src: &str, machine: &str, rpath: &str, mut progress: Progress) -> Result<i32> {
    let src_path = PathBuf::from(src);
    let plan = tokio::task::spawn_blocking(move || tree::plan(&src_path)).await??;
    for s in &plan.skipped {
        eprintln!("jaunt: skipped {}: {}", s.path, s.reason);
    }
    let service = Service::Copy { op: CopyOp::Push, path: rpath.to_string() };
    let (stream, peer, _, _) = ipc::open(paths, machine, service).await?;
    let (mut r, mut w) = stream.into_split();
    let answer = tokio::spawn(async move {
        let payload = proto::expect_frame(&mut r, FrameType::Json).await?;
        anyhow::Ok(proto::parse_json::<Msg>(&payload, MAX_MSG)?)
    });
    tokio::pin!(answer);
    let sent = tokio::select! {
        s = send_tree(&mut w, &plan, &mut progress) => s,
        a = &mut answer => return report_push(a, &peer.name, &plan, progress),
    };
    if let Err(e) = sent {
        // The host may have said why it stopped reading.
        if let Ok(Ok(Ok(Msg::Abort { reason }))) = tokio::time::timeout(Duration::from_secs(2), &mut answer).await {
            bail!("{reason}");
        }
        return Err(e.context(format!("the copy to {} broke", peer.name)));
    }
    let a = answer.await;
    report_push(a, &peer.name, &plan, progress)
}

fn report_push(a: std::result::Result<Result<Msg>, tokio::task::JoinError>, peer: &str, plan: &Plan, progress: Progress) -> Result<i32> {
    match a {
        Ok(Ok(Msg::Written { .. })) => {
            progress.finish();
            Ok(if plan.skipped.is_empty() { 0 } else { 1 })
        }
        Ok(Ok(Msg::Abort { reason })) => bail!("{reason}"),
        Ok(Ok(other)) => bail!("{peer} answered the copy with {other:?}"),
        Ok(Err(e)) => Err(e.context(format!("the link to {peer} broke before it confirmed the copy"))),
        Err(e) => Err(e.into()),
    }
}

async fn pull(paths: &Paths, machine: &str, rpath: &str, dst: &str, mut progress: Progress) -> Result<i32> {
    let service = Service::Copy { op: CopyOp::Pull, path: rpath.to_string() };
    let (stream, peer, _, _) = ipc::open(paths, machine, service).await?;
    // Keep the write half open until the end: closing it would end the stream.
    let (mut r, _w) = stream.into_split();
    let mut writer = Writer::new(PathBuf::from(dst));
    writer.expect_name(expected_name(rpath));
    let mut skipped = 0;
    let name = peer.name.clone();
    receive_tree(&mut r, &mut writer, &mut progress, |p, why| {
        skipped += 1;
        eprintln!("jaunt: skipped {p} on {name}: {why}");
    })
    .await
    .with_context(|| format!("copy from {} failed", peer.name))?;
    progress.finish();
    Ok(if skipped == 0 { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_specs() {
        assert_eq!(split_remote("toluene:/etc/hosts"), Some(("toluene", "/etc/hosts")));
        assert_eq!(split_remote("toluene:"), Some(("toluene", "")));
        assert_eq!(split_remote("./a:b"), None);
        assert_eq!(split_remote("/tmp/x"), None);
        assert_eq!(split_remote(":x"), None);
        let h = Path::new("/home/u");
        assert_eq!(expand_remote("", h).unwrap(), PathBuf::from("/home/u"));
        assert_eq!(expand_remote("~/a", h).unwrap(), PathBuf::from("/home/u/a"));
        assert_eq!(expand_remote("a/b", h).unwrap(), PathBuf::from("/home/u/a/b"));
        assert_eq!(expand_remote("/abs", h).unwrap(), PathBuf::from("/abs"));
        assert!(expand_remote("~root/x", h).is_err());
        assert!(expand_remote("a\0b", h).is_err());
        assert_eq!(expected_name("dir/x.txt").as_deref(), Some("x.txt"));
        assert_eq!(expected_name("/var/log/").as_deref(), Some("log"));
        assert_eq!(expected_name(""), None);
        assert_eq!(expected_name("~"), None);
        assert_eq!(expected_name("a/.."), None);
        assert_eq!(expected_name("/"), None);
    }

    fn entry(path: &str, kind: Kind, size: u64) -> Msg {
        Msg::Entry(Entry { path: path.into(), kind, mode: 0o644, size, mtime: None, target: None })
    }

    /// A message, or (with `None`) a data frame.
    type Item = (Option<Msg>, Vec<u8>);

    async fn stream_of(msgs: Vec<Item>) -> Vec<u8> {
        let mut out = Vec::new();
        for (m, data) in msgs {
            match m {
                Some(m) => proto::write_json(&mut out, FrameType::Json, &m).await.unwrap(),
                None => proto::write_frame(&mut out, FrameType::Data, &data).await.unwrap(),
            }
        }
        out
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("target/tmp/copy-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn hostile_streams_are_refused() {
        let base = scratch("hostile");
        let plan = || (Some(Msg::Plan { name: "t".into(), files: 1, bytes: 3 }), vec![]);
        let cases: Vec<(&str, Vec<Item>)> = vec![
            ("traversal", vec![plan(), (Some(entry("", Kind::Dir, 0)), vec![]), (Some(entry("../x", Kind::File, 3)), vec![])]),
            ("absolute", vec![plan(), (Some(entry("", Kind::Dir, 0)), vec![]), (Some(entry("/x", Kind::File, 3)), vec![])]),
            ("too much data", vec![plan(), (Some(entry("", Kind::File, 3)), vec![]), (None, b"abcd".to_vec())]),
            ("cut short", vec![plan(), (Some(entry("", Kind::Dir, 0)), vec![]), (Some(entry("a", Kind::File, 3)), vec![]), (None, b"a".to_vec()), (Some(Msg::Done), vec![])]),
            ("no plan", vec![(Some(entry("", Kind::File, 0)), vec![])]),
            ("bad name", vec![(Some(Msg::Plan { name: "..".into(), files: 0, bytes: 0 }), vec![])]),
            ("two roots", vec![plan(), (Some(entry("", Kind::Dir, 0)), vec![]), (Some(entry("", Kind::Dir, 0)), vec![])]),
            ("truncated", vec![plan(), (Some(entry("", Kind::Dir, 0)), vec![])]),
            ("renamed by sender", vec![(Some(Msg::Plan { name: ".bashrc".into(), files: 0, bytes: 0 }), vec![])]),
        ];
        for (what, msgs) in cases {
            let bytes = stream_of(msgs).await;
            let dest = base.join(what.replace(' ', "-"));
            std::fs::create_dir_all(&dest).unwrap();
            let mut w = Writer::new(dest.clone());
            w.expect_name(Some("t".into()));
            let r = receive_tree(&mut &bytes[..], &mut w, &mut Progress::new(false), |_, _| {}).await;
            assert!(r.is_err(), "{what} was accepted");
            assert!(!base.join("x").exists() && !Path::new("/x").exists());
            // No temp files left behind.
            for e in walk_all(&dest) {
                assert!(!e.to_string_lossy().contains(".jaunt-cp."), "{what}: left {}", e.display());
            }
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    fn walk_all(d: &Path) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                v.push(e.path());
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    v.extend(walk_all(&e.path()));
                }
            }
        }
        v
    }

    #[tokio::test]
    async fn send_then_receive() {
        let base = scratch("roundtrip");
        let src = base.join("src");
        std::fs::create_dir_all(src.join("d")).unwrap();
        std::fs::write(src.join("d/big"), vec![42u8; CHUNK * 2 + 17]).unwrap();
        std::fs::write(src.join("empty"), b"").unwrap();
        let plan = tree::plan(&src).unwrap();
        let mut wire = Vec::new();
        send_tree(&mut wire, &plan, &mut Progress::new(false)).await.unwrap();
        let dest = base.join("dest");
        std::fs::create_dir(&dest).unwrap();
        let mut w = Writer::new(dest.clone());
        receive_tree(&mut &wire[..], &mut w, &mut Progress::new(false), |_, _| {}).await.unwrap();
        assert_eq!(std::fs::read(dest.join("src/d/big")).unwrap(), vec![42u8; CHUNK * 2 + 17]);
        assert_eq!(std::fs::read(dest.join("src/empty")).unwrap(), b"");
        assert_eq!((w.files, w.bytes), (2, (CHUNK * 2 + 17) as u64));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(10), "10 B");
        assert_eq!(human(2048), "2.0 KiB");
        assert_eq!(human(200 * 1024 * 1024), "200.0 MiB");
    }
}

//! Clipboard sharing (`jaunt clip on`), opt-in on each machine. While it is on, this
//! machine serves `Clip` get/set to members, and sends each new text it finds on its own
//! clipboard to the connected members that share theirs. Text that a password manager marks
//! as secret is never sent. A text received is remembered, so it is not sent back.

use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::daemon::Daemon;
use crate::proto::{ClipOp, Service};
use crate::roster::MachineId;

const POLL: Duration = Duration::from_millis(1200);
/// A member that refused (sharing off there) is not asked again for this long.
const QUIET_AFTER_REFUSAL: Duration = Duration::from_secs(600);

#[derive(Default)]
pub struct ClipState {
    /// Hash of the text the watcher last read from the clipboard here.
    local: Mutex<Option<u64>>,
    /// Hashes of the texts lately received from other machines.
    received: Mutex<VecDeque<u64>>,
    refused: Mutex<HashMap<MachineId, Instant>>,
}

pub fn hash(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

impl ClipState {
    /// A text received from another machine: when it shows up on the clipboard here, it
    /// is not sent on. (Whether writing it worked does not matter: nothing is sent unless
    /// the clipboard here changes.)
    pub fn note(&self, text: &str) {
        if let Ok(mut g) = self.received.lock() {
            g.push_back(hash(text));
            if g.len() > 8 {
                g.pop_front();
            }
        }
    }

    /// Should `text`, just read from the clipboard here, be sent? Only a change that did
    /// not come from another machine, and not the text found when sharing starts.
    fn should_send(&self, text: &str) -> bool {
        let h = hash(text);
        let mut local = self.local.lock().unwrap_or_else(|e| e.into_inner());
        let first = local.is_none();
        if *local == Some(h) {
            return false;
        }
        *local = Some(h);
        let echo = self.received.lock().map(|g| g.contains(&h)).unwrap_or(false);
        !first && !echo
    }

    /// What is on the clipboard as sharing starts: not sent.
    pub async fn baseline(&self) {
        let text = crate::desktop::clip_get().await.ok().flatten().unwrap_or_default();
        if let Ok(mut g) = self.local.lock() {
            *g = Some(hash(&text));
        }
    }

    fn reset(&self) {
        if let Ok(mut g) = self.local.lock() {
            *g = None;
        }
    }
}

/// Mime types by which password managers ask clipboard tools not to keep a copy.
pub fn secret_types(types: &str) -> bool {
    types.lines().any(|t| matches!(t.trim(), "x-kde-passwordManagerHint" | "application/x-nspasteboard-concealed-type"))
}

async fn is_secret() -> bool {
    let argv: &[&str] = if cfg!(target_os = "macos") {
        return false;
    } else if crate::desktop::session_env().iter().any(|(k, _)| *k == "WAYLAND_DISPLAY") && crate::desktop::found("wl-paste") {
        &["wl-paste", "--list-types"]
    } else if crate::desktop::found("xclip") {
        &["xclip", "-selection", "clipboard", "-out", "-target", "TARGETS"]
    } else {
        return false;
    };
    let out = tokio::process::Command::new(argv[0]).args(&argv[1..]).envs(crate::desktop::session_env()).output();
    match tokio::time::timeout(Duration::from_secs(3), out).await {
        Ok(Ok(o)) => secret_types(&String::from_utf8_lossy(&o.stdout)),
        _ => true,
    }
}

pub async fn watch(d: Arc<Daemon>) {
    let mut was_on = false;
    loop {
        tokio::time::sleep(POLL).await;
        let on = d.config().clipboard;
        if !on {
            if was_on {
                d.clip.reset();
            }
            was_on = false;
            continue;
        }
        was_on = true;
        // An empty clipboard counts as a text too, so the first copy after it is sent.
        let Ok(text) = crate::desktop::clip_get().await.map(Option::unwrap_or_default) else { continue };
        if !d.clip.should_send(&text) || text.is_empty() || is_secret().await {
            continue;
        }
        for id in d.live_peers() {
            let quiet = d.clip.refused.lock().ok().and_then(|g| g.get(&id).copied()).is_some_and(|t| t.elapsed() < QUIET_AFTER_REFUSAL);
            if quiet {
                continue;
            }
            let (d, text) = (d.clone(), text.clone());
            tokio::spawn(async move {
                let sent = tokio::time::timeout(Duration::from_secs(5), async {
                    let conn = d.connect(id).await?;
                    let (mut send, mut recv) = crate::daemon::open_service(&conn, Service::Clip { op: ClipOp::Set }).await?;
                    crate::daemon::send_clip(&mut send, &mut recv, &text).await?;
                    let _ = send.finish();
                    anyhow::Ok(())
                })
                .await;
                if let Ok(Err(e)) = sent
                    && format!("{e:#}").contains("refused")
                    && let Ok(mut g) = d.clip.refused.lock()
                {
                    g.insert(id, Instant::now());
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_text_is_not_sent_and_echoes_are_not_resent() {
        let s = ClipState::default();
        assert!(!s.should_send("already there"));
        assert!(s.should_send("copied"));
        assert!(!s.should_send("copied"));
        s.note("from the other machine");
        assert!(!s.should_send("from the other machine"), "a received text is not sent back");
        assert!(s.should_send("new"));
        let s = ClipState::default();
        assert!(!s.should_send(""), "empty at the start");
        assert!(s.should_send("first copy"), "the first copy after an empty clipboard is sent");
    }

    /// Writing a received text failed here: the clipboard did not change, so nothing is
    /// sent, and the two machines do not trade texts forever.
    #[test]
    fn a_failed_write_sends_nothing() {
        let s = ClipState::default();
        assert!(!s.should_send("mine"));
        s.note("theirs");
        assert!(!s.should_send("mine"), "unchanged clipboard");
        assert!(!s.should_send("mine"));
    }

    #[test]
    fn password_manager_hints() {
        assert!(secret_types("text/plain\nx-kde-passwordManagerHint\n"));
        assert!(!secret_types("text/plain;charset=utf-8\nUTF8_STRING\n"));
    }
}

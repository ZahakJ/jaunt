//! The cryptographic parts of pairing, all standard constructions (DESIGN §6):
//! SPAKE2 (RustCrypto `spake2`, Ed25519 group, asymmetric roles), HKDF-SHA256 for the
//! nameplate key and the two directional keys, ChaCha20-Poly1305 for the sealed messages,
//! SHA-256 for the human check. Nothing here is new cryptography.

use anyhow::{Result, anyhow};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::Zeroizing;

use super::code::{Code, check_words};
use crate::proto::PairRole;
use crate::roster::MachineId;

const ID_INVITER: &[u8] = b"jaunt-inviter";
const ID_JOINER: &[u8] = b"jaunt-joiner";
const SALT: &[u8] = b"jaunt/pair/1";

/// The pairing endpoint's secret key, derived from the nameplate alone. Anyone who knows
/// the nameplate can derive it; that is by design (it only buys one guess at the PAKE).
pub fn nameplate_secret(nameplate: &str) -> iroh::SecretKey {
    let hk = Hkdf::<Sha256>::new(None, nameplate.as_bytes());
    let mut okm = Zeroizing::new([0u8; 32]);
    // 32 bytes is far below HKDF-SHA256's 8160-byte limit, so expand cannot fail.
    let _ = hk.expand(b"jaunt/pair/1/nameplate", okm.as_mut());
    iroh::SecretKey::from_bytes(&okm)
}

pub struct Pake(Spake2<Ed25519Group>);

impl Pake {
    /// Start SPAKE2 for `role` with the whole code as the password. Returns the message to
    /// send (33 bytes). The scalar comes from the OS CSPRNG inside the crate.
    pub fn start(role: PairRole, code: &Code) -> (Pake, Vec<u8>) {
        let pw = Password::new(code.password());
        let (a, b) = (Identity::new(ID_INVITER), Identity::new(ID_JOINER));
        let (s, msg) = match role {
            PairRole::Inviter => Spake2::<Ed25519Group>::start_a(&pw, &a, &b),
            PairRole::Joiner => Spake2::<Ed25519Group>::start_b(&pw, &a, &b),
        };
        (Pake(s), msg)
    }

    /// Finish with the other side's message. A wrong code does NOT fail here (SPAKE2
    /// cannot tell); it shows up as keys that do not match.
    pub fn finish(self, theirs: &[u8]) -> Result<SessionKey> {
        let k = self.0.finish(theirs).map_err(|e| anyhow!("the other side's key-exchange message is malformed ({e:?})"))?;
        let arr: [u8; 32] = k.as_slice().try_into().map_err(|_| anyhow!("unexpected SPAKE2 key length"))?;
        Ok(SessionKey(Zeroizing::new(arr)))
    }
}

pub struct SessionKey(Zeroizing<[u8; 32]>);

impl SessionKey {
    #[cfg(test)]
    pub fn from_bytes(b: [u8; 32]) -> SessionKey {
        SessionKey(Zeroizing::new(b))
    }

    fn direction_key(&self, label: &[u8]) -> Zeroizing<[u8; 32]> {
        let hk = Hkdf::<Sha256>::new(Some(SALT), self.0.as_ref());
        let mut okm = Zeroizing::new([0u8; 32]);
        let _ = hk.expand(label, okm.as_mut());
        okm
    }

    /// One sealing key per direction: what `role` sends, and what it receives.
    pub fn channel(&self, role: PairRole) -> (Sealer, Opener) {
        let i2j = self.direction_key(b"jaunt/pair/1/inviter-to-joiner");
        let j2i = self.direction_key(b"jaunt/pair/1/joiner-to-inviter");
        let (send, recv) = match role {
            PairRole::Inviter => (i2j, j2i),
            PairRole::Joiner => (j2i, i2j),
        };
        (
            Sealer { aead: ChaCha20Poly1305::new(Key::from_slice(send.as_ref())), counter: 0 },
            Opener { aead: ChaCha20Poly1305::new(Key::from_slice(recv.as_ref())), counter: 0 },
        )
    }

    /// The human check: SHA-256 over the key and both real ids (inviter first).
    pub fn human_check(&self, inviter: &MachineId, joiner: &MachineId) -> HumanCheck {
        let mut h = Sha256::new();
        h.update(b"jaunt/pair/1/check");
        h.update(self.0.as_ref());
        h.update(inviter.0);
        h.update(joiner.0);
        let d: [u8; 32] = h.finalize().into();
        HumanCheck::from_hash(&d)
    }
}

/// Sealed messages: each direction has its own key and a counter nonce, so a message
/// cannot be replayed, reordered or reflected back at its sender.
pub struct Sealer {
    aead: ChaCha20Poly1305,
    counter: u64,
}

pub struct Opener {
    aead: ChaCha20Poly1305,
    counter: u64,
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    *Nonce::from_slice(&n)
}

impl Sealer {
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let ct = self.aead.encrypt(&nonce(self.counter), plaintext).map_err(|_| anyhow!("cannot seal a message"))?;
        self.counter += 1;
        Ok(ct)
    }
}

/// A sealed message did not authenticate: wrong key (wrong code), tampering, replay.
#[derive(Debug, thiserror::Error)]
#[error("a sealed message did not open")]
pub struct NotOpened;

impl Opener {
    pub fn open(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NotOpened> {
        let p = self.aead.decrypt(&nonce(self.counter), ciphertext).map_err(|_| NotOpened)?;
        self.counter += 1;
        Ok(p)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanCheck {
    /// 5 rows of 5 cells, `#` or `.`, mirror-symmetric left/right.
    pub sigil: Vec<String>,
    pub hue: u8,
    pub words: Vec<String>,
}

impl HumanCheck {
    fn from_hash(d: &[u8; 32]) -> HumanCheck {
        // 15 bits pick the left three columns; the right two mirror them.
        let bits = u16::from_be_bytes([d[12], d[13]]);
        let sigil = (0..5)
            .map(|r| {
                let cols: Vec<bool> = (0..3).map(|c| bits >> (r * 3 + c) & 1 == 1).collect();
                [cols[0], cols[1], cols[2], cols[1], cols[0]].iter().map(|&on| if on { '#' } else { '.' }).collect()
            })
            .collect();
        HumanCheck { sigil, hue: d[14] % 6, words: check_words(d) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(w: &str) -> Code {
        Code { nameplate: "1234".into(), words: [w.into(), "kudos".into()] }
    }

    #[test]
    fn same_code_same_keys() {
        let c = code("tremor");
        let (i, mi) = Pake::start(PairRole::Inviter, &c);
        let (j, mj) = Pake::start(PairRole::Joiner, &c);
        let ki = i.finish(&mj).unwrap();
        let kj = j.finish(&mi).unwrap();
        let (mut si, mut oi) = ki.channel(PairRole::Inviter);
        let (mut sj, mut oj) = kj.channel(PairRole::Joiner);
        let ct = si.seal(b"hello").unwrap();
        assert_eq!(oj.open(&ct).unwrap(), b"hello");
        let ct = sj.seal(b"back").unwrap();
        assert_eq!(oi.open(&ct).unwrap(), b"back");
        let (a, b) = (MachineId([1; 32]), MachineId([2; 32]));
        assert_eq!(ki.human_check(&a, &b), kj.human_check(&a, &b));
        assert_ne!(ki.human_check(&a, &b), ki.human_check(&b, &a));
    }

    #[test]
    fn different_code_keys_do_not_open() {
        let (i, mi) = Pake::start(PairRole::Inviter, &code("tremor"));
        let (j, mj) = Pake::start(PairRole::Joiner, &code("zoom"));
        let (_, mut oi) = i.finish(&mj).unwrap().channel(PairRole::Inviter);
        let (mut sj, _) = j.finish(&mi).unwrap().channel(PairRole::Joiner);
        assert!(oi.open(&sj.seal(b"x").unwrap()).is_err());
    }

    #[test]
    fn tamper_reorder_and_reflection_fail() {
        let k = SessionKey::from_bytes([7; 32]);
        let (mut a, _) = k.channel(PairRole::Inviter);
        let (_, mut b) = k.channel(PairRole::Joiner);
        let m0 = a.seal(b"zero").unwrap();
        let m1 = a.seal(b"one").unwrap();
        // tampered
        let mut bad = m0.clone();
        bad[0] ^= 1;
        assert!(b.open(&bad).is_err());
        // out of order
        assert!(b.open(&m1).is_err());
        assert_eq!(b.open(&m0).unwrap(), b"zero");
        // replay of an already-opened message
        assert!(b.open(&m0).is_err());
        assert_eq!(b.open(&m1).unwrap(), b"one");
        // reflected back at the sender
        let (mut a2, mut a2_in) = k.channel(PairRole::Inviter);
        let mine = a2.seal(b"mine").unwrap();
        assert!(a2_in.open(&mine).is_err());
    }

    #[test]
    fn spake_rejects_garbage_and_same_role() {
        let c = code("tremor");
        let (i, _) = Pake::start(PairRole::Inviter, &c);
        assert!(i.finish(&[0u8; 5]).is_err());
        let (i, _) = Pake::start(PairRole::Inviter, &c);
        let (_, mi2) = Pake::start(PairRole::Inviter, &c);
        assert!(i.finish(&mi2).is_err(), "an inviter message reflected to an inviter must fail");
    }

    #[test]
    fn nameplate_key_is_deterministic() {
        assert_eq!(nameplate_secret("1234").public(), nameplate_secret("1234").public());
        assert_ne!(nameplate_secret("1234").public(), nameplate_secret("1235").public());
    }

    #[test]
    fn sigil_is_symmetric() {
        let k = SessionKey::from_bytes([9; 32]);
        let h = k.human_check(&MachineId([1; 32]), &MachineId([2; 32]));
        assert_eq!(h.sigil.len(), 5);
        for row in &h.sigil {
            let r: Vec<char> = row.chars().collect();
            assert_eq!(r.len(), 5);
            assert_eq!(r[0], r[4]);
            assert_eq!(r[1], r[3]);
        }
        assert_eq!(h.words.len(), 3);
        assert!(h.hue < 6);
    }
}

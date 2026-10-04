//! The roster: who is in the mesh. A small replicated set (DESIGN §7).
//!
//! - entries `{id, name, added_at, added_by, named_at}`: union; for the same id the entry
//!   with the greatest `(named_at, name, added_at, added_by)` wins, so renames propagate.
//! - tombstones `{id, revoked_at, revoked_by}`: union, permanent; for the same id the
//!   earliest `(revoked_at, revoked_by)` is kept. A tombstone always beats an entry.
//! - fleet settings: last writer wins by `(updated_at, updated_by, relays)`.
//!
//! Every choice is a max/min over a total order, so merge is commutative, associative and
//! idempotent. Ids are compared as raw key bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::paths::{Paths, read_capped, write_private_atomic};

pub const MAX_NAME: usize = 32;
/// Current members (entries without a tombstone).
pub const MAX_MEMBERS: usize = 256;
/// Entries of every kind, including those of revoked machines (kept for their names).
pub const MAX_ENTRIES: usize = 1024;
/// Tombstones for ids that have no entry here. Tombstones for known ids are never refused
/// (at most one per entry), so the total stays below `MAX_ENTRIES + MAX_FOREIGN_TOMBSTONES`.
pub const MAX_FOREIGN_TOMBSTONES: usize = 512;
pub const MAX_TOMBSTONES: usize = MAX_ENTRIES + MAX_FOREIGN_TOMBSTONES;
pub const MAX_RELAYS: usize = 8;
pub const MAX_RELAY_URL: usize = 256;
const MAX_ROSTER_FILE: u64 = 1 << 20;
/// Timestamps (ms) beyond 2100-01-01 are treated as garbage. A fixed bound, not "now + x",
/// so every machine sanitises the same input the same way and merge stays deterministic.
pub const MAX_TIME: u64 = 4_102_444_800_000;

/// A machine id: the raw 32 bytes of its ed25519 public key (iroh `EndpointId`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineId(pub [u8; 32]);

impl MachineId {
    pub fn from_endpoint(id: &iroh::EndpointId) -> MachineId {
        MachineId(*id.as_bytes())
    }
    pub fn to_endpoint(self) -> Result<iroh::EndpointId> {
        iroh::EndpointId::from_bytes(&self.0).map_err(|_| anyhow::anyhow!("{self} is not a valid ed25519 public key"))
    }
    pub fn short(&self) -> String {
        self.to_string()[..8].to_string()
    }
}

impl fmt::Display for MachineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&data_encoding::HEXLOWER.encode(&self.0))
    }
}
impl fmt::Debug for MachineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MachineId({})", self.short())
    }
}
impl FromStr for MachineId {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.len() != 64 {
            bail!("a machine id is 64 hex characters, got {}", s.len());
        }
        let b = data_encoding::HEXLOWER_PERMISSIVE.decode(s.as_bytes()).context("a machine id is hex")?;
        let arr: [u8; 32] = b.try_into().map_err(|_| anyhow::anyhow!("bad id length"))?;
        Ok(MachineId(arr))
    }
}
impl Serialize for MachineId {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for MachineId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Names: 1..=32 of `[a-z0-9-]`, not starting or ending with `-`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: MachineId,
    pub name: String,
    pub added_at: u64,
    pub added_by: MachineId,
    #[serde(default)]
    pub named_at: u64,
}

impl Entry {
    fn rank(&self) -> (u64, &str, u64, &MachineId) {
        (self.named_at, &self.name, self.added_at, &self.added_by)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub id: MachineId,
    pub revoked_at: u64,
    pub revoked_by: MachineId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Fleet {
    pub relays: Vec<String>,
    /// 0 = never set.
    pub updated_at: u64,
    pub updated_by: Option<MachineId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Roster {
    #[serde(default)]
    pub entries: BTreeMap<MachineId, Entry>,
    #[serde(default)]
    pub tombstones: BTreeMap<MachineId, Tombstone>,
    #[serde(default)]
    pub fleet: Fleet,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Resolve {
    Found(MachineId),
    NotFound,
    Ambiguous(Vec<String>),
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Roster {
    pub fn load(paths: &Paths) -> Result<Roster> {
        let path = paths.roster_file();
        match read_capped(&path, MAX_ROSTER_FILE)? {
            None => Ok(Roster::default()),
            Some(b) => {
                let r: Roster = serde_json::from_slice(&b).with_context(|| format!("cannot parse {}", path.display()))?;
                r.validate().with_context(|| format!("{} is invalid", path.display()))?;
                Ok(r)
            }
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let mut text = serde_json::to_vec_pretty(self)?;
        text.push(b'\n');
        write_private_atomic(&paths.roster_file(), &text)
    }

    /// Check a roster (from disk or from a peer) for shape and size.
    pub fn validate(&self) -> Result<()> {
        let members = self.members().count();
        if members > MAX_MEMBERS {
            bail!("roster has {members} members (limit {MAX_MEMBERS})");
        }
        if self.entries.len() > MAX_ENTRIES {
            bail!("roster has {} entries (limit {MAX_ENTRIES})", self.entries.len());
        }
        if self.tombstones.len() > MAX_TOMBSTONES {
            bail!("roster has {} tombstones (limit {MAX_TOMBSTONES})", self.tombstones.len());
        }
        for (k, e) in &self.entries {
            if *k != e.id {
                bail!("roster entry key {} does not match its id {}", k.short(), e.id.short());
            }
            if !valid_name(&e.name) {
                bail!("roster entry {} has an invalid name", e.id.short());
            }
        }
        for (k, t) in &self.tombstones {
            if *k != t.id {
                bail!("roster tombstone key does not match its id");
            }
        }
        if self.fleet.relays.len() > MAX_RELAYS {
            bail!("too many relays in fleet settings");
        }
        for r in &self.fleet.relays {
            if r.len() > MAX_RELAY_URL || r.parse::<iroh::RelayUrl>().is_err() {
                bail!("fleet settings contain an invalid relay URL");
            }
        }
        Ok(())
    }

    pub fn is_member(&self, id: &MachineId) -> bool {
        self.entries.contains_key(id) && !self.tombstones.contains_key(id)
    }

    /// Members (entry and no tombstone), in id order.
    pub fn members(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values().filter(|e| !self.tombstones.contains_key(&e.id))
    }

    /// Merge `other` into `self`. Returns true when anything changed.
    pub fn merge(&mut self, other: &Roster) -> bool {
        let before = self.clone();
        for (id, theirs) in &other.entries {
            match self.entries.get(id) {
                Some(ours) if ours.rank() >= theirs.rank() => {}
                _ => {
                    self.entries.insert(*id, theirs.clone());
                }
            }
        }
        for (id, theirs) in &other.tombstones {
            match self.tombstones.get(id) {
                Some(ours) if (ours.revoked_at, ours.revoked_by) <= (theirs.revoked_at, theirs.revoked_by) => {}
                _ => {
                    self.tombstones.insert(*id, theirs.clone());
                }
            }
        }
        let rank = |f: &Fleet| (f.updated_at, f.updated_by, f.relays.clone());
        if rank(&other.fleet) > rank(&self.fleet) {
            self.fleet = other.fleet.clone();
        }
        *self != before
    }

    /// Merge a roster that came from another machine without ever exceeding the limits,
    /// and without letting the limits block a revocation (R1): tombstones go first, and a
    /// tombstone for an id this roster knows is always taken; only tombstones for unknown
    /// ids, and new entries, are skipped once the limits are reached. Below the limits it
    /// is exactly `merge`. Returns (changed, how many items were skipped).
    pub fn merge_bounded(&mut self, other: &Roster) -> (bool, usize) {
        let before = self.clone();
        let mut skipped = 0;
        for (id, theirs) in &other.tombstones {
            match self.tombstones.get(id) {
                Some(ours) if (ours.revoked_at, ours.revoked_by) <= (theirs.revoked_at, theirs.revoked_by) => {}
                Some(_) => {
                    self.tombstones.insert(*id, theirs.clone());
                }
                None => {
                    let known = self.entries.contains_key(id) || other.entries.contains_key(id);
                    let foreign = self.tombstones.keys().filter(|t| !self.entries.contains_key(t)).count();
                    if known || foreign < MAX_FOREIGN_TOMBSTONES {
                        self.tombstones.insert(*id, theirs.clone());
                    } else {
                        skipped += 1;
                    }
                }
            }
        }
        for (id, theirs) in &other.entries {
            match self.entries.get(id) {
                Some(ours) if ours.rank() >= theirs.rank() => {}
                Some(_) => {
                    self.entries.insert(*id, theirs.clone());
                }
                None => {
                    let revoked = self.tombstones.contains_key(id);
                    let room = self.entries.len() < MAX_ENTRIES && (revoked || self.members().count() < MAX_MEMBERS);
                    if room {
                        self.entries.insert(*id, theirs.clone());
                    } else {
                        skipped += 1;
                    }
                }
            }
        }
        let rank = |f: &Fleet| (f.updated_at, f.updated_by, f.relays.clone());
        if rank(&other.fleet) > rank(&self.fleet) {
            self.fleet = other.fleet.clone();
        }
        (*self != before, skipped)
    }

    /// Drop what cannot be right in a roster from another machine, item by item, instead of
    /// refusing the whole roster (which would also refuse its tombstones): entries filed
    /// under another id, with a bad name, or whose id is not an ed25519 public key;
    /// tombstones filed under another id; fleet settings with a bad relay list.
    pub fn drop_malformed(&mut self) -> Vec<String> {
        let mut dropped = Vec::new();
        self.entries.retain(|k, e| {
            let ok = *k == e.id && valid_name(&e.name) && e.id.to_endpoint().is_ok();
            if !ok {
                dropped.push(format!("entry {} (malformed)", k.short()));
            }
            ok
        });
        self.tombstones.retain(|k, t| {
            let ok = *k == t.id;
            if !ok {
                dropped.push(format!("tombstone {} (filed under another id)", k.short()));
            }
            ok
        });
        let fleet_ok = self.fleet.relays.len() <= MAX_RELAYS
            && self.fleet.relays.iter().all(|r| r.len() <= MAX_RELAY_URL && r.parse::<iroh::RelayUrl>().is_ok());
        if !fleet_ok {
            dropped.push("fleet settings (bad relay list)".into());
            self.fleet = Fleet::default();
        }
        dropped
    }

    /// Add (or re-add with a new name) a machine. Refuses a tombstoned id.
    pub fn add(&mut self, id: MachineId, name: &str, by: MachineId, now: u64) -> Result<bool> {
        if !valid_name(name) {
            bail!("'{name}' is not a valid machine name (use a-z, 0-9 and '-', at most {MAX_NAME})");
        }
        if self.tombstones.contains_key(&id) {
            bail!("machine {} was unlinked; a revoked key can never come back (run 'jaunt reset' there and link again)", id.short());
        }
        let mut incoming = Roster::default();
        let prev = self.entries.get(&id);
        let entry = Entry {
            id,
            name: name.to_string(),
            added_at: prev.map(|p| p.added_at).unwrap_or(now),
            added_by: prev.map(|p| p.added_by).unwrap_or(by),
            named_at: now.max(prev.map(|p| p.named_at.saturating_add(1)).unwrap_or(0)),
        };
        if prev.map(|p| p.name == name).unwrap_or(false) {
            return Ok(false);
        }
        incoming.entries.insert(id, entry);
        Ok(self.merge(&incoming))
    }

    pub fn rename(&mut self, id: MachineId, name: &str, now: u64) -> Result<bool> {
        let by = self.entries.get(&id).map(|e| e.added_by).unwrap_or(id);
        self.add(id, name, by, now)
    }

    pub fn revoke(&mut self, id: MachineId, by: MachineId, now: u64) -> bool {
        let mut incoming = Roster::default();
        incoming.tombstones.insert(id, Tombstone { id, revoked_at: now, revoked_by: by });
        self.merge(&incoming)
    }

    pub fn set_relays(&mut self, relays: Vec<String>, by: MachineId, now: u64) -> bool {
        let at = now.max(self.fleet.updated_at.saturating_add(1));
        let incoming = Roster { fleet: Fleet { relays, updated_at: at, updated_by: Some(by) }, ..Default::default() };
        self.merge(&incoming)
    }

    /// Clean a roster that came from another machine before merging it. Entries and fleet
    /// settings with timestamps past [`MAX_TIME`] are dropped (they could otherwise pin a
    /// name or the relay list forever); tombstones are never dropped, only clamped, so no
    /// input can make a revocation disappear. Returns what was dropped, in words.
    pub fn sanitize_remote(&mut self) -> Vec<String> {
        let mut dropped = Vec::new();
        self.entries.retain(|id, e| {
            let ok = e.added_at <= MAX_TIME && e.named_at <= MAX_TIME;
            if !ok {
                dropped.push(format!("entry {} (timestamp in the far future)", id.short()));
            }
            ok
        });
        for t in self.tombstones.values_mut() {
            t.revoked_at = t.revoked_at.min(MAX_TIME);
        }
        if self.fleet.updated_at > MAX_TIME {
            dropped.push("fleet settings (timestamp in the far future)".into());
            self.fleet = Fleet::default();
        }
        dropped
    }

    /// Members that `by` added, directly or through machines it added (not `by` itself).
    pub fn added_by_closure(&self, by: &MachineId) -> BTreeSet<MachineId> {
        let mut out = BTreeSet::new();
        let mut frontier = vec![*by];
        while let Some(x) = frontier.pop() {
            for e in self.members() {
                if e.added_by == x && e.id != *by && e.id != x && out.insert(e.id) {
                    frontier.push(e.id);
                }
            }
        }
        out
    }

    /// Ids tombstoned in `self` but not in `before`.
    pub fn new_tombstones(&self, before: &Roster) -> Vec<MachineId> {
        self.tombstones.keys().filter(|id| !before.tombstones.contains_key(id)).copied().collect()
    }

    /// Names to show for members. Two machines with the same name are told apart
    /// deterministically: the earlier-added keeps the plain name, later ones get
    /// `name-<first 4 hex of id>`.
    pub fn display_names(&self) -> BTreeMap<MachineId, String> {
        let mut by_name: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
        for e in self.members() {
            by_name.entry(&e.name).or_default().push(e);
        }
        let mut out = BTreeMap::new();
        for (name, mut group) in by_name {
            group.sort_by_key(|a| (a.added_at, a.id));
            for (i, e) in group.iter().enumerate() {
                let shown = if i == 0 { name.to_string() } else { format!("{name}-{}", &e.id.to_string()[..4]) };
                out.insert(e.id, shown);
            }
        }
        out
    }

    pub fn display_name(&self, id: &MachineId) -> String {
        self.display_names().remove(id).unwrap_or_else(|| {
            self.entries.get(id).map(|e| e.name.clone()).unwrap_or_else(|| id.short())
        })
    }

    /// Find a member by exact name, unambiguous name prefix, or id prefix (>= 4 hex chars).
    /// `exclude` (this machine) is never matched.
    pub fn resolve(&self, query: &str, exclude: Option<&MachineId>) -> Resolve {
        let names = self.display_names();
        let candidates: Vec<(&MachineId, &String)> =
            names.iter().filter(|(id, _)| Some(*id) != exclude).collect();
        if let Some((id, _)) = candidates.iter().find(|(_, n)| n.as_str() == query) {
            return Resolve::Found(**id);
        }
        let mut hits: BTreeSet<&MachineId> =
            candidates.iter().filter(|(_, n)| n.starts_with(query)).map(|(id, _)| *id).collect();
        if hits.is_empty() && query.len() >= 4 && query.bytes().all(|b| b.is_ascii_hexdigit()) {
            let q = query.to_ascii_lowercase();
            hits = candidates.iter().filter(|(id, _)| id.to_string().starts_with(&q)).map(|(id, _)| *id).collect();
        }
        if hits.len() > 1 {
            return Resolve::Ambiguous(hits.iter().filter_map(|id| names.get(id).cloned()).collect());
        }
        hits.into_iter().next().map(|id| Resolve::Found(*id)).unwrap_or(Resolve::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(b: u8) -> MachineId {
        MachineId([b; 32])
    }
    fn entry(i: u8, name: &str, at: u64, by: u8) -> Entry {
        Entry { id: id(i), name: name.into(), added_at: at, added_by: id(by), named_at: at }
    }
    fn roster(entries: &[Entry], tombs: &[(u8, u64, u8)]) -> Roster {
        let mut r = Roster::default();
        for e in entries {
            r.entries.insert(e.id, e.clone());
        }
        for &(i, at, by) in tombs {
            r.tombstones.insert(id(i), Tombstone { id: id(i), revoked_at: at, revoked_by: id(by) });
        }
        r
    }
    fn merged(a: &Roster, b: &Roster) -> Roster {
        let mut x = a.clone();
        x.merge(b);
        x
    }

    fn samples() -> Vec<Roster> {
        let mut f1 = roster(&[entry(1, "fern", 10, 1)], &[]);
        f1.fleet = Fleet { relays: vec!["https://r1.example".into()], updated_at: 5, updated_by: Some(id(1)) };
        let mut f2 = roster(&[entry(2, "moss", 20, 1), entry(1, "fern2", 30, 2)], &[(3, 40, 2)]);
        f2.fleet = Fleet { relays: vec!["https://r2.example".into()], updated_at: 7, updated_by: Some(id(2)) };
        let f3 = roster(&[entry(3, "tin", 15, 1), entry(2, "moss", 20, 1)], &[(3, 35, 1), (1, 50, 3)]);
        vec![Roster::default(), f1, f2, f3]
    }

    #[test]
    fn merge_is_commutative() {
        let s = samples();
        for a in &s {
            for b in &s {
                assert_eq!(merged(a, b), merged(b, a));
            }
        }
    }

    #[test]
    fn merge_is_associative_and_idempotent() {
        let s = samples();
        for a in &s {
            assert_eq!(merged(a, a), *a);
            let mut x = a.clone();
            assert!(!x.merge(a));
            for b in &s {
                let ab = merged(a, b);
                assert_eq!(merged(&ab, b), ab);
                for c in &s {
                    assert_eq!(merged(&merged(a, b), c), merged(a, &merged(b, c)));
                }
            }
        }
    }

    #[test]
    fn tombstone_wins_and_is_permanent() {
        let mut a = roster(&[entry(1, "fern", 10, 1), entry(2, "moss", 10, 1)], &[]);
        assert!(a.is_member(&id(2)));
        assert!(a.revoke(id(2), id(1), 100));
        assert!(!a.is_member(&id(2)));
        // a later re-add from a peer who never heard of the revocation does not resurrect it
        let stale = roster(&[Entry { named_at: 500, ..entry(2, "moss", 400, 3) }], &[]);
        a.merge(&stale);
        assert!(!a.is_member(&id(2)));
        assert!(a.add(id(2), "moss", id(1), 600).is_err());
        // earliest tombstone kept
        let later = roster(&[], &[(2, 900, 3)]);
        a.merge(&later);
        assert_eq!(a.tombstones[&id(2)].revoked_at, 100);
        assert_eq!(a.members().count(), 1);
    }

    #[test]
    fn rename_propagates_by_named_at() {
        let mut a = roster(&[entry(1, "fern", 10, 1)], &[]);
        let mut b = a.clone();
        assert!(b.rename(id(1), "bracken", 20).unwrap());
        assert!(a.merge(&b));
        assert_eq!(a.entries[&id(1)].name, "bracken");
        assert_eq!(a.entries[&id(1)].added_at, 10);
        // same name again: no change
        assert!(!a.rename(id(1), "bracken", 30).unwrap());
    }

    #[test]
    fn fleet_last_writer_wins() {
        let mut a = Roster::default();
        let mut b = Roster::default();
        a.set_relays(vec!["https://a.example".into()], id(1), 100);
        b.set_relays(vec!["https://b.example".into()], id(2), 200);
        let ab = merged(&a, &b);
        assert_eq!(ab.fleet.relays, vec!["https://b.example".to_string()]);
        assert_eq!(merged(&b, &a), ab);
        // same timestamp: tie broken by writer id bytes
        let mut c = Roster::default();
        c.set_relays(vec!["https://c.example".into()], id(3), 200);
        assert_eq!(merged(&b, &c).fleet.relays, vec!["https://c.example".to_string()]);
    }

    #[test]
    fn ids_compare_as_raw_bytes() {
        let mut lo = [0u8; 32];
        lo[31] = 1;
        let mut hi = [0u8; 32];
        hi[0] = 1;
        assert!(MachineId(lo) < MachineId(hi));
        let s = MachineId(hi).to_string();
        let parsed: MachineId = s.to_uppercase().parse().unwrap();
        assert_eq!(parsed, MachineId(hi));
        assert!("abc".parse::<MachineId>().is_err());
        assert!("zz".repeat(32).parse::<MachineId>().is_err());
    }

    #[test]
    fn name_collisions_are_disambiguated() {
        let r = roster(&[entry(0xab, "fern", 10, 1), entry(0x12, "fern", 20, 1), entry(3, "moss", 5, 1)], &[]);
        let names = r.display_names();
        assert_eq!(names[&id(0xab)], "fern");
        assert_eq!(names[&id(0x12)], "fern-1212");
        assert_eq!(names[&id(3)], "moss");
        assert_eq!(r.resolve("fern", None), Resolve::Found(id(0xab)));
        assert_eq!(r.resolve("fern-1", None), Resolve::Found(id(0x12)));
        match r.resolve("fe", None) {
            Resolve::Ambiguous(v) => assert_eq!(v.len(), 2),
            other => panic!("{other:?}"),
        }
        assert_eq!(r.resolve("m", None), Resolve::Found(id(3)));
        assert_eq!(r.resolve("m", Some(&id(3))), Resolve::NotFound);
        assert_eq!(r.resolve("0303", None), Resolve::Found(id(3)));
        assert_eq!(r.resolve("zzz", None), Resolve::NotFound);
    }

    #[test]
    fn validate_rejects_bad_input() {
        let mut r = roster(&[entry(1, "fern", 10, 1)], &[]);
        assert!(r.validate().is_ok());
        r.entries.get_mut(&id(1)).unwrap().name = "Bad Name".into();
        assert!(r.validate().is_err());
        let mut r = roster(&[entry(1, "fern", 10, 1)], &[]);
        r.entries.insert(id(2), entry(1, "x", 1, 1));
        assert!(r.validate().is_err());
        let mut r = Roster::default();
        r.fleet.relays = vec!["not a url".into()];
        assert!(r.validate().is_err());
        assert!(serde_json::from_str::<Roster>(r#"{"entries":{"zz":{}}}"#).is_err());
    }

    /// A small deterministic generator of hostile rosters: few ids (so they collide), and
    /// timestamps from 0 to u64::MAX.
    struct Gen(u64);
    impl Gen {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn time(&mut self) -> u64 {
            match self.next() % 5 {
                0 => 0,
                1 => u64::MAX,
                2 => MAX_TIME + self.next() % 1000,
                _ => self.next() % 1000,
            }
        }
        fn roster(&mut self) -> Roster {
            let mut r = Roster::default();
            for _ in 0..self.next() % 5 {
                let i = (self.next() % 6) as u8;
                let names = ["fern", "moss", "tin", "x"];
                let e = Entry {
                    id: id(i),
                    name: names[(self.next() % 4) as usize].into(),
                    added_at: self.time(),
                    added_by: id((self.next() % 6) as u8),
                    named_at: self.time(),
                };
                r.entries.insert(e.id, e);
            }
            for _ in 0..self.next() % 3 {
                let i = (self.next() % 6) as u8;
                r.tombstones.insert(id(i), Tombstone { id: id(i), revoked_at: self.time(), revoked_by: id((self.next() % 6) as u8) });
            }
            if self.next().is_multiple_of(2) {
                r.fleet = Fleet { relays: vec![format!("https://r{}.example", self.next() % 3)], updated_at: self.time(), updated_by: Some(id(1)) };
            }
            r
        }
    }

    #[test]
    fn merge_laws_hold_for_hostile_rosters() {
        let mut g = Gen(0x9e3779b97f4a7c15);
        for _ in 0..300 {
            let (mut a, mut b, mut c) = (g.roster(), g.roster(), g.roster());
            for r in [&mut a, &mut b, &mut c] {
                r.sanitize_remote();
            }
            assert_eq!(merged(&a, &b), merged(&b, &a));
            assert_eq!(merged(&merged(&a, &b), &c), merged(&a, &merged(&b, &c)));
            assert_eq!(merged(&a, &a), a);
            // Revocation is monotone: whatever is merged in, a tombstoned id stays revoked.
            let ab = merged(&a, &b);
            for t in a.tombstones.keys().chain(b.tombstones.keys()) {
                assert!(!ab.is_member(t));
                assert!(ab.tombstones.contains_key(t));
            }
        }
    }

    #[test]
    fn bounded_merge_equals_merge_below_the_limits() {
        let mut g = Gen(0x1234_5678_9abc_def1);
        for _ in 0..300 {
            let (mut a, mut b) = (g.roster(), g.roster());
            a.sanitize_remote();
            b.sanitize_remote();
            let mut x = a.clone();
            let (changed, skipped) = x.merge_bounded(&b);
            assert_eq!(skipped, 0);
            assert_eq!(x, merged(&a, &b));
            assert_eq!(changed, x != a);
        }
    }

    /// R1: a member pushes junk entries until the rosters are full. A tombstone that arrives
    /// in the same or a later roster is still taken, and the result stays within the limits.
    #[test]
    fn full_rosters_never_block_a_tombstone() {
        let id16 = |n: u16| {
            let mut b = [0u8; 32];
            b[..2].copy_from_slice(&n.to_be_bytes());
            b[31] = 0x77;
            MachineId(b)
        };
        let mut b = roster(&[entry(1, "a", 1, 1), entry(2, "b", 2, 1), entry(3, "c", 3, 1)], &[]);
        let mut junk = Roster::default();
        for n in 0..2000u16 {
            let e = Entry { id: id16(n), name: "x".into(), added_at: 9, added_by: id(3), named_at: 9 };
            junk.entries.insert(e.id, e);
        }
        for n in 2000..3000u16 {
            junk.tombstones.insert(id16(n), Tombstone { id: id16(n), revoked_at: 9, revoked_by: id(3) });
        }
        let (changed, skipped) = b.merge_bounded(&junk);
        assert!(changed && skipped > 0);
        b.validate().expect("a bounded merge stays within the limits");
        assert_eq!(b.members().count(), MAX_MEMBERS);
        // Now a tells b that c (the junk pusher) is revoked, with more junk in the same roster.
        let mut from_a = junk.clone();
        from_a.tombstones.insert(id(3), Tombstone { id: id(3), revoked_at: 50, revoked_by: id(1) });
        b.merge_bounded(&from_a);
        assert!(!b.is_member(&id(3)), "the revocation got through a full roster");
        b.validate().unwrap();
        // The unbounded union would have broken the limits (what used to make sync fail whole).
        let mut naive = roster(&[entry(1, "a", 1, 1), entry(2, "b", 2, 1), entry(3, "c", 3, 1)], &[]);
        naive.merge(&from_a);
        assert!(naive.validate().is_err());
    }

    #[test]
    fn malformed_items_are_dropped_one_by_one() {
        let good = MachineId::from_endpoint(&iroh::SecretKey::from_bytes(&[9; 32]).public());
        let not_a_key = MachineId([2; 32]);
        assert!(not_a_key.to_endpoint().is_err());
        let mut r = roster(&[], &[(5, 10, 1)]);
        r.entries.insert(good, Entry { id: good, name: "ok".into(), added_at: 1, added_by: good, named_at: 1 });
        r.entries.insert(not_a_key, Entry { id: not_a_key, name: "ghost".into(), added_at: 1, added_by: good, named_at: 1 });
        r.fleet.relays = vec!["not a url".into()];
        let dropped = r.drop_malformed();
        assert_eq!(dropped.len(), 2, "{dropped:?}");
        assert!(r.entries.contains_key(&good) && !r.entries.contains_key(&not_a_key));
        assert!(r.tombstones.contains_key(&id(5)), "tombstones survive");
        assert_eq!(r.fleet, Fleet::default());
    }

    #[test]
    fn sanitize_drops_far_future_entries_but_never_tombstones() {
        let mut r = roster(&[Entry { named_at: u64::MAX, ..entry(1, "fern", 10, 1) }, entry(2, "moss", 10, 1)], &[(3, u64::MAX, 2)]);
        r.fleet = Fleet { relays: vec!["https://r.example".into()], updated_at: u64::MAX, updated_by: Some(id(2)) };
        let dropped = r.sanitize_remote();
        assert_eq!(dropped.len(), 2);
        assert!(!r.entries.contains_key(&id(1)));
        assert!(r.entries.contains_key(&id(2)));
        assert_eq!(r.tombstones[&id(3)].revoked_at, MAX_TIME);
        assert_eq!(r.fleet, Fleet::default());
    }

    #[test]
    fn clock_skew_cannot_unrevoke() {
        // Revoked by a machine whose clock says 1970; re-added by one whose clock is far ahead.
        let mut a = roster(&[entry(2, "moss", 10, 1)], &[(2, 0, 1)]);
        let ahead = roster(&[Entry { named_at: MAX_TIME, added_at: MAX_TIME, ..entry(2, "moss", 10, 3) }], &[]);
        a.merge(&ahead);
        assert!(!a.is_member(&id(2)));
        assert!(a.add(id(2), "moss", id(1), MAX_TIME).is_err());
        // A rename from a machine with a normal clock still wins over a far-future name.
        let mut b = roster(&[Entry { named_at: MAX_TIME, ..entry(4, "tin", 10, 1) }], &[]);
        assert!(b.rename(id(4), "tinny", 1_000).unwrap());
        assert_eq!(b.entries[&id(4)].name, "tinny");
        let mut c = roster(&[Entry { named_at: u64::MAX, ..entry(4, "tin", 10, 1) }], &[]);
        assert!(c.rename(id(4), "tinny", 1_000).is_ok(), "no overflow at the top of the clock");
    }

    #[test]
    fn a_revoked_machine_cannot_sync_its_way_back() {
        // b knows c is revoked; c offers its own roster without the tombstone, with itself
        // renamed and "re-added" later. Nothing changes c's status on b.
        let mut b = roster(&[entry(1, "a", 1, 1), entry(2, "b", 2, 1), entry(3, "c", 3, 2)], &[(3, 50, 1)]);
        let cs = roster(&[entry(1, "a", 1, 1), entry(2, "b", 2, 1), Entry { named_at: 999, ..entry(3, "c2", 900, 3) }], &[]);
        b.merge(&cs);
        assert!(!b.is_member(&id(3)));
        // A machine that was offline merges with c first and then with b: revoked either way.
        let mut d = roster(&[entry(1, "a", 1, 1), entry(3, "c", 3, 2), entry(4, "d", 4, 1)], &[]);
        d.merge(&cs);
        assert!(d.is_member(&id(3)), "before hearing from an informed member, d still trusts c (the documented window)");
        d.merge(&b);
        assert!(!d.is_member(&id(3)));
        let mut d2 = roster(&[entry(4, "d", 4, 1)], &[]);
        d2.merge(&b);
        d2.merge(&cs);
        assert!(!d2.is_member(&id(3)));
    }

    #[test]
    fn a_member_may_revoke_others_and_that_spreads() {
        // Flat trust (DESIGN §7): any member's tombstones are accepted, including one for
        // the receiving machine itself.
        let mut a = roster(&[entry(1, "a", 1, 1), entry(2, "b", 2, 1), entry(3, "c", 3, 1)], &[]);
        let from_c = roster(&[], &[(1, 5, 3), (2, 5, 3)]);
        assert!(a.merge(&from_c));
        assert_eq!(a.members().map(|e| e.id).collect::<Vec<_>>(), vec![id(3)]);
    }

    #[test]
    fn added_by_closure_is_transitive_and_skips_the_revoked() {
        let r = roster(
            &[entry(1, "a", 1, 1), entry(2, "b", 2, 1), entry(3, "c", 3, 2), entry(4, "d", 4, 3), entry(5, "e", 5, 1), entry(6, "f", 6, 2)],
            &[(6, 9, 1)],
        );
        let got: Vec<MachineId> = r.added_by_closure(&id(2)).into_iter().collect();
        assert_eq!(got, vec![id(3), id(4)], "c added by b, d added by c; f is already revoked");
        assert!(r.added_by_closure(&id(4)).is_empty());
    }

    #[test]
    fn names() {
        assert!(valid_name("fern"));
        assert!(valid_name("a-1"));
        assert!(!valid_name("-a"));
        assert!(!valid_name("Fern"));
        assert!(!valid_name(""));
        assert!(!valid_name(&"a".repeat(33)));
        assert!(!valid_name("a b"));
    }
}

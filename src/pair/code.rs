//! Pairing codes: `NNNN-word-word`. The 4 digits are the nameplate (not secret, it picks
//! the rendezvous endpoint); the two words come from the EFF large wordlist (7776 words,
//! 25.8 bits). Everything is drawn from the OS CSPRNG. Entry is forgiving: case, spaces,
//! underscores and repeated separators do not matter.

use std::fmt;

use anyhow::{Result, bail};

/// EFF large wordlist (https://www.eff.org/files/2016/07/18/eff_large_wordlist.txt,
/// sha256 addd3553…903e), words only, one per line, in the original order.
const WORDLIST: &str = include_str!("eff_large_wordlist.txt");
pub const WORDS: usize = 7776;

pub fn words() -> impl Iterator<Item = &'static str> {
    WORDLIST.lines()
}

pub fn word(i: usize) -> &'static str {
    words().nth(i % WORDS).unwrap_or("?")
}

fn is_word(w: &str) -> bool {
    words().any(|x| x == w)
}

/// A uniformly random integer below `n` (n ≤ 65536) from the OS CSPRNG.
pub fn random_below(n: u32) -> Result<u32> {
    debug_assert!(n > 0 && n <= 65536);
    let limit = 65536 - 65536 % n;
    loop {
        let mut b = [0u8; 2];
        getrandom::getrandom(&mut b).map_err(|e| anyhow::anyhow!("the OS random number generator failed: {e}"))?;
        let v = u16::from_be_bytes(b) as u32;
        if v < limit {
            return Ok(v % n);
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Code {
    pub nameplate: String,
    pub words: [String; 2],
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}-{}", self.nameplate, self.words[0], self.words[1])
    }
}

// Never print the secret part by accident.
impl fmt::Debug for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Code({}-…)", self.nameplate)
    }
}

impl Code {
    pub fn generate() -> Result<Code> {
        let nameplate = format!("{:04}", random_below(10_000)?);
        let w0 = word(random_below(WORDS as u32)? as usize).to_string();
        let w1 = word(random_below(WORDS as u32)? as usize).to_string();
        Ok(Code { nameplate, words: [w0, w1] })
    }

    /// The SPAKE2 password: the canonical text of the whole code.
    pub fn password(&self) -> Vec<u8> {
        self.to_string().into_bytes()
    }

    /// Parse what a human typed. Words may themselves contain a dash (`t-shirt`), so the
    /// tokens after the digits are split into two list words in every possible way and
    /// exactly one split must work.
    pub fn parse(input: &str) -> Result<Code> {
        const EXAMPLE: &str = "the code looks like 4127-tremor-kudos";
        if input.len() > 128 {
            bail!("that is too long for a code ({EXAMPLE})");
        }
        let lower = input.trim().to_lowercase();
        let tokens: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| !t.is_empty()).collect();
        let Some((first, rest)) = tokens.split_first() else { bail!("no code given ({EXAMPLE})") };
        if first.len() != 4 || !first.bytes().all(|b| b.is_ascii_digit()) {
            bail!("a code starts with 4 digits ({EXAMPLE})");
        }
        if rest.is_empty() {
            bail!("the code needs its two words too ({EXAMPLE})");
        }
        for t in rest {
            if !t.bytes().all(|b| b.is_ascii_lowercase()) {
                bail!("'{t}' is not a code word ({EXAMPLE})");
            }
        }
        let mut found = Vec::new();
        for cut in 1..rest.len() {
            let a = rest[..cut].join("-");
            let b = rest[cut..].join("-");
            if is_word(&a) && is_word(&b) {
                found.push([a, b]);
            }
        }
        match found.len() {
            1 => Ok(Code { nameplate: first.to_string(), words: found.remove(0) }),
            0 => {
                if let Some(bad) = rest.iter().find(|t| !is_word(t) && !words().any(|w| w.split('-').any(|p| p == **t))) {
                    bail!("'{bad}' is not one of the code words; check the spelling ({EXAMPLE})");
                }
                bail!("the code should have exactly two words after the digits ({EXAMPLE})")
            }
            _ => bail!("the words in that code can be read two ways; type them with dashes as shown"),
        }
    }
}

/// Three words for the human check, from 32 bytes of hash.
pub fn check_words(h: &[u8; 32]) -> Vec<String> {
    (0..3)
        .map(|i| {
            let v = u32::from_be_bytes([h[4 * i], h[4 * i + 1], h[4 * i + 2], h[4 * i + 3]]);
            word(v as usize % WORDS).to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wordlist_is_complete_and_unique() {
        let all: Vec<&str> = words().collect();
        assert_eq!(all.len(), WORDS);
        let set: std::collections::BTreeSet<&str> = all.iter().copied().collect();
        assert_eq!(set.len(), WORDS);
        assert_eq!(all[0], "abacus");
        assert_eq!(all[WORDS - 1], "zoom");
    }

    #[test]
    fn generated_codes_parse_back() {
        for _ in 0..200 {
            let c = Code::generate().unwrap();
            assert_eq!(c.nameplate.len(), 4);
            assert_eq!(Code::parse(&c.to_string()).unwrap(), c);
        }
    }

    #[test]
    fn entry_is_forgiving() {
        let want = Code { nameplate: "0427".into(), words: ["tremor".into(), "kudos".into()] };
        for s in ["0427-tremor-kudos", " 0427 Tremor KUDOS ", "0427_tremor__kudos", "0427 - tremor - kudos", "0427.tremor.kudos"] {
            assert_eq!(Code::parse(s).unwrap(), want, "{s}");
        }
    }

    #[test]
    fn dashed_words_parse() {
        for (a, b) in [("t-shirt", "down"), ("drop-down", "yo-yo"), ("felt-tip", "shirt"), ("down", "t-shirt")] {
            let c = Code { nameplate: "1234".into(), words: [a.into(), b.into()] };
            assert_eq!(Code::parse(&c.to_string()).unwrap(), c);
            assert_eq!(Code::parse(&format!("1234 {} {}", a.replace('-', " "), b.replace('-', " "))).unwrap(), c);
        }
    }

    #[test]
    fn bad_codes_are_explained() {
        assert!(Code::parse("").is_err());
        assert!(Code::parse("123-tremor-kudos").is_err());
        assert!(Code::parse("1234").is_err());
        assert!(Code::parse("1234-tremor").is_err());
        assert!(Code::parse("1234-tremor-kudos-zoom").is_err());
        let e = Code::parse("1234-tremr-kudos").unwrap_err().to_string();
        assert!(e.contains("'tremr'"), "{e}");
        assert!(Code::parse(&"9".repeat(500)).is_err());
    }

    #[test]
    fn random_below_is_in_range() {
        for _ in 0..1000 {
            assert!(random_below(7776).unwrap() < 7776);
            assert!(random_below(10).unwrap() < 10);
        }
    }

    #[test]
    fn debug_hides_the_words() {
        let c = Code { nameplate: "1234".into(), words: ["tremor".into(), "kudos".into()] };
        assert!(!format!("{c:?}").contains("tremor"));
    }
}

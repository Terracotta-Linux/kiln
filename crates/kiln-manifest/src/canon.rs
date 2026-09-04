//! The canonical encoding that `config_id` hashes.
//!
//! Deliberately hand-written rather than delegated to a serialization library:
//! the byte stream *is* the hash input, so hash stability across refactors
//! (see the hash-freeze tests) must not depend on a dependency's formatting
//! choices. Every value is self-delimiting and length-prefixed, so no two
//! distinct manifests can encode to the same bytes.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Canon {
    Str(String),
    Int(i64),
    Bool(bool),
    List(Vec<Canon>),
    Map(BTreeMap<String, Canon>),
}

impl Canon {
    pub fn map(pairs: impl IntoIterator<Item = (&'static str, Canon)>) -> Canon {
        Canon::Map(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn str(s: impl Into<String>) -> Canon {
        Canon::Str(s.into())
    }

    pub fn list(items: impl IntoIterator<Item = Canon>) -> Canon {
        Canon::List(items.into_iter().collect())
    }

    /// `None` encodes distinctly from any present value, so adding an optional
    /// key never collides with omitting it.
    pub fn opt(v: Option<Canon>) -> Canon {
        match v {
            None => Canon::List(Vec::new()),
            Some(v) => Canon::List(vec![v]),
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Canon::Str(s) => {
                out.push(b's');
                out.extend_from_slice(s.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(s.as_bytes());
            }
            // Fixed integer representation: decimal, no padding, explicit sign
            // only when negative.
            Canon::Int(i) => {
                out.push(b'i');
                out.extend_from_slice(i.to_string().as_bytes());
                out.push(b';');
            }
            Canon::Bool(b) => out.extend_from_slice(if *b { b"b1" } else { b"b0" }),
            Canon::List(items) => {
                out.push(b'[');
                for i in items {
                    i.encode(out);
                }
                out.push(b']');
            }
            Canon::Map(m) => {
                out.push(b'{');
                for (k, v) in m {
                    // BTreeMap iteration is already sorted; keys are encoded as
                    // strings so a key containing `:` cannot forge a boundary.
                    Canon::Str(k.clone()).encode(out);
                    v.encode(out);
                }
                out.push(b'}');
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }
}

pub trait Canonical {
    fn canon(&self) -> Canon;
}

impl Canonical for String {
    fn canon(&self) -> Canon {
        Canon::Str(self.clone())
    }
}

impl<T: Canonical> Canonical for Vec<T> {
    fn canon(&self) -> Canon {
        Canon::List(self.iter().map(Canonical::canon).collect())
    }
}

impl<T: Canonical> Canonical for std::collections::BTreeSet<T> {
    fn canon(&self) -> Canon {
        Canon::List(self.iter().map(Canonical::canon).collect())
    }
}

impl<V: Canonical> Canonical for BTreeMap<String, V> {
    fn canon(&self) -> Canon {
        Canon::Map(self.iter().map(|(k, v)| (k.clone(), v.canon())).collect())
    }
}

/// A blake3 digest, rendered the way Kiln prints identities.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct Hash(pub String);

impl Hash {
    pub fn of(bytes: &[u8]) -> Hash {
        Hash(format!("b3:{}", blake3::hash(bytes).to_hex()))
    }

    /// `b3:7f2a…` — Kiln never prints a full digest where a short one does.
    pub fn short(&self) -> String {
        let hex = self.0.strip_prefix("b3:").unwrap_or(&self.0);
        format!("b3:{}", &hex[..hex.len().min(8)])
    }
}

impl std::fmt::Display for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Canonical for Hash {
    fn canon(&self) -> Canon {
        Canon::Str(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_is_unambiguous() {
        // The classic length-prefix bug: {"ab":"c"} and {"a":"bc"} must differ.
        let a = Canon::map([("ab", Canon::str("c"))]);
        let b = Canon::map([("a", Canon::str("bc"))]);
        assert_ne!(a.to_bytes(), b.to_bytes());

        // An empty list, an absent option, and a present empty string differ.
        assert_ne!(
            Canon::opt(None).to_bytes(),
            Canon::opt(Some(Canon::str(""))).to_bytes()
        );
        assert_ne!(Canon::list([]).to_bytes(), Canon::str("").to_bytes());
    }

    #[test]
    fn map_order_does_not_matter() {
        let mut m1 = BTreeMap::new();
        m1.insert("b".to_string(), Canon::Int(2));
        m1.insert("a".to_string(), Canon::Int(1));
        let mut m2 = BTreeMap::new();
        m2.insert("a".to_string(), Canon::Int(1));
        m2.insert("b".to_string(), Canon::Int(2));
        assert_eq!(Canon::Map(m1).to_bytes(), Canon::Map(m2).to_bytes());
    }
}

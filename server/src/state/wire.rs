//! Byte-string encoding helpers for the persisted snapshot.
//!
//! rmp-serde serializes `Vec<u8>` / `Box<[u8]>` / `&[u8]` as an msgpack **array of integers**,
//! not as `bin`: 28 random bytes take 59 on the wire, and reading one costs a visitor call per
//! *byte* instead of a single slice read. Across the snapshot's ~10M byte keys that is hundreds
//! of megabytes and seconds of load, so the hot fields ask for `bin` explicitly via
//! `#[serde(with = "…")]`.
//!
//! This only changes the wire: the in-memory types stay `Vec<u8>` / `Box<[u8]>`, so lookups,
//! hashing and every call site are untouched. Readers accept **either** encoding, so a snapshot
//! written before this needs no format bump and no cold reset.

use imbl::{hashmap::HashMap, hashset::HashSet};
use serde::de::{Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq, Serializer};
use serde::Deserialize;

/// The reverse index shape: subject bytes → the set of credential bytes under it.
type ByteKeySets = HashMap<Vec<u8>, HashSet<Vec<u8>>>;

/// Accepts a byte string as `bin` (what we write) or as a sequence of integers (what a plain
/// `Vec<u8>` used to produce).
struct BytesVisitor;

impl<'de> Visitor<'de> for BytesVisitor {
    type Value = Vec<u8>;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a byte string (msgpack bin, or a sequence of bytes)")
    }
    fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
        Ok(v.to_vec())
    }
    fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
        Ok(v)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
        let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(32));
        while let Some(b) = seq.next_element::<u8>()? {
            out.push(b);
        }
        Ok(out)
    }
}

/// msgpack is self-describing, so `deserialize_any` is what lets one reader take both shapes.
fn read_bytes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    d.deserialize_any(BytesVisitor)
}

/// A byte string as a map key or set member, read permissively.
struct Bytes(Vec<u8>);

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        read_bytes(d).map(Bytes)
    }
}

/// `HashSet<Vec<u8>>` with permissively-read members.
struct ByteSet(HashSet<Vec<u8>>);

impl<'de> Deserialize<'de> for ByteSet {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ByteSet;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a set of byte strings")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<ByteSet, A::Error> {
                let mut out = HashSet::new();
                while let Some(Bytes(b)) = seq.next_element::<Bytes>()? {
                    out.insert(b);
                }
                Ok(ByteSet(out))
            }
        }
        d.deserialize_seq(V)
    }
}

/// `#[serde(with = "wire::byte_key_map")]` — a map keyed by raw bytes (a stake credential, a
/// payment address), values by their own impls.
pub mod byte_key_map {
    use super::*;

    pub fn serialize<S: Serializer, V: serde::Serialize>(
        m: &HashMap<Vec<u8>, V>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(m.len()))?;
        for (k, v) in m.iter() {
            map.serialize_entry(serde_bytes::Bytes::new(k), v)?;
        }
        map.end()
    }

    pub fn deserialize<'de, D, V>(d: D) -> Result<HashMap<Vec<u8>, V>, D::Error>
    where
        D: Deserializer<'de>,
        V: Deserialize<'de> + Clone,
    {
        struct V2<V>(std::marker::PhantomData<V>);
        impl<'de, V: Deserialize<'de> + Clone> Visitor<'de> for V2<V> {
            type Value = HashMap<Vec<u8>, V>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map keyed by byte strings")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = HashMap::new();
                while let Some((Bytes(k), v)) = map.next_entry::<Bytes, V>()? {
                    out.insert(k, v);
                }
                Ok(out)
            }
        }
        d.deserialize_map(V2(std::marker::PhantomData))
    }
}

/// `#[serde(with = "wire::byte_key_set_map")]` — a reverse index: bytes → set of bytes.
pub mod byte_key_set_map {
    use super::*;

    pub fn serialize<S: Serializer>(m: &ByteKeySets, s: S) -> Result<S::Ok, S::Error> {
        struct Members<'a>(&'a HashSet<Vec<u8>>);
        impl serde::Serialize for Members<'_> {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut seq = s.serialize_seq(Some(self.0.len()))?;
                for member in self.0.iter() {
                    seq.serialize_element(serde_bytes::Bytes::new(member))?;
                }
                seq.end()
            }
        }
        let mut map = s.serialize_map(Some(m.len()))?;
        for (k, members) in m.iter() {
            map.serialize_entry(serde_bytes::Bytes::new(k), &Members(members))?;
        }
        map.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ByteKeySets, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ByteKeySets;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map of byte strings to sets of byte strings")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = HashMap::new();
                while let Some((Bytes(k), ByteSet(members))) = map.next_entry::<Bytes, ByteSet>()? {
                    out.insert(k, members);
                }
                Ok(out)
            }
        }
        d.deserialize_map(V)
    }
}

/// `#[serde(with = "wire::boxed_bytes")]` — a `Box<[u8]>` struct field.
pub mod boxed_bytes {
    use super::*;

    pub fn serialize<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(b)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Box<[u8]>, D::Error> {
        read_bytes(d).map(Vec::into_boxed_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the permissive reader: a snapshot written before this change encoded byte
    /// strings as sequences of integers, and must still load.
    #[test]
    fn reads_both_encodings() {
        #[derive(serde::Serialize)]
        struct OldShape {
            // plain Vec<u8> — what rmp-serde turns into an array of integers
            map: std::collections::BTreeMap<Vec<u8>, i64>,
        }
        #[derive(serde::Serialize, serde::Deserialize)]
        struct NewShape {
            #[serde(with = "byte_key_map")]
            map: HashMap<Vec<u8>, i64>,
        }

        let key = vec![0xaau8; 28];
        let old = OldShape {
            map: [(key.clone(), 42i64)].into_iter().collect(),
        };
        let new = NewShape {
            map: HashMap::unit(key.clone(), 42i64),
        };

        let old_bytes = rmp_serde::to_vec(&old).unwrap();
        let new_bytes = rmp_serde::to_vec(&new).unwrap();
        // `bin` is the smaller encoding — that's the whole point.
        assert!(
            new_bytes.len() < old_bytes.len(),
            "bin ({}) should beat the integer array ({})",
            new_bytes.len(),
            old_bytes.len()
        );

        for (label, bytes) in [("old", old_bytes), ("new", new_bytes)] {
            let got: NewShape = rmp_serde::from_slice(&bytes).unwrap();
            assert_eq!(got.map.get(&key), Some(&42), "{label} encoding must load");
        }
    }
}

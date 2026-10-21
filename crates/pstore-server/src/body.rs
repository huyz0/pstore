//! A request body parsed once into its type, where that is provably the tree's answer (M120).
//!
//! ⚠️ **The tree is the reference.** Every handler's contract is what parsing the body into a
//! [`serde_json::Value`] and then into the type accepts and refuses. Parsing into the type
//! directly differs in exactly two ways: it refuses a key repeated in an object, which the
//! tree accepts with the last value winning, and it skips an unknown field's value without
//! checking it, where the tree refused a number out of range or a string that is not UTF-8.
//!
//! So a body is first [`plain`]-checked by one pass that builds nothing: every value read as
//! the tree reads it -- numbers range-checked, strings UTF-8-checked, by the same parser --
//! and every object's keys compared. Only a body that passes with no key repeated is parsed
//! into the type directly; any other body, and any typed parse that fails, goes the tree's
//! way, so a refusal is the tree's own, word for word.

use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use std::borrow::Cow;

/// What [`plain`] learned of a body the tree would parse with no key repeated.
#[derive(Debug)]
pub(crate) struct Plain {
    /// The top-level object's keys, or `None` when the body is not an object.
    pub(crate) keys: Option<Vec<String>>,
}

/// `Some` when the body is one JSON value the tree's parse accepts and no object in it holds
/// a key twice; `None` for anything else, the tree's to judge.
pub(crate) fn plain(body: &[u8]) -> Option<Plain> {
    let mut de = serde_json::Deserializer::from_slice(body);
    let mut keys = None;
    let repeated = Check {
        top: Some(&mut keys),
    }
    .deserialize(&mut de)
    .ok()?;
    de.end().ok()?;
    (!repeated).then_some(Plain { keys })
}

/// `body` as `T`, parsed once, when that is the tree's answer; `None` for the tree to give.
pub(crate) fn typed<T: serde::de::DeserializeOwned>(body: &[u8]) -> Option<(Plain, T)> {
    let shape = plain(body)?;
    let t = serde_json::from_slice(body).ok()?;
    Some((shape, t))
}

/// Reads one value as the tree would, building nothing; `true` if some object repeats a key.
struct Check<'k> {
    /// Where the top-level object's keys go, for the outermost value only.
    top: Option<&'k mut Option<Vec<String>>>,
}

impl<'de> DeserializeSeed<'de> for Check<'_> {
    type Value = bool;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<bool, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Check<'_> {
    type Value = bool;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_i64<E>(self, _: i64) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_u64<E>(self, _: u64) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_f64<E>(self, _: f64) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_str<E>(self, _: &str) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_unit<E>(self) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
        let mut repeated = false;
        while let Some(r) = seq.next_element_seed(Check { top: None })? {
            repeated |= r;
        }
        Ok(repeated)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
        // A list while small, a set past that, so a wide object stays linear in its keys.
        let mut few: Vec<Cow<'de, str>> = Vec::new();
        let mut many: Option<std::collections::HashSet<Cow<'de, str>>> = None;
        let mut keys: Vec<String> = Vec::new();
        let mut repeated = false;
        while let Some(k) = map.next_key_seed(KeyOf)? {
            if self.top.is_some() {
                keys.push(k.clone().into_owned());
            }
            repeated |= match &mut many {
                Some(set) => !set.insert(k),
                None => {
                    let again = few.contains(&k);
                    few.push(k);
                    if few.len() >= 32 {
                        many = Some(few.drain(..).collect());
                    }
                    again
                }
            };
            repeated |= map.next_value_seed(Check { top: None })?;
        }
        if let Some(top) = self.top {
            *top = Some(keys);
        }
        Ok(repeated)
    }
}

/// An object key, borrowed from the body where it holds no escape.
struct KeyOf;

impl<'de> DeserializeSeed<'de> for KeyOf {
    type Value = Cow<'de, str>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Cow<'de, str>, D::Error> {
        d.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for KeyOf {
    type Value = Cow<'de, str>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an object key")
    }
    fn visit_borrowed_str<E>(self, s: &'de str) -> Result<Cow<'de, str>, E> {
        Ok(Cow::Borrowed(s))
    }
    fn visit_str<E>(self, s: &str) -> Result<Cow<'de, str>, E> {
        Ok(Cow::Owned(s.to_owned()))
    }
}

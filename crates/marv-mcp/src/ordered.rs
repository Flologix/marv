//! JSON with every object's key order kept. Not a port of one TS file: Marv's
//! `JSON.parse` kept insertion order for free.
//!
//! serde_json's `Value` sorts keys, and its `preserve_order` feature would
//! change hashing elsewhere (the trust keys hash canonical, sorted JSON). Two
//! places need the order: writing your `mcp.json` must not reorder your file
//! ([`crate::personal_file`]), and a README lists servers and variables in the
//! order they're offered ([`crate::install::github`]).

use serde::de::{Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

/// One JSON value, objects as ordered lists of fields.
#[derive(Debug, Clone, PartialEq)]
pub enum Ordered {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Ordered>),
    Object(Vec<(String, Ordered)>),
}

impl Ordered {
    pub fn as_object(&self) -> Option<&[(String, Ordered)]> {
        if let Ordered::Object(o) = self { Some(o) } else { None }
    }
    pub fn get(&self, key: &str) -> Option<&Ordered> {
        self.as_object()?.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn as_str(&self) -> Option<&str> {
        if let Ordered::String(s) = self { Some(s) } else { None }
    }

    pub fn from_value(v: Value) -> Ordered {
        match v {
            Value::Null => Ordered::Null,
            Value::Bool(b) => Ordered::Bool(b),
            Value::Number(n) => Ordered::Number(n),
            Value::String(s) => Ordered::String(s),
            Value::Array(a) => Ordered::Array(a.into_iter().map(Ordered::from_value).collect()),
            Value::Object(o) => Ordered::Object(o.into_iter().map(|(k, v)| (k, Ordered::from_value(v))).collect()),
        }
    }
}

impl Serialize for Ordered {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Ordered::Null => s.serialize_unit(),
            Ordered::Bool(b) => s.serialize_bool(*b),
            Ordered::Number(n) => n.serialize(s),
            Ordered::String(t) => s.serialize_str(t),
            Ordered::Array(items) => {
                let mut seq = s.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Ordered::Object(fields) => {
                let mut map = s.serialize_map(Some(fields.len()))?;
                for (k, v) in fields {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_unit<E>(self) -> Result<Ordered, E> {
                Ok(Ordered::Null)
            }
            fn visit_none<E>(self) -> Result<Ordered, E> {
                Ok(Ordered::Null)
            }
            fn visit_bool<E>(self, b: bool) -> Result<Ordered, E> {
                Ok(Ordered::Bool(b))
            }
            fn visit_i64<E>(self, n: i64) -> Result<Ordered, E> {
                Ok(Ordered::Number(n.into()))
            }
            fn visit_u64<E>(self, n: u64) -> Result<Ordered, E> {
                Ok(Ordered::Number(n.into()))
            }
            fn visit_f64<E>(self, n: f64) -> Result<Ordered, E> {
                Ok(Number::from_f64(n).map_or(Ordered::Null, Ordered::Number))
            }
            fn visit_str<E>(self, t: &str) -> Result<Ordered, E> {
                Ok(Ordered::String(t.to_string()))
            }
            fn visit_string<E>(self, t: String) -> Result<Ordered, E> {
                Ok(Ordered::String(t))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Ordered, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Ordered::Array(items))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
                let mut fields: Vec<(String, Ordered)> = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Ordered>()? {
                    // A repeated key keeps its first place and its last value, as JSON.parse does.
                    match fields.iter_mut().find(|(key, _)| *key == k) {
                        Some(slot) => slot.1 = v,
                        None => fields.push((k, v)),
                    }
                }
                Ok(Ordered::Object(fields))
            }
        }
        d.deserialize_any(V)
    }
}

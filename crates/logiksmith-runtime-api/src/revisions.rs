use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};

/// A JSON revision token. Numeric revisions are deliberately not accepted at
/// this boundary: JavaScript cannot represent every `u64` exactly.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Revision(pub u64);

impl Revision {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    pub const fn value(self) -> u64 {
        self.0
    }
}
impl From<u64> for Revision {
    fn from(value: u64) -> Self {
        Self(value)
    }
}
impl From<Revision> for u64 {
    fn from(value: Revision) -> Self {
        value.0
    }
}
impl Serialize for Revision {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&self.0.to_string())
    }
}
impl<'de> Deserialize<'de> for Revision {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(d)?;
        value
            .parse::<u64>()
            .map(Self)
            .map_err(|_| D::Error::custom("must be a decimal revision string"))
    }
}

pub mod wire_revision {
    use super::*;
    pub fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Revision(*value).serialize(serializer)
    }
    pub fn deserialize<'de, D>(d: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Revision::deserialize(d)?.0)
    }
    pub fn serialize_option<S>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.map(Revision).serialize(serializer)
    }
    pub fn deserialize_option<'de, D>(d: D) -> Result<Option<u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Option::<Revision>::deserialize(d)?.map(|v| v.0))
    }
}

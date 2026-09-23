use anyhow::{bail, Result};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SpaceId(String);

impl SpaceId {
    pub fn new(s: &str) -> Result<Self> {
        if !miyori_config::is_safe_slug(s) {
            bail!("id {:?} не является безопасным слагом [a-z0-9-]", s);
        }
        Ok(Self(s.to_string()))
    }
}

impl AsRef<str> for SpaceId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SpaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for SpaceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SpaceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        SpaceId::new(&raw).map_err(DeError::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Color(String);

impl Color {
    pub fn new(s: &str) -> Result<Self> {
        if !is_rrggbb(s) {
            bail!("цвет {:?} должен быть в формате #RRGGBB", s);
        }
        Ok(Self(s.to_string()))
    }
}

fn is_rrggbb(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(|b| b.is_ascii_hexdigit())
}

impl AsRef<str> for Color {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Color::new(&raw).map_err(DeError::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label(String);

impl Label {
    pub fn new(s: &str) -> Result<Self> {
        if !miyori_config::is_safe_slug(s) {
            bail!("метка {:?} не является безопасным слагом [a-z0-9-]", s);
        }
        Ok(Self(s.to_string()))
    }
}

impl AsRef<str> for Label {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Label {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Label {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Label::new(&raw).map_err(DeError::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_id_accepts_safe_slug() {
        assert!(SpaceId::new("telegram").is_ok());
    }

    #[test]
    fn space_id_rejects_path_traversal() {
        assert!(SpaceId::new("../etc").is_err());
    }

    #[test]
    fn space_id_round_trips_through_json() {
        let id = SpaceId::new("telegram").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"telegram\"");
        let back: SpaceId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn space_id_deserialize_rejects_unsafe_value() {
        assert!(serde_json::from_str::<SpaceId>("\"../etc\"").is_err());
    }

    #[test]
    fn color_accepts_rrggbb() {
        assert!(Color::new("#2f9e44").is_ok());
    }

    #[test]
    fn color_rejects_bad_format() {
        for bad in ["#xyz", "red", "#1234567", "2f9e44"] {
            assert!(Color::new(bad).is_err(), "{bad} должен быть отвергнут");
        }
    }

    #[test]
    fn label_accepts_safe_slug() {
        assert!(Label::new("untrusted").is_ok());
    }

    #[test]
    fn label_rejects_uppercase() {
        assert!(Label::new("Untrusted").is_err());
    }
}

/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 */

use serde::de::{Error as _, Unexpected};
use serde::{Deserialize, Deserializer, Serialize};

/// A meeting-wide override of one join/leave announcement preference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnouncementOverride {
    On,
    Off,
    #[default]
    #[serde(other)]
    Participant,
}

impl AnnouncementOverride {
    pub const ALL: [Self; 3] = [Self::Participant, Self::On, Self::Off];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Participant => "participant",
            Self::On => "on",
            Self::Off => "off",
        }
    }

    pub const fn resolve(self, participant_preference: bool) -> bool {
        match self {
            Self::Participant => participant_preference,
            Self::On => true,
            Self::Off => false,
        }
    }

    pub fn is_participant(&self) -> bool {
        *self == Self::Participant
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

impl From<String> for AnnouncementOverride {
    fn from(s: String) -> Self {
        Self::from_wire(&s).unwrap_or_default()
    }
}

pub fn deserialize_strict<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<AnnouncementOverride>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|s| {
            AnnouncementOverride::from_wire(&s).ok_or_else(|| {
                D::Error::invalid_value(Unexpected::Str(&s), &"participant, on or off")
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_spellings_are_the_contract() {
        let wire: Vec<String> = AnnouncementOverride::ALL
            .iter()
            .map(|v| serde_json::to_string(v).expect("serialize"))
            .collect();
        assert_eq!(wire, [r#""participant""#, r#""on""#, r#""off""#]);
    }

    #[test]
    fn every_variant_round_trips_and_matches_as_str() {
        for v in AnnouncementOverride::ALL {
            let wire = serde_json::to_string(&v).expect("serialize");
            assert_eq!(wire, format!("\"{}\"", v.as_str()));
            let back: AnnouncementOverride = serde_json::from_str(&wire).expect("deserialize");
            assert_eq!(back, v);
            assert_eq!(AnnouncementOverride::from(v.as_str().to_string()), v);
        }
    }

    #[test]
    fn an_unknown_wire_value_reads_as_participant() {
        let v: AnnouncementOverride =
            serde_json::from_str(r#""chime_only""#).expect("an unknown variant must not fail");
        assert_eq!(v, AnnouncementOverride::Participant);
        assert_eq!(
            AnnouncementOverride::from("chime_only".to_string()),
            AnnouncementOverride::Participant
        );
    }

    #[test]
    fn from_wire_accepts_exactly_the_wire_spellings() {
        for v in AnnouncementOverride::ALL {
            assert_eq!(AnnouncementOverride::from_wire(v.as_str()), Some(v));
        }
        for unknown in ["On", "OFF", " on", "on ", "", "chime_only", "true"] {
            assert_eq!(
                AnnouncementOverride::from_wire(unknown),
                None,
                "{unknown:?}"
            );
        }
    }

    #[test]
    fn the_default_is_participant() {
        assert_eq!(
            AnnouncementOverride::default(),
            AnnouncementOverride::Participant
        );
    }

    #[test]
    fn resolve_overrides_the_participant_preference() {
        for pref in [true, false] {
            assert_eq!(AnnouncementOverride::Participant.resolve(pref), pref);
            assert!(AnnouncementOverride::On.resolve(pref));
            assert!(!AnnouncementOverride::Off.resolve(pref));
        }
    }

    #[derive(Deserialize)]
    struct Strict {
        #[serde(default, deserialize_with = "deserialize_strict")]
        v: Option<AnnouncementOverride>,
    }

    #[test]
    fn strict_deserialization_rejects_an_unknown_value() {
        assert!(serde_json::from_str::<Strict>(r#"{"v":"chime_only"}"#).is_err());
        assert!(serde_json::from_str::<Strict>(r#"{"v":true}"#).is_err());
    }

    #[test]
    fn strict_deserialization_accepts_known_values_null_and_absence() {
        for v in AnnouncementOverride::ALL {
            let body = format!(r#"{{"v":"{}"}}"#, v.as_str());
            let parsed: Strict = serde_json::from_str(&body).expect("known value");
            assert_eq!(parsed.v, Some(v));
        }
        let parsed: Strict = serde_json::from_str(r#"{"v":null}"#).expect("null");
        assert_eq!(parsed.v, None);
        let parsed: Strict = serde_json::from_str("{}").expect("absent");
        assert_eq!(parsed.v, None);
    }
}

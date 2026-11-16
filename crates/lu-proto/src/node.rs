//! Node heartbeat: what a peer says about itself. The forwarded Reiki document is opaque and
//! capped, and provider names are checked with the same rule as node names.

use serde::{Deserialize, Serialize};

use crate::ids::{is_name_byte, NodeName};
use crate::limits::{MAX_NAME_BYTES, MAX_PROVIDERS, MAX_REIKI_BYTES};

/// Whether the peer's harness is free to take a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessState {
    Idle,
    Busy,
}

/// A provider's availability on this peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderState {
    Ok,
    Cooling,
    Disabled,
    Unknown,
}

/// One provider and its state, as reported by the peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderStatus {
    pub name: String,
    pub state: ProviderState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soonest_secs: Option<u64>,
}

/// A periodic message from an agent. `reiki` is the opaque health document, forwarded unchanged.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Heartbeat {
    pub node: NodeName,
    pub harness: HarnessState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reiki: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHeartbeat {
    pub node: NodeName,
    pub harness: HarnessState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reiki: Option<serde_json::Value>,
}

impl TryFrom<RawHeartbeat> for Heartbeat {
    type Error = HeartbeatError;
    fn try_from(raw: RawHeartbeat) -> Result<Self, Self::Error> {
        let hb = Heartbeat {
            node: raw.node,
            harness: raw.harness,
            providers: raw.providers,
            reiki: raw.reiki,
        };
        hb.validate()?;
        Ok(hb)
    }
}

impl<'de> Deserialize<'de> for Heartbeat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawHeartbeat::deserialize(deserializer)?;
        Heartbeat::try_from(raw).map_err(serde::de::Error::custom)
    }
}

impl Heartbeat {
    /// Checks the caps a single heartbeat must respect.
    pub fn validate(&self) -> Result<(), HeartbeatError> {
        if self.providers.len() > MAX_PROVIDERS {
            return Err(HeartbeatError::TooManyProviders {
                got: self.providers.len(),
            });
        }
        for (index, provider) in self.providers.iter().enumerate() {
            if !valid_provider_name(&provider.name) {
                return Err(HeartbeatError::ProviderName {
                    index,
                    name: provider.name.clone(),
                });
            }
        }
        if let Some(reiki) = &self.reiki {
            let size = serde_json::to_vec(reiki)
                .map_err(|_| HeartbeatError::ReikiNotSerializable)?
                .len();
            if size > MAX_REIKI_BYTES {
                return Err(HeartbeatError::ReikiTooLarge { got: size });
            }
        }
        Ok(())
    }
}

/// Why a heartbeat was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatError {
    TooManyProviders { got: usize },
    ProviderName { index: usize, name: String },
    ReikiTooLarge { got: usize },
    ReikiNotSerializable,
}

impl std::fmt::Display for HeartbeatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyProviders { got } => write!(
                f,
                "a heartbeat may carry at most {MAX_PROVIDERS} providers (got {got})"
            ),
            Self::ProviderName { index, name } => write!(
                f,
                "provider #{index} has an invalid name {name:?}; it must be 1 to {MAX_NAME_BYTES} characters from A-Z a-z 0-9 . _ -"
            ),
            Self::ReikiTooLarge { got } => write!(
                f,
                "the forwarded reiki document is larger than {MAX_REIKI_BYTES} bytes (got {got})"
            ),
            Self::ReikiNotSerializable => {
                f.write_str("the forwarded reiki document could not be serialised")
            }
        }
    }
}

impl std::error::Error for HeartbeatError {}

/// Whether `name` may appear as a provider in a heartbeat.
pub fn valid_provider_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_BYTES && name.bytes().all(is_name_byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn heartbeat() -> Heartbeat {
        Heartbeat {
            node: NodeName::from_str("peer-a").unwrap(),
            harness: HarnessState::Idle,
            providers: vec![ProviderStatus {
                name: "example-provider".to_owned(),
                state: ProviderState::Ok,
                soonest_secs: None,
            }],
            reiki: None,
        }
    }

    #[test]
    fn a_heartbeat_round_trips() {
        let mut beat = heartbeat();
        beat.harness = HarnessState::Busy;
        beat.providers[0].state = ProviderState::Cooling;
        beat.providers[0].soonest_secs = Some(30);
        beat.reiki = Some(serde_json::json!({"status": "ok"}));
        assert!(beat.validate().is_ok());
        let json = serde_json::to_string(&beat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, beat);
    }

    #[test]
    fn providers_default_to_empty_and_are_omitted() {
        let json = r#"{"node":"peer-a","harness":"idle"}"#;
        let beat: Heartbeat = serde_json::from_str(json).unwrap();
        assert!(beat.providers.is_empty());
        assert!(beat.reiki.is_none());
        assert!(beat.validate().is_ok());
        let out = serde_json::to_string(&beat).unwrap();
        assert!(!out.contains("providers"), "{out}");
        assert!(!out.contains("reiki"), "{out}");
    }

    #[test]
    fn too_many_providers_are_refused() {
        let mut beat = heartbeat();
        beat.providers = (0..=MAX_PROVIDERS)
            .map(|index| ProviderStatus {
                name: format!("provider-{index}"),
                state: ProviderState::Unknown,
                soonest_secs: None,
            })
            .collect();
        assert_eq!(
            beat.validate().unwrap_err(),
            HeartbeatError::TooManyProviders {
                got: MAX_PROVIDERS + 1
            }
        );
    }

    #[test]
    fn a_bad_provider_name_is_refused() {
        for name in ["", "bad name", "peer/../a", "café", "peer\u{1b}[31m"] {
            let mut beat = heartbeat();
            beat.providers[0].name = name.to_owned();
            assert!(matches!(
                beat.validate().unwrap_err(),
                HeartbeatError::ProviderName { .. }
            ));
        }
        let mut beat = heartbeat();
        beat.providers[0].name = "a".repeat(MAX_NAME_BYTES + 1);
        assert!(matches!(
            beat.validate().unwrap_err(),
            HeartbeatError::ProviderName { .. }
        ));
    }

    #[test]
    fn an_oversized_reiki_document_is_refused() {
        let mut beat = heartbeat();
        beat.reiki = Some(serde_json::Value::String("a".repeat(MAX_REIKI_BYTES)));
        assert!(matches!(
            beat.validate().unwrap_err(),
            HeartbeatError::ReikiTooLarge { .. }
        ));
    }

    #[test]
    fn unknown_heartbeat_fields_are_refused() {
        let json = r#"{"node":"peer-a","harness":"idle","surprise":true}"#;
        assert!(serde_json::from_str::<Heartbeat>(json).is_err());
    }

    #[test]
    fn provider_states_round_trip_as_snake_case_strings() {
        for state in [
            ProviderState::Ok,
            ProviderState::Cooling,
            ProviderState::Disabled,
            ProviderState::Unknown,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<ProviderState>(&json).unwrap(), state);
        }
        for state in [HarnessState::Idle, HarnessState::Busy] {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<HarnessState>(&json).unwrap(), state);
        }
    }
}

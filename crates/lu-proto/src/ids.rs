//! Job ids and node names. A job id is 26 Crockford base32 characters: a 48-bit millisecond
//! prefix (so ids sort by creation time), then 80 random bits. A node name is a short, restricted
//! token. Both serialise as plain strings and refuse malformed input on deserialise.

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize, Serializer};

use crate::limits::{JOB_ID_LEN, MAX_NAME_BYTES};

/// Crockford base32, in ASCII order (so a canonical string sorts like the number it encodes).
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const TIME_CHARS: usize = 10;
const RANDOM_CHARS: usize = JOB_ID_LEN - TIME_CHARS;
const RANDOM_BYTES: usize = 10;

/// Why a job id was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobIdError {
    /// Not exactly [`JOB_ID_LEN`] bytes.
    Length { got: usize },
    /// A byte outside the Crockford base32 alphabet.
    Alphabet { at: usize },
}

impl fmt::Display for JobIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { got } => write!(
                f,
                "a job id must be exactly {JOB_ID_LEN} characters (got {got})"
            ),
            Self::Alphabet { at } => write!(
                f,
                "a job id may only use Crockford base32 characters (bad byte at index {at})"
            ),
        }
    }
}

impl std::error::Error for JobIdError {}

/// A sortable job id. Compare two ids to compare their creation times.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct JobId(String);

impl JobId {
    /// A fresh id: the current time in milliseconds, then 80 random bits.
    pub fn new() -> Self {
        let millis = now_millis();
        let mut out = String::with_capacity(JOB_ID_LEN);
        for index in 0..TIME_CHARS {
            let shift = 5 * (TIME_CHARS - 1 - index);
            out.push(CROCKFORD[((millis >> shift) & 0x1F) as usize] as char);
        }
        let mut random = [0u8; RANDOM_BYTES];
        if getrandom::getrandom(&mut random).is_err() {
            fallback_random(&mut random, millis);
        }
        let mut value: u128 = 0;
        for byte in random {
            value = (value << 8) | u128::from(byte);
        }
        for index in 0..RANDOM_CHARS {
            let shift = 5 * (RANDOM_CHARS - 1 - index);
            out.push(CROCKFORD[((value >> shift) & 0x1F) as usize] as char);
        }
        Self(out)
    }

    /// Parses an id, accepting either case and storing the canonical uppercase form.
    pub fn parse(text: &str) -> Result<Self, JobIdError> {
        if text.len() != JOB_ID_LEN {
            return Err(JobIdError::Length { got: text.len() });
        }
        let mut out = String::with_capacity(JOB_ID_LEN);
        for (at, byte) in text.bytes().enumerate() {
            let upper = byte.to_ascii_uppercase();
            if !CROCKFORD.contains(&upper) {
                return Err(JobIdError::Alphabet { at });
            }
            out.push(upper as char);
        }
        Ok(Self(out))
    }

    /// The canonical uppercase form.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for JobId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for JobId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl FromStr for JobId {
    type Err = JobIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for JobId {
    type Error = JobIdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for JobId {
    type Error = JobIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

/// Why a node name was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeNameError {
    Empty,
    TooLong { len: usize },
    BadChar { at: usize },
}

impl fmt::Display for NodeNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a node name must not be empty"),
            Self::TooLong { len } => write!(
                f,
                "a node name must be at most {MAX_NAME_BYTES} characters (got {len})"
            ),
            Self::BadChar { at } => write!(
                f,
                "a node name may only use A-Z a-z 0-9 . _ - (bad byte at index {at})"
            ),
        }
    }
}

impl std::error::Error for NodeNameError {}

/// A peer name: 1 to [`MAX_NAME_BYTES`] characters from `A-Z a-z 0-9 . _ -`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct NodeName(String);

impl NodeName {
    /// Parses and checks a node name.
    pub fn parse(text: &str) -> Result<Self, NodeNameError> {
        if text.is_empty() {
            return Err(NodeNameError::Empty);
        }
        if text.len() > MAX_NAME_BYTES {
            return Err(NodeNameError::TooLong { len: text.len() });
        }
        if let Some(at) = text.bytes().position(|b| !is_name_byte(b)) {
            return Err(NodeNameError::BadChar { at });
        }
        Ok(Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for NodeName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for NodeName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl FromStr for NodeName {
    type Err = NodeNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for NodeName {
    type Error = NodeNameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for NodeName {
    type Error = NodeNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

/// The byte set shared by node, harness and provider names.
pub(crate) fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

fn now_millis() -> u64 {
    const TIME_MASK: u128 = (1 << 48) - 1;
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| (elapsed.as_millis() & TIME_MASK) as u64)
        .unwrap_or(0)
}

/// The OS randomness source failed; keep going with a clock, process and counter mix rather than
/// panic, so a job id is still produced.
fn fallback_random(out: &mut [u8; RANDOM_BYTES], millis: u64) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut state = millis
        ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (u64::from(std::process::id()) << 32)
        ^ 0x2545_F491_4F6C_DD1D;
    for slot in out.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = state as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    #[test]
    fn generated_ids_have_the_right_shape_and_do_not_repeat() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = JobId::new();
            assert_eq!(id.as_str().len(), JOB_ID_LEN);
            assert!(id.as_str().bytes().all(|b| CROCKFORD.contains(&b)));
            assert!(seen.insert(id), "a generated id repeated");
        }
    }

    #[test]
    fn a_known_id_round_trips_and_keeps_its_case() {
        let id = JobId::from_str(SAMPLE).unwrap();
        assert_eq!(id.as_str(), SAMPLE);
        assert_eq!(id.to_string(), SAMPLE);
        assert_eq!(JobId::from_str(&SAMPLE.to_lowercase()).unwrap(), id);
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{SAMPLE}\""));
        assert_eq!(
            serde_json::from_str::<JobId>(&serde_json::to_string(&id).unwrap()).unwrap(),
            id
        );
    }

    #[test]
    fn ids_compare_like_their_encoded_numbers() {
        let low = JobId::from_str("00000000000000000000000001").unwrap();
        let high = JobId::from_str("00000000000000000000000002").unwrap();
        assert!(low < high);
        let later = JobId::from_str("0000000000ZZZZZZZZZZZZZZZZ").unwrap();
        assert!(high < later);
    }

    #[test]
    fn malformed_ids_are_refused() {
        let cases = [
            ("", JobIdError::Length { got: 0 }),
            ("01ARZ3NDEKTSV4RRFFQ69G5FA", JobIdError::Length { got: 25 }),
            (
                "01ARZ3NDEKTSV4RRFFQ69G5FAVV",
                JobIdError::Length { got: 27 },
            ),
            (
                "01ARZ3NDEKTSV4RRFFQ69G5FAI",
                JobIdError::Alphabet { at: 25 },
            ),
            (
                "01ARZ3NDEKTSV4RRFFQ69G5FAL",
                JobIdError::Alphabet { at: 25 },
            ),
            (
                "01ARZ3NDEKTSV4RRFFQ69G5FAO",
                JobIdError::Alphabet { at: 25 },
            ),
            (
                "01ARZ3NDEKTSV4RRFFQ69G5FAU",
                JobIdError::Alphabet { at: 25 },
            ),
            (
                "01ARZ3NDEKTSV4RRFFQ69G5FA!",
                JobIdError::Alphabet { at: 25 },
            ),
            (
                "01ARZ3NDEKTSV4RRFFQ69G5FA\u{0}",
                JobIdError::Alphabet { at: 25 },
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(JobId::from_str(text).unwrap_err(), expected, "{text:?}");
        }
    }

    #[test]
    fn serde_refuses_a_malformed_id() {
        let error = serde_json::from_str::<JobId>("\"too short\"").unwrap_err();
        assert!(error.to_string().contains("job id"), "{error}");
        assert!(serde_json::from_str::<JobId>("42").is_err());
    }

    #[test]
    fn node_names_accept_the_allowed_charset() {
        for name in ["peer-a", "host-1.example.net", "A", "0", "a_b-c.d"] {
            assert_eq!(NodeName::from_str(name).unwrap().as_str(), name);
        }
    }

    #[test]
    fn node_names_refuse_empty_long_or_odd_input() {
        assert_eq!(NodeName::from_str("").unwrap_err(), NodeNameError::Empty);
        assert_eq!(
            NodeName::from_str(&"a".repeat(MAX_NAME_BYTES + 1)).unwrap_err(),
            NodeNameError::TooLong {
                len: MAX_NAME_BYTES + 1
            }
        );
        assert!(matches!(
            NodeName::from_str("bad name").unwrap_err(),
            NodeNameError::BadChar { .. }
        ));
        assert!(matches!(
            NodeName::from_str("peer/../a").unwrap_err(),
            NodeNameError::BadChar { .. }
        ));
        assert!(matches!(
            NodeName::from_str("café").unwrap_err(),
            NodeNameError::BadChar { .. }
        ));
        assert!(matches!(
            NodeName::from_str("peer\u{1b}[31m").unwrap_err(),
            NodeNameError::BadChar { .. }
        ));
    }

    #[test]
    fn node_names_round_trip_through_serde() {
        let name = NodeName::from_str("peer-a").unwrap();
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, "\"peer-a\"");
        assert_eq!(serde_json::from_str::<NodeName>(&json).unwrap(), name);
        assert!(serde_json::from_str::<NodeName>("\"bad name\"").is_err());
    }
}

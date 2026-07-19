//! Tolerant decoding for optional rating metadata from remote JSON APIs.
//!
//! A malformed optional rating must not make an otherwise usable catalogue or
//! search response fail. Conversion into Tributary's canonical range remains
//! the responsibility of each backend after decoding.

use serde::{Deserialize, Deserializer};

/// Decode an optional JSON integer, treating null, non-integers, wrong types,
/// and values outside `i32` as absent.
pub fn optional_i32<'de, D>(deserializer: D) -> Result<Option<i32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value
        .and_then(|value| value.as_i64())
        .and_then(|value| i32::try_from(value).ok()))
}

/// Decode an optional JSON number, treating null and wrong types as absent.
pub fn optional_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| value.as_f64()))
}

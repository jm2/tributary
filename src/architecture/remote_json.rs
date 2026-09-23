//! Content-free JSON parsing for remote (server-controlled) responses.
//!
//! `serde_json`'s own `Display`/`Debug` describe wrong-type and other data
//! errors by quoting the offending value — for example
//! `invalid type: string "…", expected u32`. Remote catalogue and
//! authentication bodies are server- or attacker-controlled, so that quoted
//! value can carry private metadata or an echoed credential and must never
//! cross this parser boundary.
//!
//! Every remote JSON decode funnels through [`parse_remote_json`], which
//! reduces a `serde_json::Error` to a fixed [`RemoteJsonParseCategory`] plus
//! the safe 1-based line/column of the failure. The content-bearing source
//! error is deliberately not retained, so no error chain, `Debug`, tracing
//! field, or UI projection can recover the response content.

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;

/// A closed, content-free categorization of a remote JSON parse failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteJsonParseCategory {
    /// The document is not syntactically valid JSON.
    Syntax,
    /// The document ended before a complete JSON value could be read.
    Eof,
    /// The document is valid JSON but does not match the expected shape.
    Data,
    /// The document could not be read from the transport at all.
    Io,
}

impl RemoteJsonParseCategory {
    /// Classify `error` without retaining any of its content-bearing detail.
    #[must_use]
    pub fn from_serde(error: &serde_json::Error) -> Self {
        match error.classify() {
            serde_json::error::Category::Syntax => Self::Syntax,
            serde_json::error::Category::Eof => Self::Eof,
            serde_json::error::Category::Data => Self::Data,
            serde_json::error::Category::Io => Self::Io,
        }
    }

    /// The fixed, locale-independent label carried in diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "invalid JSON syntax",
            Self::Eof => "unexpected end of JSON input",
            Self::Data => "JSON value has an unexpected type or shape",
            Self::Io => "JSON input could not be read",
        }
    }
}

/// Build a content-free [`BackendError::ParseError`] for a remote JSON body.
///
/// The message carries only the caller-supplied fixed `context`, the fixed
/// category, and the safe line/column position. The content-bearing
/// `serde_json::Error` is intentionally dropped rather than retained as
/// `source`.
#[must_use]
pub fn remote_json_parse_error(context: &str, error: &serde_json::Error) -> BackendError {
    let category = RemoteJsonParseCategory::from_serde(error);
    BackendError::ParseError {
        message: format!(
            "{context}: {} (line {}, column {})",
            category.as_str(),
            error.line(),
            error.column()
        ),
        source: None,
    }
}

/// Deserialize a remote JSON body, reducing any failure to a content-free
/// [`BackendError::ParseError`].
///
/// # Errors
///
/// Returns a [`BackendError::ParseError`] whose message is fixed apart from
/// the caller-supplied `context` and the failure's line/column.
pub fn parse_remote_json<T: serde::de::DeserializeOwned>(
    context: &str,
    body: &[u8],
) -> BackendResult<T> {
    serde_json::from_slice(body).map_err(|error| remote_json_parse_error(context, &error))
}

/// Test-only: flatten every `Display` in an error chain, matching what a
/// logging formatter or UI projection would render. Shared by the remote
/// client leak regressions.
#[cfg(test)]
pub fn rendered_error_chain(error: &BackendError) -> String {
    use std::fmt::Write as _;

    let mut rendered = String::new();
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(link) = current {
        let _ = writeln!(rendered, "{link}");
        current = link.source();
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify<T: serde::de::DeserializeOwned + std::fmt::Debug>(
        body: &[u8],
    ) -> RemoteJsonParseCategory {
        let error = serde_json::from_slice::<T>(body).expect_err("fixture must not deserialize");
        RemoteJsonParseCategory::from_serde(&error)
    }

    fn parsed_error(body: &[u8]) -> BackendError {
        parse_remote_json::<u32>("Failed to parse fixture JSON", body)
            .expect_err("fixture must not deserialize")
    }

    #[test]
    fn categories_are_closed_and_content_free() {
        assert_eq!(
            classify::<serde_json::Value>(b"not json"),
            RemoteJsonParseCategory::Syntax
        );
        assert_eq!(
            classify::<serde_json::Value>(b""),
            RemoteJsonParseCategory::Eof
        );
        assert_eq!(classify::<u32>(br#""text""#), RemoteJsonParseCategory::Data);
        for category in [
            RemoteJsonParseCategory::Syntax,
            RemoteJsonParseCategory::Eof,
            RemoteJsonParseCategory::Data,
            RemoteJsonParseCategory::Io,
        ] {
            assert!(!category.as_str().is_empty());
        }
    }

    #[test]
    fn wrong_type_diagnostics_do_not_leak_into_display_debug_or_chain() {
        let sentinel = "REMOTE-RESPONSE-SENTINEL-4c19e7";
        let body = format!(r#""{sentinel}""#);

        // Baseline: serde keeps the content, which is exactly why the raw
        // error cannot be retained.
        let raw = serde_json::from_slice::<u32>(body.as_bytes()).expect_err("wrong type");
        assert!(raw.to_string().contains(sentinel));

        let error = parsed_error(body.as_bytes());
        match &error {
            BackendError::ParseError { message, source } => {
                assert!(source.is_none());
                assert!(message.contains(RemoteJsonParseCategory::Data.as_str()));
                assert!(!message.contains(sentinel));
            }
            other => panic!("expected ParseError, got {other:?}"),
        }
        assert!(!format!("{error:?}").contains(sentinel));
        assert!(!error.to_string().contains(sentinel));
        assert!(!rendered_error_chain(&error).contains(sentinel));
    }

    #[test]
    fn large_wrong_type_strings_are_not_retained() {
        let sentinel = "LARGE-REMOTE-SENTINEL-9a02ff";
        let body = format!(r#""{}{sentinel}""#, "x".repeat(64 * 1024));

        let error = parsed_error(body.as_bytes());
        let rendered = format!("{error:?}\n{error}\n{}", rendered_error_chain(&error));
        assert!(!rendered.contains(sentinel));
    }
}

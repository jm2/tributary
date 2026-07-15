//! Generation-owned source navigation and asynchronous result publication.
//!
//! A source key alone cannot identify a navigation request: a user can select
//! playlist A, visit another source, and select playlist A again while the
//! first load is still running.  This module gives every accepted selection a
//! monotonically increasing generation and separately answers whether a
//! completed load may update the cache and/or the visible projection.

use std::collections::HashMap;

/// Immutable identity of one accepted source-selection request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceRequest {
    source_key: String,
    generation: u64,
}

impl SourceRequest {
    pub fn source_key(&self) -> &str {
        &self.source_key
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// What a completed asynchronous source load is still allowed to change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionDisposition {
    /// A newer request for the same source exists, or its data was invalidated.
    Ignore,
    /// This is the newest result for the source, but another source is active.
    CacheOnly,
    /// This is both the newest result and the exact active selection request.
    CacheAndRender,
}

/// Shared ordering state for source navigation and asynchronous loads.
#[derive(Debug)]
pub struct SourceNavigation {
    next_generation: u64,
    active: SourceRequest,
    latest_by_key: HashMap<String, u64>,
}

impl SourceNavigation {
    pub fn new(initial_key: impl Into<String>) -> Self {
        let initial_key = initial_key.into();
        let active = SourceRequest {
            source_key: initial_key.clone(),
            generation: 1,
        };
        Self {
            next_generation: 1,
            active,
            latest_by_key: HashMap::from([(initial_key, 1)]),
        }
    }

    /// Accept a navigation intent, including re-selection of the same key.
    pub fn select(&mut self, source_key: impl Into<String>) -> SourceRequest {
        let source_key = source_key.into();
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("source navigation generation exhausted");
        let request = SourceRequest {
            source_key: source_key.clone(),
            generation: self.next_generation,
        };
        self.latest_by_key.insert(source_key, request.generation);
        self.active = request.clone();
        request
    }

    /// Return the newest request generation minted for one source key.
    ///
    /// The visible projection can intentionally differ from the active request
    /// while a remote authentication attempt owns the deferred navigation intent.
    /// Callers that maintain the still-visible projection use this token so an
    /// away-and-back re-selection still supersedes older callbacks.
    pub fn latest_request(&self, source_key: &str) -> Option<SourceRequest> {
        self.latest_by_key
            .get(source_key)
            .copied()
            .map(|generation| SourceRequest {
                source_key: source_key.to_string(),
                generation,
            })
    }

    pub fn is_current(&self, request: &SourceRequest) -> bool {
        self.active == *request
    }

    pub fn is_key(&self, source_key: &str) -> bool {
        self.active.source_key == source_key
    }

    /// Classify a completion without mutating cache or UI state.
    pub fn completion(&self, request: &SourceRequest) -> CompletionDisposition {
        if self.latest_by_key.get(request.source_key()).copied() != Some(request.generation()) {
            CompletionDisposition::Ignore
        } else if self.is_current(request) {
            CompletionDisposition::CacheAndRender
        } else {
            CompletionDisposition::CacheOnly
        }
    }

    /// Whether an exact request may refresh the source that remains visible.
    ///
    /// Usually the visible source is also the current navigation request. A
    /// pending remote connection is the one deliberate exception: it owns the
    /// deferred intent while the prior source stays on screen. In that state,
    /// the newest generation for the visible source may still refresh its
    /// derived status/browser projection, but an older same-key generation may
    /// not.
    pub fn may_refresh_visible(
        &self,
        visible_source_key: &str,
        request: &SourceRequest,
        pending_request: Option<&SourceRequest>,
    ) -> bool {
        if request.source_key() != visible_source_key
            || self.completion(request) == CompletionDisposition::Ignore
        {
            return false;
        }

        self.is_current(request)
            || pending_request.is_some_and(|pending| {
                pending.source_key() != visible_source_key && self.is_current(pending)
            })
    }

    /// Retire all pending/cache-eligible requests in one source namespace.
    ///
    /// Playlist reconciliation uses this before clearing `playlist:*` cache
    /// rows.  A pre-reconciliation query that completes afterward therefore
    /// cannot repopulate that cache.
    pub fn invalidate_prefix(&mut self, prefix: &str) {
        self.latest_by_key
            .retain(|source_key, _| !source_key.starts_with(prefix));
    }
}

/// One in-flight remote connection paired with the navigation intent that
/// started it. Backend generations prove session ownership; this token proves
/// that the user still wants successful completion to change selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingConnection {
    source_key: String,
    request: SourceRequest,
}

impl PendingConnection {
    pub fn new(source_key: impl Into<String>, request: SourceRequest) -> Self {
        Self {
            source_key: source_key.into(),
            request,
        }
    }

    pub fn source_key(&self) -> &str {
        &self.source_key
    }

    pub fn request(&self) -> &SourceRequest {
        &self.request
    }

    pub fn may_auto_select(&self, source_key: &str, navigation: &SourceNavigation) -> bool {
        self.source_key == source_key && navigation.is_current(&self.request)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{CompletionDisposition, PendingConnection, SourceNavigation};

    fn complete(
        navigation: &SourceNavigation,
        request: &super::SourceRequest,
        value: &str,
        cache: &mut HashMap<String, String>,
        rendered: &mut Option<String>,
    ) {
        match navigation.completion(request) {
            CompletionDisposition::Ignore => {}
            CompletionDisposition::CacheOnly => {
                cache.insert(request.source_key().to_string(), value.to_string());
            }
            CompletionDisposition::CacheAndRender => {
                cache.insert(request.source_key().to_string(), value.to_string());
                *rendered = Some(value.to_string());
            }
        }
    }

    #[test]
    fn latest_inactive_result_is_cached_without_rendering() {
        let mut navigation = SourceNavigation::new("local");
        let radio = navigation.select("radio-topvote");
        navigation.select("local");

        assert_eq!(
            navigation.completion(&radio),
            CompletionDisposition::CacheOnly
        );
    }

    #[test]
    fn same_key_reselection_rejects_the_older_completion() {
        let mut navigation = SourceNavigation::new("local");
        let first = navigation.select("playlist:a");
        navigation.select("local");
        let second = navigation.select("playlist:a");

        assert_eq!(navigation.completion(&first), CompletionDisposition::Ignore);
        assert_eq!(
            navigation.completion(&second),
            CompletionDisposition::CacheAndRender
        );
    }

    #[test]
    fn reversed_completions_cannot_overwrite_newer_cache_or_rendering() {
        let mut navigation = SourceNavigation::new("local");
        let first = navigation.select("playlist:a");
        navigation.select("local");
        let second = navigation.select("playlist:a");
        let mut cache = HashMap::new();
        let mut rendered = None;

        complete(&navigation, &second, "new", &mut cache, &mut rendered);
        complete(&navigation, &first, "old", &mut cache, &mut rendered);

        assert_eq!(cache.get("playlist:a").map(String::as_str), Some("new"));
        assert_eq!(rendered.as_deref(), Some("new"));
    }

    #[test]
    fn playlist_invalidation_rejects_a_pre_reconciliation_result() {
        let mut navigation = SourceNavigation::new("local");
        let request = navigation.select("playlist:a");

        navigation.invalidate_prefix("playlist:");

        assert_eq!(
            navigation.completion(&request),
            CompletionDisposition::Ignore
        );
    }

    #[test]
    fn playlist_invalidation_does_not_supersede_pending_remote_navigation() {
        let mut navigation = SourceNavigation::new("local");
        navigation.select("playlist:a");
        let remote = navigation.select("https://music.example.test/");

        navigation.invalidate_prefix("playlist:");

        assert!(navigation.is_current(&remote));
        assert!(!navigation.is_key("playlist:a"));
    }

    #[test]
    fn pending_remote_intent_becomes_stale_after_navigation() {
        let mut navigation = SourceNavigation::new("local");
        let remote = navigation.select("https://music.example.test/");
        let pending = PendingConnection::new("https://music.example.test/", remote);
        assert!(pending.may_auto_select("https://music.example.test/", &navigation));

        navigation.select("local");

        assert!(!pending.may_auto_select("https://music.example.test/", &navigation));
    }

    #[test]
    fn local_debounce_token_is_rejected_after_away_and_back_navigation() {
        let mut navigation = SourceNavigation::new("local");
        let debounce = navigation
            .latest_request("local")
            .expect("initial local request");
        navigation.select("playlist:a");
        navigation.select("local");

        assert!(!navigation.may_refresh_visible("local", &debounce, None));
    }

    #[test]
    fn pending_remote_intent_keeps_exact_visible_generation_refreshable() {
        let mut navigation = SourceNavigation::new("local");
        let visible_local = navigation
            .latest_request("local")
            .expect("initial local request");
        let remote = navigation.select("https://music.example.test/");

        assert!(navigation.may_refresh_visible("local", &visible_local, Some(&remote)));

        navigation.select("playlist:a");

        assert!(!navigation.may_refresh_visible("local", &visible_local, Some(&remote)));
    }
}

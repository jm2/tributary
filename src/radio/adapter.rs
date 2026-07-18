//! Stateless Radio-Browser adapter for the centralized source lifecycle.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use uuid::Uuid;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::models::Track;
use crate::architecture::{SourceId, TrackId, ViewOrigin};
use crate::source_lifecycle::{
    AdapterCloseFuture, CancellationObserver, CloseAuthority, FailureCategory, LifecycleAdapter,
};
use crate::source_registry::{
    AcceptedView, ManagedSourceAdapter, PublicStreamContribution, ViewFuture, ViewLoadResult,
};

use super::api::{GeoLocation, RadioStation};
use super::client::{
    fetch_geolocation, validated_public_stream_url, RadioBrowserClient, RadioClientError,
};
use super::geo::{country_centroid, haversine_km, us_state_centroid};

const TOP_CLICKED_VIEW_KEY: &str = "top-clicked";
const TOP_VOTED_VIEW_KEY: &str = "top-voted";
const NEAR_ME_VIEW_KEY: &str = "near-me";

/// Named Radio-Browser view accepted by this adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RadioView {
    TopClicked,
    TopVoted,
    NearMe,
}

impl RadioView {
    #[cfg(test)]
    fn view_origin(self) -> ViewOrigin {
        ViewOrigin::radio(match self {
            Self::TopClicked => TOP_CLICKED_VIEW_KEY,
            Self::TopVoted => TOP_VOTED_VIEW_KEY,
            Self::NearMe => NEAR_ME_VIEW_KEY,
        })
        .expect("built-in Radio-Browser view keys are valid")
    }

    fn from_origin(origin: &ViewOrigin) -> Option<Self> {
        match origin {
            ViewOrigin::Radio(value) if value == TOP_CLICKED_VIEW_KEY => Some(Self::TopClicked),
            ViewOrigin::Radio(value) if value == TOP_VOTED_VIEW_KEY => Some(Self::TopVoted),
            ViewOrigin::Radio(value) if value == NEAR_ME_VIEW_KEY => Some(Self::NearMe),
            ViewOrigin::Radio(_) | ViewOrigin::Playlist(_) => None,
        }
    }
}

/// One stateless built-in Radio-Browser session.
pub struct RadioBrowserAdapter {
    client: RadioBrowserClient,
}

impl RadioBrowserAdapter {
    /// Construct locally; this performs no DNS lookup or network request.
    pub fn new() -> BackendResult<Self> {
        RadioBrowserClient::new()
            .map(|client| Self { client })
            .map_err(|_| {
                BackendError::Internal(anyhow::anyhow!(
                    "Radio-Browser HTTP client construction failed"
                ))
            })
    }

    async fn fetch_view(
        &self,
        view: RadioView,
        cancellation: &CancellationObserver,
    ) -> Result<Vec<RadioStation>, LoadFailure> {
        match view {
            RadioView::TopClicked => {
                cancellable(cancellation, self.client.fetch_top_click(None)).await
            }
            RadioView::TopVoted => {
                cancellable(cancellation, self.client.fetch_top_vote(None)).await
            }
            RadioView::NearMe => self.fetch_near_me(cancellation).await,
        }
    }

    async fn fetch_near_me(
        &self,
        cancellation: &CancellationObserver,
    ) -> Result<Vec<RadioStation>, LoadFailure> {
        let location = cancellable(cancellation, fetch_geolocation()).await?;
        let country_code = location.country_code.as_str();
        let region = location.region.as_str();

        let coordinate = async {
            Some(if country_code.is_empty() {
                self.client
                    .fetch_near_me(location.latitude, location.longitude, None)
                    .await
            } else {
                self.client
                    .fetch_near_me_with_country(
                        location.latitude,
                        location.longitude,
                        country_code,
                        None,
                    )
                    .await
            })
        };
        let state = async {
            if country_code.is_empty() || region.is_empty() {
                None
            } else {
                Some(
                    self.client
                        .fetch_near_me_with_state(country_code, region, None)
                        .await,
                )
            }
        };
        let country = async {
            if country_code.is_empty() {
                None
            } else {
                Some(
                    self.client
                        .fetch_near_me_country_only(country_code, Some(50))
                        .await,
                )
            }
        };

        let tiers = cancellable_value(cancellation, async {
            tokio::join!(coordinate, state, country)
        })
        .await?;
        merge_partial_near_me_tiers(tiers, &location).map_err(LoadFailure::Client)
    }
}

impl LifecycleAdapter for RadioBrowserAdapter {
    fn close(self: Arc<Self>, _authority: CloseAuthority) -> AdapterCloseFuture {
        Box::pin(async { Ok(()) })
    }
}

impl ManagedSourceAdapter for RadioBrowserAdapter {
    fn load_initial_catalogue(
        self: Arc<Self>,
    ) -> Pin<Box<dyn Future<Output = BackendResult<Vec<Track>>> + Send + 'static>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn load_view(
        self: Arc<Self>,
        view: ViewOrigin,
        cancellation: CancellationObserver,
    ) -> ViewFuture {
        Box::pin(async move {
            let Some(view) = RadioView::from_origin(&view) else {
                return ViewLoadResult::Failed(FailureCategory::Backend);
            };
            match self.fetch_view(view, &cancellation).await {
                Ok(stations) => match accepted_station_view(stations) {
                    Ok(view) => ViewLoadResult::Loaded(view),
                    Err(error) => ViewLoadResult::Failed(failure_category(error)),
                },
                Err(LoadFailure::Client(error)) => ViewLoadResult::Failed(failure_category(error)),
                Err(LoadFailure::Cancelled) => ViewLoadResult::Cancelled,
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoadFailure {
    Client(RadioClientError),
    Cancelled,
}

async fn cancellable<T>(
    cancellation: &CancellationObserver,
    request: impl Future<Output = Result<T, RadioClientError>>,
) -> Result<T, LoadFailure> {
    cancellable_value(cancellation, request)
        .await?
        .map_err(LoadFailure::Client)
}

async fn cancellable_value<T>(
    cancellation: &CancellationObserver,
    request: impl Future<Output = T>,
) -> Result<T, LoadFailure> {
    if cancellation.is_cancelled() {
        return Err(LoadFailure::Cancelled);
    }
    let mut observer = cancellation.clone();
    tokio::select! {
        biased;
        () = observer.cancelled() => Err(LoadFailure::Cancelled),
        result = request => {
            if cancellation.is_cancelled() {
                Err(LoadFailure::Cancelled)
            } else {
                Ok(result)
            }
        }
    }
}

type TierResult = Option<Result<Vec<RadioStation>, RadioClientError>>;

/// Retain every successful Near Me tier, including a successful empty tier.
/// Only a complete attempted-tier failure becomes a lifecycle failure.
fn merge_partial_near_me_tiers(
    tiers: (TierResult, TierResult, TierResult),
    location: &GeoLocation,
) -> Result<Vec<RadioStation>, RadioClientError> {
    let mut any_success = false;
    let mut preferred_failure = RadioClientError::ClientConstruction;
    let mut stations = Vec::new();
    for tier in [tiers.0, tiers.1, tiers.2].into_iter().flatten() {
        match tier {
            Ok(mut values) => {
                any_success = true;
                stations.append(&mut values);
            }
            Err(error) => preferred_failure = preferred_failure.prefer(error),
        }
    }
    if !any_success {
        return Err(preferred_failure);
    }

    Ok(deduplicate_and_sort_nearby(stations, location))
}

/// Validate and deduplicate in tier precedence order before the global stable
/// distance sort. A closer duplicate from a later tier never replaces the
/// first valid exact station ID or exact URL contributed by an earlier tier.
fn deduplicate_and_sort_nearby(
    stations: Vec<RadioStation>,
    location: &GeoLocation,
) -> Vec<RadioStation> {
    let mut seen_ids = HashSet::new();
    let mut seen_urls = HashSet::new();
    let mut unique = Vec::new();
    for station in stations {
        if validated_station_identity(&station).is_err()
            || seen_ids.contains(&station.stationuuid)
            || seen_urls.contains(&station.url_resolved)
        {
            continue;
        }
        seen_ids.insert(station.stationuuid.clone());
        seen_urls.insert(station.url_resolved.clone());
        let distance = estimate_station_distance(&station, location.latitude, location.longitude);
        unique.push((distance, station));
    }

    // Distance estimation may perform centroid lookup and Haversine
    // trigonometry. Decorate each accepted row once instead of repeating that
    // work for every sort comparison; stable sorting preserves tier/server
    // order when two rows have the same estimated distance.
    unique.sort_by(|(left, _), (right, _)| left.total_cmp(right));
    unique.into_iter().map(|(_, station)| station).collect()
}

fn deduplicate_in_source_order(stations: Vec<RadioStation>) -> Vec<RadioStation> {
    let mut seen_ids = HashSet::new();
    let mut seen_urls = HashSet::new();
    stations
        .into_iter()
        .filter(|station| {
            if validated_station_identity(station).is_err()
                || seen_ids.contains(&station.stationuuid)
                || seen_urls.contains(&station.url_resolved)
            {
                return false;
            }
            seen_ids.insert(station.stationuuid.clone());
            seen_urls.insert(station.url_resolved.clone());
            true
        })
        .collect()
}

fn validated_station_identity(
    station: &RadioStation,
) -> Result<(TrackId, url::Url), RadioClientError> {
    let track_id = TrackId::remote(station.stationuuid.clone())
        .map_err(|_| RadioClientError::InvalidResponse)?;
    let endpoint = validated_public_stream_url(&station.url_resolved)?;
    Ok((track_id, endpoint))
}

fn accepted_station_view(stations: Vec<RadioStation>) -> Result<AcceptedView, RadioClientError> {
    let stations = deduplicate_in_source_order(stations);
    let mut tracks = Vec::with_capacity(stations.len());
    let mut contributions = Vec::with_capacity(stations.len());
    for station in stations {
        let (track_id, endpoint) = validated_station_identity(&station)?;
        tracks.push(station_track(&station, track_id.clone()));
        let contribution = PublicStreamContribution::new(track_id, endpoint)
            .map_err(|_| RadioClientError::InvalidResponse)?;
        contributions.push(contribution);
    }
    AcceptedView::public_http(Arc::new(tracks), contributions)
        .map_err(|_| RadioClientError::InvalidResponse)
}

fn station_track(station: &RadioStation, track_id: TrackId) -> Track {
    let compatibility_id = Uuid::new_v5(
        &SourceId::radio_browser().as_uuid(),
        station.stationuuid.as_bytes(),
    );
    Track {
        id: compatibility_id,
        native_track_id: Some(track_id),
        title: station.name.clone(),
        artist_name: station.country.clone(),
        album_artist_name: None,
        artist_id: None,
        album_title: station.state.clone(),
        album_id: None,
        track_number: None,
        disc_number: None,
        duration_secs: None,
        composer: None,
        genre: nonempty(&station.tags),
        year: None,
        file_path: None,
        stream_url: None,
        cover_art_url: None,
        date_added: None,
        date_modified: None,
        bitrate_kbps: (station.bitrate != 0).then_some(station.bitrate),
        sample_rate_hz: None,
        format: nonempty(&station.codec),
        play_count: None,
    }
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn estimate_station_distance(station: &RadioStation, user_lat: f64, user_lon: f64) -> f64 {
    if let (Some(latitude), Some(longitude)) = (station.geo_lat, station.geo_long) {
        if valid_station_coordinates(latitude, longitude) && (latitude != 0.0 || longitude != 0.0) {
            return haversine_km(user_lat, user_lon, latitude, longitude);
        }
    }
    if !station.state.is_empty() && station.countrycode == "US" {
        if let Some((latitude, longitude)) = us_state_centroid(&station.state) {
            return haversine_km(user_lat, user_lon, latitude, longitude);
        }
    }
    if !station.countrycode.is_empty() {
        if let Some((latitude, longitude)) = country_centroid(&station.countrycode) {
            return haversine_km(user_lat, user_lon, latitude, longitude);
        }
    }
    f64::MAX
}

fn valid_station_coordinates(latitude: f64, longitude: f64) -> bool {
    latitude.is_finite()
        && longitude.is_finite()
        && (-90.0..=90.0).contains(&latitude)
        && (-180.0..=180.0).contains(&longitude)
}

const fn failure_category(error: RadioClientError) -> FailureCategory {
    match error {
        RadioClientError::ClientConstruction => FailureCategory::Backend,
        RadioClientError::Timeout => FailureCategory::Timeout,
        RadioClientError::Transport => FailureCategory::Connection,
        RadioClientError::HttpStatus
        | RadioClientError::BodyLimit
        | RadioClientError::Parse
        | RadioClientError::InvalidResponse => FailureCategory::InvalidResponse,
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use tokio::sync::oneshot;
    use tokio::time::{timeout, Duration};

    use crate::source_lifecycle::{RefreshLane, SourceLifecycleRegistry, SourceProvenance};

    use super::*;

    struct CancellationProbe;

    impl LifecycleAdapter for CancellationProbe {
        fn close(self: Arc<Self>, _authority: CloseAuthority) -> AdapterCloseFuture {
            Box::pin(async { Ok(()) })
        }
    }

    fn station(id: &str, url: &str, title: &str, latitude: f64, longitude: f64) -> RadioStation {
        RadioStation {
            stationuuid: id.to_string(),
            name: title.to_string(),
            url_resolved: url.to_string(),
            country: "United States".to_string(),
            countrycode: "US".to_string(),
            state: "Indiana".to_string(),
            codec: "MP3".to_string(),
            bitrate: 128,
            tags: "public radio".to_string(),
            favicon: String::new(),
            geo_lat: Some(latitude),
            geo_long: Some(longitude),
        }
    }

    fn location() -> GeoLocation {
        GeoLocation {
            latitude: 39.7684,
            longitude: -86.1581,
            country_code: "US".to_string(),
            region: "Indiana".to_string(),
        }
    }

    #[test]
    fn radio_view_keys_are_exact_and_other_origins_are_rejected() {
        for (view, key) in [
            (RadioView::TopClicked, TOP_CLICKED_VIEW_KEY),
            (RadioView::TopVoted, TOP_VOTED_VIEW_KEY),
            (RadioView::NearMe, NEAR_ME_VIEW_KEY),
        ] {
            let origin = view.view_origin();
            assert_eq!(origin, ViewOrigin::radio(key).expect("valid test key"));
            assert_eq!(RadioView::from_origin(&origin), Some(view));
        }
        assert_eq!(
            RadioView::from_origin(&ViewOrigin::radio("unknown").expect("valid test key")),
            None
        );
        assert_eq!(
            RadioView::from_origin(&ViewOrigin::playlist("top-clicked").expect("valid test key")),
            None
        );
    }

    #[test]
    fn station_rows_are_pathless_and_preserve_case_sensitive_bounded_ids() {
        let view = accepted_station_view(vec![station(
            "Case/Sensitive Station ID",
            "https://stream.example.test/Live?quality=high",
            "Station",
            40.0,
            -86.0,
        )])
        .expect("valid view");
        let [track] = view.tracks() else {
            panic!("one accepted station")
        };
        assert_eq!(
            track.native_track_id.as_ref().map(TrackId::as_str),
            Some("Case/Sensitive Station ID")
        );
        assert!(track.file_path.is_none());
        assert!(track.stream_url.is_none());
        assert!(track.cover_art_url.is_none());

        let invalid = station(
            "",
            "https://stream.example.test/live",
            "invalid",
            40.0,
            -86.0,
        );
        assert!(accepted_station_view(vec![invalid])
            .expect("invalid rows are quarantined")
            .tracks()
            .is_empty());
    }

    #[test]
    fn deduplication_uses_exact_id_or_url_and_keeps_first_valid_row() {
        let invalid_first = station(
            "Station-A",
            "file:///must-not-reserve-identity",
            "invalid first",
            40.0,
            -86.0,
        );
        let first = station(
            "Station-A",
            "https://stream.example.test/Live",
            "first",
            40.0,
            -86.0,
        );
        let same_id = station(
            "Station-A",
            "https://stream.example.test/other",
            "same id",
            40.0,
            -86.0,
        );
        let same_url = station(
            "station-a",
            "https://stream.example.test/Live",
            "same URL",
            40.0,
            -86.0,
        );
        let case_distinct = station(
            "station-a",
            "https://stream.example.test/live",
            "case-distinct",
            40.0,
            -86.0,
        );
        let rows = deduplicate_in_source_order(vec![
            invalid_first,
            first,
            same_id,
            same_url,
            case_distinct,
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "first");
        assert_eq!(rows[1].name, "case-distinct");
    }

    #[test]
    fn cross_tier_closer_duplicate_cannot_replace_precedence_winner_before_sort() {
        let tier_one_far = station(
            "duplicate",
            "https://stream.example.test/tier-one",
            "tier-one-far",
            48.8566,
            2.3522,
        );
        let tier_two_unique = station(
            "unique",
            "https://stream.example.test/unique",
            "tier-two-unique",
            41.0,
            -86.0,
        );
        let tier_two_closer_duplicate = station(
            "duplicate",
            "https://stream.example.test/tier-two",
            "tier-two-closer",
            39.8,
            -86.1,
        );
        let merged = merge_partial_near_me_tiers(
            (
                Some(Ok(vec![tier_one_far])),
                Some(Ok(vec![tier_two_unique, tier_two_closer_duplicate])),
                None,
            ),
            &location(),
        )
        .expect("partial tiers load");

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "tier-two-unique");
        assert_eq!(merged[1].name, "tier-one-far");
        assert!(merged
            .iter()
            .all(|station| station.name != "tier-two-closer"));
    }

    #[test]
    fn equal_distance_sort_keys_preserve_tier_and_server_order() {
        let first = station(
            "first",
            "https://stream.example.test/first",
            "first",
            40.0,
            -86.0,
        );
        let second = station(
            "second",
            "https://stream.example.test/second",
            "second",
            40.0,
            -86.0,
        );
        let rows = deduplicate_and_sort_nearby(vec![first, second], &location());
        assert_eq!(rows[0].name, "first");
        assert_eq!(rows[1].name, "second");
    }

    #[test]
    fn partial_near_me_success_including_empty_beats_other_tier_failures() {
        let one = station(
            "state",
            "https://stream.example.test/state",
            "state",
            40.0,
            -86.0,
        );
        let merged = merge_partial_near_me_tiers(
            (
                Some(Err(RadioClientError::Timeout)),
                Some(Ok(vec![one])),
                Some(Err(RadioClientError::Parse)),
            ),
            &location(),
        )
        .expect("one successful tier publishes");
        assert_eq!(merged.len(), 1);

        let empty = merge_partial_near_me_tiers(
            (
                Some(Ok(Vec::new())),
                Some(Err(RadioClientError::Transport)),
                None,
            ),
            &location(),
        )
        .expect("successful empty remains success");
        assert!(empty.is_empty());
    }

    #[test]
    fn complete_near_me_failure_uses_closed_deterministic_priority() {
        assert!(matches!(
            merge_partial_near_me_tiers(
                (
                    Some(Err(RadioClientError::HttpStatus)),
                    Some(Err(RadioClientError::Timeout)),
                    Some(Err(RadioClientError::BodyLimit)),
                ),
                &location(),
            ),
            Err(RadioClientError::Timeout)
        ));
    }

    #[test]
    fn client_failures_map_to_closed_lifecycle_categories() {
        for (error, expected) in [
            (
                RadioClientError::ClientConstruction,
                FailureCategory::Backend,
            ),
            (RadioClientError::Timeout, FailureCategory::Timeout),
            (RadioClientError::Transport, FailureCategory::Connection),
            (
                RadioClientError::HttpStatus,
                FailureCategory::InvalidResponse,
            ),
            (
                RadioClientError::BodyLimit,
                FailureCategory::InvalidResponse,
            ),
            (RadioClientError::Parse, FailureCategory::InvalidResponse),
            (
                RadioClientError::InvalidResponse,
                FailureCategory::InvalidResponse,
            ),
        ] {
            assert_eq!(failure_category(error), expected);
        }
    }

    #[tokio::test]
    async fn lifecycle_cancellation_preempts_an_inflight_adapter_request() {
        let registry = SourceLifecycleRegistry::<CancellationProbe, ()>::new(
            tokio::runtime::Handle::current(),
        );
        let source_id = SourceId::random();
        registry
            .claim_provenance(source_id, SourceProvenance::BuiltIn)
            .expect("claim built-in probe");
        registry
            .adopt_stateless_session(source_id, Box::new(CancellationProbe), ())
            .expect("adopt stateless probe");
        let owner = registry
            .begin_refresh(source_id, RefreshLane::Catalogue)
            .expect("begin cancellable refresh");
        let cancellation = owner.cancellation();
        let (started_tx, started_rx) = oneshot::channel();
        let request = tokio::spawn(async move {
            cancellable_value(&cancellation, async move {
                let _ = started_tx.send(());
                pending::<()>().await;
            })
            .await
        });

        started_rx.await.expect("request began");
        assert!(registry.cancel_refresh(source_id, &RefreshLane::Catalogue));
        assert_eq!(
            timeout(Duration::from_secs(1), request)
                .await
                .expect("cancellation wakes request")
                .expect("request task does not panic"),
            Err(LoadFailure::Cancelled)
        );
        drop(owner);
    }

    #[test]
    fn invalid_station_coordinates_fall_back_without_nan_sort_keys() {
        let invalid = station(
            "invalid-coordinates",
            "https://stream.example.test/invalid",
            "invalid",
            f64::NAN,
            999.0,
        );
        let distance = estimate_station_distance(&invalid, 39.0, -86.0);
        assert!(distance.is_finite());
    }
}

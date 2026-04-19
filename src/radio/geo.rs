//! Haversine geo-distance computation and centroid lookup tables.
//!
//! Provides client-side distance estimation for radio stations that
//! lack geographic coordinates but have country or US state metadata.
//! Uses compiled-in centroid lookup tables — no external API dependency.

/// Earth's mean radius in kilometres.
#[allow(dead_code)]
const EARTH_RADIUS_KM: f64 = 6371.0;

/// Compute the great-circle distance between two points on Earth
/// using the Haversine formula.
///
/// # Arguments
///
/// * `lat1`, `lon1` — Latitude and longitude of point 1 (degrees).
/// * `lat2`, `lon2` — Latitude and longitude of point 2 (degrees).
///
/// # Returns
///
/// Distance in kilometres.
#[allow(dead_code)]
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();

    let lat1_rad = lat1.to_radians();
    let lat2_rad = lat2.to_radians();

    let sin_d_lat_half = (d_lat / 2.0).sin();
    let sin_d_lon_half = (d_lon / 2.0).sin();
    let a = (lat1_rad.cos() * lat2_rad.cos()).mul_add(
        sin_d_lon_half * sin_d_lon_half,
        sin_d_lat_half * sin_d_lat_half,
    );
    let c = 2.0 * a.sqrt().asin();

    EARTH_RADIUS_KM * c
}

/// Look up the approximate geographic centroid of a US state.
///
/// Returns `Some((latitude, longitude))` for recognised state names
/// (case-insensitive), or `None` if the state is not found.
#[allow(dead_code)]
pub fn us_state_centroid(state: &str) -> Option<(f64, f64)> {
    let lower = state.to_lowercase();
    US_STATE_CENTROIDS
        .iter()
        .find(|(name, _, _)| *name == lower)
        .map(|(_, lat, lon)| (*lat, *lon))
}

/// Look up the approximate geographic centroid of a country by
/// ISO 3166-1 alpha-2 code.
///
/// Returns `Some((latitude, longitude))` for recognised codes
/// (case-insensitive), or `None` if the code is not found.
#[allow(dead_code)]
pub fn country_centroid(country_code: &str) -> Option<(f64, f64)> {
    let upper = country_code.to_uppercase();
    COUNTRY_CENTROIDS
        .iter()
        .find(|(code, _, _)| *code == upper)
        .map(|(_, lat, lon)| (*lat, *lon))
}

// ── US state centroids (approximate geographic centres) ─────────────
// Source: Wikipedia / US Census Bureau centroid data.

#[allow(dead_code)]
const US_STATE_CENTROIDS: &[(&str, f64, f64)] = &[
    ("alabama", 32.806671, -86.791130),
    ("alaska", 63.588753, -154.493062),
    ("arizona", 34.048928, -111.093731),
    ("arkansas", 34.969704, -92.373123),
    ("california", 36.778259, -119.417931),
    ("colorado", 39.550051, -105.782067),
    ("connecticut", 41.603221, -73.087749),
    ("delaware", 38.910832, -75.527670),
    ("florida", 27.664827, -81.515754),
    ("georgia", 32.157435, -82.907123),
    ("hawaii", 19.898682, -155.665857),
    ("idaho", 44.068202, -114.742041),
    ("illinois", 40.633125, -89.398528),
    ("indiana", 40.267194, -86.134902),
    ("iowa", 41.878003, -93.097702),
    ("kansas", 39.011902, -98.484246),
    ("kentucky", 37.839333, -84.270018),
    ("louisiana", 30.984298, -91.962333),
    ("maine", 45.253783, -69.445469),
    ("maryland", 39.045755, -76.641271),
    ("massachusetts", 42.407211, -71.382437),
    ("michigan", 44.314844, -85.602364),
    ("minnesota", 46.729553, -94.685899),
    ("mississippi", 32.354668, -89.398528),
    ("missouri", 37.964253, -91.831833),
    ("montana", 46.879682, -110.362566),
    ("nebraska", 41.492537, -99.901813),
    ("nevada", 38.802610, -116.419389),
    ("new hampshire", 43.193852, -71.572395),
    ("new jersey", 40.058324, -74.405661),
    ("new mexico", 34.519940, -105.870090),
    ("new york", 43.299428, -74.217933),
    ("north carolina", 35.759573, -79.019300),
    ("north dakota", 47.551493, -101.002012),
    ("ohio", 40.417287, -82.907123),
    ("oklahoma", 35.007752, -97.092877),
    ("oregon", 43.804133, -120.554201),
    ("pennsylvania", 41.203322, -77.194525),
    ("rhode island", 41.580095, -71.477429),
    ("south carolina", 33.836081, -81.163725),
    ("south dakota", 43.969515, -99.901813),
    ("tennessee", 35.517491, -86.580447),
    ("texas", 31.968599, -99.901813),
    ("utah", 39.320980, -111.093731),
    ("vermont", 44.558803, -72.577841),
    ("virginia", 37.431573, -78.656894),
    ("washington", 47.751074, -120.740139),
    ("west virginia", 38.597626, -80.454903),
    ("wisconsin", 43.784440, -88.787868),
    ("wyoming", 43.075968, -107.290284),
    // DC
    ("district of columbia", 38.907192, -77.036871),
];

// ── Country centroids (approximate geographic centres) ──────────────
// ISO 3166-1 alpha-2 codes.  Covers the top ~60 countries by radio
// station count in the Radio-Browser database.

#[allow(dead_code)]
const COUNTRY_CENTROIDS: &[(&str, f64, f64)] = &[
    ("AD", 42.546245, 1.601554),
    ("AE", 23.424076, 53.847818),
    ("AF", 33.939110, 67.709953),
    ("AR", -38.416097, -63.616672),
    ("AT", 47.516231, 14.550072),
    ("AU", -25.274398, 133.775136),
    ("BE", 50.503887, 4.469936),
    ("BG", 42.733883, 25.485830),
    ("BR", -14.235004, -51.925282),
    ("CA", 56.130366, -106.346771),
    ("CH", 46.818188, 8.227512),
    ("CL", -35.675147, -71.542969),
    ("CN", 35.861660, 104.195397),
    ("CO", 4.570868, -74.297333),
    ("CZ", 49.817492, 15.472962),
    ("DE", 51.165691, 10.451526),
    ("DK", 56.263920, 9.501785),
    ("EC", -1.831239, -78.183406),
    ("EE", 58.595272, 25.013607),
    ("EG", 26.820553, 30.802498),
    ("ES", 40.463667, -3.749220),
    ("FI", 61.924110, 25.748151),
    ("FR", 46.227638, 2.213749),
    ("GB", 55.378051, -3.435973),
    ("GR", 39.074208, 21.824312),
    ("HR", 45.100000, 15.200000),
    ("HU", 47.162494, 19.503304),
    ("ID", -0.789275, 113.921327),
    ("IE", 53.142100, -7.692100),
    ("IL", 31.046051, 34.851612),
    ("IN", 20.593684, 78.962880),
    ("IR", 32.427908, 53.688046),
    ("IT", 41.871940, 12.567380),
    ("JP", 36.204824, 138.252924),
    ("KE", -0.023559, 37.906193),
    ("KR", 35.907757, 127.766922),
    ("LT", 55.169438, 23.881275),
    ("LV", 56.879635, 24.603189),
    ("MX", 23.634501, -102.552784),
    ("MY", 4.210484, 101.975766),
    ("NG", 9.081999, 8.675277),
    ("NL", 52.132633, 5.291266),
    ("NO", 60.472024, 8.468946),
    ("NZ", -40.900557, 174.885971),
    ("PE", -9.189967, -75.015152),
    ("PH", 12.879721, 121.774017),
    ("PK", 30.375321, 69.345116),
    ("PL", 51.919438, 19.145136),
    ("PT", 39.399872, -8.224454),
    ("RO", 45.943161, 24.966760),
    ("RS", 44.016521, 21.005859),
    ("RU", 61.524010, 105.318756),
    ("SA", 23.885942, 45.079162),
    ("SE", 60.128161, 18.643501),
    ("SG", 1.352083, 103.819836),
    ("SK", 48.669026, 19.699024),
    ("TH", 15.870032, 100.992541),
    ("TR", 38.963745, 35.243322),
    ("TW", 23.697810, 120.960515),
    ("UA", 48.379433, 31.165580),
    ("US", 37.090240, -95.712891),
    ("UY", -32.522779, -55.765835),
    ("VE", 6.423750, -66.589730),
    ("VN", 14.058324, 108.277199),
    ("ZA", -30.559482, 22.937506),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_haversine_same_point() {
        let d = haversine_km(40.0, -86.0, 40.0, -86.0);
        assert!(d.abs() < 0.001);
    }

    #[test]
    fn test_haversine_new_york_to_london() {
        // NYC (40.7128, -74.0060) → London (51.5074, -0.1278)
        // Expected: ~5570 km
        let d = haversine_km(40.7128, -74.0060, 51.5074, -0.1278);
        assert!(d > 5500.0 && d < 5650.0, "NYC→London: {d} km");
    }

    #[test]
    fn test_haversine_antipodal() {
        // North pole to south pole: ~20015 km (half circumference)
        let d = haversine_km(90.0, 0.0, -90.0, 0.0);
        assert!(d > 20000.0 && d < 20030.0, "Pole to pole: {d} km");
    }

    #[test]
    fn test_haversine_symmetry() {
        let d1 = haversine_km(40.0, -86.0, 48.0, 2.0);
        let d2 = haversine_km(48.0, 2.0, 40.0, -86.0);
        assert!((d1 - d2).abs() < 0.001, "Asymmetric: {d1} vs {d2}");
    }

    #[test]
    fn test_us_state_centroid_found() {
        let (lat, lon) = us_state_centroid("Indiana").unwrap();
        assert!((lat - 40.267).abs() < 0.01);
        assert!((lon - (-86.135)).abs() < 0.01);
    }

    #[test]
    fn test_us_state_centroid_case_insensitive() {
        assert!(us_state_centroid("CALIFORNIA").is_some());
        assert!(us_state_centroid("california").is_some());
        assert!(us_state_centroid("California").is_some());
    }

    #[test]
    fn test_us_state_centroid_not_found() {
        assert!(us_state_centroid("Narnia").is_none());
    }

    #[test]
    fn test_country_centroid_found() {
        let (lat, lon) = country_centroid("US").unwrap();
        assert!((lat - 37.09).abs() < 0.01);
        assert!((lon - (-95.71)).abs() < 0.01);
    }

    #[test]
    fn test_country_centroid_case_insensitive() {
        assert!(country_centroid("us").is_some());
        assert!(country_centroid("US").is_some());
        assert!(country_centroid("Us").is_some());
    }

    #[test]
    fn test_country_centroid_not_found() {
        assert!(country_centroid("XX").is_none());
    }

    #[test]
    fn test_indiana_to_new_york_state() {
        let (lat1, lon1) = us_state_centroid("Indiana").unwrap();
        let (lat2, lon2) = us_state_centroid("New York").unwrap();
        let d = haversine_km(lat1, lon1, lat2, lon2);
        // ~900 km
        assert!(d > 800.0 && d < 1000.0, "IN→NY: {d} km");
    }
}

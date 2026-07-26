//! Distance between US ZIP codes.
//!
//! Coordinates come from the US Census Bureau's 2024 ZCTA Gazetteer (public
//! domain), trimmed to the Mountain West (ZIPs beginning with `8`) — the
//! region KSL Classifieds actually covers. Unknown ZIPs simply yield no
//! distance rather than a wrong one.

use std::collections::HashMap;
use std::sync::OnceLock;

/// `zip,lat,lon` per line. See `data/README` for provenance.
const CENTROIDS_CSV: &str = include_str!("../data/zip_centroids.csv");

const EARTH_RADIUS_MILES: f64 = 3958.7613;

fn table() -> &'static HashMap<&'static str, (f64, f64)> {
    static TABLE: OnceLock<HashMap<&'static str, (f64, f64)>> = OnceLock::new();
    TABLE.get_or_init(|| {
        CENTROIDS_CSV
            .lines()
            .filter_map(|line| {
                let mut parts = line.split(',');
                let zip = parts.next()?;
                let lat = parts.next()?.parse().ok()?;
                let lon = parts.next()?.parse().ok()?;
                Some((zip, (lat, lon)))
            })
            .collect()
    })
}

/// Centroid of a ZIP code, if it is in the bundled table.
pub fn coords(zip: &str) -> Option<(f64, f64)> {
    table().get(zip.trim()).copied()
}

/// Great-circle distance in miles between two ZIP centroids.
///
/// This is straight-line distance, not driving distance; real routes run
/// longer. [`crate::haul`] applies a road-winding factor on top.
pub fn miles_between(from_zip: &str, to_zip: &str) -> Option<f64> {
    let (lat1, lon1) = coords(from_zip)?;
    let (lat2, lon2) = coords(to_zip)?;
    Some(haversine_miles(lat1, lon1, lat2, lon2))
}

fn haversine_miles(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (phi1, phi2) = (lat1.to_radians(), lat2.to_radians());
    let delta_phi = (lat2 - lat1).to_radians();
    let delta_lambda = (lon2 - lon1).to_radians();
    let a = (delta_phi / 2.0).sin().powi(2)
        + phi1.cos() * phi2.cos() * (delta_lambda / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_MILES * a.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_zips_resolve() {
        let (lat, lon) = coords("84119").expect("West Valley City");
        assert!((lat - 40.70).abs() < 0.05, "lat was {lat}");
        assert!((lon + 111.95).abs() < 0.05, "lon was {lon}");
    }

    #[test]
    fn unknown_zip_is_none() {
        assert_eq!(coords("00000"), None);
        // Out of region (New York) — deliberately not bundled.
        assert_eq!(coords("10001"), None);
        assert_eq!(miles_between("84119", "10001"), None);
    }

    #[test]
    fn distance_matches_known_separation() {
        // West Valley City -> South Weber is roughly 31 miles as the crow
        // flies; allow slack for centroid placement.
        let miles = miles_between("84119", "84405").unwrap();
        assert!(
            (25.0..38.0).contains(&miles),
            "84119->84405 came out {miles:.1} mi"
        );
    }

    #[test]
    fn distance_is_symmetric_and_zero_to_self() {
        assert_eq!(miles_between("84119", "84119"), Some(0.0));
        let a = miles_between("84119", "84604").unwrap();
        let b = miles_between("84604", "84119").unwrap();
        assert!((a - b).abs() < 1e-9);
    }

    #[test]
    fn table_covers_utah() {
        let utah = table().keys().filter(|z| z.starts_with("84")).count();
        assert!(utah > 250, "only {utah} Utah ZIPs bundled");
    }
}

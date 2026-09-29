use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;

use crate::db::{self, Db, PathPoint, StoredPosition};
use crate::geo;

const MIN_SPAN_SECS: i64 = 20;
pub const SPEED_WINDOW: Duration = Duration::from_secs(60);
pub const KMH_PER_METER_PER_SECOND: f64 = 3.6;

#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn kmh(distance_m: f64, secs: i64) -> Option<f64> {
    (secs > 0).then(|| distance_m / secs as f64 * KMH_PER_METER_PER_SECOND)
}

#[must_use]
pub fn average_speed_kmh(
    points: &[PathPoint],
    window_start_unix: i64,
    window_end_unix: i64,
) -> Option<f64> {
    let first = points.first()?;
    let last = points.last()?;
    let start = window_start_unix.max(first.first_seen_unix);
    let end = window_end_unix.min(last.last_seen_unix);
    let span_secs = end - start;
    if span_secs < MIN_SPAN_SECS {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let span_secs = span_secs as f64;
    let distance_m = points.windows(2).fold(0.0, |total, pair| {
        total
            + geo::distance_m(
                (pair[0].latitude, pair[0].longitude),
                (pair[1].latitude, pair[1].longitude),
            )
    });
    Some(distance_m / span_secs * KMH_PER_METER_PER_SECOND)
}

pub async fn speeds_kmh(
    db: &Db,
    positions: &[StoredPosition],
    at_unix: i64,
) -> Result<HashMap<(i64, i64), f64>> {
    let mut vehicles_per_course: HashMap<(i64, i64), usize> = HashMap::new();
    for position in positions {
        *vehicles_per_course
            .entry((position.line_id, position.course_id))
            .or_default() += 1;
    }
    let courses: Vec<(i64, i64)> = vehicles_per_course
        .iter()
        .filter(|(_, vehicles)| **vehicles == 1)
        .map(|(course, _)| *course)
        .collect();
    let window_start_unix = at_unix - db::duration_secs(SPEED_WINDOW);
    let points = db.path_points(&courses, window_start_unix, at_unix).await?;
    let mut paths: HashMap<(i64, i64), Vec<PathPoint>> = HashMap::new();
    for point in points {
        paths
            .entry((point.line_id, point.course_id))
            .or_default()
            .push(point);
    }
    Ok(paths
        .into_iter()
        .filter_map(|(course, path)| {
            average_speed_kmh(&path, window_start_unix, at_unix).map(|speed| (course, speed))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::MicroDegrees;

    const HUNDRED_METERS_NORTH_E6: i64 = 898;

    fn point(north_e6: i64, first_seen_unix: i64, last_seen_unix: i64) -> PathPoint {
        PathPoint {
            line_id: 1,
            course_id: 1,
            latitude: MicroDegrees(51_100_000 + north_e6),
            longitude: MicroDegrees(17_000_000),
            first_seen_unix,
            last_seen_unix,
        }
    }

    #[test]
    fn computes_speed_over_the_covered_span() {
        let points = [
            point(0, 0, 45),
            point(HUNDRED_METERS_NORTH_E6, 50, 65),
            point(2 * HUNDRED_METERS_NORTH_E6, 70, 100),
        ];
        let speed = average_speed_kmh(&points, 40, 80).unwrap();
        assert!((speed - 18.0).abs() < 0.1, "{speed}");
    }

    #[test]
    fn reports_zero_for_a_parked_vehicle() {
        let speed = average_speed_kmh(&[point(0, 0, 100)], 40, 100).unwrap();
        assert!(speed.abs() < f64::EPSILON);
        assert!(speed.is_sign_positive());
    }

    #[test]
    fn needs_enough_data() {
        assert_eq!(average_speed_kmh(&[], 40, 100), None);
        assert_eq!(average_speed_kmh(&[point(0, 90, 100)], 40, 100), None);
        assert_eq!(average_speed_kmh(&[point(0, 0, 50)], 40, 100), None);
    }
}

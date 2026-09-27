use crate::db::PathPoint;
use crate::geo;

const MIN_SPAN_SECS: i64 = 20;
const KMH_PER_METER_PER_SECOND: f64 = 3.6;

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

use std::collections::{HashMap, HashSet};

use crate::db::{CourseSummary, CourseVehicle, MatchSource};
use crate::geo;

pub struct InferenceSettings {
    pub max_gap_secs: i64,
    pub max_distance_m: f64,
}

struct Proposal {
    source_course_id: i64,
    distance_m: f64,
}

fn link_distance_m(
    previous: &CourseSummary,
    next: &CourseSummary,
    settings: &InferenceSettings,
) -> Option<f64> {
    if previous.line_id != next.line_id || !next.start_is_complete {
        return None;
    }
    let gap_secs = next.first_seen_unix - previous.last_seen_unix;
    if !(0..=settings.max_gap_secs).contains(&gap_secs) {
        return None;
    }
    let distance_m = geo::distance_m(previous.end, next.start);
    (distance_m <= settings.max_distance_m).then_some(distance_m)
}

#[must_use]
pub fn infer(
    summaries: &[CourseSummary],
    known: &[CourseVehicle],
    settings: &InferenceSettings,
) -> Vec<CourseVehicle> {
    let by_course: HashMap<i64, &CourseSummary> =
        summaries.iter().map(|s| (s.course_id, s)).collect();
    let mut vehicles_per_course: HashMap<i64, Vec<&CourseVehicle>> = HashMap::new();
    for vehicle in known {
        vehicles_per_course
            .entry(vehicle.course_id)
            .or_default()
            .push(vehicle);
    }
    let mut labels: HashMap<i64, CourseVehicle> = vehicles_per_course
        .iter()
        .filter(|(_, vehicles)| vehicles.len() == 1)
        .map(|(course_id, vehicles)| (*course_id, vehicles[0].clone()))
        .collect();
    let mut blocked: HashSet<i64> = vehicles_per_course.keys().copied().collect();
    let mut inferred = Vec::new();

    loop {
        let mut proposals: Vec<(&CourseSummary, Proposal)> = Vec::new();
        for summary in summaries {
            if labels.contains_key(&summary.course_id) || blocked.contains(&summary.course_id) {
                continue;
            }
            let mut candidates = Vec::new();
            if let Some(previous) = by_course.get(&(summary.course_id - 1))
                && labels.contains_key(&previous.course_id)
                && let Some(distance_m) = link_distance_m(previous, summary, settings)
            {
                candidates.push(Proposal {
                    source_course_id: previous.course_id,
                    distance_m,
                });
            }
            if let Some(next) = by_course.get(&(summary.course_id + 1))
                && labels.contains_key(&next.course_id)
                && let Some(distance_m) = link_distance_m(summary, next, settings)
            {
                candidates.push(Proposal {
                    source_course_id: next.course_id,
                    distance_m,
                });
            }
            let side_numbers: HashSet<i64> = candidates
                .iter()
                .map(|c| labels[&c.source_course_id].side_number)
                .collect();
            if side_numbers.len() > 1 {
                blocked.insert(summary.course_id);
                continue;
            }
            let Some(proposal) = candidates.into_iter().next() else {
                continue;
            };
            proposals.push((summary, proposal));
        }
        if proposals.is_empty() {
            return inferred;
        }
        for (summary, proposal) in proposals {
            let source = &labels[&proposal.source_course_id];
            let vehicle = CourseVehicle {
                line_id: summary.line_id,
                course_id: summary.course_id,
                side_number: source.side_number,
                plate: source.plate.clone(),
                brigade: source.brigade.clone(),
                observed_unix: source.observed_unix,
                distance_m: proposal.distance_m,
                source: MatchSource::Inferred,
                inferred_from_course_id: Some(proposal.source_course_id),
            };
            labels.insert(summary.course_id, vehicle.clone());
            inferred.push(vehicle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::MicroDegrees;

    const LINE: i64 = 1;
    const OTHER_LINE: i64 = 2;
    const TERMINAL: (MicroDegrees, MicroDegrees) =
        (MicroDegrees(51_100_000), MicroDegrees(17_000_000));
    const FAR_AWAY: (MicroDegrees, MicroDegrees) =
        (MicroDegrees(51_200_000), MicroDegrees(17_000_000));
    const SETTINGS: InferenceSettings = InferenceSettings {
        max_gap_secs: 45 * 60,
        max_distance_m: 500.0,
    };

    fn trip(course_id: i64, first_seen_unix: i64, last_seen_unix: i64) -> CourseSummary {
        CourseSummary {
            line_id: LINE,
            course_id,
            first_seen_unix,
            last_seen_unix,
            start: TERMINAL,
            end: TERMINAL,
            start_is_complete: true,
        }
    }

    fn matched(course_id: i64, side_number: i64) -> CourseVehicle {
        CourseVehicle {
            line_id: LINE,
            course_id,
            side_number,
            plate: Some("DW517KF".to_owned()),
            brigade: Some("12207".to_owned()),
            observed_unix: 1_000,
            distance_m: 1.0,
            source: MatchSource::Matched,
            inferred_from_course_id: None,
        }
    }

    fn inferred_sides(result: &[CourseVehicle]) -> Vec<(i64, i64, Option<i64>)> {
        let mut sides: Vec<_> = result
            .iter()
            .map(|v| (v.course_id, v.side_number, v.inferred_from_course_id))
            .collect();
        sides.sort();
        sides
    }

    #[test]
    fn fills_neighbours_and_chains_in_both_directions() {
        let summaries = [
            trip(10, 0, 3_000),
            trip(11, 3_600, 6_600),
            trip(12, 7_200, 10_200),
            trip(13, 10_800, 13_800),
        ];
        let result = infer(&summaries, &[matched(11, 7349)], &SETTINGS);
        assert_eq!(
            inferred_sides(&result),
            [
                (10, 7349, Some(11)),
                (12, 7349, Some(11)),
                (13, 7349, Some(12))
            ]
        );
        assert!(result.iter().all(|v| v.source == MatchSource::Inferred));
        assert_eq!(result[0].brigade.as_deref(), Some("12207"));
    }

    #[test]
    fn requires_short_gap_same_line_and_nearby_terminal() {
        let late = trip(11, 3_000 + 46 * 60, 9_000);
        assert!(infer(&[trip(10, 0, 3_000), late], &[matched(10, 1)], &SETTINGS).is_empty());

        let other_line = CourseSummary {
            line_id: OTHER_LINE,
            ..trip(11, 3_600, 6_600)
        };
        assert!(
            infer(
                &[trip(10, 0, 3_000), other_line],
                &[matched(10, 1)],
                &SETTINGS
            )
            .is_empty()
        );

        let far = CourseSummary {
            start: FAR_AWAY,
            ..trip(11, 3_600, 6_600)
        };
        assert!(infer(&[trip(10, 0, 3_000), far], &[matched(10, 1)], &SETTINGS).is_empty());

        let overlapping = trip(11, 2_000, 6_600);
        assert!(
            infer(
                &[trip(10, 0, 3_000), overlapping],
                &[matched(10, 1)],
                &SETTINGS
            )
            .is_empty()
        );

        let clipped = CourseSummary {
            start_is_complete: false,
            ..trip(11, 3_600, 6_600)
        };
        assert!(infer(&[trip(10, 0, 3_000), clipped], &[matched(10, 1)], &SETTINGS).is_empty());
    }

    #[test]
    fn skips_courses_whose_neighbours_disagree() {
        let summaries = [
            trip(10, 0, 3_000),
            trip(11, 3_600, 6_600),
            trip(12, 7_200, 10_200),
        ];
        let result = infer(&summaries, &[matched(10, 1), matched(12, 2)], &SETTINGS);
        assert!(result.is_empty());
    }

    #[test]
    fn never_overrides_or_copies_from_shared_courses() {
        let summaries = [
            trip(10, 0, 3_000),
            trip(11, 3_600, 6_600),
            trip(12, 7_200, 10_200),
        ];
        let shared = [matched(10, 1), matched(10, 2), matched(12, 3)];
        let result = infer(&summaries, &shared, &SETTINGS);
        assert_eq!(inferred_sides(&result), [(11, 3, Some(12))]);
    }
}

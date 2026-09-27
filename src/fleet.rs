use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use jiff::civil;
use tracing::info;

use crate::db::{self, ActivePosition, CourseVehicle, Db, MatchSource};
use crate::fetch_errors;
use crate::geo;
use crate::inference::{self, InferenceSettings};
use crate::open_data::{Client, VehicleRecord};

const MATCH_RADIUS_M: f64 = 20.0;
const TIME_SLACK: Duration = Duration::from_secs(30);
const INFERENCE_WINDOW: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, PartialEq)]
enum Outcome {
    Matched {
        line_id: i64,
        course_id: i64,
        distance_m: f64,
        observed_unix: i64,
    },
    Ambiguous,
    NoMatch,
}

fn pick_course(
    record: &VehicleRecord,
    candidates: &[ActivePosition],
    observed_unix: i64,
) -> Outcome {
    let mut nearest_per_course: HashMap<i64, (f64, i64)> = HashMap::new();
    for candidate in candidates {
        let distance_m = geo::distance_m(
            (record.latitude, record.longitude),
            (candidate.latitude, candidate.longitude),
        );
        if distance_m > MATCH_RADIUS_M {
            continue;
        }
        let nearest = nearest_per_course
            .entry(candidate.course_id)
            .or_insert((distance_m, candidate.line_id));
        nearest.0 = nearest.0.min(distance_m);
    }
    let mut courses = nearest_per_course.into_iter();
    let Some((course_id, (distance_m, line_id))) = courses.next() else {
        return Outcome::NoMatch;
    };
    if courses.next().is_some() {
        return Outcome::Ambiguous;
    }
    Outcome::Matched {
        line_id,
        course_id,
        distance_m,
        observed_unix,
    }
}

fn combine(outcomes: Vec<Outcome>) -> Outcome {
    if outcomes.contains(&Outcome::Ambiguous) {
        return Outcome::Ambiguous;
    }
    let mut matches = outcomes
        .into_iter()
        .filter(|outcome| matches!(outcome, Outcome::Matched { .. }));
    let Some(first) = matches.next() else {
        return Outcome::NoMatch;
    };
    let Outcome::Matched { course_id, .. } = first else {
        return Outcome::NoMatch;
    };
    let other_course = matches.any(|outcome| {
        matches!(outcome, Outcome::Matched { course_id: other, .. } if other != course_id)
    });
    if other_course {
        return Outcome::Ambiguous;
    }
    first
}

#[derive(Default)]
struct Counts {
    unchanged: usize,
    matched: usize,
    ambiguous: usize,
    ambiguous_local_time: usize,
    unmatched: usize,
}

pub struct FleetCollector {
    client: Client,
    db: Db,
    inference: Option<InferenceSettings>,
    processed: HashMap<i64, civil::DateTime>,
}

impl FleetCollector {
    pub fn new(client: Client, db: Db, inference: Option<InferenceSettings>) -> Self {
        Self {
            client,
            db,
            inference,
            processed: HashMap::new(),
        }
    }

    pub async fn collect(&mut self) -> Result<()> {
        let records = fetch_errors::recorded(&self.db, self.client.fetch_vehicles().await).await?;
        let mut counts = Counts::default();
        let mut vehicles = Vec::new();
        for record in &records {
            if self.is_processed(record) {
                counts.unchanged += 1;
                continue;
            }
            if record.updated_unix_candidates.len() > 1 {
                counts.ambiguous_local_time += 1;
            }
            match self.match_record(record).await? {
                Outcome::Matched {
                    line_id,
                    course_id,
                    distance_m,
                    observed_unix,
                } => {
                    counts.matched += 1;
                    vehicles.push(CourseVehicle {
                        line_id,
                        course_id,
                        side_number: record.side_number,
                        plate: record.plate.clone(),
                        brigade: record.brigade.clone(),
                        observed_unix,
                        distance_m,
                        source: MatchSource::Matched,
                        inferred_from_course_id: None,
                    });
                }
                Outcome::Ambiguous => counts.ambiguous += 1,
                Outcome::NoMatch => counts.unmatched += 1,
            }
        }
        let saved = self.db.save_course_vehicles(&vehicles).await?;
        for record in &records {
            self.processed
                .insert(record.side_number, record.updated_local);
        }
        info!(
            records = records.len(),
            unchanged = counts.unchanged,
            matched = counts.matched,
            new = saved.inserted,
            corrected_inferences = saved.corrected,
            removed_inferences = saved.removed,
            ambiguous = counts.ambiguous,
            ambiguous_local_time = counts.ambiguous_local_time,
            unmatched = counts.unmatched,
            "saved fleet matches"
        );
        self.infer_recent().await
    }

    async fn infer_recent(&self) -> Result<()> {
        let Some(settings) = &self.inference else {
            return Ok(());
        };
        let now_unix = jiff::Timestamp::now().as_second();
        let summaries = self
            .db
            .course_summaries(now_unix - db::duration_secs(INFERENCE_WINDOW), now_unix)
            .await?;
        let course_ids: Vec<i64> = summaries.iter().map(|s| s.course_id).collect();
        let known = self.db.course_vehicles(&course_ids).await?;
        let inferred = inference::infer(&summaries, &known, settings);
        let saved = self.db.save_course_vehicles(&inferred).await?;
        info!(
            courses = summaries.len(),
            known = known.len(),
            inferred = saved.inserted,
            "saved fleet inferences"
        );
        Ok(())
    }

    async fn match_record(&self, record: &VehicleRecord) -> Result<Outcome> {
        let mut outcomes = Vec::with_capacity(record.updated_unix_candidates.len());
        for &observed_unix in &record.updated_unix_candidates {
            let candidates = self
                .db
                .positions_active_at(&record.line, observed_unix, TIME_SLACK)
                .await?;
            outcomes.push(pick_course(record, &candidates, observed_unix));
        }
        Ok(combine(outcomes))
    }

    fn is_processed(&self, record: &VehicleRecord) -> bool {
        self.processed.get(&record.side_number) == Some(&record.updated_local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::MicroDegrees;

    const TEN_METERS_NORTH_E6: i64 = 90;
    const LINE_ID: i64 = 7;
    const COURSE: i64 = 29_114_066;
    const OTHER_COURSE: i64 = 29_114_079;
    const OBSERVED: i64 = 1_000;
    const HOUR_LATER: i64 = 4_600;

    fn matched(course_id: i64, observed_unix: i64) -> Outcome {
        Outcome::Matched {
            line_id: LINE_ID,
            course_id,
            distance_m: 0.0,
            observed_unix,
        }
    }

    fn record() -> VehicleRecord {
        VehicleRecord {
            side_number: 3109,
            plate: None,
            brigade: Some("01811".to_owned()),
            line: "18".to_owned(),
            latitude: MicroDegrees(51_100_000),
            longitude: MicroDegrees(17_000_000),
            updated_local: "2026-09-26T15:00:00".parse().unwrap(),
            updated_unix_candidates: vec![OBSERVED],
        }
    }

    fn candidate(course_id: i64, north_e6: i64) -> ActivePosition {
        ActivePosition {
            line_id: LINE_ID,
            course_id,
            latitude: MicroDegrees(51_100_000 + north_e6),
            longitude: MicroDegrees(17_000_000),
        }
    }

    #[test]
    fn matches_the_only_course_nearby() {
        let candidates = [
            candidate(COURSE, 0),
            candidate(COURSE, TEN_METERS_NORTH_E6),
            candidate(OTHER_COURSE, 100 * TEN_METERS_NORTH_E6),
        ];
        assert_eq!(
            pick_course(&record(), &candidates, OBSERVED),
            matched(COURSE, OBSERVED)
        );
    }

    #[test]
    fn rejects_two_courses_nearby() {
        let candidates = [
            candidate(COURSE, 0),
            candidate(OTHER_COURSE, TEN_METERS_NORTH_E6),
        ];
        assert_eq!(
            pick_course(&record(), &candidates, OBSERVED),
            Outcome::Ambiguous
        );
    }

    #[test]
    fn rejects_courses_outside_radius() {
        let candidates = [candidate(COURSE, 3 * TEN_METERS_NORTH_E6)];
        assert_eq!(
            pick_course(&record(), &candidates, OBSERVED),
            Outcome::NoMatch
        );
        assert_eq!(pick_course(&record(), &[], OBSERVED), Outcome::NoMatch);
    }

    #[test]
    fn uses_the_only_local_time_reading_that_matches() {
        assert_eq!(
            combine(vec![Outcome::NoMatch, matched(COURSE, HOUR_LATER)]),
            matched(COURSE, HOUR_LATER)
        );
        assert_eq!(
            combine(vec![matched(COURSE, OBSERVED)]),
            matched(COURSE, OBSERVED)
        );
        assert_eq!(
            combine(vec![Outcome::NoMatch, Outcome::NoMatch]),
            Outcome::NoMatch
        );
    }

    #[test]
    fn rejects_readings_that_match_different_courses() {
        assert_eq!(
            combine(vec![
                matched(COURSE, OBSERVED),
                matched(OTHER_COURSE, HOUR_LATER)
            ]),
            Outcome::Ambiguous
        );
        assert_eq!(
            combine(vec![Outcome::Ambiguous, matched(COURSE, HOUR_LATER)]),
            Outcome::Ambiguous
        );
    }

    #[test]
    fn keeps_the_earlier_reading_when_both_match_the_same_course() {
        assert_eq!(
            combine(vec![matched(COURSE, OBSERVED), matched(COURSE, HOUR_LATER)]),
            matched(COURSE, OBSERVED)
        );
    }

    #[tokio::test]
    async fn matches_a_repeated_local_hour_to_the_right_reading() {
        const LOCAL_0230_SUMMER_TIME: i64 = 1_792_888_200;
        const LOCAL_0230_WINTER_TIME: i64 = 1_792_891_800;
        let db = Db::open(
            std::path::Path::new(":memory:"),
            db::OpenMode::CreateIfMissing,
        )
        .await
        .unwrap();
        let tram = crate::mpk::Position {
            vehicle_type: crate::mpk::VehicleType::Tram,
            line: "18".to_owned(),
            course_id: COURSE,
            latitude: MicroDegrees(51_100_000),
            longitude: MicroDegrees(17_000_000),
        };
        db.save_positions(LOCAL_0230_WINTER_TIME, &[&tram], &[])
            .await
            .unwrap();
        let client = Client::new(reqwest::Client::new()).unwrap();
        let fleet = FleetCollector::new(client, db, None);
        let record = VehicleRecord {
            updated_local: "2026-10-25T02:30:00".parse().unwrap(),
            updated_unix_candidates: vec![LOCAL_0230_SUMMER_TIME, LOCAL_0230_WINTER_TIME],
            ..record()
        };

        let outcome = fleet.match_record(&record).await.unwrap();
        assert!(
            matches!(
                outcome,
                Outcome::Matched {
                    course_id: COURSE,
                    observed_unix: LOCAL_0230_WINTER_TIME,
                    ..
                }
            ),
            "{outcome:?}"
        );
    }
}

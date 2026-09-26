use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use tracing::info;

use crate::db::{self, ActivePosition, CourseVehicle, Db, MatchSource};
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
    },
    Ambiguous,
    NoMatch,
}

fn pick_course(record: &VehicleRecord, candidates: &[ActivePosition]) -> Outcome {
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
    }
}

#[derive(Default)]
struct Counts {
    unchanged: usize,
    matched: usize,
    ambiguous: usize,
    unmatched: usize,
}

pub struct FleetCollector {
    client: Client,
    db: Db,
    inference: Option<InferenceSettings>,
    processed: HashMap<i64, i64>,
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
        let records = self.client.fetch_vehicles().await?;
        let mut counts = Counts::default();
        let mut vehicles = Vec::new();
        for record in &records {
            if self.is_processed(record) {
                counts.unchanged += 1;
                continue;
            }
            let candidates = self
                .db
                .positions_active_at(&record.line, record.updated_unix, TIME_SLACK)
                .await?;
            match pick_course(record, &candidates) {
                Outcome::Matched {
                    line_id,
                    course_id,
                    distance_m,
                } => {
                    counts.matched += 1;
                    vehicles.push(CourseVehicle {
                        line_id,
                        course_id,
                        side_number: record.side_number,
                        plate: record.plate.clone(),
                        brigade: record.brigade.clone(),
                        observed_unix: record.updated_unix,
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
                .insert(record.side_number, record.updated_unix);
        }
        info!(
            records = records.len(),
            unchanged = counts.unchanged,
            matched = counts.matched,
            new = saved.inserted,
            corrected_inferences = saved.corrected,
            removed_inferences = saved.removed,
            ambiguous = counts.ambiguous,
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

    fn is_processed(&self, record: &VehicleRecord) -> bool {
        self.processed
            .get(&record.side_number)
            .is_some_and(|&updated_unix| updated_unix >= record.updated_unix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::MicroDegrees;

    const TEN_METERS_NORTH_E6: i64 = 90;
    const LINE_ID: i64 = 7;

    fn record() -> VehicleRecord {
        VehicleRecord {
            side_number: 3109,
            plate: None,
            brigade: Some("01811".to_owned()),
            line: "18".to_owned(),
            latitude: MicroDegrees(51_100_000),
            longitude: MicroDegrees(17_000_000),
            updated_unix: 1_000,
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
            candidate(1, 0),
            candidate(1, TEN_METERS_NORTH_E6),
            candidate(2, 100 * TEN_METERS_NORTH_E6),
        ];
        assert_eq!(
            pick_course(&record(), &candidates),
            Outcome::Matched {
                line_id: LINE_ID,
                course_id: 1,
                distance_m: 0.0
            }
        );
    }

    #[test]
    fn rejects_two_courses_nearby() {
        let candidates = [candidate(1, 0), candidate(2, TEN_METERS_NORTH_E6)];
        assert_eq!(pick_course(&record(), &candidates), Outcome::Ambiguous);
    }

    #[test]
    fn rejects_courses_outside_radius() {
        let candidates = [candidate(1, 3 * TEN_METERS_NORTH_E6)];
        assert_eq!(pick_course(&record(), &candidates), Outcome::NoMatch);
        assert_eq!(pick_course(&record(), &[]), Outcome::NoMatch);
    }
}

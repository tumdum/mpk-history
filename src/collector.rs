use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::db::{self, Db};
use crate::geo::MicroDegrees;
use crate::mpk::{Client, Line, Position, VehicleType};

const MAX_MISSED_FETCHES: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VehicleKey {
    vehicle_type: VehicleType,
    line: String,
    course_id: i64,
}

impl VehicleKey {
    fn of(position: &Position) -> Self {
        Self {
            vehicle_type: position.vehicle_type,
            line: position.line.clone(),
            course_id: position.course_id,
        }
    }
}

#[derive(Debug, Clone)]
struct Tracked {
    row_id: i64,
    latitude: MicroDegrees,
    longitude: MicroDegrees,
    first_seen_unix: i64,
    last_seen_unix: i64,
}

impl Tracked {
    fn can_extend(&self, position: &Position, now_unix: i64, limits: &Limits) -> bool {
        self.latitude == position.latitude
            && self.longitude == position.longitude
            && now_unix - self.last_seen_unix <= limits.max_gap_secs
            && now_unix - self.first_seen_unix <= limits.max_span_secs
    }
}

struct Limits {
    max_gap_secs: i64,
    max_span_secs: i64,
}

#[derive(Default)]
struct Matches<'a> {
    new: Vec<&'a Position>,
    seen_again: Vec<(VehicleKey, Tracked)>,
}

impl Matches<'_> {
    fn seen_again_ids(&self) -> Vec<i64> {
        self.seen_again
            .iter()
            .map(|(_, tracked)| tracked.row_id)
            .collect()
    }
}

struct Tracker {
    vehicles: HashMap<VehicleKey, Vec<Tracked>>,
    limits: Limits,
}

impl Tracker {
    fn new(interval: Duration) -> Self {
        Self {
            vehicles: HashMap::new(),
            limits: Limits {
                max_gap_secs: db::duration_secs(interval * MAX_MISSED_FETCHES),
                max_span_secs: db::duration_secs(db::MAX_ROW_SPAN),
            },
        }
    }

    fn match_positions<'a>(&mut self, positions: &'a [Position], now_unix: i64) -> Matches<'a> {
        let mut previous = std::mem::take(&mut self.vehicles);
        let mut matches = Matches::default();
        for position in positions {
            let key = VehicleKey::of(position);
            let Some(candidates) = previous.get_mut(&key) else {
                matches.new.push(position);
                continue;
            };
            let Some(index) = candidates
                .iter()
                .position(|tracked| tracked.can_extend(position, now_unix, &self.limits))
            else {
                matches.new.push(position);
                continue;
            };
            matches
                .seen_again
                .push((key, candidates.swap_remove(index)));
        }
        matches
    }

    fn record(&mut self, matches: Matches, new_ids: Vec<i64>, now_unix: i64) {
        for (key, mut tracked) in matches.seen_again {
            tracked.last_seen_unix = now_unix;
            self.vehicles.entry(key).or_default().push(tracked);
        }
        for (position, row_id) in matches.new.into_iter().zip(new_ids) {
            let tracked = Tracked {
                row_id,
                latitude: position.latitude,
                longitude: position.longitude,
                first_seen_unix: now_unix,
                last_seen_unix: now_unix,
            };
            self.vehicles
                .entry(VehicleKey::of(position))
                .or_default()
                .push(tracked);
        }
    }
}

pub struct VehicleCollector {
    client: Client,
    db: Db,
    tracker: Tracker,
    lines: Vec<Line>,
    lines_fetched_at: Option<Instant>,
    lines_refresh: Duration,
    last_saved_unix: Option<i64>,
}

impl VehicleCollector {
    pub fn new(client: Client, db: Db, interval: Duration, lines_refresh: Duration) -> Self {
        Self {
            client,
            db,
            tracker: Tracker::new(interval),
            lines: Vec::new(),
            lines_fetched_at: None,
            lines_refresh,
            last_saved_unix: None,
        }
    }

    pub async fn collect(&mut self) -> Result<()> {
        self.refresh_lines_if_stale().await;
        if self.lines.is_empty() {
            warn!("no known lines, skipping");
            return Ok(());
        }
        let now_unix = jiff::Timestamp::now().as_second();
        if self.last_saved_unix.is_some_and(|last| now_unix <= last) {
            debug!("already saved positions in this second, skipping");
            return Ok(());
        }
        let mut positions = self.client.fetch_positions(&self.lines).await?;
        let fetched = positions.len();
        positions.retain(Position::has_valid_coordinates);
        let matches = self.tracker.match_positions(&positions, now_unix);
        let new_ids = self
            .db
            .save_positions(now_unix, &matches.new, &matches.seen_again_ids())
            .await?;
        self.last_saved_unix = Some(now_unix);
        info!(
            total = fetched,
            invalid = fetched - positions.len(),
            new = matches.new.len(),
            unchanged = matches.seen_again.len(),
            "saved positions"
        );
        self.tracker.record(matches, new_ids, now_unix);
        Ok(())
    }

    async fn refresh_lines_if_stale(&mut self) {
        let fresh = self
            .lines_fetched_at
            .is_some_and(|at| at.elapsed() < self.lines_refresh);
        if fresh {
            return;
        }
        match self.client.fetch_lines().await {
            Ok(lines) => {
                info!(count = lines.len(), "fetched line list");
                self.lines = lines;
                self.lines_fetched_at = Some(Instant::now());
            }
            Err(e) => warn!("failed to fetch line list: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_secs(10);
    const INTERVAL_SECS: i64 = 10;

    fn position(course_id: i64, latitude: f64, longitude: f64) -> Position {
        Position {
            vehicle_type: VehicleType::Bus,
            line: "133".to_owned(),
            course_id,
            latitude: latitude.into(),
            longitude: longitude.into(),
        }
    }

    fn step(
        tracker: &mut Tracker,
        positions: &[Position],
        now_unix: i64,
        next_id: &mut i64,
    ) -> (usize, Vec<i64>) {
        let matches = tracker.match_positions(positions, now_unix);
        let new_ids: Vec<i64> = (0..matches.new.len())
            .map(|_| {
                *next_id += 1;
                *next_id
            })
            .collect();
        let result = (matches.new.len(), matches.seen_again_ids());
        tracker.record(matches, new_ids, now_unix);
        result
    }

    #[test]
    fn extends_unchanged_and_inserts_moved() {
        let mut tracker = Tracker::new(INTERVAL);
        let mut next_id = 0;
        let first = [position(1, 51.0, 17.0), position(2, 51.1, 17.1)];
        assert_eq!(step(&mut tracker, &first, 0, &mut next_id), (2, vec![]));

        let second = [position(1, 51.0, 17.0), position(2, 51.2, 17.2)];
        assert_eq!(
            step(&mut tracker, &second, INTERVAL_SECS, &mut next_id),
            (1, vec![1])
        );

        let third = [position(1, 51.0, 17.0), position(2, 51.2, 17.2)];
        let (new, mut seen_again) = step(&mut tracker, &third, 2 * INTERVAL_SECS, &mut next_id);
        seen_again.sort();
        assert_eq!((new, seen_again), (0, vec![1, 3]));
    }

    #[test]
    fn tracks_two_vehicles_sharing_a_key() {
        let mut tracker = Tracker::new(INTERVAL);
        let mut next_id = 0;
        let first = [position(1, 51.0, 17.0), position(1, 51.5, 17.5)];
        assert_eq!(step(&mut tracker, &first, 0, &mut next_id), (2, vec![]));

        let second = [position(1, 51.5, 17.5), position(1, 51.0, 17.0)];
        let (new, mut seen_again) = step(&mut tracker, &second, INTERVAL_SECS, &mut next_id);
        seen_again.sort();
        assert_eq!((new, seen_again), (0, vec![1, 2]));
    }

    #[test]
    fn starts_new_row_after_gap() {
        let mut tracker = Tracker::new(INTERVAL);
        let mut next_id = 0;
        let positions = [position(1, 51.0, 17.0)];
        step(&mut tracker, &positions, 0, &mut next_id);
        let after_gap = INTERVAL_SECS * i64::from(MAX_MISSED_FETCHES) + 1;
        assert_eq!(
            step(&mut tracker, &positions, after_gap, &mut next_id),
            (1, vec![])
        );
    }

    #[test]
    fn starts_new_row_after_max_span() {
        let mut tracker = Tracker::new(INTERVAL);
        let mut next_id = 0;
        let positions = [position(1, 51.0, 17.0)];
        let span_secs = db::duration_secs(db::MAX_ROW_SPAN);
        let mut now_unix = 0;
        step(&mut tracker, &positions, now_unix, &mut next_id);
        while now_unix + INTERVAL_SECS <= span_secs {
            now_unix += INTERVAL_SECS;
            assert_eq!(
                step(&mut tracker, &positions, now_unix, &mut next_id),
                (0, vec![1])
            );
        }
        now_unix += INTERVAL_SECS;
        assert_eq!(
            step(&mut tracker, &positions, now_unix, &mut next_id),
            (1, vec![])
        );
    }

    #[test]
    fn forgets_vehicles_missing_from_fetch() {
        let mut tracker = Tracker::new(INTERVAL);
        let mut next_id = 0;
        let positions = [position(1, 51.0, 17.0)];
        step(&mut tracker, &positions, 0, &mut next_id);
        step(&mut tracker, &[], INTERVAL_SECS, &mut next_id);
        assert_eq!(
            step(&mut tracker, &positions, 2 * INTERVAL_SECS, &mut next_id),
            (1, vec![])
        );
    }
}

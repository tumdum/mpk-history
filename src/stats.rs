use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;

use crate::db::{self, Db, TypedPathPoint};
use crate::geo;
use crate::mpk::VehicleType;
use crate::speed::KMH_PER_METER_PER_SECOND;

pub const BUCKET: Duration = Duration::from_mins(1);
const COMPUTE_CHUNK: Duration = Duration::from_hours(6);
const SAVE_DELAY: Duration = Duration::from_mins(1);
const MAX_STEP_GAP: Duration = Duration::from_mins(1);
const MAX_PLAUSIBLE_SPEED_KMH: f64 = 120.0;
const METERS_PER_KILOMETER: f64 = 1000.0;
const KIND_COUNT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    All,
    Vehicle(VehicleType),
}

impl Kind {
    pub const ALL: [Kind; KIND_COUNT] = [
        Kind::All,
        Kind::Vehicle(VehicleType::Bus),
        Kind::Vehicle(VehicleType::Tram),
        Kind::Vehicle(VehicleType::Train),
    ];

    fn index(self) -> usize {
        match self {
            Kind::All => 0,
            Kind::Vehicle(VehicleType::Bus) => 1,
            Kind::Vehicle(VehicleType::Tram) => 2,
            Kind::Vehicle(VehicleType::Train) => 3,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Totals {
    samples: u64,
    vehicle_samples: u64,
    peak_vehicles: u64,
    distance_m: f64,
    travel_secs: i64,
}

impl Totals {
    fn add(&mut self, other: &Totals) {
        self.samples += other.samples;
        self.vehicle_samples += other.vehicle_samples;
        self.peak_vehicles = self.peak_vehicles.max(other.peak_vehicles);
        self.distance_m += other.distance_m;
        self.travel_secs += other.travel_secs;
    }

    fn add_sample(&mut self, vehicles: u64) {
        self.samples += 1;
        self.vehicle_samples += vehicles;
        self.peak_vehicles = self.peak_vehicles.max(vehicles);
    }

    fn add_step(&mut self, distance_m: f64, travel_secs: i64) {
        self.distance_m += distance_m;
        self.travel_secs += travel_secs;
    }

    #[must_use]
    pub fn has_vehicles(&self) -> bool {
        self.vehicle_samples > 0
    }

    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn average_vehicles(&self) -> Option<f64> {
        (self.samples > 0).then(|| self.vehicle_samples as f64 / self.samples as f64)
    }

    #[must_use]
    pub fn peak_vehicles(&self) -> Option<u64> {
        (self.samples > 0).then_some(self.peak_vehicles)
    }

    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn average_speed_kmh(&self) -> Option<f64> {
        (self.travel_secs > 0)
            .then(|| self.distance_m / self.travel_secs as f64 * KMH_PER_METER_PER_SECOND)
    }

    #[must_use]
    pub fn distance_km(&self) -> Option<f64> {
        (self.samples > 0).then(|| self.distance_m / METERS_PER_KILOMETER)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Bucket([Totals; KIND_COUNT]);

impl Bucket {
    #[must_use]
    pub fn get(&self, kind: Kind) -> &Totals {
        &self.0[kind.index()]
    }

    fn get_mut(&mut self, kind: Kind) -> &mut Totals {
        &mut self.0[kind.index()]
    }

    pub fn add(&mut self, other: &Bucket) {
        for (totals, other) in self.0.iter_mut().zip(&other.0) {
            totals.add(other);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimedBucket {
    pub start_unix: i64,
    pub bucket: Bucket,
}

#[must_use]
pub fn sum<'a>(buckets: impl IntoIterator<Item = &'a TimedBucket>) -> Bucket {
    let mut total = Bucket::default();
    for timed in buckets {
        total.add(&timed.bucket);
    }
    total
}

#[must_use]
pub fn regroup(buckets: &[TimedBucket], step: Duration) -> Vec<TimedBucket> {
    let step_secs = db::duration_secs(step);
    regroup_by(buckets, |start_unix| align_down(start_unix, step_secs))
}

#[must_use]
pub fn regroup_by(buckets: &[TimedBucket], group_start: impl Fn(i64) -> i64) -> Vec<TimedBucket> {
    let mut grouped: Vec<TimedBucket> = Vec::new();
    for timed in buckets {
        let start_unix = group_start(timed.start_unix);
        match grouped.last_mut() {
            Some(last) if last.start_unix == start_unix => last.bucket.add(&timed.bucket),
            _ => grouped.push(TimedBucket {
                start_unix,
                bucket: timed.bucket,
            }),
        }
    }
    grouped
}

#[must_use]
pub fn align_down(unix: i64, step_secs: i64) -> i64 {
    unix.div_euclid(step_secs) * step_secs
}

#[must_use]
pub fn align_up(unix: i64, step_secs: i64) -> i64 {
    align_down(unix + step_secs - 1, step_secs)
}

fn bucket_starts(from_unix: i64, to_unix: i64) -> impl Iterator<Item = i64> {
    starts(from_unix, to_unix, db::duration_secs(BUCKET))
}

fn starts(from_unix: i64, to_unix: i64, step_secs: i64) -> impl Iterator<Item = i64> {
    std::iter::successors(Some(from_unix), move |start| Some(start + step_secs))
        .take_while(move |start| *start < to_unix)
}

#[derive(Debug, Clone, Copy)]
pub enum Retention {
    Last(Duration),
    Forever,
}

pub struct StatsCache {
    resolution: Duration,
    retention: Retention,
    finished: Mutex<BTreeMap<i64, Bucket>>,
    computing: tokio::sync::Mutex<()>,
}

impl StatsCache {
    /// `resolution` must be a whole number of minutes that divides six hours.
    #[must_use]
    pub fn new(resolution: Duration, retention: Retention) -> Self {
        Self {
            resolution,
            retention,
            finished: Mutex::default(),
            computing: tokio::sync::Mutex::default(),
        }
    }

    pub async fn buckets(
        &self,
        db: &Db,
        from_unix: i64,
        to_unix: i64,
        now_unix: i64,
    ) -> Result<Vec<TimedBucket>> {
        let resolution_secs = db::duration_secs(self.resolution);
        let from_unix = align_down(from_unix, resolution_secs);
        let to_unix = align_up(to_unix, resolution_secs);
        let final_before_unix =
            now_unix - resolution_secs - db::duration_secs(max_step_span() + SAVE_DELAY);
        let _computing = self.computing.lock().await;
        let mut unfinished = BTreeMap::new();
        if let Some((first_unix, end_unix)) = self.missing_range(from_unix, to_unix) {
            let chunk_secs = db::duration_secs(COMPUTE_CHUNK);
            let margin_secs = db::duration_secs(max_step_span());
            let mut chunk_start_unix = first_unix;
            while chunk_start_unix < end_unix {
                let chunk_end_unix = (chunk_start_unix + chunk_secs).min(end_unix);
                let points = db
                    .path_points_started_between(
                        chunk_start_unix - margin_secs,
                        chunk_end_unix + margin_secs,
                    )
                    .await?;
                let minutes: Vec<TimedBucket> = bucket_starts(chunk_start_unix, chunk_end_unix)
                    .zip(compute(&points, chunk_start_unix, chunk_end_unix))
                    .map(|(start_unix, bucket)| TimedBucket { start_unix, bucket })
                    .collect();
                let mut finished = self.finished.lock();
                for timed in regroup(&minutes, self.resolution) {
                    if timed.start_unix <= final_before_unix {
                        finished.insert(timed.start_unix, timed.bucket);
                    } else {
                        unfinished.insert(timed.start_unix, timed.bucket);
                    }
                }
                chunk_start_unix = chunk_end_unix;
            }
        }
        let mut finished = self.finished.lock();
        let buckets = starts(from_unix, to_unix, resolution_secs)
            .map(|start_unix| TimedBucket {
                start_unix,
                bucket: unfinished
                    .get(&start_unix)
                    .or_else(|| finished.get(&start_unix))
                    .copied()
                    .unwrap_or_default(),
            })
            .collect();
        if let Retention::Last(retention) = self.retention {
            let oldest_kept_unix = now_unix - db::duration_secs(retention) - resolution_secs;
            *finished = finished.split_off(&oldest_kept_unix);
        }
        Ok(buckets)
    }

    fn missing_range(&self, from_unix: i64, to_unix: i64) -> Option<(i64, i64)> {
        let resolution_secs = db::duration_secs(self.resolution);
        let finished = self.finished.lock();
        let mut missing = starts(from_unix, to_unix, resolution_secs)
            .filter(|start| !finished.contains_key(start));
        let first = missing.next()?;
        let last = missing.last().unwrap_or(first);
        Some((first, last + resolution_secs))
    }
}

fn max_step_span() -> Duration {
    db::MAX_ROW_SPAN + MAX_STEP_GAP
}

fn compute(points: &[TypedPathPoint], from_unix: i64, to_unix: i64) -> Vec<Bucket> {
    let mut buckets: Vec<Bucket> = bucket_starts(from_unix, to_unix)
        .map(|_| Bucket::default())
        .collect();
    add_vehicle_samples(points, from_unix, to_unix, &mut buckets);
    add_steps(points, from_unix, to_unix, &mut buckets);
    buckets
}

fn bucket_index(unix: i64, from_unix: i64) -> usize {
    usize::try_from((unix - from_unix) / db::duration_secs(BUCKET)).unwrap_or_default()
}

fn fetch_times(points: &[TypedPathPoint], from_unix: i64, to_unix: i64) -> Vec<i64> {
    let mut times: Vec<i64> = points
        .iter()
        .flat_map(|p| [p.point.first_seen_unix, p.point.last_seen_unix])
        .filter(|unix| (from_unix..to_unix).contains(unix))
        .collect();
    times.sort_unstable();
    times.dedup();
    times
}

fn add_vehicle_samples(
    points: &[TypedPathPoint],
    from_unix: i64,
    to_unix: i64,
    buckets: &mut [Bucket],
) {
    let times = fetch_times(points, from_unix, to_unix);
    let mut changes = vec![[0i64; KIND_COUNT]; times.len() + 1];
    for p in points {
        let start = times.partition_point(|unix| *unix < p.point.first_seen_unix);
        let end = times.partition_point(|unix| *unix <= p.point.last_seen_unix);
        if start >= end {
            continue;
        }
        let kind = Kind::Vehicle(p.vehicle_type).index();
        changes[start][kind] += 1;
        changes[end][kind] -= 1;
    }
    let mut visible = [0i64; KIND_COUNT];
    for (unix, change) in times.iter().zip(&changes) {
        for (count, delta) in visible.iter_mut().zip(change) {
            *count += delta;
        }
        let bucket = &mut buckets[bucket_index(*unix, from_unix)];
        let mut all = 0;
        for vehicle_type in VehicleType::ALL {
            let kind = Kind::Vehicle(vehicle_type);
            let count = u64::try_from(visible[kind.index()]).unwrap_or_default();
            bucket.get_mut(kind).add_sample(count);
            all += count;
        }
        bucket.get_mut(Kind::All).add_sample(all);
    }
}

fn add_steps(points: &[TypedPathPoint], from_unix: i64, to_unix: i64, buckets: &mut [Bucket]) {
    let fetches = fetch_times(points, i64::MIN, i64::MAX);
    for (index, current) in points.iter().enumerate() {
        let a = &current.point;
        let next = points
            .get(index + 1)
            .filter(|next| (next.point.line_id, next.point.course_id) == (a.line_id, a.course_id));
        let travel = match next.and_then(|next| step(current, next)) {
            Some(travel) => travel,
            None if collector_paused_after(&fetches, a.last_seen_unix) => standing(current),
            None => continue,
        };
        add_travel(buckets, from_unix, to_unix, &travel);
    }
}

fn collector_paused_after(fetches: &[i64], last_seen_unix: i64) -> bool {
    let after = fetches.partition_point(|unix| *unix <= last_seen_unix);
    fetches
        .get(after)
        .is_none_or(|unix| *unix - last_seen_unix > db::duration_secs(MAX_STEP_GAP))
}

fn standing(current: &TypedPathPoint) -> Travel {
    Travel {
        kind: Kind::Vehicle(current.vehicle_type),
        start_unix: current.point.first_seen_unix,
        end_unix: current.point.last_seen_unix,
        distance_m: 0.0,
    }
}

fn step(current: &TypedPathPoint, next: &TypedPathPoint) -> Option<Travel> {
    let (a, b) = (&current.point, &next.point);
    if b.first_seen_unix - a.last_seen_unix > db::duration_secs(MAX_STEP_GAP) {
        return None;
    }
    let travel_secs = b.first_seen_unix - a.first_seen_unix;
    if travel_secs <= 0 {
        return None;
    }
    let distance_m = geo::distance_m((a.latitude, a.longitude), (b.latitude, b.longitude));
    #[allow(clippy::cast_precision_loss)]
    let speed_kmh = distance_m / travel_secs as f64 * KMH_PER_METER_PER_SECOND;
    if speed_kmh > MAX_PLAUSIBLE_SPEED_KMH {
        return None;
    }
    Some(Travel {
        kind: Kind::Vehicle(current.vehicle_type),
        start_unix: a.first_seen_unix,
        end_unix: b.first_seen_unix,
        distance_m,
    })
}

struct Travel {
    kind: Kind,
    start_unix: i64,
    end_unix: i64,
    distance_m: f64,
}

#[allow(clippy::cast_precision_loss)]
fn add_travel(buckets: &mut [Bucket], from_unix: i64, to_unix: i64, travel: &Travel) {
    let span_secs = travel.end_unix - travel.start_unix;
    if span_secs <= 0 {
        return;
    }
    let bucket_secs = db::duration_secs(BUCKET);
    let end_unix = travel.end_unix.min(to_unix);
    let mut piece_start_unix = travel.start_unix.max(from_unix);
    while piece_start_unix < end_unix {
        let piece_end_unix =
            (align_down(piece_start_unix, bucket_secs) + bucket_secs).min(end_unix);
        let secs = piece_end_unix - piece_start_unix;
        let distance_m = travel.distance_m * secs as f64 / span_secs as f64;
        let bucket = &mut buckets[bucket_index(piece_start_unix, from_unix)];
        bucket.get_mut(travel.kind).add_step(distance_m, secs);
        bucket.get_mut(Kind::All).add_step(distance_m, secs);
        piece_start_unix = piece_end_unix;
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::db::{OpenMode, PathPoint};
    use crate::geo::MicroDegrees;
    use crate::mpk::Position;

    const IN_MEMORY: &str = ":memory:";
    const START: i64 = 1_800_000_000;
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const FETCH_SECS: i64 = 10;
    const LATITUDE_E6: i64 = 51_100_000;
    const LONGITUDE_E6: i64 = 17_000_000;
    const HUNDRED_METERS_NORTH_E6: i64 = 898;
    const TEN_KILOMETERS_NORTH_E6: i64 = 100 * HUNDRED_METERS_NORTH_E6;
    const HUNDRED_METERS_PER_FETCH_KMH: f64 = 36.0;
    const SPEED_TOLERANCE_KMH: f64 = 0.1;
    const FLOAT_TOLERANCE: f64 = 1e-9;

    fn point(
        vehicle_type: VehicleType,
        course_id: i64,
        north_e6: i64,
        first_seen_unix: i64,
        last_seen_unix: i64,
    ) -> TypedPathPoint {
        TypedPathPoint {
            vehicle_type,
            point: PathPoint {
                line_id: vehicle_type.id(),
                course_id,
                latitude: MicroDegrees(LATITUDE_E6 + north_e6),
                longitude: MicroDegrees(LONGITUDE_E6),
                first_seen_unix,
                last_seen_unix,
            },
        }
    }

    fn moving(
        vehicle_type: VehicleType,
        course_id: i64,
        from_unix: i64,
        to_unix: i64,
        north_e6_per_fetch: i64,
    ) -> Vec<TypedPathPoint> {
        std::iter::successors(Some(from_unix), |unix| Some(unix + FETCH_SECS))
            .take_while(|unix| *unix <= to_unix)
            .enumerate()
            .map(|(index, unix)| {
                let fetches = i64::try_from(index).unwrap();
                point(
                    vehicle_type,
                    course_id,
                    fetches * north_e6_per_fetch,
                    unix,
                    unix,
                )
            })
            .collect()
    }

    fn sorted(mut points: Vec<TypedPathPoint>) -> Vec<TypedPathPoint> {
        points.sort_by_key(|p| (p.point.line_id, p.point.course_id, p.point.first_seen_unix));
        points
    }

    fn assert_close(actual: Option<f64>, expected: f64, tolerance: f64) {
        let actual = actual.unwrap();
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} != {expected}"
        );
    }

    const BUS: Kind = Kind::Vehicle(VehicleType::Bus);
    const TRAM: Kind = Kind::Vehicle(VehicleType::Tram);
    const TRAIN: Kind = Kind::Vehicle(VehicleType::Train);

    #[test]
    fn counts_vehicles_visible_at_each_fetch() {
        let mut points = moving(VehicleType::Bus, 1, START, START + 50, 0);
        points.push(point(VehicleType::Tram, 2, 0, START + 20, START + 50));
        let [bucket] = compute(&sorted(points), START, START + MINUTE)[..] else {
            panic!("expected one bucket");
        };

        assert_close(bucket.get(BUS).average_vehicles(), 1.0, FLOAT_TOLERANCE);
        assert_close(
            bucket.get(TRAM).average_vehicles(),
            4.0 / 6.0,
            FLOAT_TOLERANCE,
        );
        assert_close(
            bucket.get(Kind::All).average_vehicles(),
            10.0 / 6.0,
            FLOAT_TOLERANCE,
        );
        assert_eq!(bucket.get(Kind::All).peak_vehicles(), Some(2));
        assert_eq!(bucket.get(BUS).peak_vehicles(), Some(1));
        assert_eq!(bucket.get(TRAIN).average_vehicles(), Some(0.0));
        assert!(!bucket.get(TRAIN).has_vehicles());
    }

    #[test]
    fn counts_rows_that_started_before_the_window() {
        let points = [point(VehicleType::Tram, 1, 0, START - 100, START + 20)];
        let [bucket] = compute(&points, START, START + MINUTE)[..] else {
            panic!("expected one bucket");
        };
        assert_eq!(bucket.get(TRAM).average_vehicles(), Some(1.0));
    }

    #[test]
    fn computes_speed_from_consecutive_positions() {
        let points = moving(
            VehicleType::Bus,
            1,
            START,
            START + 50,
            HUNDRED_METERS_NORTH_E6,
        );
        let [bucket] = compute(&points, START, START + MINUTE)[..] else {
            panic!("expected one bucket");
        };
        let bus = bucket.get(BUS);
        assert_close(
            bus.average_speed_kmh(),
            HUNDRED_METERS_PER_FETCH_KMH,
            SPEED_TOLERANCE_KMH,
        );
        assert_close(bus.distance_km(), 0.5, 0.001);
        assert_eq!(bucket.get(Kind::All), bus);
    }

    #[test]
    fn standing_time_lowers_the_speed() {
        let span_secs = db::duration_secs(db::MAX_ROW_SPAN);
        let tram_end_unix = START + span_secs + FETCH_SECS;
        let mut points = moving(
            VehicleType::Bus,
            1,
            START,
            tram_end_unix,
            HUNDRED_METERS_NORTH_E6,
        );
        points.push(point(VehicleType::Tram, 2, 0, START, START + span_secs));
        points.push(point(VehicleType::Tram, 2, 0, tram_end_unix, tram_end_unix));
        let total = sum(&bucket_starts(START, START + 11 * MINUTE)
            .zip(compute(&sorted(points), START, START + 11 * MINUTE))
            .map(|(start_unix, bucket)| TimedBucket { start_unix, bucket })
            .collect::<Vec<_>>());

        assert_eq!(total.get(TRAM).average_speed_kmh(), Some(0.0));
        assert_close(
            total.get(Kind::All).average_speed_kmh(),
            HUNDRED_METERS_PER_FETCH_KMH / 2.0,
            SPEED_TOLERANCE_KMH,
        );
    }

    #[test]
    fn skips_steps_after_gaps_and_implausible_jumps() {
        let points = [
            point(VehicleType::Bus, 1, 0, START, START),
            point(
                VehicleType::Bus,
                1,
                HUNDRED_METERS_NORTH_E6,
                START + 10,
                START + 10,
            ),
            point(
                VehicleType::Bus,
                1,
                2 * HUNDRED_METERS_NORTH_E6,
                START + 90,
                START + 90,
            ),
            point(
                VehicleType::Bus,
                1,
                TEN_KILOMETERS_NORTH_E6,
                START + 100,
                START + 100,
            ),
        ];
        let buckets = compute(&points, START, START + 2 * MINUTE);
        let total = sum(&buckets
            .into_iter()
            .map(|bucket| TimedBucket {
                start_unix: START,
                bucket,
            })
            .collect::<Vec<_>>());
        let bus = total.get(BUS);
        assert_eq!(bus.travel_secs, FETCH_SECS);
        assert_close(
            bus.average_speed_kmh(),
            HUNDRED_METERS_PER_FETCH_KMH,
            SPEED_TOLERANCE_KMH,
        );
    }

    #[test]
    fn does_not_join_different_courses() {
        let points = sorted(vec![
            point(VehicleType::Bus, 1, 0, START, START),
            point(
                VehicleType::Bus,
                2,
                HUNDRED_METERS_NORTH_E6,
                START + 10,
                START + 10,
            ),
        ]);
        let [bucket] = compute(&points, START, START + MINUTE)[..] else {
            panic!("expected one bucket");
        };
        assert_eq!(bucket.get(BUS).average_speed_kmh(), None);
    }

    #[test]
    fn puts_samples_and_steps_in_their_minute() {
        let points = moving(
            VehicleType::Bus,
            1,
            START + 40,
            START + 70,
            HUNDRED_METERS_NORTH_E6,
        );
        let buckets = compute(&points, START, START + 3 * MINUTE);
        let samples: Vec<u64> = buckets.iter().map(|b| b.get(BUS).samples).collect();
        assert_eq!(samples, [2, 2, 0]);
        let travel: Vec<i64> = buckets.iter().map(|b| b.get(BUS).travel_secs).collect();
        assert_eq!(travel, [2 * FETCH_SECS, FETCH_SECS, 0]);
        assert_eq!(buckets[2].get(Kind::All).average_vehicles(), None);
        assert_eq!(buckets[2].get(Kind::All).distance_km(), None);
    }

    fn final_delay_secs() -> i64 {
        db::duration_secs(max_step_span() + SAVE_DELAY)
    }

    fn travel_per_minute(buckets: &[Bucket], kind: Kind) -> Vec<i64> {
        buckets.iter().map(|b| b.get(kind).travel_secs).collect()
    }

    #[test]
    fn spreads_standing_time_over_the_minutes_it_covers() {
        let points = [
            point(VehicleType::Tram, 1, 0, START + 30, START + 150),
            point(VehicleType::Tram, 1, 0, START + 160, START + 160),
        ];
        let buckets = compute(&points, START, START + 3 * MINUTE);
        assert_eq!(travel_per_minute(&buckets, TRAM), [30, 60, 40]);
        assert_eq!(travel_per_minute(&buckets, Kind::All), [30, 60, 40]);
        assert!(buckets.iter().all(|b| b.get(TRAM).distance_m == 0.0));
    }

    #[test]
    fn splits_a_move_across_minutes() {
        let points = [
            point(VehicleType::Bus, 1, 0, START + 55, START + 55),
            point(
                VehicleType::Bus,
                1,
                HUNDRED_METERS_NORTH_E6,
                START + 65,
                START + 65,
            ),
        ];
        let buckets = compute(&points, START, START + 2 * MINUTE);
        assert_eq!(travel_per_minute(&buckets, BUS), [5, 5]);
        for bucket in &buckets {
            assert_close(bucket.get(BUS).distance_km(), 0.05, 0.001);
            assert_close(
                bucket.get(BUS).average_speed_kmh(),
                HUNDRED_METERS_PER_FETCH_KMH,
                SPEED_TOLERANCE_KMH,
            );
        }
    }

    #[test]
    fn counts_standing_time_while_nothing_newer_was_fetched() {
        let tram = point(VehicleType::Tram, 1, 0, START, START + 100);
        let latest = compute(&[tram], START, START + 3 * MINUTE);
        assert_eq!(travel_per_minute(&latest, TRAM), [60, 40, 0]);
        assert_eq!(latest[0].get(TRAM).average_speed_kmh(), Some(0.0));

        let mut points = moving(VehicleType::Bus, 2, START + 110, START + 150, 0);
        points.push(tram);
        let course_ended = compute(&sorted(points), START, START + 3 * MINUTE);
        assert_eq!(travel_per_minute(&course_ended, TRAM), [0, 0, 0]);
    }

    #[test]
    fn skips_standing_time_when_only_the_vehicle_went_missing() {
        let mut points = moving(VehicleType::Bus, 2, START + 40, START + 150, 0);
        points.push(point(VehicleType::Tram, 1, 0, START, START + 50));
        points.push(point(VehicleType::Tram, 1, 0, START + 150, START + 150));
        let buckets = compute(&sorted(points), START, START + 3 * MINUTE);
        assert_eq!(travel_per_minute(&buckets, TRAM), [0, 0, 0]);
    }

    #[test]
    fn counts_the_time_at_a_stale_position_as_part_of_the_move() {
        let points = [
            point(VehicleType::Bus, 1, 0, START, START + 15),
            point(
                VehicleType::Bus,
                1,
                2 * HUNDRED_METERS_NORTH_E6,
                START + 20,
                START + 20,
            ),
        ];
        let [bucket] = compute(&points, START, START + MINUTE)[..] else {
            panic!("expected one bucket");
        };
        assert_eq!(bucket.get(BUS).travel_secs, 20);
        assert_close(
            bucket.get(BUS).average_speed_kmh(),
            HUNDRED_METERS_PER_FETCH_KMH,
            SPEED_TOLERANCE_KMH,
        );
    }

    #[test]
    fn keeps_standing_time_before_a_break_in_the_data() {
        let points = sorted(vec![
            point(VehicleType::Tram, 1, 0, START, START + 50),
            point(VehicleType::Tram, 1, 0, START + 150, START + 150),
            point(VehicleType::Bus, 2, 0, START + 40, START + 40),
            point(
                VehicleType::Bus,
                2,
                HUNDRED_METERS_NORTH_E6,
                START + 50,
                START + 50,
            ),
        ]);
        let [bucket, _, _] = compute(&points, START, START + 3 * MINUTE)[..] else {
            panic!("expected three buckets");
        };
        assert_eq!(bucket.get(TRAM).travel_secs, 50);
        assert_close(
            bucket.get(Kind::All).average_speed_kmh(),
            HUNDRED_METERS_PER_FETCH_KMH * 10.0 / 60.0,
            SPEED_TOLERANCE_KMH,
        );
    }

    #[test]
    fn regroups_minutes_into_aligned_steps() {
        let points = moving(VehicleType::Bus, 1, START, START + 170, 0);
        let buckets: Vec<TimedBucket> = bucket_starts(START + MINUTE, START + 4 * MINUTE)
            .zip(compute(&points, START + MINUTE, START + 4 * MINUTE))
            .map(|(start_unix, bucket)| TimedBucket { start_unix, bucket })
            .collect();
        let grouped = regroup(&buckets, Duration::from_mins(2));
        let starts: Vec<i64> = grouped.iter().map(|b| b.start_unix).collect();
        assert_eq!(starts, [START, START + 2 * MINUTE]);
        let samples: Vec<u64> = grouped.iter().map(|b| b.bucket.get(BUS).samples).collect();
        assert_eq!(samples, [6, 6]);
        assert_eq!(sum(&grouped), sum(&buckets));
    }

    #[test]
    fn aligns_to_steps() {
        assert_eq!(align_down(START + 59, MINUTE), START);
        assert_eq!(align_up(START + 1, MINUTE), START + MINUTE);
        assert_eq!(align_up(START, MINUTE), START);
        assert_eq!(align_down(-1, MINUTE), -MINUTE);
    }

    fn position(vehicle_type: VehicleType, course_id: i64) -> Position {
        Position {
            vehicle_type,
            line: vehicle_type.as_str().to_owned(),
            course_id,
            latitude: MicroDegrees(LATITUDE_E6),
            longitude: MicroDegrees(LONGITUDE_E6),
        }
    }

    fn vehicles_per_minute(buckets: &[TimedBucket], kind: Kind) -> Vec<Option<f64>> {
        buckets
            .iter()
            .map(|b| b.bucket.get(kind).average_vehicles())
            .collect()
    }

    #[tokio::test]
    async fn caches_only_finished_minutes() {
        let db = Db::open(Path::new(IN_MEMORY), OpenMode::CreateIfMissing)
            .await
            .unwrap();
        let bus = position(VehicleType::Bus, 1);
        for minute in 0..3 {
            db.save_positions(START + minute * MINUTE, &[&bus], &[])
                .await
                .unwrap();
        }
        let cache = StatsCache::new(BUCKET, Retention::Last(Duration::from_hours(1)));
        let now_unix = START + 2 * MINUTE + final_delay_secs();
        let range = (START, START + 3 * MINUTE);
        let before = cache
            .buckets(&db, range.0, range.1, now_unix)
            .await
            .unwrap();
        assert_eq!(
            vehicles_per_minute(&before, TRAM),
            [Some(0.0), Some(0.0), Some(0.0)]
        );
        assert_eq!(
            vehicles_per_minute(&before, BUS),
            [Some(1.0), Some(1.0), Some(1.0)]
        );

        let tram = position(VehicleType::Tram, 2);
        for minute in 0..3 {
            db.save_positions(START + minute * MINUTE + FETCH_SECS, &[&tram], &[])
                .await
                .unwrap();
        }
        let after = cache
            .buckets(&db, range.0, range.1, now_unix)
            .await
            .unwrap();
        assert_eq!(after[..2], before[..2]);
        assert_eq!(vehicles_per_minute(&after, TRAM)[2], Some(0.5));
    }

    #[tokio::test]
    async fn counts_steps_across_chunk_boundaries_once() {
        let db = Db::open(Path::new(IN_MEMORY), OpenMode::CreateIfMissing)
            .await
            .unwrap();
        let boundary_unix = START + db::duration_secs(COMPUTE_CHUNK);
        let tram = position(VehicleType::Tram, 1);
        let ids = db
            .save_positions(boundary_unix - 5 * MINUTE, &[&tram], &[])
            .await
            .unwrap();
        db.save_positions(boundary_unix + 5 * MINUTE, &[], &ids)
            .await
            .unwrap();
        db.save_positions(boundary_unix + 5 * MINUTE + FETCH_SECS, &[&tram], &[])
            .await
            .unwrap();

        let cache = StatsCache::new(BUCKET, Retention::Last(Duration::from_hours(24)));
        let to_unix = boundary_unix + 10 * MINUTE;
        let buckets = cache
            .buckets(&db, START, to_unix, to_unix + HOUR)
            .await
            .unwrap();
        assert_eq!(
            sum(&buckets).get(TRAM).travel_secs,
            10 * MINUTE + FETCH_SECS
        );
        let around_boundary: Vec<i64> = buckets
            .iter()
            .filter(|b| (boundary_unix - MINUTE..=boundary_unix).contains(&b.start_unix))
            .map(|b| b.bucket.get(TRAM).travel_secs)
            .collect();
        assert_eq!(around_boundary, [MINUTE, MINUTE]);
    }

    #[tokio::test]
    async fn keeps_coarse_buckets_forever() {
        let db = Db::open(Path::new(IN_MEMORY), OpenMode::CreateIfMissing)
            .await
            .unwrap();
        let bus = position(VehicleType::Bus, 1);
        for minute in 0..20 {
            db.save_positions(START + minute * MINUTE, &[&bus], &[])
                .await
                .unwrap();
        }
        let quarter = Duration::from_mins(15);
        let cache = StatsCache::new(quarter, Retention::Forever);
        let now_unix = START + 24 * HOUR;
        let buckets = cache
            .buckets(&db, START + 1, START + 20 * MINUTE, now_unix)
            .await
            .unwrap();
        let starts: Vec<i64> = buckets.iter().map(|b| b.start_unix).collect();
        assert_eq!(starts, [START, START + 15 * MINUTE]);
        let samples: Vec<u64> = buckets.iter().map(|b| b.bucket.get(BUS).samples).collect();
        assert_eq!(samples, [15, 5]);

        let much_later = now_unix + 365 * 24 * HOUR;
        cache
            .buckets(&db, much_later, much_later + HOUR, much_later)
            .await
            .unwrap();
        assert!(cache.finished.lock().contains_key(&START));
    }

    #[tokio::test]
    async fn forgets_minutes_older_than_the_retention() {
        let db = Db::open(Path::new(IN_MEMORY), OpenMode::CreateIfMissing)
            .await
            .unwrap();
        let cache = StatsCache::new(
            BUCKET,
            Retention::Last(Duration::from_secs(
                u64::try_from(final_delay_secs() + 2 * MINUTE).unwrap(),
            )),
        );
        let now_unix = START + 8 * MINUTE + final_delay_secs();
        let buckets = cache
            .buckets(&db, START, START + 8 * MINUTE, now_unix)
            .await
            .unwrap();
        assert_eq!(buckets.len(), 8);
        let kept: Vec<i64> = cache.finished.lock().keys().copied().collect();
        assert_eq!(
            kept,
            [START + 5 * MINUTE, START + 6 * MINUTE, START + 7 * MINUTE]
        );
    }
}

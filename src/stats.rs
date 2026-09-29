use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, bail};
use parking_lot::Mutex;

use crate::db::{self, Db, TypedPathPoint};
use crate::geo::{self, MicroDegrees};
use crate::mpk::VehicleType;
use crate::speed::{self, KMH_PER_METER_PER_SECOND};

pub const BUCKET: Duration = Duration::from_mins(1);
const COMPUTE_CHUNK: Duration = Duration::from_hours(6);
const SAVE_DELAY: Duration = Duration::from_mins(1);
const MAX_STEP_GAP: Duration = Duration::from_mins(1);
const GPS_REFRESH: Duration = Duration::from_secs(15);
const MIN_MOVING_STEP_M: f64 = 5.0;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Kind(Kind),
    Line(i64),
}

impl Group {
    #[must_use]
    pub fn includes(self, vehicle_type: VehicleType, line_id: i64) -> bool {
        match self {
            Group::Kind(Kind::All) => true,
            Group::Kind(Kind::Vehicle(selected)) => selected == vehicle_type,
            Group::Line(selected) => selected == line_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    Kinds,
    Lines,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Totals {
    samples: u64,
    vehicle_samples: u64,
    peak_vehicles: u64,
    distance_m: f64,
    travel_secs: i64,
    moving_distance_m: f64,
    moving_secs: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Piece {
    distance_m: f64,
    secs: i64,
    moving_distance_m: f64,
    moving_secs: i64,
}

impl Totals {
    pub fn add(&mut self, other: &Totals) {
        self.samples += other.samples;
        self.vehicle_samples += other.vehicle_samples;
        self.peak_vehicles = self.peak_vehicles.max(other.peak_vehicles);
        self.distance_m += other.distance_m;
        self.travel_secs += other.travel_secs;
        self.moving_distance_m += other.moving_distance_m;
        self.moving_secs += other.moving_secs;
    }

    fn add_sample(&mut self, vehicles: u64) {
        self.samples += 1;
        self.vehicle_samples += vehicles;
        self.peak_vehicles = self.peak_vehicles.max(vehicles);
    }

    fn add_step(&mut self, piece: &Piece) {
        self.distance_m += piece.distance_m;
        self.travel_secs += piece.secs;
        self.moving_distance_m += piece.moving_distance_m;
        self.moving_secs += piece.moving_secs;
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
    pub fn average_speed_kmh(&self) -> Option<f64> {
        speed::kmh(self.distance_m, self.travel_secs)
    }

    #[must_use]
    pub fn moving_speed_kmh(&self) -> Option<f64> {
        speed::kmh(self.moving_distance_m, self.moving_secs)
    }

    #[must_use]
    pub fn distance_km(&self) -> Option<f64> {
        (self.samples > 0).then(|| self.distance_m / METERS_PER_KILOMETER)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LineTotals {
    line_id: i64,
    vehicle_samples: u32,
    peak_vehicles: u32,
    travel_secs: u32,
    distance_m: f32,
    moving_secs: u32,
    moving_distance_m: f32,
}

impl LineTotals {
    fn new(line_id: i64) -> Self {
        Self {
            line_id,
            vehicle_samples: 0,
            peak_vehicles: 0,
            travel_secs: 0,
            distance_m: 0.0,
            moving_secs: 0,
            moving_distance_m: 0.0,
        }
    }

    fn add(&mut self, other: &LineTotals) {
        self.vehicle_samples = self.vehicle_samples.saturating_add(other.vehicle_samples);
        self.peak_vehicles = self.peak_vehicles.max(other.peak_vehicles);
        self.travel_secs = self.travel_secs.saturating_add(other.travel_secs);
        self.distance_m += other.distance_m;
        self.moving_secs = self.moving_secs.saturating_add(other.moving_secs);
        self.moving_distance_m += other.moving_distance_m;
    }

    fn add_sample(&mut self, vehicles: u32) {
        self.vehicle_samples = self.vehicle_samples.saturating_add(vehicles);
        self.peak_vehicles = self.peak_vehicles.max(vehicles);
    }

    #[allow(clippy::cast_possible_truncation)]
    fn add_step(&mut self, piece: &Piece) {
        self.distance_m += piece.distance_m as f32;
        self.travel_secs = self.travel_secs.saturating_add(saturated_secs(piece.secs));
        self.moving_distance_m += piece.moving_distance_m as f32;
        self.moving_secs = self
            .moving_secs
            .saturating_add(saturated_secs(piece.moving_secs));
    }

    fn totals(&self, samples: u64) -> Totals {
        Totals {
            samples,
            vehicle_samples: u64::from(self.vehicle_samples),
            peak_vehicles: u64::from(self.peak_vehicles),
            distance_m: f64::from(self.distance_m),
            travel_secs: i64::from(self.travel_secs),
            moving_distance_m: f64::from(self.moving_distance_m),
            moving_secs: i64::from(self.moving_secs),
        }
    }
}

fn saturated_secs(secs: i64) -> u32 {
    u32::try_from(secs).unwrap_or(u32::MAX)
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Bucket {
    kinds: [Totals; KIND_COUNT],
    lines: Vec<LineTotals>,
}

impl Bucket {
    fn get(&self, kind: Kind) -> &Totals {
        &self.kinds[kind.index()]
    }

    fn get_mut(&mut self, kind: Kind) -> &mut Totals {
        &mut self.kinds[kind.index()]
    }

    fn line_mut(&mut self, line_id: i64) -> &mut LineTotals {
        let index = match self
            .lines
            .binary_search_by_key(&line_id, |line| line.line_id)
        {
            Ok(index) => index,
            Err(index) => {
                self.lines.insert(index, LineTotals::new(line_id));
                index
            }
        };
        &mut self.lines[index]
    }

    fn totals(&self, group: Group) -> Totals {
        match group {
            Group::Kind(kind) => *self.get(kind),
            Group::Line(line_id) => {
                let samples = self.get(Kind::All).samples;
                self.lines
                    .binary_search_by_key(&line_id, |line| line.line_id)
                    .map_or(
                        Totals {
                            samples,
                            ..Totals::default()
                        },
                        |index| self.lines[index].totals(samples),
                    )
            }
        }
    }

    fn add(&mut self, other: &Bucket) {
        for (totals, other) in self.kinds.iter_mut().zip(&other.kinds) {
            totals.add(other);
        }
        for other in &other.lines {
            self.line_mut(other.line_id).add(other);
        }
    }

    fn kept(mut self, detail: Detail) -> Self {
        match detail {
            Detail::Kinds => self.lines = Vec::new(),
            Detail::Lines => self.lines.shrink_to_fit(),
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
struct TimedBucket {
    start_unix: i64,
    bucket: Bucket,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimedTotals {
    pub start_unix: i64,
    pub totals: Totals,
}

#[must_use]
pub fn sum<'a>(series: impl IntoIterator<Item = &'a TimedTotals>) -> Totals {
    let mut total = Totals::default();
    for timed in series {
        total.add(&timed.totals);
    }
    total
}

#[must_use]
pub fn regroup_by(series: &[TimedTotals], group_start: impl Fn(i64) -> i64) -> Vec<TimedTotals> {
    let mut grouped: Vec<TimedTotals> = Vec::new();
    for timed in series {
        let start_unix = group_start(timed.start_unix);
        match grouped.last_mut() {
            Some(last) if last.start_unix == start_unix => last.totals.add(&timed.totals),
            _ => grouped.push(TimedTotals {
                start_unix,
                totals: timed.totals,
            }),
        }
    }
    grouped
}

/// Empties every hour in which no vehicle was seen, so averages count only the hours
/// the vehicles ran.
#[must_use]
pub fn skip_idle_hours(
    series: &[TimedTotals],
    hour_start: impl Fn(i64) -> i64,
) -> Vec<TimedTotals> {
    series
        .chunk_by(|a, b| hour_start(a.start_unix) == hour_start(b.start_unix))
        .flat_map(|hour| {
            let ran = sum(hour).has_vehicles();
            hour.iter().map(move |timed| TimedTotals {
                start_unix: timed.start_unix,
                totals: if ran { timed.totals } else { Totals::default() },
            })
        })
        .collect()
}

fn regroup(buckets: Vec<TimedBucket>, step: Duration) -> Vec<TimedBucket> {
    let step_secs = db::duration_secs(step);
    let mut grouped: Vec<TimedBucket> = Vec::new();
    for timed in buckets {
        let start_unix = align_down(timed.start_unix, step_secs);
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
    detail: Detail,
    finished: Mutex<BTreeMap<i64, Bucket>>,
    computing: tokio::sync::Mutex<()>,
}

impl StatsCache {
    /// `resolution` must be a whole number of minutes that divides six hours.
    #[must_use]
    pub fn new(resolution: Duration, retention: Retention, detail: Detail) -> Self {
        Self {
            resolution,
            retention,
            detail,
            finished: Mutex::default(),
            computing: tokio::sync::Mutex::default(),
        }
    }

    /// Returns one series per group, in the order of `groups`.
    pub async fn totals(
        &self,
        db: &Db,
        from_unix: i64,
        to_unix: i64,
        now_unix: i64,
        groups: &[Group],
    ) -> Result<Vec<Vec<TimedTotals>>> {
        if self.detail == Detail::Kinds && groups.iter().any(|g| matches!(g, Group::Line(_))) {
            bail!("this statistics cache keeps no per-line totals");
        }
        let resolution_secs = db::duration_secs(self.resolution);
        let from_unix = align_down(from_unix, resolution_secs);
        let to_unix = align_up(to_unix, resolution_secs);
        let final_before_unix = now_unix - resolution_secs - db::duration_secs(final_delay());
        let _computing = self.computing.lock().await;
        let mut unfinished = BTreeMap::new();
        if let Some((first_unix, end_unix)) = self.missing_range(from_unix, to_unix) {
            for (chunk_start_unix, chunk_end_unix) in chunks(first_unix, end_unix) {
                let points = chunk_points(db, chunk_start_unix, chunk_end_unix).await?;
                let minutes: Vec<TimedBucket> = bucket_starts(chunk_start_unix, chunk_end_unix)
                    .zip(compute(&points, chunk_start_unix, chunk_end_unix))
                    .map(|(start_unix, bucket)| TimedBucket { start_unix, bucket })
                    .collect();
                let mut finished = self.finished.lock();
                for timed in regroup(minutes, self.resolution) {
                    let bucket = timed.bucket.kept(self.detail);
                    if timed.start_unix <= final_before_unix {
                        finished.insert(timed.start_unix, bucket);
                    } else {
                        unfinished.insert(timed.start_unix, bucket);
                    }
                }
            }
        }
        let mut finished = self.finished.lock();
        let mut series = vec![Vec::new(); groups.len()];
        for start_unix in starts(from_unix, to_unix, resolution_secs) {
            let bucket = unfinished
                .get(&start_unix)
                .or_else(|| finished.get(&start_unix));
            for (group, series) in groups.iter().zip(&mut series) {
                series.push(TimedTotals {
                    start_unix,
                    totals: bucket
                        .map(|bucket| bucket.totals(*group))
                        .unwrap_or_default(),
                });
            }
        }
        if let Retention::Last(retention) = self.retention {
            let oldest_kept_unix = now_unix - db::duration_secs(retention) - resolution_secs;
            *finished = finished.split_off(&oldest_kept_unix);
        }
        Ok(series)
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

/// Splits a range into pieces that are each read with one query, so the collector
/// waits at most for one of them.
pub fn chunks(from_unix: i64, to_unix: i64) -> impl Iterator<Item = (i64, i64)> {
    starts(from_unix, to_unix, db::duration_secs(COMPUTE_CHUNK)).map(move |start| {
        (
            start,
            (start + db::duration_secs(COMPUTE_CHUNK)).min(to_unix),
        )
    })
}

/// The rows needed for all steps that cover any time in the range.
pub async fn chunk_points(db: &Db, from_unix: i64, to_unix: i64) -> Result<Vec<TypedPathPoint>> {
    let margin_secs = db::duration_secs(max_step_span());
    db.path_points_started_between(from_unix - margin_secs, to_unix + margin_secs)
        .await
}

fn max_step_span() -> Duration {
    db::MAX_ROW_SPAN + MAX_STEP_GAP
}

#[must_use]
pub fn final_delay() -> Duration {
    max_step_span() + SAVE_DELAY
}

fn compute(points: &[TypedPathPoint], from_unix: i64, to_unix: i64) -> Vec<Bucket> {
    let mut buckets: Vec<Bucket> = bucket_starts(from_unix, to_unix)
        .map(|_| Bucket::default())
        .collect();
    let times = fetch_times(points, from_unix, to_unix);
    add_vehicle_samples(points, &times, from_unix, &mut buckets);
    add_line_samples(points, &times, from_unix, &mut buckets);
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

fn visible_at(times: &[i64], p: &TypedPathPoint) -> Option<(usize, usize)> {
    let start = times.partition_point(|unix| *unix < p.point.first_seen_unix);
    let end = times.partition_point(|unix| *unix <= p.point.last_seen_unix);
    (start < end).then_some((start, end))
}

fn add_vehicle_samples(
    points: &[TypedPathPoint],
    times: &[i64],
    from_unix: i64,
    buckets: &mut [Bucket],
) {
    let mut changes = vec![[0i64; KIND_COUNT]; times.len() + 1];
    for p in points {
        let Some((start, end)) = visible_at(times, p) else {
            continue;
        };
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

fn add_line_samples(
    points: &[TypedPathPoint],
    times: &[i64],
    from_unix: i64,
    buckets: &mut [Bucket],
) {
    for line_points in points.chunk_by(|a, b| a.point.line_id == b.point.line_id) {
        let Some(first) = line_points.first() else {
            continue;
        };
        let mut changes = vec![0i64; times.len() + 1];
        for p in line_points {
            let Some((start, end)) = visible_at(times, p) else {
                continue;
            };
            changes[start] += 1;
            changes[end] -= 1;
        }
        let mut visible = 0i64;
        for (unix, change) in times.iter().zip(&changes) {
            visible += change;
            let Ok(count) = u32::try_from(visible) else {
                continue;
            };
            if count == 0 {
                continue;
            }
            buckets[bucket_index(*unix, from_unix)]
                .line_mut(first.point.line_id)
                .add_sample(count);
        }
    }
}

fn add_steps(points: &[TypedPathPoint], from_unix: i64, to_unix: i64, buckets: &mut [Bucket]) {
    for travel in travels(points) {
        add_travel(buckets, from_unix, to_unix, &travel);
    }
}

/// `points` must be sorted by line, course and start, as the database returns them.
#[must_use]
pub fn travels(points: &[TypedPathPoint]) -> Vec<Travel> {
    let fetches = fetch_times(points, i64::MIN, i64::MAX);
    let mut travels = Vec::with_capacity(points.len());
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
        travels.push(travel);
    }
    travels
}

fn collector_paused_after(fetches: &[i64], last_seen_unix: i64) -> bool {
    let after = fetches.partition_point(|unix| *unix <= last_seen_unix);
    fetches
        .get(after)
        .is_none_or(|unix| *unix - last_seen_unix > db::duration_secs(MAX_STEP_GAP))
}

fn standing(current: &TypedPathPoint) -> Travel {
    let position = (current.point.latitude, current.point.longitude);
    Travel {
        vehicle_type: current.vehicle_type,
        line_id: current.point.line_id,
        from: position,
        to: position,
        start_unix: current.point.first_seen_unix,
        moving_start_unix: current.point.last_seen_unix,
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
    let moving_start_unix = if distance_m >= MIN_MOVING_STEP_M {
        a.first_seen_unix
            .max(a.last_seen_unix - db::duration_secs(GPS_REFRESH))
    } else {
        b.first_seen_unix
    };
    Some(Travel {
        vehicle_type: current.vehicle_type,
        line_id: a.line_id,
        from: (a.latitude, a.longitude),
        to: (b.latitude, b.longitude),
        start_unix: a.first_seen_unix,
        moving_start_unix,
        end_unix: b.first_seen_unix,
        distance_m,
    })
}

/// One step of a vehicle: it stood at `from` until `moving_start_unix`, then moved to
/// `to`, where it arrived at `end_unix`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Travel {
    pub vehicle_type: VehicleType,
    pub line_id: i64,
    pub from: (MicroDegrees, MicroDegrees),
    pub to: (MicroDegrees, MicroDegrees),
    pub start_unix: i64,
    pub moving_start_unix: i64,
    pub end_unix: i64,
    pub distance_m: f64,
}

#[allow(clippy::cast_precision_loss)]
fn add_travel(buckets: &mut [Bucket], from_unix: i64, to_unix: i64, travel: &Travel) {
    let span_secs = travel.end_unix - travel.start_unix;
    if span_secs <= 0 {
        return;
    }
    let moving_span_secs = travel.end_unix - travel.moving_start_unix;
    let bucket_secs = db::duration_secs(BUCKET);
    let end_unix = travel.end_unix.min(to_unix);
    let mut piece_start_unix = travel.start_unix.max(from_unix);
    while piece_start_unix < end_unix {
        let piece_end_unix =
            (align_down(piece_start_unix, bucket_secs) + bucket_secs).min(end_unix);
        let secs = piece_end_unix - piece_start_unix;
        let moving_secs = (piece_end_unix - piece_start_unix.max(travel.moving_start_unix)).max(0);
        let moving_distance_m = if moving_secs > 0 {
            travel.distance_m * moving_secs as f64 / moving_span_secs as f64
        } else {
            0.0
        };
        let piece = Piece {
            distance_m: travel.distance_m * secs as f64 / span_secs as f64,
            secs,
            moving_distance_m,
            moving_secs,
        };
        let bucket = &mut buckets[bucket_index(piece_start_unix, from_unix)];
        bucket
            .get_mut(Kind::Vehicle(travel.vehicle_type))
            .add_step(&piece);
        bucket.get_mut(Kind::All).add_step(&piece);
        bucket.line_mut(travel.line_id).add_step(&piece);
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
    const SECOND_LINE: &str = "b";
    const START: i64 = 1_800_000_000;
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const HALF_HOUR: i64 = 30 * MINUTE;
    const FETCH_SECS: i64 = 10;
    const LATITUDE_E6: i64 = 51_100_000;
    const LONGITUDE_E6: i64 = 17_000_000;
    const HUNDRED_METERS_NORTH_E6: i64 = 898;
    const TEN_KILOMETERS_NORTH_E6: i64 = 100 * HUNDRED_METERS_NORTH_E6;
    const HUNDRED_METERS_PER_FETCH_KMH: f64 = 36.0;
    const SPEED_TOLERANCE_KMH: f64 = 0.1;
    const FLOAT_TOLERANCE: f64 = 1e-9;
    const DISTANCE_TOLERANCE_M: f64 = 1e-3;
    const LINE_ID_OFFSET: i64 = 100;

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

    fn total(buckets: &[Bucket]) -> Bucket {
        let mut total = Bucket::default();
        for bucket in buckets {
            total.add(bucket);
        }
        total
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
        let [bucket] = &compute(&sorted(points), START, START + MINUTE)[..] else {
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

    fn on_line(line_id: i64, mut p: TypedPathPoint) -> TypedPathPoint {
        p.point.line_id = line_id;
        p
    }

    #[test]
    fn keeps_totals_per_line() {
        let first_line = VehicleType::Bus.id();
        let second_line = first_line + LINE_ID_OFFSET;
        let mut points = moving(
            VehicleType::Bus,
            1,
            START,
            START + 50,
            HUNDRED_METERS_NORTH_E6,
        );
        points.extend(
            moving(VehicleType::Bus, 2, START + 30, START + 50, 0)
                .into_iter()
                .map(|p| on_line(second_line, p)),
        );
        let [bucket] = &compute(&sorted(points), START, START + MINUTE)[..] else {
            panic!("expected one bucket");
        };
        let first = bucket.totals(Group::Line(first_line));
        let second = bucket.totals(Group::Line(second_line));

        assert_close(first.average_vehicles(), 1.0, FLOAT_TOLERANCE);
        assert_close(second.average_vehicles(), 0.5, FLOAT_TOLERANCE);
        assert_eq!(second.peak_vehicles(), Some(1));
        assert_close(
            first.average_speed_kmh(),
            HUNDRED_METERS_PER_FETCH_KMH,
            SPEED_TOLERANCE_KMH,
        );
        assert_eq!(second.average_speed_kmh(), Some(0.0));

        let bus = bucket.get(BUS);
        assert_eq!(
            first.vehicle_samples + second.vehicle_samples,
            bus.vehicle_samples
        );
        assert_eq!(first.travel_secs + second.travel_secs, bus.travel_secs);
        assert_close(
            Some(first.distance_m + second.distance_m),
            bus.distance_m,
            DISTANCE_TOLERANCE_M,
        );
        assert_eq!(bus.peak_vehicles(), Some(2));

        let unknown = bucket.totals(Group::Line(second_line + LINE_ID_OFFSET));
        assert_eq!(unknown.average_vehicles(), Some(0.0));
        assert!(!unknown.has_vehicles());
    }

    #[test]
    fn counts_rows_that_started_before_the_window() {
        let points = [point(VehicleType::Tram, 1, 0, START - 100, START + 20)];
        let [bucket] = &compute(&points, START, START + MINUTE)[..] else {
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
        let [bucket] = &compute(&points, START, START + MINUTE)[..] else {
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
        let total = total(&compute(&sorted(points), START, START + 11 * MINUTE));

        assert_eq!(total.get(TRAM).average_speed_kmh(), Some(0.0));
        assert_close(
            total.get(Kind::All).average_speed_kmh(),
            HUNDRED_METERS_PER_FETCH_KMH / 2.0,
            SPEED_TOLERANCE_KMH,
        );
    }

    #[test]
    fn moving_speed_leaves_out_standing_time() {
        let gps_refresh_secs = db::duration_secs(GPS_REFRESH);
        let points = [
            point(VehicleType::Bus, 1, 0, START, START + 40),
            point(
                VehicleType::Bus,
                1,
                HUNDRED_METERS_NORTH_E6,
                START + 50,
                START + 50,
            ),
        ];
        let [bucket] = &compute(&points, START, START + MINUTE)[..] else {
            panic!("expected one bucket");
        };
        let bus = bucket.get(BUS);
        assert_eq!(bus.travel_secs, 50);
        assert_eq!(bus.moving_secs, gps_refresh_secs + FETCH_SECS);
        assert_close(
            bus.moving_speed_kmh(),
            HUNDRED_METERS_PER_FETCH_KMH * 10.0 / 25.0,
            SPEED_TOLERANCE_KMH,
        );
        assert_close(
            bus.average_speed_kmh(),
            HUNDRED_METERS_PER_FETCH_KMH * 10.0 / 50.0,
            SPEED_TOLERANCE_KMH,
        );
    }

    #[test]
    fn moving_speed_counts_a_stale_position_as_moving() {
        let points = moving(
            VehicleType::Bus,
            1,
            START,
            START + 50,
            HUNDRED_METERS_NORTH_E6,
        );
        let [bucket] = &compute(&points, START, START + MINUTE)[..] else {
            panic!("expected one bucket");
        };
        let bus = bucket.get(BUS);
        assert_eq!(bus.moving_secs, bus.travel_secs);
        assert_eq!(bus.moving_speed_kmh(), bus.average_speed_kmh());
    }

    #[test]
    fn moving_speed_is_missing_for_standing_vehicles() {
        let span_secs = db::duration_secs(db::MAX_ROW_SPAN);
        let points = [
            point(VehicleType::Tram, 1, 0, START, START + span_secs),
            point(
                VehicleType::Tram,
                1,
                0,
                START + span_secs + FETCH_SECS,
                START + span_secs + FETCH_SECS,
            ),
        ];
        let total = total(&compute(&points, START, START + 11 * MINUTE));
        assert_eq!(total.get(TRAM).average_speed_kmh(), Some(0.0));
        assert_eq!(total.get(TRAM).moving_speed_kmh(), None);
    }

    #[test]
    fn moving_speed_treats_gps_drift_as_standing() {
        const THREE_METERS_NORTH_E6: i64 = 27;
        let points = [
            point(VehicleType::Bus, 1, 0, START, START + 20),
            point(
                VehicleType::Bus,
                1,
                THREE_METERS_NORTH_E6,
                START + 30,
                START + 30,
            ),
        ];
        let [bucket] = &compute(&points, START, START + MINUTE)[..] else {
            panic!("expected one bucket");
        };
        let bus = bucket.get(BUS);
        assert_eq!(bus.moving_speed_kmh(), None);
        assert!(bus.average_speed_kmh().unwrap() > 0.0);
    }

    #[test]
    fn puts_moving_time_in_the_minutes_it_happened() {
        let points = [
            point(VehicleType::Bus, 1, 0, START + 10, START + 70),
            point(
                VehicleType::Bus,
                1,
                HUNDRED_METERS_NORTH_E6,
                START + 75,
                START + 75,
            ),
        ];
        let buckets = compute(&points, START, START + 2 * MINUTE);
        let moving: Vec<i64> = buckets.iter().map(|b| b.get(BUS).moving_secs).collect();
        assert_eq!(moving, [5, 15]);
        let moving_km: Vec<f64> = buckets
            .iter()
            .map(|b| b.get(BUS).moving_distance_m / METERS_PER_KILOMETER)
            .collect();
        assert_close(Some(moving_km[0]), 0.025, 0.001);
        assert_close(Some(moving_km[1]), 0.075, 0.001);
        assert_close(
            buckets[0]
                .totals(Group::Line(VehicleType::Bus.id()))
                .moving_speed_kmh(),
            HUNDRED_METERS_PER_FETCH_KMH * 10.0 / 20.0,
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
        let total = total(&compute(&points, START, START + 2 * MINUTE));
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
        let [bucket] = &compute(&points, START, START + MINUTE)[..] else {
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
        db::duration_secs(final_delay())
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
        let [bucket] = &compute(&points, START, START + MINUTE)[..] else {
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
        let [bucket, _, _] = &compute(&points, START, START + 3 * MINUTE)[..] else {
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
        let grouped = regroup(buckets.clone(), Duration::from_mins(2));
        let starts: Vec<i64> = grouped.iter().map(|b| b.start_unix).collect();
        assert_eq!(starts, [START, START + 2 * MINUTE]);
        let samples: Vec<u64> = grouped.iter().map(|b| b.bucket.get(BUS).samples).collect();
        assert_eq!(samples, [6, 6]);
        let unwrap = |timed: Vec<TimedBucket>| -> Vec<Bucket> {
            timed.into_iter().map(|timed| timed.bucket).collect()
        };
        assert_eq!(total(&unwrap(grouped)), total(&unwrap(buckets)));
    }

    #[test]
    fn empties_hours_without_vehicles() {
        let with_vehicles = |vehicles| {
            let mut totals = Totals::default();
            totals.add_sample(vehicles);
            totals.add_step(&Piece {
                distance_m: 0.0,
                secs: FETCH_SECS,
                moving_distance_m: 0.0,
                moving_secs: 0,
            });
            totals
        };
        let series: Vec<TimedTotals> = [0, 0, 2, 0, 0, 0]
            .into_iter()
            .enumerate()
            .map(|(index, vehicles)| TimedTotals {
                start_unix: START + i64::try_from(index).unwrap() * HALF_HOUR,
                totals: with_vehicles(vehicles),
            })
            .collect();
        let hour_start = |unix| align_down(unix - START, HOUR) + START;
        let counted = skip_idle_hours(&series, hour_start);

        let starts: Vec<i64> = counted.iter().map(|timed| timed.start_unix).collect();
        let original: Vec<i64> = series.iter().map(|timed| timed.start_unix).collect();
        assert_eq!(starts, original);
        let samples: Vec<u64> = counted.iter().map(|timed| timed.totals.samples).collect();
        assert_eq!(samples, [0, 0, 1, 1, 0, 0]);
        assert_eq!(sum(&counted).average_vehicles(), Some(1.0));
        assert_eq!(sum(&series).average_vehicles(), Some(2.0 / 6.0));
        assert_eq!(skip_idle_hours(&counted, hour_start), counted);
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

    fn vehicles_per_minute(series: &[TimedTotals]) -> Vec<Option<f64>> {
        series
            .iter()
            .map(|timed| timed.totals.average_vehicles())
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
        let cache = StatsCache::new(
            BUCKET,
            Retention::Last(Duration::from_hours(1)),
            Detail::Kinds,
        );
        let now_unix = START + 2 * MINUTE + final_delay_secs();
        let range = (START, START + 3 * MINUTE);
        let groups = [Group::Kind(TRAM), Group::Kind(BUS)];
        let before = cache
            .totals(&db, range.0, range.1, now_unix, &groups)
            .await
            .unwrap();
        assert_eq!(
            vehicles_per_minute(&before[0]),
            [Some(0.0), Some(0.0), Some(0.0)]
        );
        assert_eq!(
            vehicles_per_minute(&before[1]),
            [Some(1.0), Some(1.0), Some(1.0)]
        );

        let tram = position(VehicleType::Tram, 2);
        for minute in 0..3 {
            db.save_positions(START + minute * MINUTE + FETCH_SECS, &[&tram], &[])
                .await
                .unwrap();
        }
        let after = cache
            .totals(&db, range.0, range.1, now_unix, &groups)
            .await
            .unwrap();
        assert_eq!(after[0][..2], before[0][..2]);
        assert_eq!(vehicles_per_minute(&after[0])[2], Some(0.5));
    }

    #[tokio::test]
    async fn serves_line_totals_only_from_caches_that_keep_them() {
        let db = Db::open(Path::new(IN_MEMORY), OpenMode::CreateIfMissing)
            .await
            .unwrap();
        let first = position(VehicleType::Bus, 1);
        let second = Position {
            line: SECOND_LINE.to_owned(),
            ..position(VehicleType::Bus, 2)
        };
        for minute in 0..20 {
            db.save_positions(START + minute * MINUTE, &[&first, &second], &[])
                .await
                .unwrap();
        }
        let line_id = |name: &'static str| {
            let db = db.clone();
            async move { db.line(VehicleType::Bus, name).await.unwrap().unwrap().id }
        };
        let groups = [
            Group::Kind(BUS),
            Group::Line(line_id(VehicleType::Bus.as_str()).await),
            Group::Line(line_id(SECOND_LINE).await),
        ];
        let now_unix = START + 24 * HOUR;
        let window = (START, START + 20 * MINUTE);

        let kinds_only = StatsCache::new(BUCKET, Retention::Forever, Detail::Kinds);
        assert!(
            kinds_only
                .totals(&db, window.0, window.1, now_unix, &groups)
                .await
                .is_err()
        );

        let quarter = Duration::from_mins(15);
        let with_lines = StatsCache::new(quarter, Retention::Forever, Detail::Lines);
        let series = with_lines
            .totals(&db, window.0, window.1, now_unix, &groups)
            .await
            .unwrap();
        let averages: Vec<Option<f64>> = series
            .iter()
            .map(|series| sum(series).average_vehicles())
            .collect();
        assert_eq!(averages, [Some(2.0), Some(1.0), Some(1.0)]);
        let cached = with_lines
            .totals(&db, window.0, window.1, now_unix, &groups)
            .await
            .unwrap();
        assert_eq!(cached, series);
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

        let cache = StatsCache::new(
            BUCKET,
            Retention::Last(Duration::from_hours(24)),
            Detail::Kinds,
        );
        let to_unix = boundary_unix + 10 * MINUTE;
        let [tram_series] = &cache
            .totals(&db, START, to_unix, to_unix + HOUR, &[Group::Kind(TRAM)])
            .await
            .unwrap()[..]
        else {
            panic!("expected one series");
        };
        assert_eq!(sum(tram_series).travel_secs, 10 * MINUTE + FETCH_SECS);
        let around_boundary: Vec<i64> = tram_series
            .iter()
            .filter(|b| (boundary_unix - MINUTE..=boundary_unix).contains(&b.start_unix))
            .map(|b| b.totals.travel_secs)
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
        let cache = StatsCache::new(quarter, Retention::Forever, Detail::Lines);
        let now_unix = START + 24 * HOUR;
        let [bus_series] = &cache
            .totals(
                &db,
                START + 1,
                START + 20 * MINUTE,
                now_unix,
                &[Group::Kind(BUS)],
            )
            .await
            .unwrap()[..]
        else {
            panic!("expected one series");
        };
        let starts: Vec<i64> = bus_series.iter().map(|b| b.start_unix).collect();
        assert_eq!(starts, [START, START + 15 * MINUTE]);
        let samples: Vec<u64> = bus_series.iter().map(|b| b.totals.samples).collect();
        assert_eq!(samples, [15, 5]);

        let much_later = now_unix + 365 * 24 * HOUR;
        cache
            .totals(&db, much_later, much_later + HOUR, much_later, &[])
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
            Detail::Kinds,
        );
        let now_unix = START + 8 * MINUTE + final_delay_secs();
        let series = cache
            .totals(
                &db,
                START,
                START + 8 * MINUTE,
                now_unix,
                &[Group::Kind(Kind::All)],
            )
            .await
            .unwrap();
        assert_eq!(series[0].len(), 8);
        let kept: Vec<i64> = cache.finished.lock().keys().copied().collect();
        assert_eq!(
            kept,
            [START + 5 * MINUTE, START + 6 * MINUTE, START + 7 * MINUTE]
        );
    }
}

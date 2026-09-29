use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use h3o::{CellIndex, LatLng, Resolution};
use parking_lot::Mutex;

use crate::db::{self, Db};
use crate::geo::MicroDegrees;
use crate::mpk::VehicleType;
use crate::speed;
use crate::stats::{self, Group, Travel};

pub const RESOLUTION: Resolution = Resolution::Nine;

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct CellTotals {
    distance_m: f64,
    travel_secs: i64,
    moving_distance_m: f64,
    moving_secs: i64,
}

impl CellTotals {
    fn add(&mut self, other: &CellTotals) {
        self.distance_m += other.distance_m;
        self.travel_secs += other.travel_secs;
        self.moving_distance_m += other.moving_distance_m;
        self.moving_secs += other.moving_secs;
    }

    #[must_use]
    pub fn travel(&self) -> Duration {
        Duration::from_secs(u64::try_from(self.travel_secs).unwrap_or_default())
    }

    #[must_use]
    pub fn average_speed_kmh(&self) -> Option<f64> {
        speed::kmh(self.distance_m, self.travel_secs)
    }

    #[must_use]
    pub fn moving_speed_kmh(&self) -> Option<f64> {
        speed::kmh(self.moving_distance_m, self.moving_secs)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Entry {
    cell: CellIndex,
    line_id: i64,
    vehicle_type: VehicleType,
    distance_m: f32,
    travel_secs: u32,
    moving_distance_m: f32,
    moving_secs: u32,
}

impl Entry {
    #[allow(clippy::cast_possible_truncation)]
    fn new(cell: CellIndex, line_id: i64, vehicle_type: VehicleType, totals: &CellTotals) -> Self {
        Self {
            cell,
            line_id,
            vehicle_type,
            distance_m: totals.distance_m as f32,
            travel_secs: u32::try_from(totals.travel_secs).unwrap_or(u32::MAX),
            moving_distance_m: totals.moving_distance_m as f32,
            moving_secs: u32::try_from(totals.moving_secs).unwrap_or(u32::MAX),
        }
    }

    fn totals(&self) -> CellTotals {
        CellTotals {
            distance_m: f64::from(self.distance_m),
            travel_secs: i64::from(self.travel_secs),
            moving_distance_m: f64::from(self.moving_distance_m),
            moving_secs: i64::from(self.moving_secs),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CellSpeeds {
    pub cell: CellIndex,
    /// One per group, in the order of the groups asked for.
    pub totals: Vec<CellTotals>,
}

#[derive(Default)]
pub struct CellCache {
    finished: Mutex<BTreeMap<i64, Arc<[Entry]>>>,
    computing: tokio::sync::Mutex<()>,
}

impl CellCache {
    /// `days` are `(start, end)` pairs. A day is kept once its data is final, under its
    /// start, so the same start must always come with the same end.
    pub async fn totals(
        &self,
        db: &Db,
        days: &[(i64, i64)],
        now_unix: i64,
        groups: &[Group],
    ) -> Result<Vec<CellSpeeds>> {
        let final_before_unix = now_unix - db::duration_secs(stats::final_delay());
        let _computing = self.computing.lock().await;
        let mut cells: BTreeMap<CellIndex, Vec<CellTotals>> = BTreeMap::new();
        for (from_unix, to_unix) in days.iter().copied() {
            let entries = self.day(db, from_unix, to_unix, final_before_unix).await?;
            for entry in entries.iter() {
                let included: Vec<bool> = groups
                    .iter()
                    .map(|group| group.includes(entry.vehicle_type, entry.line_id))
                    .collect();
                if !included.contains(&true) {
                    continue;
                }
                let totals = cells
                    .entry(entry.cell)
                    .or_insert_with(|| vec![CellTotals::default(); groups.len()]);
                for (totals, included) in totals.iter_mut().zip(included) {
                    if included {
                        totals.add(&entry.totals());
                    }
                }
            }
        }
        Ok(cells
            .into_iter()
            .map(|(cell, totals)| CellSpeeds { cell, totals })
            .collect())
    }
}

impl CellCache {
    async fn day(
        &self,
        db: &Db,
        from_unix: i64,
        to_unix: i64,
        final_before_unix: i64,
    ) -> Result<Arc<[Entry]>> {
        if let Some(entries) = self.finished.lock().get(&from_unix).cloned() {
            return Ok(entries);
        }
        let entries = compute(db, from_unix, to_unix).await?;
        if to_unix <= final_before_unix {
            self.finished.lock().insert(from_unix, entries.clone());
        }
        Ok(entries)
    }
}

type Sums = HashMap<(CellIndex, i64), (VehicleType, CellTotals)>;

async fn compute(db: &Db, from_unix: i64, to_unix: i64) -> Result<Arc<[Entry]>> {
    let mut sums = Sums::new();
    for (chunk_start_unix, chunk_end_unix) in stats::chunks(from_unix, to_unix) {
        let points = stats::chunk_points(db, chunk_start_unix, chunk_end_unix).await?;
        for travel in stats::travels(&points) {
            add_travel(&mut sums, &travel, chunk_start_unix, chunk_end_unix);
        }
    }
    Ok(sums
        .into_iter()
        .map(|((cell, line_id), (vehicle_type, totals))| {
            Entry::new(cell, line_id, vehicle_type, &totals)
        })
        .collect())
}

/// The standing time counts in the cell of the old position, the move in the cell
/// halfway to the new one.
#[allow(clippy::cast_precision_loss)]
fn add_travel(sums: &mut Sums, travel: &Travel, from_unix: i64, to_unix: i64) {
    let standing_span_secs = travel.moving_start_unix - travel.start_unix;
    let moving_span_secs = travel.end_unix - travel.moving_start_unix;
    let standing_secs = overlap_secs(
        (travel.start_unix, travel.moving_start_unix),
        (from_unix, to_unix),
    );
    if standing_secs > 0 {
        let distance_m = if moving_span_secs > 0 {
            0.0
        } else {
            travel.distance_m * standing_secs as f64 / standing_span_secs as f64
        };
        let totals = CellTotals {
            distance_m,
            travel_secs: standing_secs,
            ..CellTotals::default()
        };
        add(sums, travel, travel.from, &totals);
    }
    let moving_secs = overlap_secs(
        (travel.moving_start_unix, travel.end_unix),
        (from_unix, to_unix),
    );
    if moving_secs > 0 {
        let distance_m = travel.distance_m * moving_secs as f64 / moving_span_secs as f64;
        let totals = CellTotals {
            distance_m,
            travel_secs: moving_secs,
            moving_distance_m: distance_m,
            moving_secs,
        };
        add(sums, travel, midpoint(travel.from, travel.to), &totals);
    }
}

fn add(
    sums: &mut Sums,
    travel: &Travel,
    position: (MicroDegrees, MicroDegrees),
    totals: &CellTotals,
) {
    let Some(cell) = cell_of(position) else {
        return;
    };
    sums.entry((cell, travel.line_id))
        .or_insert((travel.vehicle_type, CellTotals::default()))
        .1
        .add(totals);
}

fn overlap_secs((start, end): (i64, i64), (from_unix, to_unix): (i64, i64)) -> i64 {
    (end.min(to_unix) - start.max(from_unix)).max(0)
}

fn midpoint(
    a: (MicroDegrees, MicroDegrees),
    b: (MicroDegrees, MicroDegrees),
) -> (MicroDegrees, MicroDegrees) {
    (
        MicroDegrees(i64::midpoint(a.0.0, b.0.0)),
        MicroDegrees(i64::midpoint(a.1.0, b.1.0)),
    )
}

#[must_use]
pub fn cell_of((latitude, longitude): (MicroDegrees, MicroDegrees)) -> Option<CellIndex> {
    LatLng::new(latitude.degrees(), longitude.degrees())
        .ok()
        .map(|position| position.to_cell(RESOLUTION))
}

#[must_use]
pub fn center(cell: CellIndex) -> (MicroDegrees, MicroDegrees) {
    let center = LatLng::from(cell);
    (
        MicroDegrees::from(center.lat()),
        MicroDegrees::from(center.lng()),
    )
}

#[must_use]
pub fn boundary(cell: CellIndex) -> Vec<(MicroDegrees, MicroDegrees)> {
    cell.boundary()
        .iter()
        .map(|corner| {
            (
                MicroDegrees::from(corner.lat()),
                MicroDegrees::from(corner.lng()),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::db::OpenMode;
    use crate::mpk::Position;
    use crate::stats::{Detail, Kind, Retention, StatsCache};

    const IN_MEMORY: &str = ":memory:";
    const START: i64 = 1_800_000_000;
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const FETCH_SECS: i64 = 10;
    const LATITUDE_E6: i64 = 51_100_000;
    const LONGITUDE_E6: i64 = 17_000_000;
    const HUNDRED_METERS_NORTH_E6: i64 = 898;
    const HUNDRED_METERS_PER_FETCH_KMH: f64 = 36.0;
    const SPEED_TOLERANCE_KMH: f64 = 0.1;
    const DRIVE_FETCHES: i64 = 60;
    const STOP_SECS: i64 = 5 * MINUTE;
    const TRAM_LINE: &str = "1";

    fn position(vehicle_type: VehicleType, line: &str, north_e6: i64) -> Position {
        Position {
            vehicle_type,
            line: line.to_owned(),
            course_id: vehicle_type.id(),
            latitude: MicroDegrees(LATITUDE_E6 + north_e6),
            longitude: MicroDegrees(LONGITUDE_E6),
        }
    }

    async fn open() -> Db {
        Db::open(Path::new(IN_MEMORY), OpenMode::CreateIfMissing)
            .await
            .unwrap()
    }

    async fn db_with_a_drive_and_a_stop() -> Db {
        let db = open().await;
        for fetch in 0..=DRIVE_FETCHES {
            let bus = position(
                VehicleType::Bus,
                VehicleType::Bus.as_str(),
                fetch * HUNDRED_METERS_NORTH_E6,
            );
            db.save_positions(START + fetch * FETCH_SECS, &[&bus], &[])
                .await
                .unwrap();
        }
        let tram = position(VehicleType::Tram, TRAM_LINE, 0);
        let ids = db.save_positions(START, &[&tram], &[]).await.unwrap();
        db.save_positions(START + STOP_SECS, &[], &ids)
            .await
            .unwrap();
        db.save_positions(START + STOP_SECS + FETCH_SECS, &[&tram], &[])
            .await
            .unwrap();
        db
    }

    fn day() -> [(i64, i64); 1] {
        [(START - HOUR, START + HOUR)]
    }

    fn total(cells: &[CellSpeeds], group: usize) -> CellTotals {
        let mut total = CellTotals::default();
        for cell in cells {
            total.add(&cell.totals[group]);
        }
        total
    }

    fn assert_close(actual: Option<f64>, expected: f64) {
        let actual = actual.unwrap();
        assert!(
            (actual - expected).abs() <= SPEED_TOLERANCE_KMH,
            "{actual} != {expected}"
        );
    }

    fn assert_same(actual: Option<f64>, expected: Option<f64>) {
        match expected {
            Some(expected) => assert_close(actual, expected),
            None => assert_eq!(actual, None),
        }
    }

    #[tokio::test]
    async fn spreads_a_drive_over_the_cells_it_went_through() {
        let db = db_with_a_drive_and_a_stop().await;
        let bus = Group::Kind(Kind::Vehicle(VehicleType::Bus));
        let cells = CellCache::default()
            .totals(&db, &day(), START + HOUR, &[bus])
            .await
            .unwrap();
        assert!(cells.len() > 1, "{cells:?}");
        for cell in &cells {
            assert_close(
                cell.totals[0].average_speed_kmh(),
                HUNDRED_METERS_PER_FETCH_KMH,
            );
        }
        assert_eq!(total(&cells, 0).travel_secs, DRIVE_FETCHES * FETCH_SECS);
    }

    #[tokio::test]
    async fn puts_standing_time_in_the_cell_of_the_stop() {
        let db = db_with_a_drive_and_a_stop().await;
        let tram = Group::Kind(Kind::Vehicle(VehicleType::Tram));
        let cells = CellCache::default()
            .totals(&db, &day(), START + HOUR, &[tram])
            .await
            .unwrap();
        let stop = cell_of((MicroDegrees(LATITUDE_E6), MicroDegrees(LONGITUDE_E6))).unwrap();
        let [cell] = &cells[..] else {
            panic!("expected one cell: {cells:?}");
        };
        assert_eq!(cell.cell, stop);
        assert_eq!(cell.totals[0].average_speed_kmh(), Some(0.0));
        assert_eq!(cell.totals[0].moving_speed_kmh(), None);
        assert_eq!(cell.totals[0].travel_secs, STOP_SECS + FETCH_SECS);
    }

    #[tokio::test]
    async fn adds_up_to_the_totals_of_the_charts() {
        let db = db_with_a_drive_and_a_stop().await;
        let groups = Kind::ALL.map(Group::Kind);
        let now_unix = START + 2 * HOUR;
        let cells = CellCache::default()
            .totals(&db, &day(), now_unix, &groups)
            .await
            .unwrap();
        let series = StatsCache::new(stats::BUCKET, Retention::Forever, Detail::Kinds)
            .totals(&db, day()[0].0, day()[0].1, now_unix, &groups)
            .await
            .unwrap();
        for (index, series) in series.iter().enumerate() {
            let expected = stats::sum(series);
            let actual = total(&cells, index);
            assert_same(actual.average_speed_kmh(), expected.average_speed_kmh());
            assert_same(actual.moving_speed_kmh(), expected.moving_speed_kmh());
        }
    }

    #[tokio::test]
    async fn keeps_only_the_lines_asked_for() {
        let db = db_with_a_drive_and_a_stop().await;
        let tram_line = db
            .line(VehicleType::Tram, TRAM_LINE)
            .await
            .unwrap()
            .unwrap()
            .id;
        let groups = [Group::Kind(Kind::All), Group::Line(tram_line)];
        let cells = CellCache::default()
            .totals(&db, &day(), START + HOUR, &groups)
            .await
            .unwrap();
        let with_tram: Vec<&CellSpeeds> = cells
            .iter()
            .filter(|cell| cell.totals[1].travel_secs > 0)
            .collect();
        assert_eq!(with_tram.len(), 1);
        assert_eq!(total(&cells, 1).travel_secs, STOP_SECS + FETCH_SECS);
        assert!(total(&cells, 0).travel_secs > total(&cells, 1).travel_secs);

        let only_tram = CellCache::default()
            .totals(&db, &day(), START + HOUR, &[Group::Line(tram_line)])
            .await
            .unwrap();
        assert_eq!(only_tram.len(), 1);
    }

    #[tokio::test]
    async fn keeps_only_finished_days() {
        let db = db_with_a_drive_and_a_stop().await;
        let cache = CellCache::default();
        let groups = [Group::Kind(Kind::All)];
        let unfinished_unix = day()[0].1;
        cache
            .totals(&db, &day(), unfinished_unix, &groups)
            .await
            .unwrap();
        assert!(cache.finished.lock().is_empty());

        let finished_unix = day()[0].1 + db::duration_secs(stats::final_delay());
        let before = cache
            .totals(&db, &day(), finished_unix, &groups)
            .await
            .unwrap();
        assert!(cache.finished.lock().contains_key(&day()[0].0));
        let bus = position(VehicleType::Bus, VehicleType::Bus.as_str(), 0);
        db.save_positions(START - MINUTE, &[&bus], &[])
            .await
            .unwrap();
        let after = cache
            .totals(&db, &day(), finished_unix, &groups)
            .await
            .unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn draws_cells_as_hexagons_around_their_center() {
        let center = (MicroDegrees(LATITUDE_E6), MicroDegrees(LONGITUDE_E6));
        let corners = boundary(cell_of(center).unwrap());
        assert_eq!(corners.len(), 6);
        for corner in corners {
            let distance_m = crate::geo::distance_m(center, corner);
            assert!((100.0..1000.0).contains(&distance_m), "{distance_m}");
        }
    }
}

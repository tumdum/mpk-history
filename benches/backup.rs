use std::collections::HashMap;
use std::fs;
use std::hint::black_box;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use mpk_history::db::{
    self, CourseSummary, CourseVehicle, Db, MatchSource, OpenMode, PathPoint, PositionFilter,
    StoredPosition,
};
use mpk_history::geo::MicroDegrees;
use mpk_history::inference::{self, InferenceSettings};
use mpk_history::mpk::{Position, VehicleType};
use mpk_history::{fleet, query, speed};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tempfile::TempDir;
use tokio::runtime::Runtime;

const COMPRESSED_BACKUP: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/test-data/mpk-history.backup.sqlite.xz"
);
const BACKUP_FILE: &str = "backup.sqlite";
const READ_DATABASE_FILE: &str = "read.sqlite";
const WRITE_DATABASE_FILE: &str = "write.sqlite";
const MAX_AGE: Duration = Duration::from_secs(30);
const COLLECT_INTERVAL: Duration = Duration::from_secs(10);
const BOUNDING_BOX_RANGE: Duration = Duration::from_secs(60 * 60);
const INFERENCE: InferenceSettings = InferenceSettings {
    max_gap_secs: 45 * 60,
    max_distance_m: 500.0,
};
const SLOW_SAMPLE_SIZE: usize = 10;

struct Sample {
    line: String,
    busy_unix: i64,
    plate: String,
    plate_line: String,
    plate_unix: i64,
    last_unix: i64,
}

struct Fixture {
    dir: TempDir,
    runtime: Runtime,
    db: Db,
    sample: Sample,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let backup = dir.path().join(BACKUP_FILE);
        decompress_backup(&backup);
        let read = dir.path().join(READ_DATABASE_FILE);
        fs::copy(&backup, &read).unwrap();
        let runtime = Runtime::new().unwrap();
        let (db, sample) = runtime.block_on(async {
            let db = Db::open(&read, OpenMode::MustExist).await.unwrap();
            let raw = open_raw(&read).await;
            let sample = Sample::pick(&raw).await;
            raw.close().await;
            (db, sample)
        });
        Self {
            dir,
            runtime,
            db,
            sample,
        }
    }

    fn backup(&self) -> PathBuf {
        self.dir.path().join(BACKUP_FILE)
    }

    fn line_positions(&self) -> Vec<StoredPosition> {
        self.runtime.block_on(async {
            self.db
                .positions_at(
                    PositionFilter::Line(&self.sample.line),
                    self.sample.busy_unix,
                    MAX_AGE,
                )
                .await
                .unwrap()
        })
    }

    fn speed_window(&self) -> (i64, i64) {
        let end = self.sample.busy_unix;
        (end - db::duration_secs(query::SPEED_WINDOW), end)
    }

    fn inference_window(&self) -> (i64, i64) {
        let end = self.sample.last_unix;
        (end - db::duration_secs(fleet::INFERENCE_WINDOW), end)
    }

    fn course_summaries(&self) -> Vec<CourseSummary> {
        let (from, to) = self.inference_window();
        self.runtime
            .block_on(self.db.course_summaries(from, to))
            .unwrap()
    }
}

impl Sample {
    async fn pick(raw: &SqlitePool) -> Self {
        let (line,): (String,) = sqlx::query_as(
            "SELECT l.name FROM vehicle_positions p JOIN lines l ON l.id = p.line_id \
             GROUP BY l.id ORDER BY COUNT(*) DESC, l.name LIMIT 1",
        )
        .fetch_one(raw)
        .await
        .unwrap();
        let (busy_unix,): (i64,) = sqlx::query_as(
            "SELECT first_seen_unix FROM vehicle_positions \
             GROUP BY first_seen_unix ORDER BY COUNT(*) DESC, first_seen_unix LIMIT 1",
        )
        .fetch_one(raw)
        .await
        .unwrap();
        let (plate, plate_line, plate_unix): (String, String, i64) = sqlx::query_as(
            "SELECT cv.plate, l.name, cv.observed_unix \
             FROM course_vehicles cv JOIN lines l ON l.id = cv.line_id \
             WHERE cv.plate IS NOT NULL AND cv.source = ?1 \
             ORDER BY cv.observed_unix, cv.course_id \
             LIMIT 1 OFFSET (SELECT COUNT(*) / 2 FROM course_vehicles \
                             WHERE plate IS NOT NULL AND source = ?1)",
        )
        .bind(MatchSource::Matched.id())
        .fetch_one(raw)
        .await
        .unwrap();
        let (last_unix,): (i64,) =
            sqlx::query_as("SELECT MAX(first_seen_unix + seen_for_secs) FROM vehicle_positions")
                .fetch_one(raw)
                .await
                .unwrap();
        Self {
            line,
            busy_unix,
            plate,
            plate_line,
            plate_unix,
            last_unix,
        }
    }
}

fn decompress_backup(target: &Path) {
    let compressed = fs::File::open(COMPRESSED_BACKUP)
        .unwrap_or_else(|e| panic!("cannot open {COMPRESSED_BACKUP}: {e}"));
    let mut decoder = liblzma::read::XzDecoder::new(BufReader::new(compressed));
    let mut file = fs::File::create(target).unwrap();
    io::copy(&mut decoder, &mut file).unwrap();
}

async fn open_raw(path: &Path) -> SqlitePool {
    let options = SqliteConnectOptions::new().filename(path).read_only(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap()
}

fn courses_of(positions: &[StoredPosition]) -> Vec<(i64, i64)> {
    positions.iter().map(|p| (p.line_id, p.course_id)).collect()
}

fn group_paths(points: Vec<PathPoint>) -> Vec<Vec<PathPoint>> {
    let mut paths: HashMap<(i64, i64), Vec<PathPoint>> = HashMap::new();
    for point in points {
        paths
            .entry((point.line_id, point.course_id))
            .or_default()
            .push(point);
    }
    paths.into_values().collect()
}

fn query_benches(c: &mut Criterion, fx: &Fixture) {
    let sample = &fx.sample;
    let positions = fx.line_positions();
    assert!(!positions.is_empty(), "no positions of the sample line");
    let course_ids: Vec<i64> = positions.iter().map(|p| p.course_id).collect();
    let courses = courses_of(&positions);
    let (window_start, window_end) = fx.speed_window();
    let paths = group_paths(
        fx.runtime
            .block_on(fx.db.path_points(&courses, window_start, window_end))
            .unwrap(),
    );
    assert!(!paths.is_empty(), "no paths of the sample line");
    let bbox_from = sample.busy_unix - db::duration_secs(BOUNDING_BOX_RANGE);

    let mut group = c.benchmark_group("query");
    group.bench_function("positions_at/line", |b| {
        b.to_async(&fx.runtime).iter(|| {
            fx.db.positions_at(
                PositionFilter::Line(&sample.line),
                sample.busy_unix,
                MAX_AGE,
            )
        })
    });
    group.bench_function("positions_at/plate", |b| {
        b.to_async(&fx.runtime).iter(|| {
            fx.db.positions_at(
                PositionFilter::Plate(&sample.plate),
                sample.plate_unix,
                MAX_AGE,
            )
        })
    });
    group.bench_function("fleet_vehicles", |b| {
        b.to_async(&fx.runtime)
            .iter(|| fx.db.fleet_vehicles(&course_ids))
    });
    group.bench_function("path_points", |b| {
        b.to_async(&fx.runtime)
            .iter(|| fx.db.path_points(&courses, window_start, window_end))
    });
    group.bench_function("average_speed", |b| {
        b.iter(|| {
            for path in black_box(&paths) {
                black_box(speed::average_speed_kmh(path, window_start, window_end));
            }
        })
    });
    group.bench_function("bounding_box/all", |b| {
        b.to_async(&fx.runtime)
            .iter(|| fx.db.bounding_box(None, None))
    });
    group.bench_function("bounding_box/range", |b| {
        b.to_async(&fx.runtime)
            .iter(|| fx.db.bounding_box(Some(bbox_from), Some(sample.busy_unix)))
    });
    group.finish();
}

fn fleet_benches(c: &mut Criterion, fx: &Fixture) {
    let sample = &fx.sample;
    let mut group = c.benchmark_group("fleet");
    group.bench_function("positions_active_at", |b| {
        b.to_async(&fx.runtime).iter(|| {
            fx.db
                .positions_active_at(&sample.plate_line, sample.plate_unix, fleet::TIME_SLACK)
        })
    });
    group.finish();
}

fn inference_benches(c: &mut Criterion, fx: &Fixture) {
    let (from, to) = fx.inference_window();
    let summaries = fx.course_summaries();
    let course_ids: Vec<i64> = summaries.iter().map(|s| s.course_id).collect();
    let known: Vec<CourseVehicle> = fx
        .runtime
        .block_on(fx.db.course_vehicles(&course_ids))
        .unwrap();
    assert!(
        !known.is_empty(),
        "no known vehicles in the inference window"
    );

    let mut group = c.benchmark_group("inference");
    group.bench_function("course_summaries", |b| {
        b.to_async(&fx.runtime)
            .iter(|| fx.db.course_summaries(from, to))
    });
    group.bench_function("course_vehicles", |b| {
        b.to_async(&fx.runtime)
            .iter(|| fx.db.course_vehicles(&course_ids))
    });
    group.bench_function("infer", |b| {
        b.iter(|| inference::infer(black_box(&summaries), black_box(&known), &INFERENCE))
    });
    group.finish();
}

async fn fetched_positions(raw: &SqlitePool, seen_at_unix: i64) -> Vec<(i64, Position)> {
    let rows: Vec<(i64, i64, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT p.id, l.vehicle_type, l.name, p.course_id, p.latitude_e6, p.longitude_e6 \
         FROM vehicle_positions p JOIN lines l ON l.id = p.line_id \
         WHERE p.first_seen_unix = ? ORDER BY p.id",
    )
    .bind(seen_at_unix)
    .fetch_all(raw)
    .await
    .unwrap();
    rows.into_iter()
        .map(
            |(id, vehicle_type, line, course_id, latitude_e6, longitude_e6)| {
                let position = Position {
                    vehicle_type: VehicleType::try_from(vehicle_type).unwrap(),
                    line,
                    course_id,
                    latitude: MicroDegrees(latitude_e6),
                    longitude: MicroDegrees(longitude_e6),
                };
                (id, position)
            },
        )
        .collect()
}

fn write_benches(c: &mut Criterion, fx: &Fixture) {
    let sample = &fx.sample;
    let path = fx.dir.path().join(WRITE_DATABASE_FILE);
    fs::copy(fx.backup(), &path).unwrap();
    let (db, fetched) = fx.runtime.block_on(async {
        let db = Db::open(&path, OpenMode::MustExist).await.unwrap();
        let raw = open_raw(&path).await;
        let fetched = fetched_positions(&raw, sample.busy_unix).await;
        raw.close().await;
        (db, fetched)
    });
    assert!(!fetched.is_empty(), "no positions in the sample fetch");
    let row_ids: Vec<i64> = fetched.iter().map(|(id, _)| *id).collect();
    let positions: Vec<&Position> = fetched.iter().map(|(_, position)| position).collect();
    let interval_secs = db::duration_secs(COLLECT_INTERVAL);
    let seen_again_unix = sample.busy_unix + interval_secs;
    let mut new_unix = sample.last_unix;

    let mut group = c.benchmark_group("write");
    group.bench_function("save_positions/new", |b| {
        b.to_async(&fx.runtime).iter(|| {
            new_unix += interval_secs;
            db.save_positions(new_unix, &positions, &[])
        })
    });
    group.bench_function("save_positions/seen_again", |b| {
        b.to_async(&fx.runtime)
            .iter(|| db.save_positions(seen_again_unix, &[], &row_ids))
    });
    group.finish();
    fx.runtime.block_on(db.close());
}

fn open_benches(c: &mut Criterion, fx: &Fixture) {
    let read = fx.dir.path().join(READ_DATABASE_FILE);
    let backup = fx.backup();

    let mut group = c.benchmark_group("open");
    group.bench_function("migrated", |b| {
        b.to_async(&fx.runtime).iter(|| async {
            let db = Db::open(&read, OpenMode::MustExist).await.unwrap();
            db.close().await;
        })
    });
    group.sample_size(SLOW_SAMPLE_SIZE);
    group.bench_function("backup_with_migrations", |b| {
        b.to_async(&fx.runtime).iter_batched(
            || {
                let dir = tempfile::tempdir_in(fx.dir.path()).unwrap();
                let path = dir.path().join(BACKUP_FILE);
                fs::copy(&backup, &path).unwrap();
                (dir, path)
            },
            |(dir, path)| async move {
                let db = Db::open(&path, OpenMode::MustExist).await.unwrap();
                db.close().await;
                dir
            },
            BatchSize::PerIteration,
        )
    });
    group.finish();
}

fn benches(c: &mut Criterion) {
    let fx = Fixture::new();
    query_benches(c, &fx);
    fleet_benches(c, &fx);
    inference_benches(c, &fx);
    write_benches(c, &fx);
    open_benches(c, &fx);
    let Fixture {
        dir, runtime, db, ..
    } = fx;
    runtime.block_on(db.close());
    drop(dir);
}

criterion_group!(backup, benches);
criterion_main!(backup);

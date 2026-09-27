use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jiff::Timestamp;
use jiff::civil;
use jiff::tz::TimeZone;
use mpk_history::db::{Db, OpenMode};
use mpk_history::mpk::VehicleType;
use mpk_history::stats::{self, Kind, Retention, StatsCache};
use mpk_history::web::{self, Clock};
use scraper::{Html, Selector};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tempfile::TempDir;
use tower::ServiceExt;

const COMPRESSED_BACKUP: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/test-data/mpk-history.backup.sqlite.xz"
);
const BINARY: &str = env!("CARGO_BIN_EXE_mpk-history");
const DATABASE_FILE: &str = "mpk-history.sqlite";
const BACKUP_COPY_FILE: &str = "copy.sqlite";
const TIME_ZONE: &str = "Europe/Warsaw";
const LOG_FILTER: &str = "error";
const MIGRATIONS_TABLE: &str = "_sqlx_migrations";
const INTEGRITY_OK: &str = "ok";
const ROLLBACK_JOURNAL: &str = "delete";

const MAX_ROW_SPAN_SECS: i64 = 10 * 60;
const MAX_AGE_SECS: i64 = 30;
const MATCH_RADIUS_M: f64 = 20.0;
const MATCH_TIME_SLACK_SECS: i64 = 30;
const INFERENCE_MAX_GAP_SECS: i64 = 45 * 60;
const INFERENCE_MAX_DISTANCE_M: f64 = 500.0;
const DISTANCE_TOLERANCE_M: f64 = 1.0;
const MICRO_DEGREES_PER_DEGREE: f64 = 1_000_000.0;
const MAX_LATITUDE_E6: i64 = 90_000_000;
const MAX_LONGITUDE_E6: i64 = 180_000_000;
// https://en.wikipedia.org/wiki/Earth_radius
const EARTH_RADIUS_M: f64 = 6_371_000.0;
// src/geo.rs counts 111 320 m per degree, which is the equatorial radius
const METERS_PER_DEGREE_RADIUS_M: f64 = 6_378_137.0;
const WEATHER_STEP_SECS: i64 = 15 * 60;
const TRIP_BREAK_SECS: i64 = 6 * 60 * 60;
const PHYSICAL_RANGES: [(&str, f64, f64); 12] = [
    ("temperature_2m", -50.0, 50.0),
    ("relative_humidity_2m", 0.0, 100.0),
    ("precipitation", 0.0, 200.0),
    ("rain", 0.0, 200.0),
    ("showers", 0.0, 200.0),
    ("snowfall", 0.0, 200.0),
    ("weather_code", 0.0, 99.0),
    ("cloud_cover", 0.0, 100.0),
    ("surface_pressure", 900.0, 1100.0),
    ("wind_speed_10m", 0.0, 250.0),
    ("wind_direction_10m", 0.0, 360.0),
    ("wind_gusts_10m", 0.0, 250.0),
];

const SAMPLE_TIMES: usize = 12;
const SAMPLE_LINES: i64 = 3;
const SAMPLE_COURSES: i64 = 20;
const MIN_SPEED_SAMPLES: usize = 50;
const MEDIAN_SPEED_KMH: std::ops::RangeInclusive<f64> = 8.0..=30.0;
const IMPLAUSIBLE_SPEED_KMH: f64 = 100.0;
const MAX_IMPLAUSIBLE_SPEED_SHARE: f64 = 0.01;
const PLATE_REGION_LEN: usize = 2;
const INFERRED_MARKER: char = '*';
const FLEET_VALUE_SEPARATOR: char = '/';
const UNKNOWN_SPEED: &str = "-";
const VEHICLE_TYPES: [&str; 3] = ["bus", "tram", "train"];
const QUERY_COLUMNS: usize = 13;
const MINUTE_SECS: i64 = 60;
const DAY_SECS: i64 = 24 * 60 * 60;
const MONTH_SECS: i64 = 30 * DAY_SECS;
const STEP_GAP_SECS: i64 = 60;
const STEP_MARGIN_SECS: i64 = MAX_ROW_SPAN_SECS + STEP_GAP_SECS;
const MAX_PLAUSIBLE_SPEED_KMH: f64 = 120.0;
const KMH_PER_METER_PER_SECOND: f64 = 3.6;
const SPEED_TOLERANCE_KMH: f64 = 0.001;
const KIND_ALL: &str = "all";
const DAY_RANGE: &str = "day";
const BACKUP_FULL_DAY: (i16, i8, i8) = (2026, 9, 26);
const HISTORY_RESOLUTION: Duration = Duration::from_mins(15);
const LOCALHOST: &str = "127.0.0.1";
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(10);
const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SVG_CONTENT_TYPE: &str = "image/svg+xml";
const TERMINATE_SIGNAL: &str = "-TERM";

enum MatchSourceId {
    Matched = 0,
    Inferred = 1,
}

static BACKUP: LazyLock<Vec<u8>> = LazyLock::new(|| {
    let compressed = fs::File::open(COMPRESSED_BACKUP)
        .unwrap_or_else(|e| panic!("cannot open {COMPRESSED_BACKUP}: {e}"));
    let mut decoder = liblzma::read::XzDecoder::new(BufReader::new(compressed));
    let mut backup = Vec::new();
    decoder.read_to_end(&mut backup).unwrap();
    backup
});

struct TestDatabase {
    _dir: TempDir,
    path: PathBuf,
}

impl TestDatabase {
    fn in_new_dir() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DATABASE_FILE);
        (dir, path)
    }

    fn empty() -> Self {
        let (dir, path) = Self::in_new_dir();
        fs::File::create(&path).unwrap();
        Self { _dir: dir, path }
    }

    fn copy_of_backup() -> Self {
        let (dir, path) = Self::in_new_dir();
        fs::write(&path, &*BACKUP).unwrap();
        Self { _dir: dir, path }
    }

    fn migrated_backup() -> Self {
        let database = Self::copy_of_backup();
        run_ok(&database.path, &["bbox"]);
        database
    }
}

fn run(database: &Path, args: &[&str]) -> Output {
    Command::new(BINARY)
        .arg("--database")
        .arg(database)
        .args(args)
        .env("TZ", TIME_ZONE)
        .env("RUST_LOG", LOG_FILTER)
        .output()
        .unwrap()
}

fn run_ok(database: &Path, args: &[&str]) -> String {
    let output = run(database, args);
    assert!(
        output.status.success(),
        "mpk-history {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

async fn open(path: &Path) -> SqlitePool {
    let options = SqliteConnectOptions::new().filename(path).read_only(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap()
}

async fn count(db: &SqlitePool, sql: &str) -> i64 {
    let (count,): (i64,) = sqlx::query_as(sql).fetch_one(db).await.unwrap();
    count
}

async fn assert_none(db: &SqlitePool, what: &str, sql: &str) {
    let found = count(db, &format!("SELECT COUNT(*) FROM ({sql})")).await;
    assert_eq!(found, 0, "{what}: {found} row(s) from {sql}");
}

async fn table_row_counts(db: &SqlitePool) -> BTreeMap<String, i64> {
    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(db)
    .await
    .unwrap();
    let mut counts = BTreeMap::new();
    for (table,) in tables {
        let rows = count(db, &format!("SELECT COUNT(*) FROM \"{table}\"")).await;
        counts.insert(table, rows);
    }
    counts
}

async fn pragma_text(db: &SqlitePool, pragma: &str) -> Vec<String> {
    let rows: Vec<(String,)> = sqlx::query_as(&format!("PRAGMA {pragma}"))
        .fetch_all(db)
        .await
        .unwrap();
    rows.into_iter().map(|(text,)| text).collect()
}

async fn assert_consistent(db: &SqlitePool) {
    assert_eq!(pragma_text(db, "integrity_check").await, [INTEGRITY_OK]);
    assert_none(
        db,
        "foreign key violations",
        "SELECT * FROM pragma_foreign_key_check",
    )
    .await;
}

async fn schema(db: &SqlitePool) -> Vec<String> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT m.type || ' ' || m.name || ': ' || \
                (SELECT group_concat(c.name || ' ' || c.type || ' notnull=' || c.\"notnull\" \
                                     || ' default=' || ifnull(c.dflt_value, '-') || ' pk=' || c.pk, ', ') \
                 FROM pragma_table_info(m.name) c) \
         FROM sqlite_master m WHERE m.type = 'table' AND m.name != ?1 \
         UNION ALL \
         SELECT 'index ' || i.name || ' on ' || m.name || ' unique=' || i.\"unique\" || ': ' || \
                (SELECT group_concat(ifnull(c.name, '?'), ', ') FROM pragma_index_info(i.name) c) \
         FROM sqlite_master m, pragma_index_list(m.name) i \
         WHERE m.type = 'table' AND m.name != ?1 \
         ORDER BY 1",
    )
    .bind(MIGRATIONS_TABLE)
    .fetch_all(db)
    .await
    .unwrap();
    rows.into_iter().map(|(line,)| line).collect()
}

async fn applied_migrations(db: &SqlitePool) -> Vec<(i64, bool)> {
    sqlx::query_as(&format!(
        "SELECT version, success FROM {MIGRATIONS_TABLE} ORDER BY version"
    ))
    .fetch_all(db)
    .await
    .unwrap()
}

#[derive(Debug)]
struct ShownVehicle {
    course_id: i64,
    first_seen_unix: i64,
    last_seen_unix: i64,
    latitude_e6: i64,
    longitude_e6: i64,
    sides: Vec<String>,
    plates: Vec<String>,
    speed_kmh: Option<f64>,
}

impl ShownVehicle {
    fn position(&self) -> (i64, i64, i64, i64, i64) {
        (
            self.course_id,
            self.first_seen_unix,
            self.last_seen_unix,
            self.latitude_e6,
            self.longitude_e6,
        )
    }
}

fn parse_local_time(date: &str, time: &str) -> i64 {
    let local: civil::DateTime = format!("{date} {time}").parse().unwrap();
    let time_zone = TimeZone::get(TIME_ZONE).unwrap();
    local.to_zoned(time_zone).unwrap().timestamp().as_second()
}

fn parse_e6(degrees: &str) -> i64 {
    let degrees: f64 = degrees.parse().unwrap();
    (degrees * MICRO_DEGREES_PER_DEGREE).round() as i64
}

fn split_fleet_value(value: &str) -> Vec<String> {
    value
        .split(FLEET_VALUE_SEPARATOR)
        .map(str::to_owned)
        .collect()
}

fn parse_query_output(stdout: &str) -> Vec<ShownVehicle> {
    let mut vehicles = Vec::new();
    for line in stdout.lines() {
        let columns: Vec<&str> = line.split_whitespace().collect();
        let Some(vehicle_type) = columns.first() else {
            continue;
        };
        if !VEHICLE_TYPES.contains(vehicle_type) {
            continue;
        }
        let [
            _,
            _,
            course,
            side,
            plate,
            _,
            speed,
            lat,
            lon,
            since_date,
            since_time,
            last_date,
            last_time,
        ] = columns[..]
        else {
            panic!("expected {QUERY_COLUMNS} columns in {line:?}");
        };
        vehicles.push(ShownVehicle {
            course_id: course.parse().unwrap(),
            first_seen_unix: parse_local_time(since_date, since_time),
            last_seen_unix: parse_local_time(last_date, last_time),
            latitude_e6: parse_e6(lat),
            longitude_e6: parse_e6(lon),
            sides: split_fleet_value(side),
            plates: split_fleet_value(plate),
            speed_kmh: (speed != UNKNOWN_SPEED).then(|| speed.parse().unwrap()),
        });
    }
    vehicles
}

fn rfc3339(unix: i64) -> String {
    Timestamp::from_second(unix).unwrap().to_string()
}

fn query_line(database: &Path, line: &str, at_unix: i64) -> String {
    run_ok(
        database,
        &["query", "--line", line, "--at", &rfc3339(at_unix)],
    )
}

async fn sample_times(db: &SqlitePool) -> Vec<i64> {
    let fetch_times: Vec<(i64,)> = sqlx::query_as(
        "SELECT DISTINCT first_seen_unix FROM vehicle_positions ORDER BY first_seen_unix",
    )
    .fetch_all(db)
    .await
    .unwrap();
    assert!(
        fetch_times.len() >= SAMPLE_TIMES,
        "backup has too few fetches"
    );
    let step = fetch_times.len() / SAMPLE_TIMES;
    fetch_times
        .iter()
        .step_by(step)
        .take(SAMPLE_TIMES)
        .map(|(unix,)| *unix)
        .collect()
}

async fn busiest_lines(db: &SqlitePool) -> Vec<String> {
    let lines: Vec<(String,)> = sqlx::query_as(
        "SELECT l.name FROM vehicle_positions p JOIN lines l ON l.id = p.line_id \
         GROUP BY l.id ORDER BY COUNT(*) DESC, l.name LIMIT ?",
    )
    .bind(SAMPLE_LINES)
    .fetch_all(db)
    .await
    .unwrap();
    lines.into_iter().map(|(name,)| name).collect()
}

async fn line_query_moments(db: &SqlitePool) -> Vec<(String, i64)> {
    let lines = busiest_lines(db).await;
    let mut moments = Vec::new();
    for at_unix in sample_times(db).await {
        for offset in [0, MAX_AGE_SECS + 1] {
            moments.extend(lines.iter().map(|line| (line.clone(), at_unix + offset)));
        }
    }
    let long_row_ends: Vec<(String, i64)> = sqlx::query_as(
        "SELECT l.name, p.first_seen_unix + p.seen_for_secs FROM vehicle_positions p \
         JOIN lines l ON l.id = p.line_id WHERE p.seen_for_secs >= ? ORDER BY p.id",
    )
    .bind(MAX_ROW_SPAN_SECS - MAX_AGE_SECS)
    .fetch_all(db)
    .await
    .unwrap();
    assert!(
        long_row_ends.len() >= SAMPLE_TIMES,
        "backup has too few long rows"
    );
    let step = long_row_ends.len() / SAMPLE_TIMES;
    for (line, end_unix) in long_row_ends.into_iter().step_by(step).take(SAMPLE_TIMES) {
        for offset in [0, MAX_AGE_SECS, MAX_AGE_SECS + 1] {
            moments.push((line.clone(), end_unix + offset));
        }
    }
    moments
}

async fn latest_start_of_course(db: &SqlitePool, line_id: i64, course_id: i64) -> i64 {
    let (unix,): (i64,) = sqlx::query_as(
        "SELECT MAX(first_seen_unix) FROM vehicle_positions WHERE line_id = ? AND course_id = ?",
    )
    .bind(line_id)
    .bind(course_id)
    .fetch_one(db)
    .await
    .unwrap();
    unix
}

async fn expected_positions(
    db: &SqlitePool,
    line: &str,
    at_unix: i64,
) -> Vec<(i64, i64, i64, i64, i64)> {
    sqlx::query_as(
        "WITH started AS ( \
             SELECT p.course_id, p.first_seen_unix, p.first_seen_unix + p.seen_for_secs AS last_seen_unix, \
                    p.latitude_e6, p.longitude_e6, \
                    MAX(p.first_seen_unix) OVER (PARTITION BY p.line_id, p.course_id) AS latest_start \
             FROM vehicle_positions p \
             JOIN lines l ON l.id = p.line_id \
             WHERE l.name = ?1 AND p.first_seen_unix <= ?2 \
         ) \
         SELECT course_id, first_seen_unix, last_seen_unix, latitude_e6, longitude_e6 FROM started \
         WHERE last_seen_unix >= latest_start AND last_seen_unix >= ?2 - ?3 \
         ORDER BY 1, 2, 3, 4, 5",
    )
    .bind(line)
    .bind(at_unix)
    .bind(MAX_AGE_SECS)
    .fetch_all(db)
    .await
    .unwrap()
}

fn haversine_m(a: (i64, i64), b: (i64, i64)) -> f64 {
    haversine_with_radius_m(a, b, EARTH_RADIUS_M)
}

fn haversine_with_radius_m(a: (i64, i64), b: (i64, i64), radius_m: f64) -> f64 {
    // https://en.wikipedia.org/wiki/Haversine_formula
    let radians = |e6: i64| (e6 as f64 / MICRO_DEGREES_PER_DEGREE).to_radians();
    let (lat_a, lon_a) = (radians(a.0), radians(a.1));
    let (lat_b, lon_b) = (radians(b.0), radians(b.1));
    let h = ((lat_b - lat_a) / 2.0).sin().powi(2)
        + lat_a.cos() * lat_b.cos() * ((lon_b - lon_a) / 2.0).sin().powi(2);
    2.0 * radius_m * h.sqrt().asin()
}

#[tokio::test]
async fn migrations_apply_to_the_backup_and_keep_every_row() {
    let database = TestDatabase::copy_of_backup();
    let before = {
        let db = open(&database.path).await;
        let counts = table_row_counts(&db).await;
        db.close().await;
        counts
    };

    run_ok(&database.path, &["bbox"]);

    let db = open(&database.path).await;
    let after = table_row_counts(&db).await;
    for (table, rows) in &before {
        if table == MIGRATIONS_TABLE {
            continue;
        }
        assert_eq!(
            after.get(table),
            Some(rows),
            "table {table} lost rows or disappeared during migrations"
        );
    }
    assert_consistent(&db).await;
    assert!(
        applied_migrations(&db)
            .await
            .iter()
            .all(|(_, success)| *success),
        "a migration is marked as failed"
    );
}

#[tokio::test]
async fn migrated_backup_has_the_same_schema_as_a_new_database() {
    let fresh_database = TestDatabase::empty();
    run_ok(&fresh_database.path, &["bbox"]);
    let fresh = open(&fresh_database.path).await;
    let migrated_database = TestDatabase::migrated_backup();
    let migrated = open(&migrated_database.path).await;

    assert_eq!(schema(&migrated).await, schema(&fresh).await);
    assert_eq!(
        applied_migrations(&migrated).await,
        applied_migrations(&fresh).await
    );
}

#[tokio::test]
async fn line_queries_match_a_scan_of_the_whole_line_history() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    let mut shown_total = 0;
    for (line, at_unix) in line_query_moments(&db).await {
        let shown = parse_query_output(&query_line(&database.path, &line, at_unix));
        let mut positions: Vec<_> = shown.iter().map(ShownVehicle::position).collect();
        positions.sort();
        assert_eq!(
            positions,
            expected_positions(&db, &line, at_unix).await,
            "line {line} at {}",
            rfc3339(at_unix)
        );
        shown_total += shown.len();
    }
    assert!(shown_total > 0, "no sample showed any vehicle");
}

#[tokio::test]
async fn query_accepts_local_time_and_upper_case_line_names() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    let (name, at_unix): (String, i64) = sqlx::query_as(
        "SELECT l.name, MAX(p.first_seen_unix) FROM vehicle_positions p \
         JOIN lines l ON l.id = p.line_id \
         WHERE l.name GLOB '[a-z]*' GROUP BY l.id ORDER BY COUNT(*) DESC LIMIT 1",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    let local = Timestamp::from_second(at_unix)
        .unwrap()
        .to_zoned(TimeZone::get(TIME_ZONE).unwrap())
        .strftime("%Y-%m-%d %H:%M:%S")
        .to_string();

    let canonical = query_line(&database.path, &name, at_unix);
    let upper_case_local = run_ok(
        &database.path,
        &["query", "--line", &name.to_uppercase(), "--at", &local],
    );
    assert_eq!(upper_case_local, canonical);
    assert!(!parse_query_output(&canonical).is_empty(), "{canonical}");
}

#[tokio::test]
async fn plate_queries_find_the_matched_vehicle() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    let matches: Vec<(i64, i64, i64, String)> = sqlx::query_as(
        "SELECT line_id, course_id, side_number, plate FROM course_vehicles \
         WHERE plate IS NOT NULL AND line_id IS NOT NULL AND source = ? \
         ORDER BY course_id, side_number LIMIT ?",
    )
    .bind(MatchSourceId::Matched as i64)
    .bind(SAMPLE_COURSES)
    .fetch_all(&db)
    .await
    .unwrap();
    assert!(!matches.is_empty(), "backup has no plates");

    for (line_id, course_id, side_number, plate) in matches {
        let at_unix = latest_start_of_course(&db, line_id, course_id).await;
        let (region, rest) = plate.split_at(PLATE_REGION_LEN);
        let typed = format!("{} {}", region.to_lowercase(), rest.to_lowercase());
        let stdout = run_ok(
            &database.path,
            &["query", "--plate", &typed, "--at", &rfc3339(at_unix)],
        );
        let shown = parse_query_output(&stdout);
        assert!(
            shown
                .iter()
                .any(|v| v.course_id == course_id && v.sides.contains(&side_number.to_string())),
            "plate {plate} course {course_id}: {stdout}"
        );
        assert!(
            shown.iter().all(|v| v.plates.contains(&plate)),
            "plate {plate} shows other vehicles: {stdout}"
        );
    }
}

#[tokio::test]
async fn inferred_vehicles_are_marked_in_line_queries() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    let inferred: Vec<(i64, String, i64, i64)> = sqlx::query_as(
        "SELECT cv.line_id, l.name, cv.course_id, cv.side_number FROM course_vehicles cv \
         JOIN lines l ON l.id = cv.line_id \
         WHERE cv.source = ? ORDER BY cv.course_id LIMIT ?",
    )
    .bind(MatchSourceId::Inferred as i64)
    .bind(SAMPLE_COURSES)
    .fetch_all(&db)
    .await
    .unwrap();
    assert!(!inferred.is_empty(), "backup has no inferred vehicles");

    for (line_id, line, course_id, side_number) in inferred {
        let at_unix = latest_start_of_course(&db, line_id, course_id).await;
        let stdout = query_line(&database.path, &line, at_unix);
        let marked = format!("{side_number}{INFERRED_MARKER}");
        assert!(
            parse_query_output(&stdout)
                .iter()
                .any(|v| v.course_id == course_id && v.sides == [marked.clone()]),
            "course {course_id} should show side {marked}: {stdout}"
        );
        assert!(
            stdout.lines().any(|l| l.starts_with(INFERRED_MARKER)),
            "missing footnote: {stdout}"
        );
    }
}

#[tokio::test]
async fn speeds_are_realistic() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    let lines = busiest_lines(&db).await;
    let mut speeds = Vec::new();
    for at_unix in sample_times(&db).await {
        for line in &lines {
            let shown = parse_query_output(&query_line(&database.path, line, at_unix));
            speeds.extend(shown.iter().filter_map(|v| v.speed_kmh));
        }
    }
    assert!(
        speeds.len() >= MIN_SPEED_SAMPLES,
        "only {} speeds",
        speeds.len()
    );
    assert!(speeds.iter().all(|s| *s >= 0.0), "{speeds:?}");
    speeds.sort_by(f64::total_cmp);
    let median = speeds[speeds.len() / 2];
    assert!(MEDIAN_SPEED_KMH.contains(&median), "median {median} km/h");
    let implausible = speeds
        .iter()
        .filter(|s| **s > IMPLAUSIBLE_SPEED_KMH)
        .count();
    assert!(
        implausible as f64 <= speeds.len() as f64 * MAX_IMPLAUSIBLE_SPEED_SHARE,
        "{implausible} of {} speeds above {IMPLAUSIBLE_SPEED_KMH} km/h",
        speeds.len()
    );
}

fn parse_bbox_output(stdout: &str) -> (i64, [i64; 4]) {
    let mut lines = stdout.lines();
    let positions = lines
        .next()
        .and_then(|l| l.strip_prefix("bounding box of "))
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or_else(|| panic!("unexpected bbox output: {stdout}"))
        .parse()
        .unwrap();
    let mut range = |label: &str| {
        let line = lines.next().unwrap();
        let values = line
            .strip_prefix(label)
            .unwrap_or_else(|| panic!("expected {label} in {line:?}"));
        let (low, high) = values.trim().split_once(" .. ").unwrap();
        (parse_e6(low), parse_e6(high))
    };
    let (south, north) = range("latitude:");
    let (west, east) = range("longitude:");
    (positions, [south, north, west, east])
}

async fn expected_bbox(db: &SqlitePool, from_unix: i64, to_unix: i64) -> (i64, [i64; 4]) {
    let (positions, south, north, west, east): (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), MIN(latitude_e6), MAX(latitude_e6), MIN(longitude_e6), MAX(longitude_e6) \
         FROM vehicle_positions WHERE first_seen_unix + seen_for_secs >= ? AND first_seen_unix <= ?",
    )
    .bind(from_unix)
    .bind(to_unix)
    .fetch_one(db)
    .await
    .unwrap();
    (positions, [south, north, west, east])
}

#[tokio::test]
async fn bbox_matches_the_stored_positions() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    let times = sample_times(&db).await;

    let all = parse_bbox_output(&run_ok(&database.path, &["bbox"]));
    assert_eq!(all, expected_bbox(&db, i64::MIN, i64::MAX).await);

    let (from, to) = (times[1], times[times.len() / 2]);
    let window = run_ok(
        &database.path,
        &["bbox", "--from", &rfc3339(from), "--to", &rfc3339(to)],
    );
    let window = parse_bbox_output(&window);
    assert_eq!(window, expected_bbox(&db, from, to).await);
    assert!(0 < window.0 && window.0 < all.0, "{window:?} vs {all:?}");
}

#[tokio::test]
async fn backup_copies_everything_and_leaves_the_source_untouched() {
    let database = TestDatabase::copy_of_backup();
    let target = database.path.with_file_name(BACKUP_COPY_FILE);
    let source_bytes = fs::read(&database.path).unwrap();

    run_ok(&database.path, &["backup", target.to_str().unwrap()]);

    assert!(
        fs::read(&database.path).unwrap() == source_bytes,
        "backup changed the source database"
    );
    let source_db = open(&database.path).await;
    let target_db = open(&target).await;
    assert_eq!(
        table_row_counts(&target_db).await,
        table_row_counts(&source_db).await
    );
    assert_consistent(&target_db).await;
    assert_eq!(
        pragma_text(&target_db, "journal_mode").await,
        [ROLLBACK_JOURNAL]
    );

    let target_bytes = fs::read(&target).unwrap();
    let again = run(&database.path, &["backup", target.to_str().unwrap()]);
    assert!(!again.status.success(), "backup overwrote an existing file");
    assert!(fs::read(&target).unwrap() == target_bytes);
}

#[tokio::test]
async fn position_rows_follow_the_tracker_rules() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    assert_none(
        &db,
        "rows longer than the maximum span",
        &format!(
            "SELECT id FROM vehicle_positions \
             WHERE seen_for_secs NOT BETWEEN 0 AND {MAX_ROW_SPAN_SECS}"
        ),
    )
    .await;
    assert_none(
        &db,
        "implausible coordinates",
        &format!(
            "SELECT id FROM vehicle_positions \
             WHERE abs(latitude_e6) > {MAX_LATITUDE_E6} OR abs(longitude_e6) > {MAX_LONGITUDE_E6} \
                OR (latitude_e6 = 0 AND longitude_e6 = 0)"
        ),
    )
    .await;
    assert_none(
        &db,
        "course IDs used by more than one line; fleet lookups by course ID would mix them up",
        "SELECT course_id FROM vehicle_positions \
         GROUP BY course_id HAVING COUNT(DISTINCT line_id) > 1",
    )
    .await;
}

#[tokio::test]
async fn lines_are_normalized() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    assert_none(
        &db,
        "line names that don't match the positions feed",
        "SELECT id FROM lines WHERE name = '' OR name != lower(trim(name))",
    )
    .await;
    assert_none(
        &db,
        "unknown vehicle types",
        &format!(
            "SELECT id FROM lines WHERE vehicle_type NOT BETWEEN 0 AND {}",
            VEHICLE_TYPES.len() - 1
        ),
    )
    .await;
    assert_none(
        &db,
        "lines without positions",
        "SELECT id FROM lines l \
         WHERE NOT EXISTS (SELECT 1 FROM vehicle_positions p WHERE p.line_id = l.id)",
    )
    .await;
}

#[tokio::test]
async fn course_vehicles_follow_the_matching_rules() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    let matched = MatchSourceId::Matched as i64;
    let inferred = MatchSourceId::Inferred as i64;
    let checks = [
        (
            "unknown sources",
            format!("SELECT * FROM course_vehicles WHERE source NOT IN ({matched}, {inferred})"),
        ),
        (
            "matched rows with an inference source",
            format!(
                "SELECT * FROM course_vehicles \
                 WHERE source = {matched} AND inferred_from_course_id IS NOT NULL"
            ),
        ),
        (
            "inferred rows without an inference source",
            format!(
                "SELECT * FROM course_vehicles \
                 WHERE source = {inferred} AND inferred_from_course_id IS NULL"
            ),
        ),
        (
            "courses with both matched and inferred rows",
            "SELECT course_id FROM course_vehicles \
             GROUP BY course_id HAVING COUNT(DISTINCT source) > 1"
                .to_owned(),
        ),
        (
            "courses with more than one inferred vehicle",
            format!(
                "SELECT course_id FROM course_vehicles WHERE source = {inferred} \
                 GROUP BY course_id HAVING COUNT(*) > 1"
            ),
        ),
        (
            "plates not normalized",
            "SELECT * FROM course_vehicles \
             WHERE plate IS NOT NULL AND (plate = '' OR plate = '-' OR plate != upper(replace(plate, ' ', '')))"
                .to_owned(),
        ),
        (
            "empty brigades",
            "SELECT * FROM course_vehicles WHERE brigade IS NOT NULL AND trim(brigade) = ''".to_owned(),
        ),
        (
            "missing side numbers",
            "SELECT * FROM course_vehicles WHERE side_number <= 0".to_owned(),
        ),
        (
            "matches farther than the match radius",
            format!(
                "SELECT * FROM course_vehicles WHERE source = {matched} AND distance_m > {MATCH_RADIUS_M}"
            ),
        ),
        (
            "rows whose course has no positions on their line",
            "SELECT * FROM course_vehicles WHERE line_id IS NOT NULL AND (line_id, course_id) NOT IN ( \
                 SELECT DISTINCT line_id, course_id FROM vehicle_positions)"
                .to_owned(),
        ),
        (
            "inferred rows not copied from a neighbouring course",
            format!(
                "SELECT * FROM course_vehicles \
                 WHERE source = {inferred} AND abs(inferred_from_course_id - course_id) != 1"
            ),
        ),
        (
            "inferred rows that differ from their source course",
            format!(
                "SELECT c.* FROM course_vehicles c \
                 WHERE c.source = {inferred} AND NOT EXISTS ( \
                     SELECT 1 FROM course_vehicles s \
                     WHERE s.course_id = c.inferred_from_course_id AND s.line_id IS c.line_id \
                       AND s.side_number = c.side_number AND s.plate IS c.plate AND s.brigade IS c.brigade)"
            ),
        ),
    ];
    for (what, sql) in checks {
        assert_none(&db, what, &sql).await;
    }
}

#[tokio::test]
async fn inference_chains_end_at_a_direct_match() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    let rows: Vec<(i64, i64, Option<i64>)> =
        sqlx::query_as("SELECT course_id, source, inferred_from_course_id FROM course_vehicles")
            .fetch_all(&db)
            .await
            .unwrap();
    let parents: HashMap<i64, Option<i64>> = rows
        .iter()
        .map(|(course_id, _, from)| (*course_id, *from))
        .collect();
    let matched: HashSet<i64> = rows
        .iter()
        .filter(|(_, source, _)| *source == MatchSourceId::Matched as i64)
        .map(|(course_id, _, _)| *course_id)
        .collect();

    for (start, _, _) in rows.iter().filter(|(_, _, from)| from.is_some()) {
        let mut visited = HashSet::from([*start]);
        let mut course_id = *start;
        while let Some(Some(parent)) = parents.get(&course_id) {
            assert!(visited.insert(*parent), "cycle through course {start}");
            course_id = *parent;
        }
        assert!(
            matched.contains(&course_id),
            "chain from course {start} ends at {course_id}, which has no direct match"
        );
    }
}

#[derive(Debug, Clone, Copy)]
struct Trip {
    start_unix: i64,
    start: (i64, i64),
    end_unix: i64,
    end: (i64, i64),
}

type CourseKey = (i64, i64);

async fn trips_of_inferred_links(db: &SqlitePool) -> HashMap<CourseKey, Vec<Trip>> {
    let rows: Vec<(i64, i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT line_id, course_id, first_seen_unix, first_seen_unix + seen_for_secs, \
                latitude_e6, longitude_e6 \
         FROM vehicle_positions \
         WHERE (line_id, course_id) IN ( \
             SELECT line_id, course_id FROM course_vehicles WHERE source = ?1 \
             UNION SELECT line_id, inferred_from_course_id FROM course_vehicles WHERE source = ?1) \
         ORDER BY line_id, course_id, first_seen_unix",
    )
    .bind(MatchSourceId::Inferred as i64)
    .fetch_all(db)
    .await
    .unwrap();
    let mut trips: HashMap<CourseKey, Vec<Trip>> = HashMap::new();
    for (line_id, course_id, first_seen_unix, last_seen_unix, latitude_e6, longitude_e6) in rows {
        let position = (latitude_e6, longitude_e6);
        let course_trips = trips.entry((line_id, course_id)).or_default();
        let Some(trip) = course_trips
            .last_mut()
            .filter(|trip| first_seen_unix - trip.end_unix <= TRIP_BREAK_SECS)
        else {
            course_trips.push(Trip {
                start_unix: first_seen_unix,
                start: position,
                end_unix: last_seen_unix,
                end: position,
            });
            continue;
        };
        if last_seen_unix >= trip.end_unix {
            trip.end_unix = last_seen_unix;
            trip.end = position;
        }
    }
    trips
}

fn link_distance_m(earlier: &Trip, later: &Trip) -> Option<f64> {
    let gap_secs = later.start_unix - earlier.end_unix;
    if !(0..=INFERENCE_MAX_GAP_SECS).contains(&gap_secs) {
        return None;
    }
    Some(haversine_m(earlier.end, later.start))
}

#[tokio::test]
async fn inferred_vehicles_link_trips_that_follow_each_other() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    let inferred: Vec<(i64, i64, i64, f64)> = sqlx::query_as(
        "SELECT line_id, course_id, inferred_from_course_id, distance_m FROM course_vehicles \
         WHERE source = ?",
    )
    .bind(MatchSourceId::Inferred as i64)
    .fetch_all(&db)
    .await
    .unwrap();
    let trips = trips_of_inferred_links(&db).await;
    let trips_of = |line_id, course_id| {
        trips
            .get(&(line_id, course_id))
            .map_or(&[][..], Vec::as_slice)
    };

    for (line_id, course_id, from_course_id, stored_distance_m) in inferred {
        let earlier = trips_of(line_id, course_id.min(from_course_id));
        let later = trips_of(line_id, course_id.max(from_course_id));
        let distances: Vec<f64> = earlier
            .iter()
            .flat_map(|e| later.iter().filter_map(|l| link_distance_m(e, l)))
            .collect();
        assert!(
            !distances.is_empty(),
            "course {course_id}: no trip of course {from_course_id} starts or ends \
             within {INFERENCE_MAX_GAP_SECS} s of it"
        );
        assert!(
            distances
                .iter()
                .any(|d| (d - stored_distance_m).abs() <= DISTANCE_TOLERANCE_M
                    && *d <= INFERENCE_MAX_DISTANCE_M + DISTANCE_TOLERANCE_M),
            "course {course_id}: stored distance {stored_distance_m:.1} m, trips are {distances:?} m apart"
        );
    }
}

#[tokio::test]
async fn matched_courses_were_running_at_the_observed_time() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    assert_none(
        &db,
        "direct matches whose course wasn't running at the fleet record's time; \
         a time zone or DST mistake shifts observed_unix",
        &format!(
            "SELECT * FROM course_vehicles cv \
             WHERE source = {} AND line_id IS NOT NULL AND NOT EXISTS ( \
                 SELECT 1 FROM vehicle_positions p \
                 WHERE p.line_id = cv.line_id AND p.course_id = cv.course_id \
                   AND p.first_seen_unix BETWEEN cv.observed_unix - {MAX_ROW_SPAN_SECS} - {slack} \
                                             AND cv.observed_unix + {slack} \
                   AND p.first_seen_unix + p.seen_for_secs >= cv.observed_unix - {slack})",
            MatchSourceId::Matched as i64,
            slack = MATCH_TIME_SLACK_SECS,
        ),
    )
    .await;
}

#[tokio::test]
async fn weather_observations_are_consistent() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    assert_none(
        &db,
        "observations not at the start of a 15-minute step",
        &format!(
            "SELECT * FROM weather_observations WHERE observed_unix % {WEATHER_STEP_SECS} != 0"
        ),
    )
    .await;
    assert_none(
        &db,
        "observations fetched before their step started",
        "SELECT * FROM weather_observations WHERE fetched_unix < observed_unix",
    )
    .await;
    assert_none(
        &db,
        "steps with a different number of grid points than the rest",
        "SELECT model FROM (SELECT model, observed_unix, COUNT(*) AS points FROM weather_observations \
                            GROUP BY model, observed_unix) \
         GROUP BY model HAVING COUNT(DISTINCT points) > 1",
    )
    .await;
    let outside_ranges = PHYSICAL_RANGES
        .iter()
        .map(|(column, low, high)| format!("{column} NOT BETWEEN {low} AND {high}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    assert_none(
        &db,
        "values outside physical ranges; a unit or column mix-up",
        &format!("SELECT * FROM weather_observations WHERE {outside_ranges}"),
    )
    .await;
}

#[derive(Debug, Default)]
struct ScannedStats {
    samples: u64,
    vehicle_samples: [u64; VEHICLE_TYPES.len()],
    peak_vehicles: u64,
    distance_m: [f64; VEHICLE_TYPES.len()],
    travel_secs: [i64; VEHICLE_TYPES.len()],
}

impl ScannedStats {
    fn average_vehicles(&self, vehicle_type: Option<usize>) -> f64 {
        let vehicles: u64 = match vehicle_type {
            Some(index) => self.vehicle_samples[index],
            None => self.vehicle_samples.iter().sum(),
        };
        vehicles as f64 / self.samples as f64
    }

    fn average_speed_kmh(&self, vehicle_type: Option<usize>) -> f64 {
        let (distance_m, travel_secs) = match vehicle_type {
            Some(index) => (self.distance_m[index], self.travel_secs[index]),
            None => (self.distance_m.iter().sum(), self.travel_secs.iter().sum()),
        };
        distance_m / travel_secs as f64 * KMH_PER_METER_PER_SECOND
    }
}

#[derive(sqlx::FromRow)]
struct StepRow {
    vehicle_type: i64,
    lat: i64,
    lon: i64,
    first: i64,
    last: i64,
    next_lat: Option<i64>,
    next_lon: Option<i64>,
    next_first: Option<i64>,
}

impl StepRow {
    fn step(&self) -> Option<((i64, i64), f64)> {
        let (Some(next_lat), Some(next_lon), Some(next_first)) =
            (self.next_lat, self.next_lon, self.next_first)
        else {
            return None;
        };
        let travel_secs = next_first - self.first;
        if next_first - self.last > STEP_GAP_SECS || travel_secs <= 0 {
            return None;
        }
        let distance_m = haversine_with_radius_m(
            (self.lat, self.lon),
            (next_lat, next_lon),
            METERS_PER_DEGREE_RADIUS_M,
        );
        if distance_m / travel_secs as f64 * KMH_PER_METER_PER_SECOND > MAX_PLAUSIBLE_SPEED_KMH {
            return None;
        }
        Some(((self.first, next_first), distance_m))
    }
}

impl ScannedStats {
    fn add_travel(
        &mut self,
        vehicle_type: i64,
        window: (i64, i64),
        (start_unix, end_unix): (i64, i64),
        distance_m: f64,
    ) {
        let span_secs = end_unix - start_unix;
        let overlap_secs = end_unix.min(window.1) - start_unix.max(window.0);
        if span_secs <= 0 || overlap_secs <= 0 {
            return;
        }
        self.distance_m[vehicle_type as usize] +=
            distance_m * overlap_secs as f64 / span_secs as f64;
        self.travel_secs[vehicle_type as usize] += overlap_secs;
    }
}

async fn scan_stats(db: &SqlitePool, from_unix: i64, to_unix: i64) -> ScannedStats {
    let oldest_first_seen_unix = from_unix - STEP_MARGIN_SECS;
    let rows: Vec<(i64, i64, i64)> = sqlx::query_as(
        "SELECT l.vehicle_type, p.first_seen_unix, p.first_seen_unix + p.seen_for_secs \
         FROM vehicle_positions p JOIN lines l ON l.id = p.line_id \
         WHERE p.first_seen_unix >= ? AND p.first_seen_unix < ?",
    )
    .bind(oldest_first_seen_unix)
    .bind(to_unix + STEP_MARGIN_SECS)
    .fetch_all(db)
    .await
    .unwrap();
    let all_times: std::collections::BTreeSet<i64> = rows
        .iter()
        .flat_map(|(_, first, last)| [*first, *last])
        .collect();
    let times: std::collections::BTreeSet<i64> = all_times
        .iter()
        .copied()
        .filter(|unix| (from_unix..to_unix).contains(unix))
        .collect();
    let mut visible: HashMap<i64, [u64; VEHICLE_TYPES.len()]> = HashMap::new();
    for (vehicle_type, first, last) in &rows {
        for unix in times.range(*first..=*last) {
            visible.entry(*unix).or_default()[*vehicle_type as usize] += 1;
        }
    }
    let mut stats = ScannedStats {
        samples: times.len() as u64,
        ..ScannedStats::default()
    };
    for counts in visible.values() {
        for (total, count) in stats.vehicle_samples.iter_mut().zip(counts) {
            *total += count;
        }
        stats.peak_vehicles = stats.peak_vehicles.max(counts.iter().sum());
    }

    let window = (from_unix, to_unix);
    let steps: Vec<StepRow> = sqlx::query_as(
        "SELECT l.vehicle_type, p.latitude_e6 AS lat, p.longitude_e6 AS lon, \
                p.first_seen_unix AS first, p.first_seen_unix + p.seen_for_secs AS last, \
                LEAD(p.latitude_e6) OVER w AS next_lat, LEAD(p.longitude_e6) OVER w AS next_lon, \
                LEAD(p.first_seen_unix) OVER w AS next_first \
         FROM vehicle_positions p JOIN lines l ON l.id = p.line_id \
         WHERE p.first_seen_unix >= ? AND p.first_seen_unix < ? \
         WINDOW w AS (PARTITION BY p.line_id, p.course_id ORDER BY p.first_seen_unix)",
    )
    .bind(oldest_first_seen_unix)
    .bind(to_unix + STEP_MARGIN_SECS)
    .fetch_all(db)
    .await
    .unwrap();
    for row in &steps {
        let standing = (row.first, row.last);
        match row.step() {
            Some((span, distance_m)) => {
                stats.add_travel(row.vehicle_type, window, span, distance_m);
            }
            None => {
                let next_fetch = all_times.range(row.last + 1..).next();
                if next_fetch.is_none_or(|next| next - row.last > STEP_GAP_SECS) {
                    stats.add_travel(row.vehicle_type, window, standing, 0.0);
                }
            }
        }
    }
    stats
}

async fn vehicle_types_seen(db: &SqlitePool, from_unix: i64, to_unix: i64) -> Vec<usize> {
    let types: Vec<(i64,)> = sqlx::query_as(
        "SELECT DISTINCT l.vehicle_type FROM vehicle_positions p JOIN lines l ON l.id = p.line_id \
         WHERE p.first_seen_unix + p.seen_for_secs >= ? AND p.first_seen_unix < ? ORDER BY 1",
    )
    .bind(from_unix)
    .bind(to_unix)
    .fetch_all(db)
    .await
    .unwrap();
    types.into_iter().map(|(t,)| t as usize).collect()
}

async fn web_page(database: &Path, uri: &str, now_unix: i64) -> Html {
    let db = Db::open(database, OpenMode::MustExist).await.unwrap();
    let clock = Clock::Fixed(Timestamp::from_second(now_unix).unwrap());
    let response = web::router(db.clone(), clock)
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    db.close().await;
    Html::parse_document(std::str::from_utf8(&body).unwrap())
}

async fn computed_totals(
    database: &Path,
    resolution: Duration,
    window: (i64, i64),
    now_unix: i64,
) -> stats::Bucket {
    let db = Db::open(database, OpenMode::MustExist).await.unwrap();
    let buckets = StatsCache::new(resolution, Retention::Forever)
        .buckets(&db, window.0, window.1, now_unix)
        .await
        .unwrap();
    db.close().await;
    stats::sum(&buckets)
}

async fn last_seen_unix(db: &SqlitePool) -> i64 {
    count(
        db,
        "SELECT MAX(first_seen_unix + seen_for_secs) FROM vehicle_positions",
    )
    .await
}

fn single_text(html: &Html, selector: &str) -> String {
    let parsed = Selector::parse(selector).unwrap();
    let texts: Vec<String> = html
        .select(&parsed)
        .map(|element| element.text().collect())
        .collect();
    let [text] = &texts[..] else {
        panic!("expected one element for {selector}, got {texts:?}");
    };
    text.clone()
}

fn assert_matches_scan(
    shown_types: &[usize],
    scanned: &ScannedStats,
    computed: &stats::Bucket,
    cell: impl Fn(&str, &str) -> String,
) {
    let kinds = std::iter::once((KIND_ALL, None))
        .chain(shown_types.iter().map(|t| (VEHICLE_TYPES[*t], Some(*t))));
    for (kind, vehicle_type) in kinds {
        assert_eq!(
            cell(kind, "average-vehicles"),
            format!("{:.1}", scanned.average_vehicles(vehicle_type)),
            "{kind}"
        );
        let speed = computed
            .get(stats_kind(vehicle_type))
            .average_speed_kmh()
            .unwrap();
        let scanned_speed = scanned.average_speed_kmh(vehicle_type);
        assert!(
            (speed - scanned_speed).abs() <= SPEED_TOLERANCE_KMH,
            "{kind}: computed {speed:.3} km/h, scan gives {scanned_speed:.3} km/h"
        );
        assert_eq!(cell(kind, "average-speed"), format!("{speed:.1}"), "{kind}");
        assert!(MEDIAN_SPEED_KMH.contains(&speed), "{kind}: {speed} km/h");
    }
    assert_eq!(
        cell(KIND_ALL, "peak-vehicles"),
        scanned.peak_vehicles.to_string()
    );
}

fn stats_kind(vehicle_type: Option<usize>) -> Kind {
    vehicle_type.map_or(Kind::All, |index| {
        Kind::Vehicle(VehicleType::try_from(i64::try_from(index).unwrap()).unwrap())
    })
}

#[tokio::test]
async fn web_statistics_match_a_scan_of_the_backup() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    let now_unix = last_seen_unix(&db).await;
    let to_unix = (now_unix + MINUTE_SECS - 1).div_euclid(MINUTE_SECS) * MINUTE_SECS;
    let window = (to_unix - DAY_SECS, to_unix);
    let scanned = scan_stats(&db, window.0, window.1).await;
    let html = web_page(&database.path, "/", now_unix).await;
    let computed = computed_totals(&database.path, stats::BUCKET, window, now_unix).await;

    let shown_types = vehicle_types_seen(&db, to_unix - MONTH_SECS, to_unix).await;
    let expected_tables: Vec<String> = std::iter::once(KIND_ALL)
        .chain(shown_types.iter().map(|t| VEHICLE_TYPES[*t]))
        .map(|kind| format!("stats-{kind}"))
        .collect();
    let table_selector = Selector::parse("#summary table.stats").unwrap();
    let tables: Vec<String> = html
        .select(&table_selector)
        .filter_map(|table| table.value().attr("id").map(str::to_owned))
        .collect();
    assert_eq!(tables, expected_tables);

    assert_matches_scan(&shown_types, &scanned, &computed, |kind, metric| {
        single_text(
            &html,
            &format!("#stats-{kind} tr[data-metric={metric}] td[data-range={DAY_RANGE}]"),
        )
    });
}

#[tokio::test]
async fn calendar_day_page_matches_a_scan_of_that_day() {
    let database = TestDatabase::migrated_backup();
    let db = open(&database.path).await;
    let now_unix = last_seen_unix(&db).await;
    let first_unix = count(&db, "SELECT MIN(first_seen_unix) FROM vehicle_positions").await;
    let tz = TimeZone::system();
    let day = civil::date(BACKUP_FULL_DAY.0, BACKUP_FULL_DAY.1, BACKUP_FULL_DAY.2);
    let start = |date: civil::Date| date.to_zoned(tz.clone()).unwrap().timestamp().as_second();
    let window = (start(day), start(day.tomorrow().unwrap()));
    assert!(
        first_unix < window.0 && window.1 < now_unix,
        "the backup doesn't cover {day}"
    );
    let scanned = scan_stats(&db, window.0, window.1).await;
    let computed = computed_totals(&database.path, HISTORY_RESOLUTION, window, now_unix).await;
    let html = web_page(
        &database.path,
        &format!("/day/{}", day.strftime("%Y-%m-%d")),
        now_unix,
    )
    .await;

    assert_eq!(
        single_text(&html, "h1"),
        day.strftime("%A, %-d %B %Y").to_string()
    );
    let shown_types = vehicle_types_seen(&db, window.0, window.1).await;
    assert_matches_scan(&shown_types, &scanned, &computed, |kind, metric| {
        single_text(
            &html,
            &format!("#period-stats tr[data-metric={metric}] td[data-kind={kind}]"),
        )
    });
}

struct RunningServer(Child);

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind((LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn get_when_ready(client: &reqwest::Client, url: &str) -> reqwest::Response {
    let started = Instant::now();
    loop {
        match client.get(url).send().await {
            Ok(response) => return response,
            Err(e) if started.elapsed() > SERVER_START_TIMEOUT => {
                panic!("server did not answer {url}: {e}")
            }
            Err(_) => tokio::time::sleep(SERVER_POLL_INTERVAL).await,
        }
    }
}

fn collect(database: &Path, web: &str, address: &str, dead_proxy: &str) -> RunningServer {
    let child = Command::new(BINARY)
        .arg("--database")
        .arg(database)
        .args(["collect", "--web", web, "--listen", address])
        .args(["--weather", "disabled", "--fleet", "disabled"])
        .env("TZ", TIME_ZONE)
        .env("RUST_LOG", LOG_FILTER)
        .env("HTTP_PROXY", dead_proxy)
        .env("HTTPS_PROXY", dead_proxy)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .spawn()
        .unwrap();
    RunningServer(child)
}

fn dead_proxy() -> String {
    format!("http://{LOCALHOST}:{}", free_port())
}

fn local_client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn fetch_error_count(database: &Path) -> i64 {
    let db = open(database).await;
    let errors = count(&db, "SELECT COUNT(*) FROM fetch_errors").await;
    db.close().await;
    errors
}

async fn wait_for_new_fetch_errors(database: &Path, before: i64) {
    let started = Instant::now();
    while fetch_error_count(database).await <= before {
        assert!(
            started.elapsed() < SERVER_START_TIMEOUT,
            "collect saved no fetch errors"
        );
        tokio::time::sleep(SERVER_POLL_INTERVAL).await;
    }
}

fn terminate(server: &mut RunningServer) {
    let terminated = Command::new("kill")
        .arg(TERMINATE_SIGNAL)
        .arg(server.0.id().to_string())
        .status()
        .unwrap();
    assert!(terminated.success());
    let status = server.0.wait().unwrap();
    assert!(status.success(), "collect exited with {status}");
}

#[tokio::test]
async fn collect_serves_the_web_page_until_terminated() {
    let database = TestDatabase::migrated_backup();
    let errors_before = fetch_error_count(&database.path).await;
    let address = format!("{LOCALHOST}:{}", free_port());
    let mut server = collect(&database.path, "enabled", &address, &dead_proxy());
    let client = local_client();

    let page = get_when_ready(&client, &format!("http://{address}/")).await;
    assert_eq!(page.status(), reqwest::StatusCode::OK);
    assert!(page.text().await.unwrap().contains("id=\"stats-all\""));
    let chart = client
        .get(format!(
            "http://{address}/charts/average-speed.svg?range=month"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(chart.status(), reqwest::StatusCode::OK);
    assert_eq!(
        chart.headers()[reqwest::header::CONTENT_TYPE],
        SVG_CONTENT_TYPE
    );
    wait_for_new_fetch_errors(&database.path, errors_before).await;
    let page = client
        .get(format!("http://{address}/quality"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let line_list_failures = single_text(
        &Html::parse_document(&page),
        "#fetch-failures tr[data-source=mpk-lines] td[data-range=hour]",
    );
    assert_eq!(line_list_failures, "100.0%");

    terminate(&mut server);
}

#[tokio::test]
async fn collect_without_web_does_not_listen() {
    let database = TestDatabase::migrated_backup();
    let errors_before = fetch_error_count(&database.path).await;
    let address = format!("{LOCALHOST}:{}", free_port());
    let mut server = collect(&database.path, "disabled", &address, &dead_proxy());

    wait_for_new_fetch_errors(&database.path, errors_before).await;
    let refused = local_client()
        .get(format!("http://{address}/"))
        .send()
        .await
        .unwrap_err();
    assert!(refused.is_connect(), "{refused}");

    terminate(&mut server);
}

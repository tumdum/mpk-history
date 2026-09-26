use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};

use crate::geo::MicroDegrees;
use crate::mpk::{Position, VehicleType};
use crate::open_meteo::GridPoint;

const MAX_CONNECTIONS: u32 = 1;

pub const MAX_ROW_SPAN: Duration = Duration::from_secs(10 * 60);

pub enum OpenMode {
    CreateIfMissing,
    MustExist,
    ExistingAsIs,
}

#[derive(Debug)]
pub struct StoredPosition {
    pub line_id: i64,
    pub vehicle_type: VehicleType,
    pub line: String,
    pub course_id: i64,
    pub latitude: MicroDegrees,
    pub longitude: MicroDegrees,
    pub first_seen_unix: i64,
    pub last_seen_unix: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PathPoint {
    pub line_id: i64,
    pub course_id: i64,
    pub latitude: MicroDegrees,
    pub longitude: MicroDegrees,
    pub first_seen_unix: i64,
    pub last_seen_unix: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActivePosition {
    pub line_id: i64,
    pub course_id: i64,
    pub latitude: MicroDegrees,
    pub longitude: MicroDegrees,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CourseVehicle {
    pub line_id: i64,
    pub course_id: i64,
    pub side_number: i64,
    pub plate: Option<String>,
    pub brigade: Option<String>,
    pub observed_unix: i64,
    pub distance_m: f64,
    pub source: MatchSource,
    pub inferred_from_course_id: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchSource {
    Matched = 0,
    Inferred = 1,
}

impl MatchSource {
    pub const ALL: [MatchSource; 2] = [MatchSource::Matched, MatchSource::Inferred];

    pub fn id(self) -> i64 {
        self as i64
    }
}

impl TryFrom<i64> for MatchSource {
    type Error = anyhow::Error;

    fn try_from(id: i64) -> Result<Self> {
        MatchSource::ALL
            .into_iter()
            .find(|source| source.id() == id)
            .ok_or_else(|| anyhow::anyhow!("unknown match source id: {id}"))
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SavedCourseVehicles {
    pub inserted: u64,
    pub corrected: u64,
    pub removed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
struct VehicleIdentity {
    side_number: i64,
    plate: Option<String>,
    brigade: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FleetVehicle {
    pub side_number: i64,
    pub plate: Option<String>,
    pub brigade: Option<String>,
    pub source: MatchSource,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CourseSummary {
    pub line_id: i64,
    pub course_id: i64,
    pub first_seen_unix: i64,
    pub last_seen_unix: i64,
    pub start: (MicroDegrees, MicroDegrees),
    pub end: (MicroDegrees, MicroDegrees),
    pub start_is_complete: bool,
}

#[derive(sqlx::FromRow)]
struct FleetVehicleRow {
    course_id: i64,
    side_number: i64,
    plate: Option<String>,
    brigade: Option<String>,
    source: i64,
}

#[derive(sqlx::FromRow)]
struct CourseVehicleRow {
    line_id: i64,
    course_id: i64,
    side_number: i64,
    plate: Option<String>,
    brigade: Option<String>,
    observed_unix: i64,
    distance_m: f64,
    source: i64,
    inferred_from_course_id: Option<i64>,
}

impl TryFrom<CourseVehicleRow> for CourseVehicle {
    type Error = anyhow::Error;

    fn try_from(row: CourseVehicleRow) -> Result<Self> {
        Ok(Self {
            line_id: row.line_id,
            course_id: row.course_id,
            side_number: row.side_number,
            plate: row.plate,
            brigade: row.brigade,
            observed_unix: row.observed_unix,
            distance_m: row.distance_m,
            source: row.source.try_into()?,
            inferred_from_course_id: row.inferred_from_course_id,
        })
    }
}

pub enum PositionFilter<'a> {
    Line(&'a str),
    Plate(&'a str),
}

impl PositionFilter<'_> {
    fn condition(&self) -> &'static str {
        match self {
            PositionFilter::Line(_) => "line_id IN (SELECT id FROM lines WHERE name = ?1)",
            PositionFilter::Plate(_) => {
                "line_id IN (SELECT line_id FROM course_vehicles WHERE plate = ?1) \
                 AND course_id IN (SELECT course_id FROM course_vehicles WHERE plate = ?1)"
            }
        }
    }

    fn value(&self) -> &str {
        match self {
            PositionFilter::Line(value) | PositionFilter::Plate(value) => value,
        }
    }
}

#[derive(Debug)]
pub struct BoundingBox {
    pub positions: i64,
    pub south: MicroDegrees,
    pub north: MicroDegrees,
    pub west: MicroDegrees,
    pub east: MicroDegrees,
}

#[derive(sqlx::FromRow)]
struct PositionRow {
    line_id: i64,
    vehicle_type: i64,
    line: String,
    course_id: i64,
    latitude_e6: i64,
    longitude_e6: i64,
    first_seen_unix: i64,
    last_seen_unix: i64,
}

impl TryFrom<PositionRow> for StoredPosition {
    type Error = anyhow::Error;

    fn try_from(row: PositionRow) -> Result<Self> {
        Ok(Self {
            line_id: row.line_id,
            vehicle_type: row.vehicle_type.try_into()?,
            line: row.line,
            course_id: row.course_id,
            latitude: MicroDegrees(row.latitude_e6),
            longitude: MicroDegrees(row.longitude_e6),
            first_seen_unix: row.first_seen_unix,
            last_seen_unix: row.last_seen_unix,
        })
    }
}

type LineKey = (VehicleType, String);

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
    line_ids: Arc<Mutex<HashMap<LineKey, i64>>>,
}

impl Db {
    pub async fn open(path: &Path, mode: OpenMode) -> Result<Self> {
        let options = SqliteConnectOptions::new().filename(path);
        let options = match mode {
            OpenMode::ExistingAsIs => options,
            OpenMode::CreateIfMissing | OpenMode::MustExist => options
                .create_if_missing(matches!(mode, OpenMode::CreateIfMissing))
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal)
                .foreign_keys(true),
        };
        let pool = SqlitePoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .connect_with(options)
            .await
            .with_context(|| format!("failed to open database {}", path.display()))?;
        if let OpenMode::CreateIfMissing | OpenMode::MustExist = mode {
            sqlx::migrate!().run(&pool).await?;
        }
        Ok(Self {
            pool,
            line_ids: Arc::default(),
        })
    }

    pub async fn save_positions(
        &self,
        seen_at_unix: i64,
        new: &[&Position],
        seen_again: &[i64],
    ) -> Result<Vec<i64>> {
        let mut tx = self.pool.begin().await?;
        for row_id in seen_again {
            sqlx::query(
                "UPDATE vehicle_positions SET seen_for_secs = ? - first_seen_unix WHERE id = ?",
            )
            .bind(seen_at_unix)
            .bind(row_id)
            .execute(&mut *tx)
            .await?;
        }
        let mut added_lines = HashMap::new();
        let mut new_ids = Vec::with_capacity(new.len());
        for position in new {
            let line_id = self.line_id(&mut tx, position, &mut added_lines).await?;
            let row_id = sqlx::query(
                "INSERT INTO vehicle_positions \
                 (line_id, course_id, latitude_e6, longitude_e6, first_seen_unix, seen_for_secs) \
                 VALUES (?, ?, ?, ?, ?, 0)",
            )
            .bind(line_id)
            .bind(position.course_id)
            .bind(position.latitude.0)
            .bind(position.longitude.0)
            .bind(seen_at_unix)
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();
            new_ids.push(row_id);
        }
        tx.commit().await?;
        self.lock_line_ids().extend(added_lines);
        Ok(new_ids)
    }

    async fn line_id(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        position: &Position,
        added_lines: &mut HashMap<LineKey, i64>,
    ) -> Result<i64> {
        let key = (position.vehicle_type, position.line.clone());
        let cached = self.lock_line_ids().get(&key).copied();
        if let Some(line_id) = cached.or_else(|| added_lines.get(&key).copied()) {
            return Ok(line_id);
        }
        sqlx::query("INSERT OR IGNORE INTO lines (vehicle_type, name) VALUES (?, ?)")
            .bind(position.vehicle_type.id())
            .bind(&position.line)
            .execute(&mut **tx)
            .await?;
        let (line_id,): (i64,) =
            sqlx::query_as("SELECT id FROM lines WHERE vehicle_type = ? AND name = ?")
                .bind(position.vehicle_type.id())
                .bind(&position.line)
                .fetch_one(&mut **tx)
                .await?;
        added_lines.insert(key, line_id);
        Ok(line_id)
    }

    fn lock_line_ids(&self) -> std::sync::MutexGuard<'_, HashMap<LineKey, i64>> {
        self.line_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub async fn save_weather(
        &self,
        fetched_unix: i64,
        model: &str,
        points: &[GridPoint],
    ) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let mut inserted = 0;
        for point in points {
            let current = &point.current;
            inserted += sqlx::query(
                "INSERT OR IGNORE INTO weather_observations \
                 (model, latitude_e6, longitude_e6, observed_unix, fetched_unix, \
                  temperature_2m, relative_humidity_2m, precipitation, rain, showers, snowfall, \
                  weather_code, cloud_cover, surface_pressure, wind_speed_10m, wind_direction_10m, wind_gusts_10m) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(model)
            .bind(point.latitude.0)
            .bind(point.longitude.0)
            .bind(current.observed_unix)
            .bind(fetched_unix)
            .bind(current.temperature_2m)
            .bind(current.relative_humidity_2m)
            .bind(current.precipitation)
            .bind(current.rain)
            .bind(current.showers)
            .bind(current.snowfall)
            .bind(current.weather_code)
            .bind(current.cloud_cover)
            .bind(current.surface_pressure)
            .bind(current.wind_speed_10m)
            .bind(current.wind_direction_10m)
            .bind(current.wind_gusts_10m)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
        Ok(inserted)
    }

    pub async fn positions_at(
        &self,
        filter: PositionFilter<'_>,
        at_unix: i64,
        max_age: Duration,
    ) -> Result<Vec<StoredPosition>> {
        let max_age_secs = duration_secs(max_age);
        let oldest_first_seen_unix = at_unix - duration_secs(MAX_ROW_SPAN) - max_age_secs;
        let sql = format!(
            "SELECT p.line_id, l.vehicle_type, l.name AS line, p.course_id, p.latitude_e6, p.longitude_e6, \
                    p.first_seen_unix, p.last_seen_unix \
             FROM ( \
                 SELECT *, first_seen_unix + seen_for_secs AS last_seen_unix, \
                        MAX(first_seen_unix) OVER (PARTITION BY line_id, course_id) AS latest_first_seen_unix \
                 FROM vehicle_positions \
                 WHERE {} \
                   AND first_seen_unix BETWEEN ?2 AND ?3 \
             ) p \
             JOIN lines l ON l.id = p.line_id \
             WHERE p.last_seen_unix >= p.latest_first_seen_unix AND p.last_seen_unix >= ?4 \
             ORDER BY l.vehicle_type, p.course_id",
            filter.condition()
        );
        let rows: Vec<PositionRow> = sqlx::query_as(&sql)
            .bind(filter.value())
            .bind(oldest_first_seen_unix)
            .bind(at_unix)
            .bind(at_unix - max_age_secs)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(StoredPosition::try_from).collect()
    }

    pub async fn path_points(
        &self,
        courses: &[(i64, i64)],
        from_unix: i64,
        to_unix: i64,
    ) -> Result<Vec<PathPoint>> {
        let line_ids: Vec<i64> = courses.iter().map(|(line_id, _)| *line_id).collect();
        let course_ids: Vec<i64> = courses.iter().map(|(_, course_id)| *course_id).collect();
        let rows: Vec<(i64, i64, i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT line_id, course_id, latitude_e6, longitude_e6, first_seen_unix, \
                    first_seen_unix + seen_for_secs \
             FROM vehicle_positions \
             WHERE line_id IN (SELECT value FROM json_each(?1)) \
               AND course_id IN (SELECT value FROM json_each(?2)) \
               AND first_seen_unix BETWEEN ?3 AND ?4 \
               AND first_seen_unix + seen_for_secs >= ?5 \
             ORDER BY line_id, course_id, first_seen_unix",
        )
        .bind(serde_json::to_string(&line_ids)?)
        .bind(serde_json::to_string(&course_ids)?)
        .bind(from_unix - duration_secs(MAX_ROW_SPAN))
        .bind(to_unix)
        .bind(from_unix)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    line_id,
                    course_id,
                    latitude_e6,
                    longitude_e6,
                    first_seen_unix,
                    last_seen_unix,
                )| {
                    PathPoint {
                        line_id,
                        course_id,
                        latitude: MicroDegrees(latitude_e6),
                        longitude: MicroDegrees(longitude_e6),
                        first_seen_unix,
                        last_seen_unix,
                    }
                },
            )
            .collect())
    }

    pub async fn positions_active_at(
        &self,
        line: &str,
        at_unix: i64,
        slack: Duration,
    ) -> Result<Vec<ActivePosition>> {
        let slack_secs = duration_secs(slack);
        let rows: Vec<(i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT line_id, course_id, latitude_e6, longitude_e6 \
             FROM vehicle_positions \
             WHERE line_id IN (SELECT id FROM lines WHERE name = ?1) \
               AND first_seen_unix BETWEEN ?2 AND ?3 \
               AND first_seen_unix + seen_for_secs >= ?4",
        )
        .bind(line)
        .bind(at_unix - duration_secs(MAX_ROW_SPAN) - slack_secs)
        .bind(at_unix + slack_secs)
        .bind(at_unix - slack_secs)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(line_id, course_id, latitude_e6, longitude_e6)| ActivePosition {
                    line_id,
                    course_id,
                    latitude: MicroDegrees(latitude_e6),
                    longitude: MicroDegrees(longitude_e6),
                },
            )
            .collect())
    }

    pub async fn save_course_vehicles(
        &self,
        vehicles: &[CourseVehicle],
    ) -> Result<SavedCourseVehicles> {
        let mut tx = self.pool.begin().await?;
        let mut saved = SavedCourseVehicles::default();
        let mut replaced_inferences = BTreeSet::new();
        for vehicle in vehicles {
            if let MatchSource::Matched = vehicle.source {
                let replaced =
                    sqlx::query("DELETE FROM course_vehicles WHERE course_id = ? AND source = ?")
                        .bind(vehicle.course_id)
                        .bind(MatchSource::Inferred.id())
                        .execute(&mut *tx)
                        .await?
                        .rows_affected();
                if replaced > 0 {
                    replaced_inferences.insert(vehicle.course_id);
                }
            }
            saved.inserted += sqlx::query(
                "INSERT OR IGNORE INTO course_vehicles \
                 (line_id, course_id, side_number, plate, brigade, observed_unix, distance_m, \
                  source, inferred_from_course_id) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(vehicle.line_id)
            .bind(vehicle.course_id)
            .bind(vehicle.side_number)
            .bind(&vehicle.plate)
            .bind(&vehicle.brigade)
            .bind(vehicle.observed_unix)
            .bind(vehicle.distance_m)
            .bind(vehicle.source.id())
            .bind(vehicle.inferred_from_course_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        for course_id in replaced_inferences {
            let (corrected, removed) = propagate_correction(&mut tx, course_id).await?;
            saved.corrected += corrected;
            saved.removed += removed;
        }
        tx.commit().await?;
        Ok(saved)
    }

    pub async fn fleet_vehicles(
        &self,
        course_ids: &[i64],
    ) -> Result<HashMap<i64, Vec<FleetVehicle>>> {
        let rows: Vec<FleetVehicleRow> = sqlx::query_as(
            "SELECT course_id, side_number, plate, brigade, source FROM course_vehicles \
             WHERE course_id IN (SELECT value FROM json_each(?)) \
             ORDER BY course_id, side_number",
        )
        .bind(serde_json::to_string(course_ids)?)
        .fetch_all(&self.pool)
        .await?;
        let mut vehicles: HashMap<i64, Vec<FleetVehicle>> = HashMap::new();
        for row in rows {
            vehicles
                .entry(row.course_id)
                .or_default()
                .push(FleetVehicle {
                    side_number: row.side_number,
                    plate: row.plate,
                    brigade: row.brigade,
                    source: row.source.try_into()?,
                });
        }
        Ok(vehicles)
    }

    pub async fn course_summaries(
        &self,
        from_unix: i64,
        to_unix: i64,
    ) -> Result<Vec<CourseSummary>> {
        let starts: Vec<(i64, i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT line_id, course_id, MIN(first_seen_unix), latitude_e6, longitude_e6 \
             FROM vehicle_positions \
             WHERE line_id IN (SELECT id FROM lines) AND first_seen_unix BETWEEN ?1 AND ?2 \
             GROUP BY line_id, course_id",
        )
        .bind(from_unix)
        .bind(to_unix)
        .fetch_all(&self.pool)
        .await?;
        let ends: Vec<(i64, i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT line_id, course_id, MAX(first_seen_unix + seen_for_secs), latitude_e6, longitude_e6 \
             FROM vehicle_positions \
             WHERE line_id IN (SELECT id FROM lines) AND first_seen_unix BETWEEN ?1 AND ?2 \
             GROUP BY line_id, course_id",
        )
        .bind(from_unix)
        .bind(to_unix)
        .fetch_all(&self.pool)
        .await?;
        let ends: HashMap<(i64, i64), (i64, i64, i64)> = ends
            .into_iter()
            .map(|(line_id, course_id, last, latitude, longitude)| {
                ((line_id, course_id), (last, latitude, longitude))
            })
            .collect();
        let complete_start_unix = from_unix + duration_secs(MAX_ROW_SPAN);
        Ok(starts
            .into_iter()
            .filter_map(|(line_id, course_id, first, latitude, longitude)| {
                let (last, end_latitude, end_longitude) = *ends.get(&(line_id, course_id))?;
                Some(CourseSummary {
                    line_id,
                    course_id,
                    first_seen_unix: first,
                    last_seen_unix: last,
                    start: (MicroDegrees(latitude), MicroDegrees(longitude)),
                    end: (MicroDegrees(end_latitude), MicroDegrees(end_longitude)),
                    start_is_complete: first >= complete_start_unix,
                })
            })
            .collect())
    }

    pub async fn course_vehicles(&self, course_ids: &[i64]) -> Result<Vec<CourseVehicle>> {
        let rows: Vec<CourseVehicleRow> = sqlx::query_as(
            "SELECT line_id, course_id, side_number, plate, brigade, observed_unix, distance_m, \
                    source, inferred_from_course_id \
             FROM course_vehicles \
             WHERE course_id IN (SELECT value FROM json_each(?)) AND line_id IS NOT NULL",
        )
        .bind(serde_json::to_string(course_ids)?)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(CourseVehicle::try_from).collect()
    }

    pub async fn bounding_box(
        &self,
        from_unix: Option<i64>,
        to_unix: Option<i64>,
    ) -> Result<Option<BoundingBox>> {
        let (positions, south, north, west, east): (
            i64,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT COUNT(*), MIN(latitude_e6), MAX(latitude_e6), MIN(longitude_e6), MAX(longitude_e6) \
             FROM vehicle_positions \
             WHERE (?1 IS NULL OR first_seen_unix + seen_for_secs >= ?1) \
               AND (?2 IS NULL OR first_seen_unix <= ?2)",
        )
        .bind(from_unix)
        .bind(to_unix)
        .fetch_one(&self.pool)
        .await?;
        let (Some(south), Some(north), Some(west), Some(east)) = (south, north, west, east) else {
            return Ok(None);
        };
        Ok(Some(BoundingBox {
            positions,
            south: MicroDegrees(south),
            north: MicroDegrees(north),
            west: MicroDegrees(west),
            east: MicroDegrees(east),
        }))
    }

    pub async fn backup_to(&self, target: &Path) -> Result<()> {
        let target_str = target
            .to_str()
            .with_context(|| format!("backup path {} is not valid UTF-8", target.display()))?;
        sqlx::query("VACUUM INTO ?")
            .bind(target_str)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to back up into {}", target.display()))?;
        Ok(())
    }

    pub async fn close(self) {
        self.pool.close().await;
    }
}

async fn propagate_correction(conn: &mut SqliteConnection, course_id: i64) -> Result<(u64, u64)> {
    let mut corrected = 0;
    let mut removed = 0;
    let identities: Vec<VehicleIdentity> = sqlx::query_as(
        "SELECT side_number, plate, brigade FROM course_vehicles WHERE course_id = ?",
    )
    .bind(course_id)
    .fetch_all(&mut *conn)
    .await?;
    let [identity] = identities.as_slice() else {
        for (child, _) in inferred_children(conn, course_id).await? {
            removed += delete_inferred_subtree(conn, child).await?;
        }
        return Ok((corrected, removed));
    };
    let mut queue = VecDeque::from([course_id]);
    while let Some(parent) = queue.pop_front() {
        for (child, child_identity) in inferred_children(conn, parent).await? {
            if child_identity == *identity {
                continue;
            }
            if contradicts_neighbour(conn, child, parent, identity.side_number).await? {
                removed += delete_inferred_subtree(conn, child).await?;
                continue;
            }
            sqlx::query(
                "UPDATE course_vehicles SET side_number = ?, plate = ?, brigade = ? \
                 WHERE course_id = ? AND source = ?",
            )
            .bind(identity.side_number)
            .bind(&identity.plate)
            .bind(&identity.brigade)
            .bind(child)
            .bind(MatchSource::Inferred.id())
            .execute(&mut *conn)
            .await?;
            corrected += 1;
            queue.push_back(child);
        }
    }
    Ok((corrected, removed))
}

async fn inferred_children(
    conn: &mut SqliteConnection,
    parent: i64,
) -> Result<Vec<(i64, VehicleIdentity)>> {
    let rows: Vec<(i64, i64, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT course_id, side_number, plate, brigade FROM course_vehicles \
         WHERE source = ? AND inferred_from_course_id = ?",
    )
    .bind(MatchSource::Inferred.id())
    .bind(parent)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(course_id, side_number, plate, brigade)| {
            (
                course_id,
                VehicleIdentity {
                    side_number,
                    plate,
                    brigade,
                },
            )
        })
        .collect())
}

async fn contradicts_neighbour(
    conn: &mut SqliteConnection,
    course_id: i64,
    parent: i64,
    side_number: i64,
) -> Result<bool> {
    let (contradictions,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM course_vehicles \
         WHERE course_id IN (?1 - 1, ?1 + 1) AND course_id != ?2 \
           AND source = ?3 AND side_number != ?4",
    )
    .bind(course_id)
    .bind(parent)
    .bind(MatchSource::Matched.id())
    .bind(side_number)
    .fetch_one(&mut *conn)
    .await?;
    Ok(contradictions > 0)
}

async fn delete_inferred_subtree(conn: &mut SqliteConnection, course_id: i64) -> Result<u64> {
    Ok(sqlx::query(
        "WITH RECURSIVE subtree(course_id) AS ( \
             SELECT ?1 \
             UNION \
             SELECT cv.course_id FROM course_vehicles cv \
             JOIN subtree s ON cv.inferred_from_course_id = s.course_id \
             WHERE cv.source = ?2 \
         ) \
         DELETE FROM course_vehicles \
         WHERE source = ?2 AND course_id IN (SELECT course_id FROM subtree)",
    )
    .bind(course_id)
    .bind(MatchSource::Inferred.id())
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

pub fn duration_secs(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IN_MEMORY: &str = ":memory:";
    const MAX_AGE: Duration = Duration::from_secs(30);

    fn position(vehicle_type: VehicleType, line: &str, course_id: i64) -> Position {
        Position {
            vehicle_type,
            line: line.to_owned(),
            course_id,
            latitude: MicroDegrees(51_100_000 + course_id),
            longitude: MicroDegrees(17_000_000 + course_id),
        }
    }

    async fn open() -> Db {
        Db::open(Path::new(IN_MEMORY), OpenMode::CreateIfMissing)
            .await
            .unwrap()
    }

    async fn line_id(db: &Db, name: &str) -> i64 {
        let (id,): (i64,) = sqlx::query_as("SELECT id FROM lines WHERE name = ?")
            .bind(name)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        id
    }

    fn course_vehicle(line_id: i64, course_id: i64, side_number: i64) -> CourseVehicle {
        CourseVehicle {
            line_id,
            course_id,
            side_number,
            plate: None,
            brigade: None,
            observed_unix: 100,
            distance_m: 0.0,
            source: MatchSource::Matched,
            inferred_from_course_id: None,
        }
    }

    async fn line_count(db: &Db) -> i64 {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lines")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        count
    }

    fn summary(positions: &[StoredPosition]) -> Vec<(VehicleType, &str, i64, i64, i64)> {
        positions
            .iter()
            .map(|p| {
                (
                    p.vehicle_type,
                    p.line.as_str(),
                    p.course_id,
                    p.first_seen_unix,
                    p.last_seen_unix,
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn stores_positions_with_shared_line_ids() {
        let db = open().await;
        let bus = position(VehicleType::Bus, "a", 1);
        let tram = position(VehicleType::Tram, "1", 2);
        let ids = db.save_positions(100, &[&bus, &tram], &[]).await.unwrap();

        let another_bus = position(VehicleType::Bus, "a", 3);
        db.save_positions(105, &[&another_bus], &[ids[0]])
            .await
            .unwrap();

        let buses = db
            .positions_at(PositionFilter::Line("a"), 105, MAX_AGE)
            .await
            .unwrap();
        assert_eq!(
            summary(&buses),
            [
                (VehicleType::Bus, "a", 1, 100, 105),
                (VehicleType::Bus, "a", 3, 105, 105),
            ]
        );
        let trams = db
            .positions_at(PositionFilter::Line("1"), 105, MAX_AGE)
            .await
            .unwrap();
        assert_eq!(summary(&trams), [(VehicleType::Tram, "1", 2, 100, 100)]);
        assert_eq!(line_count(&db).await, 2);
    }

    #[tokio::test]
    async fn finds_positions_active_at_a_time() {
        let db = open().await;
        let bus = position(VehicleType::Bus, "a", 1);
        let ids = db.save_positions(100, &[&bus], &[]).await.unwrap();
        db.save_positions(160, &[], &ids).await.unwrap();
        let slack = Duration::from_secs(30);

        let at = |t| {
            let db = db.clone();
            async move { db.positions_active_at("a", t, slack).await.unwrap() }
        };
        let expected = ActivePosition {
            line_id: line_id(&db, "a").await,
            course_id: 1,
            latitude: bus.latitude,
            longitude: bus.longitude,
        };
        assert_eq!(at(130).await, [expected]);
        assert_eq!(at(71).await, [expected]);
        assert_eq!(at(189).await, [expected]);
        assert!(at(69).await.is_empty());
        assert!(at(191).await.is_empty());
    }

    #[tokio::test]
    async fn stores_fleet_vehicles_per_course() {
        let db = open().await;
        let bus = position(VehicleType::Bus, "a", 1);
        db.save_positions(100, &[&bus], &[]).await.unwrap();
        let line = line_id(&db, "a").await;
        let vehicle = |course_id, side_number| course_vehicle(line, course_id, side_number);
        let with_plate = |course_id, side_number, plate: &str| CourseVehicle {
            plate: Some(plate.to_owned()),
            ..vehicle(course_id, side_number)
        };
        let saved = db
            .save_course_vehicles(&[
                vehicle(1, 3109),
                vehicle(1, 3110),
                CourseVehicle {
                    brigade: Some("12203".to_owned()),
                    ..with_plate(2, 8335, "DW916KF")
                },
            ])
            .await
            .unwrap();
        assert_eq!(saved.inserted, 3);
        assert_eq!(
            db.save_course_vehicles(&[vehicle(1, 3109)])
                .await
                .unwrap()
                .inserted,
            0
        );

        let fleet = |side_number, plate: Option<&str>| FleetVehicle {
            side_number,
            plate: plate.map(str::to_owned),
            brigade: None,
            source: MatchSource::Matched,
        };
        let vehicles = db.fleet_vehicles(&[1, 2, 3]).await.unwrap();
        assert_eq!(
            vehicles.get(&1),
            Some(&vec![fleet(3109, None), fleet(3110, None)])
        );
        assert_eq!(
            vehicles.get(&2),
            Some(&vec![FleetVehicle {
                brigade: Some("12203".to_owned()),
                ..fleet(8335, Some("DW916KF"))
            }])
        );
        assert_eq!(vehicles.get(&3), None);
    }

    #[tokio::test]
    async fn finds_vehicle_by_plate() {
        let db = open().await;
        let first = position(VehicleType::Bus, "a", 1);
        let second = position(VehicleType::Bus, "a", 2);
        let tram = position(VehicleType::Tram, "1", 3);
        db.save_positions(100, &[&first, &second, &tram], &[])
            .await
            .unwrap();
        let with_plate = |line_id, course_id, side_number, plate: &str| CourseVehicle {
            plate: Some(plate.to_owned()),
            ..course_vehicle(line_id, course_id, side_number)
        };
        let bus_line = line_id(&db, "a").await;
        let tram_line = line_id(&db, "1").await;
        db.save_course_vehicles(&[
            with_plate(bus_line, 1, 8335, "DW916KF"),
            course_vehicle(bus_line, 2, 8336),
            with_plate(tram_line, 3, 3109, "DW111AA"),
        ])
        .await
        .unwrap();

        let found = db
            .positions_at(PositionFilter::Plate("DW916KF"), 100, MAX_AGE)
            .await
            .unwrap();
        assert_eq!(summary(&found), [(VehicleType::Bus, "a", 1, 100, 100)]);
        let unknown = db
            .positions_at(PositionFilter::Plate("DW000XX"), 100, MAX_AGE)
            .await
            .unwrap();
        assert!(unknown.is_empty());
    }

    #[tokio::test]
    async fn direct_match_replaces_inferred_vehicle() {
        let db = open().await;
        let bus = position(VehicleType::Bus, "a", 1);
        db.save_positions(100, &[&bus], &[]).await.unwrap();
        let line = line_id(&db, "a").await;
        let inferred = CourseVehicle {
            source: MatchSource::Inferred,
            inferred_from_course_id: Some(2),
            ..course_vehicle(line, 1, 7349)
        };
        db.save_course_vehicles(std::slice::from_ref(&inferred))
            .await
            .unwrap();
        assert_eq!(db.course_vehicles(&[1]).await.unwrap(), [inferred]);

        let matched = course_vehicle(line, 1, 7350);
        db.save_course_vehicles(std::slice::from_ref(&matched))
            .await
            .unwrap();
        assert_eq!(db.course_vehicles(&[1]).await.unwrap(), [matched]);
    }

    fn inferred_vehicle(
        line_id: i64,
        course_id: i64,
        side_number: i64,
        from: i64,
    ) -> CourseVehicle {
        CourseVehicle {
            source: MatchSource::Inferred,
            inferred_from_course_id: Some(from),
            ..course_vehicle(line_id, course_id, side_number)
        }
    }

    async fn chain_state(db: &Db, course_ids: &[i64]) -> Vec<(i64, i64, MatchSource, Option<i64>)> {
        let mut rows: Vec<_> = db
            .course_vehicles(course_ids)
            .await
            .unwrap()
            .into_iter()
            .map(|v| {
                (
                    v.course_id,
                    v.side_number,
                    v.source,
                    v.inferred_from_course_id,
                )
            })
            .collect();
        rows.sort_by_key(|row| (row.0, row.1));
        rows
    }

    async fn db_with_line() -> (Db, i64) {
        let db = open().await;
        let bus = position(VehicleType::Bus, "a", 1);
        db.save_positions(100, &[&bus], &[]).await.unwrap();
        let line = line_id(&db, "a").await;
        (db, line)
    }

    #[tokio::test]
    async fn corrects_chain_when_direct_match_differs() {
        let (db, line) = db_with_line().await;
        db.save_course_vehicles(&[
            course_vehicle(line, 10, 1000),
            inferred_vehicle(line, 11, 1000, 10),
            inferred_vehicle(line, 12, 1000, 11),
            inferred_vehicle(line, 13, 1000, 12),
        ])
        .await
        .unwrap();

        let saved = db
            .save_course_vehicles(&[course_vehicle(line, 11, 2000)])
            .await
            .unwrap();
        assert_eq!(
            saved,
            SavedCourseVehicles {
                inserted: 1,
                corrected: 2,
                removed: 0
            }
        );
        use MatchSource::{Inferred, Matched};
        assert_eq!(
            chain_state(&db, &[10, 11, 12, 13]).await,
            [
                (10, 1000, Matched, None),
                (11, 2000, Matched, None),
                (12, 2000, Inferred, Some(11)),
                (13, 2000, Inferred, Some(12)),
            ]
        );
    }

    #[tokio::test]
    async fn keeps_chain_when_direct_match_confirms_it() {
        let (db, line) = db_with_line().await;
        db.save_course_vehicles(&[
            course_vehicle(line, 10, 1000),
            inferred_vehicle(line, 11, 1000, 10),
            inferred_vehicle(line, 12, 1000, 11),
        ])
        .await
        .unwrap();

        let saved = db
            .save_course_vehicles(&[course_vehicle(line, 11, 1000)])
            .await
            .unwrap();
        assert_eq!((saved.corrected, saved.removed), (0, 0));
        assert_eq!(
            chain_state(&db, &[12]).await,
            [(12, 1000, MatchSource::Inferred, Some(11))]
        );
    }

    #[tokio::test]
    async fn removes_inference_that_would_contradict_a_neighbour() {
        let (db, line) = db_with_line().await;
        db.save_course_vehicles(&[
            course_vehicle(line, 10, 1000),
            inferred_vehicle(line, 11, 1000, 10),
            inferred_vehicle(line, 12, 1000, 11),
            course_vehicle(line, 13, 1000),
        ])
        .await
        .unwrap();

        let saved = db
            .save_course_vehicles(&[course_vehicle(line, 11, 2000)])
            .await
            .unwrap();
        assert_eq!((saved.corrected, saved.removed), (0, 1));
        assert!(chain_state(&db, &[12]).await.is_empty());
    }

    #[tokio::test]
    async fn removes_chain_below_a_course_with_two_vehicles() {
        let (db, line) = db_with_line().await;
        db.save_course_vehicles(&[
            course_vehicle(line, 10, 1000),
            inferred_vehicle(line, 11, 1000, 10),
            inferred_vehicle(line, 12, 1000, 11),
            inferred_vehicle(line, 13, 1000, 12),
        ])
        .await
        .unwrap();

        let saved = db
            .save_course_vehicles(&[
                course_vehicle(line, 11, 2000),
                course_vehicle(line, 11, 3000),
            ])
            .await
            .unwrap();
        assert_eq!((saved.corrected, saved.removed), (0, 2));
        assert!(chain_state(&db, &[12, 13]).await.is_empty());
    }

    #[tokio::test]
    async fn summarizes_courses_in_a_time_window() {
        let db = open().await;
        let start = position(VehicleType::Bus, "a", 1);
        let ids = db.save_positions(1_000, &[&start], &[]).await.unwrap();
        db.save_positions(1_100, &[], &ids).await.unwrap();
        let end = Position {
            latitude: MicroDegrees(51_200_000),
            ..position(VehicleType::Bus, "a", 1)
        };
        db.save_positions(1_200, &[&end], &[]).await.unwrap();

        let span = duration_secs(MAX_ROW_SPAN);
        let summaries = db.course_summaries(1_000 - span, 2_000).await.unwrap();
        assert_eq!(
            summaries,
            [CourseSummary {
                line_id: line_id(&db, "a").await,
                course_id: 1,
                first_seen_unix: 1_000,
                last_seen_unix: 1_200,
                start: (start.latitude, start.longitude),
                end: (end.latitude, end.longitude),
                start_is_complete: true,
            }]
        );
        let clipped = db.course_summaries(1_000 - span + 1, 2_000).await.unwrap();
        assert!(!clipped[0].start_is_complete);
    }

    #[tokio::test]
    async fn keeps_same_line_name_of_different_types_apart() {
        let db = open().await;
        let bus = position(VehicleType::Bus, "x", 1);
        let train = position(VehicleType::Train, "x", 2);
        db.save_positions(100, &[&bus, &train], &[]).await.unwrap();

        let positions = db
            .positions_at(PositionFilter::Line("x"), 100, MAX_AGE)
            .await
            .unwrap();
        assert_eq!(
            summary(&positions),
            [
                (VehicleType::Bus, "x", 1, 100, 100),
                (VehicleType::Train, "x", 2, 100, 100),
            ]
        );
        assert_eq!(line_count(&db).await, 2);
    }
}

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Sqlite, SqlitePool, Transaction};

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
    pub vehicle_type: VehicleType,
    pub line: String,
    pub course_id: i64,
    pub latitude: MicroDegrees,
    pub longitude: MicroDegrees,
    pub first_seen_unix: i64,
    pub last_seen_unix: i64,
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
        line: &str,
        at_unix: i64,
        max_age: Duration,
    ) -> Result<Vec<StoredPosition>> {
        let max_age_secs = duration_secs(max_age);
        let oldest_first_seen_unix = at_unix - duration_secs(MAX_ROW_SPAN) - max_age_secs;
        let rows: Vec<PositionRow> = sqlx::query_as(
            "SELECT l.vehicle_type, l.name AS line, p.course_id, p.latitude_e6, p.longitude_e6, \
                    p.first_seen_unix, p.last_seen_unix \
             FROM ( \
                 SELECT *, first_seen_unix + seen_for_secs AS last_seen_unix, \
                        MAX(first_seen_unix) OVER (PARTITION BY line_id, course_id) AS latest_first_seen_unix \
                 FROM vehicle_positions \
                 WHERE line_id IN (SELECT id FROM lines WHERE name = ?1) \
                   AND first_seen_unix BETWEEN ?2 AND ?3 \
             ) p \
             JOIN lines l ON l.id = p.line_id \
             WHERE p.last_seen_unix >= p.latest_first_seen_unix AND p.last_seen_unix >= ?4 \
             ORDER BY l.vehicle_type, p.course_id",
        )
        .bind(line)
        .bind(oldest_first_seen_unix)
        .bind(at_unix)
        .bind(at_unix - max_age_secs)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(StoredPosition::try_from).collect()
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

        let buses = db.positions_at("a", 105, MAX_AGE).await.unwrap();
        assert_eq!(
            summary(&buses),
            [
                (VehicleType::Bus, "a", 1, 100, 105),
                (VehicleType::Bus, "a", 3, 105, 105),
            ]
        );
        let trams = db.positions_at("1", 105, MAX_AGE).await.unwrap();
        assert_eq!(summary(&trams), [(VehicleType::Tram, "1", 2, 100, 100)]);
        assert_eq!(line_count(&db).await, 2);
    }

    #[tokio::test]
    async fn keeps_same_line_name_of_different_types_apart() {
        let db = open().await;
        let bus = position(VehicleType::Bus, "x", 1);
        let train = position(VehicleType::Train, "x", 2);
        db.save_positions(100, &[&bus, &train], &[]).await.unwrap();

        let positions = db.positions_at("x", 100, MAX_AGE).await.unwrap();
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

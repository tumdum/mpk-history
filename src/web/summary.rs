use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use maud::{Markup, html};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

use super::i18n::{Text, Translation};
use super::selection::{self, Subject};
use super::{Clock, Range, range_table, updated};
use crate::db::Db;
use crate::stats::{self, Detail, Group, Kind, Retention, StatsCache, Totals};

const REFRESH_INTERVAL: Duration = Duration::from_mins(15);

pub(super) struct Table {
    pub at_unix: i64,
    pub rows: Vec<Vec<(Range, Totals)>>,
}

pub(super) fn section(
    tr: &dyn Translation,
    subjects: &[Subject],
    table: Option<&Table>,
    tz: &TimeZone,
) -> Result<Markup> {
    let rows = table.map(|table| table.rows.as_slice()).unwrap_or_default();
    let shown = selection::shown(subjects, rows, |rows: &Vec<(Range, Totals)>| {
        rows.iter().any(|(_, totals)| totals.has_vehicles())
    });
    Ok(html! {
        section #summary {
            h2 { (tr.text(Text::Summary)) }
            @if let Some(table) = table {
                p #summary-updated {
                    (tr.text(Text::AsOf))
                    (updated(Timestamp::from_second(table.at_unix)?, tz))
                }
            } @else {
                p { (tr.text(Text::PreparingStatistics)) }
            }
            @for (subject, rows) in &shown {
                (range_table(tr, subject, rows))
            }
        }
    })
}

struct Snapshot {
    at_unix: i64,
    rows: Vec<(Group, Vec<(Range, Totals)>)>,
}

impl Snapshot {
    fn table(&self, groups: &[Group]) -> Table {
        Table {
            at_unix: self.at_unix,
            rows: groups
                .iter()
                .map(|group| {
                    self.rows
                        .iter()
                        .find(|(stored, _)| stored == group)
                        .map(|(_, rows)| rows.clone())
                        .unwrap_or_default()
                })
                .collect(),
        }
    }
}

#[derive(Clone)]
pub(super) struct Cache {
    snapshot: watch::Receiver<Option<Arc<Snapshot>>>,
}

impl Cache {
    pub fn new(db: Db, clock: Clock) -> Self {
        let (updates, snapshot) = watch::channel(None);
        tokio::spawn(refresh(db, clock, updates));
        Self { snapshot }
    }

    pub fn table(&self, groups: &[Group]) -> Option<Table> {
        self.snapshot
            .borrow()
            .as_ref()
            .map(|snapshot| snapshot.table(groups))
    }

    #[cfg(test)]
    pub async fn wait_until_ready(&self) {
        self.snapshot
            .clone()
            .wait_for(Option::is_some)
            .await
            .unwrap();
    }
}

async fn refresh(db: Db, clock: Clock, updates: watch::Sender<Option<Arc<Snapshot>>>) {
    let stats = StatsCache::new(
        stats::BUCKET,
        Retention::Last(Range::LONGEST.duration()),
        Detail::Kinds,
    );
    let mut interval = tokio::time::interval(REFRESH_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = updates.closed() => return,
            _ = interval.tick() => {}
        }
        let result = tokio::select! {
            () = updates.closed() => return,
            result = compute(&db, &stats, clock.now().as_second()) => result,
        };
        match result {
            Ok(snapshot) => {
                updates.send_replace(Some(Arc::new(snapshot)));
                info!("overview summary ready");
            }
            Err(error) => warn!("failed to refresh overview summary: {error:#}"),
        }
    }
}

async fn compute(db: &Db, stats: &StatsCache, now_unix: i64) -> Result<Snapshot> {
    let groups = Kind::ALL.map(Group::Kind);
    let (from_unix, to_unix) = Range::LONGEST.window(now_unix);
    let series = stats
        .totals(db, from_unix, to_unix, now_unix, &groups)
        .await?;
    let rows = groups
        .into_iter()
        .zip(series)
        .map(|(group, series)| {
            let totals = Range::ALL
                .into_iter()
                .map(|range| {
                    let (from_unix, _) = range.window(now_unix);
                    let total =
                        stats::sum(series.iter().filter(|timed| timed.start_unix >= from_unix));
                    (range, total)
                })
                .collect();
            (group, totals)
        })
        .collect();
    Ok(Snapshot {
        at_unix: now_unix,
        rows,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::db::OpenMode;
    use crate::geo::MicroDegrees;
    use crate::mpk::{Position, VehicleType};

    const NOW: i64 = 1_800_000_000;
    const LINE: &str = "a";
    const BEFORE_REFRESH: Duration = Duration::from_mins(14);
    const AFTER_REFRESH: Duration = Duration::from_mins(2);
    const RECENT_AGE: Duration = Duration::from_mins(1);

    async fn database() -> Db {
        Db::open(Path::new(":memory:"), OpenMode::CreateIfMissing)
            .await
            .unwrap()
    }

    fn clock() -> Clock {
        Clock::Fixed(Timestamp::from_second(NOW).unwrap())
    }

    async fn record(db: &Db, age: Duration, vehicle_type: VehicleType, courses: &[i64]) {
        let positions: Vec<_> = courses
            .iter()
            .map(|course_id| Position {
                vehicle_type,
                line: LINE.to_owned(),
                course_id: *course_id,
                latitude: MicroDegrees(51_100_000),
                longitude: MicroDegrees(17_000_000),
            })
            .collect();
        db.save_positions(
            NOW - crate::db::duration_secs(age),
            &positions.iter().collect::<Vec<_>>(),
            &[],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn starts_immediately_and_precomputes_every_kind_and_range() {
        let db = database().await;
        for (age, vehicle_type, courses) in [
            (Duration::from_mins(15), VehicleType::Bus, &[1][..]),
            (Duration::from_hours(2), VehicleType::Tram, &[2, 3]),
            (Duration::from_hours(48), VehicleType::Train, &[4, 5, 6]),
            (
                Range::Week.duration() + Range::Day.duration(),
                VehicleType::Bus,
                &[7, 8, 9, 10],
            ),
            (
                Range::Month.duration() + Range::Day.duration(),
                VehicleType::Bus,
                &[11, 12, 13, 14, 15],
            ),
        ] {
            record(&db, age, vehicle_type, courses).await;
        }
        let cache = Cache::new(db.clone(), clock());
        let groups = Kind::ALL.map(Group::Kind);
        assert!(cache.table(&groups).is_none());
        cache.wait_until_ready().await;
        db.close().await;
        let table = cache.table(&groups).unwrap();
        assert_eq!(table.at_unix, NOW);
        let expected = [[1, 2, 3, 4], [1, 1, 1, 4], [0, 2, 2, 2], [0, 0, 3, 3]];
        for (rows, peaks) in table.rows.iter().zip(expected) {
            assert_eq!(
                rows.iter().map(|(range, _)| *range).collect::<Vec<_>>(),
                Range::ALL
            );
            assert_eq!(
                rows.iter()
                    .map(|(_, total)| total.peak_vehicles().unwrap_or_default())
                    .collect::<Vec<_>>(),
                peaks
            );
        }
    }

    #[tokio::test]
    async fn publishes_a_new_snapshot_only_after_the_refresh_interval() {
        let db = database().await;
        record(&db, RECENT_AGE, VehicleType::Bus, &[1]).await;
        let cache = Cache::new(db.clone(), clock());
        cache.wait_until_ready().await;
        let first = cache.snapshot.borrow().clone().unwrap();
        record(&db, RECENT_AGE, VehicleType::Bus, &[2]).await;

        tokio::time::pause();
        tokio::time::advance(BEFORE_REFRESH).await;
        assert!(Arc::ptr_eq(
            cache.snapshot.borrow().as_ref().unwrap(),
            &first
        ));
        tokio::time::advance(AFTER_REFRESH).await;
        let mut receiver = cache.snapshot.clone();
        receiver
            .wait_for(|snapshot| {
                snapshot
                    .as_ref()
                    .is_some_and(|snapshot| !Arc::ptr_eq(snapshot, &first))
            })
            .await
            .unwrap();
        tokio::time::resume();

        let table = cache.table(&[Group::Kind(Kind::All)]).unwrap();
        assert!(
            table.rows[0]
                .iter()
                .all(|(_, total)| total.peak_vehicles() == Some(2))
        );
        assert!(
            first.rows[0]
                .1
                .iter()
                .all(|(_, total)| total.peak_vehicles() == Some(1))
        );

        db.close().await;
        tokio::time::pause();
        tokio::time::advance(REFRESH_INTERVAL).await;
        tokio::task::yield_now().await;
        tokio::time::resume();
        assert_eq!(
            cache.table(&[Group::Kind(Kind::All)]).unwrap().rows,
            table.rows
        );
    }

    #[tokio::test]
    async fn stops_when_the_last_reader_is_dropped() {
        let db = database().await;
        let (updates, snapshot) = watch::channel(None);
        let worker = tokio::spawn(refresh(db, clock(), updates));
        drop(snapshot);
        worker.await.unwrap();
    }
}

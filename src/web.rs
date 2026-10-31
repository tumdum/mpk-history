use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{RoundMode, Timestamp, Unit, ZonedRound};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::cells::{CellCache, CellSpeeds};
use crate::charts::{self, Labels, LineChart, Point, RGBColor, Rendered, Series, Stroke, TickStep};
use crate::db::{self, Db};
use crate::mpk::VehicleType;
use crate::stats::{self, Detail, Group, Kind, Retention, StatsCache, TimedTotals, Totals};

mod about;
mod caching;
mod i18n;
mod map;
mod page_cache;
mod period;
mod quality;
mod selection;
mod speed_map;
mod summary;

use about::ABOUT_PATH;
use caching::{Caching, IfNoneMatch, Revision};
use i18n::{ChartTime, Language, Neighbour, Relative, Text, Translation, capitalized};
pub use map::MapTiles;
use map::{BASEMAP_PATH, MAP_PATH, SharedBasemap};
pub use page_cache::DEFAULT_PAGE_CACHE_CAPACITY;
use page_cache::PageCache;
use period::{Period, PeriodKind, RecentPeriod};
use quality::{OVERVIEW_QUALITY_PATH, QUALITY_SEGMENT};
use selection::{Page, Selection, SelectionKey, Subject};

const OVERVIEW_PATH: &str = "/";
const CHARTS_PATH: &str = "/charts/{file}";
const SVG_EXTENSION: &str = ".svg";
const SVG_CONTENT_TYPE: &str = "image/svg+xml";
const MISSING_VALUE: &str = "–";
const UPDATED_FORMAT: &str = "%Y-%m-%d %H:%M";
const INTERNAL_ERROR: &str = "internal error";
const CHART_SCRIPT: &str = include_str!("chart_hover.js");
const MAP_SCRIPT: &str = include_str!("map_hover.js");
const SPEED_MAP_SCRIPT: &str = include_str!("speed_map_hover.js");
const GTAG_ID: &str = "G-ZCQG7QP403";
const GTAG_SRC: &str = "https://www.googletagmanager.com/gtag/js";
const GTAG_SCRIPT: &str = "
  window.dataLayer = window.dataLayer || [];
  function gtag(){dataLayer.push(arguments);}
  gtag('js', new Date());
";
const SCRIPT_END_TAG_START: &str = "</";
const ESCAPED_SCRIPT_END_TAG_START: &str = "<\\/";
const HISTORY_RESOLUTION: Duration = Duration::from_mins(15);
const SECS_PER_MINUTE: i64 = 60;
const SECS_PER_HOUR: i64 = 60 * SECS_PER_MINUTE;
const SECS_PER_DAY: i64 = 24 * SECS_PER_HOUR;
const STYLE: &str = r"
:root {
  color-scheme: light;
  --surface: #fcfcfb;
  --text-primary: #0b0b0b;
  --text-secondary: #52514e;
  --border: #e1e0d9;
  --speed-cell-opacity: 0.55;
}
body {
  margin: 0 auto;
  max-width: 1000px;
  padding: 16px;
  background: var(--surface);
  color: var(--text-primary);
  font-family: system-ui, sans-serif;
}
h1 { font-size: 1.5rem; margin: 0 0 4px; }
h2 { font-size: 1.2rem; margin: 24px 0 12px; }
h3 { font-size: 1rem; margin: 16px 0 8px; }
#updated { color: var(--text-secondary); margin: 0; }
.table-wrap { overflow-x: auto; margin-bottom: 16px; }
.stats { border-collapse: collapse; width: 100%; font-variant-numeric: tabular-nums; }
.stats caption { text-align: left; font-weight: 600; padding-bottom: 8px; }
.stats th, .stats td { padding: 6px 8px; border-bottom: 1px solid var(--border); white-space: nowrap; }
.stats td, .stats thead th { text-align: right; }
.stats thead th:first-child { text-align: left; }
.stats tbody th { text-align: left; font-weight: normal; }
nav { display: flex; flex-wrap: wrap; gap: 8px; margin-bottom: 16px; }
nav a {
  padding: 4px 12px;
  border: 1px solid var(--border);
  border-radius: 16px;
  color: var(--text-primary);
  text-decoration: none;
}
nav a[aria-current=page] { background: var(--text-primary); border-color: var(--text-primary); color: var(--surface); }
figure { margin: 0 0 24px; }
.chart { position: relative; }
.chart-plot svg {
  display: block;
  width: 100%;
  height: auto;
  border: 1px solid var(--border);
  border-radius: 8px;
  touch-action: pan-y;
}
.chart-plot svg:focus-visible { outline: 2px solid var(--text-primary); outline-offset: 2px; }
.chart-crosshair { stroke: #c3c2b7; stroke-width: 1; }
.chart-dot { stroke: var(--surface); stroke-width: 2; }
.chart-tooltip, .map-tooltip {
  position: absolute;
  pointer-events: none;
  min-width: 180px;
  padding: 8px 10px;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  box-shadow: 0 2px 8px rgba(11, 11, 11, 0.08);
  font-size: 0.875rem;
}
.chart-tooltip-time { color: var(--text-secondary); margin-bottom: 4px; }
.chart-tooltip-row { display: grid; grid-template-columns: 12px 1fr auto; gap: 8px; align-items: center; }
.chart-tooltip-key { height: 2px; border-radius: 1px; }
.chart-tooltip-value { font-weight: 600; font-variant-numeric: tabular-nums; text-align: right; }
figcaption { color: var(--text-secondary); margin-top: 4px; }
.map { position: relative; }
.map-plot svg {
  display: block;
  width: 100%;
  height: auto;
  border: 1px solid var(--border);
  border-radius: 8px;
  touch-action: pan-y;
}
.map-highlight { fill: none; stroke: var(--text-primary); stroke-width: 2; }
.map-legend { display: flex; flex-wrap: wrap; gap: 16px; list-style: none; margin: 0; padding: 0; }
.map-legend li { display: flex; align-items: center; gap: 6px; }
.map-count { font-weight: 600; font-variant-numeric: tabular-nums; }
.map-empty, .map-attribution { margin: 0; }
.map-attribution { font-size: 0.75rem; margin-top: 4px; }
.map-attribution a { color: var(--text-secondary); }
.speed-map { position: relative; }
.speed-cells polygon { fill-opacity: var(--speed-cell-opacity); stroke: var(--surface); stroke-width: 1; }
.speed-highlight { fill: none; stroke: var(--text-primary); stroke-width: 2.5; pointer-events: none; }
.speed-legend-title { margin: 0 0 4px; }
.speed-legend { display: flex; flex-wrap: wrap; gap: 4px 12px; list-style: none; margin: 0; padding: 0; font-variant-numeric: tabular-nums; }
.speed-legend li { display: flex; align-items: center; gap: 6px; }
.speed-swatch { width: 14px; height: 14px; border-radius: 3px; opacity: var(--speed-cell-opacity); }
.speed-tooltip-row { display: grid; grid-template-columns: 12px 1fr 4em 6em; gap: 8px; align-items: center; }
.speed-tooltip-heading { color: var(--text-secondary); font-size: 0.75rem; text-align: right; }
.breadcrumbs { color: var(--text-secondary); margin: 0 0 8px; }
.breadcrumbs a, .subtitle a { color: var(--text-secondary); }
.subtitle { color: var(--text-secondary); margin: 0; }
.period-nav { margin-top: 16px; }
.about p { max-width: 70ch; line-height: 1.5; }
";

const BLUE: RGBColor = RGBColor(0x2a, 0x78, 0xd6);
const ORANGE: RGBColor = RGBColor(0xeb, 0x68, 0x34);
const AQUA: RGBColor = RGBColor(0x1b, 0xaf, 0x7a);
const YELLOW: RGBColor = RGBColor(0xed, 0xa1, 0x00);
const MAGENTA: RGBColor = RGBColor(0xe8, 0x7b, 0xa4);
const GREEN: RGBColor = RGBColor(0x00, 0x83, 0x00);
const VIOLET: RGBColor = RGBColor(0x4a, 0x3a, 0xa7);

#[derive(Debug, Clone, Copy)]
pub enum Clock {
    System,
    Fixed(Timestamp),
}

impl Clock {
    fn now(self) -> Timestamp {
        match self {
            Clock::System => Timestamp::now(),
            Clock::Fixed(timestamp) => timestamp,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ChartStep {
    Minutes(i64),
    Hours(i64),
    Days,
}

impl ChartStep {
    fn start_of(self, unix: i64, tz: &TimeZone) -> i64 {
        let Ok(timestamp) = Timestamp::from_second(unix) else {
            return unix;
        };
        let zoned = timestamp.to_zoned(tz.clone());
        let truncate = |unit, increment| {
            zoned.round(
                ZonedRound::new()
                    .smallest(unit)
                    .increment(increment)
                    .mode(RoundMode::Trunc),
            )
        };
        let start = match self {
            ChartStep::Minutes(minutes) => truncate(Unit::Minute, minutes),
            ChartStep::Hours(hours) => truncate(Unit::Hour, hours),
            ChartStep::Days => zoned.start_of_day(),
        };
        start.map_or(unix, |start| start.timestamp().as_second())
    }

    fn nominal_secs(self) -> i64 {
        match self {
            ChartStep::Minutes(minutes) => minutes * SECS_PER_MINUTE,
            ChartStep::Hours(hours) => hours * SECS_PER_HOUR,
            ChartStep::Days => SECS_PER_DAY,
        }
    }

    fn next_start(self, start: i64, tz: &TimeZone) -> Option<i64> {
        let nominal_secs = self.nominal_secs();
        let mut next = self.start_of(start + nominal_secs, tz);
        if next <= start {
            next = self.start_of(start + nominal_secs + nominal_secs / 2, tz);
        }
        (next > start).then_some(next)
    }

    fn starts(self, from_unix: i64, to_unix: i64, tz: &TimeZone) -> Vec<i64> {
        let mut starts = Vec::new();
        let mut start = self.start_of(from_unix, tz);
        while start < to_unix {
            starts.push(start);
            let Some(next) = self.next_start(start, tz) else {
                break;
            };
            start = next;
        }
        starts
    }
}

#[derive(Debug, Clone, Copy)]
struct ChartSpec {
    step: ChartStep,
    tick_step: TickStep,
    tick: ChartTime,
    hover: ChartTime,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Range {
    Hour,
    #[default]
    Day,
    Week,
    Month,
}

impl Range {
    const ALL: [Range; 4] = [Range::Hour, Range::Day, Range::Week, Range::Month];
    const LONGEST: Range = Range::Month;

    fn duration(self) -> Duration {
        match self {
            Range::Hour => Duration::from_hours(1),
            Range::Day => Duration::from_hours(24),
            Range::Week => Duration::from_hours(7 * 24),
            Range::Month => Duration::from_hours(30 * 24),
        }
    }

    fn chart(self) -> ChartSpec {
        match self {
            Range::Hour => ChartSpec {
                step: ChartStep::Minutes(1),
                tick_step: TickStep::Minutes(10),
                tick: ChartTime::Time,
                hover: ChartTime::Time,
            },
            Range::Day => ChartSpec {
                step: ChartStep::Minutes(15),
                tick_step: TickStep::Hours(3),
                tick: ChartTime::Time,
                hover: ChartTime::WeekdayTime,
            },
            Range::Week => ChartSpec {
                step: ChartStep::Hours(1),
                tick_step: TickStep::Days(1),
                tick: ChartTime::WeekdayDayMonth,
                hover: ChartTime::WeekdayDayMonthTime,
            },
            Range::Month => ChartSpec {
                step: ChartStep::Hours(6),
                tick_step: TickStep::Days(5),
                tick: ChartTime::DayMonth,
                hover: ChartTime::WeekdayDayMonthTime,
            },
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Range::Hour => "hour",
            Range::Day => "day",
            Range::Week => "week",
            Range::Month => "month",
        }
    }

    fn window(self, now_unix: i64) -> (i64, i64) {
        let to_unix = stats::align_up(now_unix, db::duration_secs(stats::BUCKET));
        (to_unix - db::duration_secs(self.duration()), to_unix)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Metric {
    AverageSpeed,
    MovingSpeed,
    AverageVehicles,
    PeakVehicles,
    Distance,
}

impl Metric {
    const ALL: [Metric; 5] = [
        Metric::AverageSpeed,
        Metric::MovingSpeed,
        Metric::AverageVehicles,
        Metric::PeakVehicles,
        Metric::Distance,
    ];
    fn slug(self) -> &'static str {
        match self {
            Metric::AverageSpeed => "average-speed",
            Metric::MovingSpeed => "moving-speed",
            Metric::AverageVehicles => "average-vehicles",
            Metric::PeakVehicles => "peak-vehicles",
            Metric::Distance => "distance",
        }
    }

    fn decimals(self) -> usize {
        match self {
            Metric::AverageSpeed | Metric::MovingSpeed | Metric::AverageVehicles => 1,
            Metric::PeakVehicles | Metric::Distance => 0,
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn value(self, totals: &Totals) -> Option<f64> {
        match self {
            Metric::AverageSpeed => totals.average_speed_kmh(),
            Metric::MovingSpeed => totals.moving_speed_kmh(),
            Metric::AverageVehicles => totals.average_vehicles(),
            Metric::PeakVehicles => totals.peak_vehicles().map(|peak| peak as f64),
            Metric::Distance => totals.distance_km(),
        }
    }

    fn format(self, totals: &Totals, tr: &dyn Translation) -> String {
        self.format_value(self.value(totals), tr)
    }

    fn format_value(self, value: Option<f64>, tr: &dyn Translation) -> String {
        value.map_or_else(
            || MISSING_VALUE.to_owned(),
            |value| tr.number(value, self.decimals()),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chart {
    Speed,
    Vehicles,
}

impl Chart {
    const ALL: [Chart; 2] = [Chart::Speed, Chart::Vehicles];

    fn metric(self) -> Metric {
        match self {
            Chart::Speed => Metric::AverageSpeed,
            Chart::Vehicles => Metric::AverageVehicles,
        }
    }

    fn dashed_metric(self) -> Option<Metric> {
        match self {
            Chart::Speed => Some(Metric::MovingSpeed),
            Chart::Vehicles => None,
        }
    }

    fn slug(self) -> &'static str {
        self.metric().slug()
    }

    fn from_file(file: &str) -> Option<Chart> {
        let slug = file.strip_suffix(SVG_EXTENSION)?;
        Chart::ALL.into_iter().find(|chart| chart.slug() == slug)
    }
}

fn kind_slug(kind: Kind) -> &'static str {
    match kind {
        Kind::All => "all",
        Kind::Vehicle(vehicle_type) => vehicle_type.as_str(),
    }
}

fn kind_color(kind: Kind) -> RGBColor {
    match kind {
        Kind::All => BLUE,
        Kind::Vehicle(VehicleType::Bus) => ORANGE,
        Kind::Vehicle(VehicleType::Tram) => AQUA,
        Kind::Vehicle(VehicleType::Train) => VIOLET,
    }
}

const LINE_MINUTES_RANGE: Range = Range::Day;
const HOUR_STEP: ChartStep = ChartStep::Hours(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecentSource {
    Minutes,
    LineMinutes,
    Quarters,
}

impl RecentSource {
    fn of(groups: &[Group], range: Range) -> Self {
        if !groups.iter().any(|group| matches!(group, Group::Line(_))) {
            return RecentSource::Minutes;
        }
        if range.duration() <= LINE_MINUTES_RANGE.duration() {
            RecentSource::LineMinutes
        } else {
            RecentSource::Quarters
        }
    }

    fn resolution(self) -> Duration {
        match self {
            RecentSource::Minutes | RecentSource::LineMinutes => stats::BUCKET,
            RecentSource::Quarters => HISTORY_RESOLUTION,
        }
    }

    fn window(self, range: Range, now_unix: i64) -> (i64, i64) {
        let (from_unix, to_unix) = range.window(now_unix);
        let resolution_secs = db::duration_secs(self.resolution());
        (stats::align_down(from_unix, resolution_secs), to_unix)
    }
}

struct RangeTotals {
    window: (i64, i64),
    series: Vec<Vec<TimedTotals>>,
}

impl RangeTotals {
    fn since(&self, from_unix: i64) -> RangeTotals {
        RangeTotals {
            window: (from_unix, self.window.1),
            series: self
                .series
                .iter()
                .map(|series| {
                    series
                        .iter()
                        .filter(|timed| timed.start_unix >= from_unix)
                        .copied()
                        .collect()
                })
                .collect(),
        }
    }
}

fn has_vehicles(series: &[TimedTotals]) -> bool {
    stats::sum(series).has_vehicles()
}

#[derive(Clone)]
struct AppState {
    db: Db,
    recent: Arc<StatsCache>,
    recent_lines: Arc<StatsCache>,
    history: Arc<StatsCache>,
    cells: Arc<CellCache>,
    clock: Clock,
    tz: TimeZone,
    revision: Revision,
    basemap: SharedBasemap,
    pages: Arc<PageCache>,
    summary: summary::Cache,
}

impl AppState {
    fn new(db: Db, clock: Clock) -> Self {
        let summary = summary::Cache::new(db.clone(), clock);
        Self {
            db,
            recent: Arc::new(StatsCache::new(
                stats::BUCKET,
                Retention::Last(Range::LONGEST.duration()),
                Detail::Kinds,
            )),
            recent_lines: Arc::new(StatsCache::new(
                stats::BUCKET,
                Retention::Last(LINE_MINUTES_RANGE.duration()),
                Detail::Lines,
            )),
            history: Arc::new(StatsCache::new(
                HISTORY_RESOLUTION,
                Retention::Forever,
                Detail::Lines,
            )),
            cells: Arc::default(),
            clock,
            tz: crate::time::warsaw(),
            revision: Revision::current(),
            basemap: SharedBasemap::default(),
            pages: Arc::new(PageCache::new(DEFAULT_PAGE_CACHE_CAPACITY)),
            summary,
        }
    }

    async fn warm_up(self) {
        let started = Instant::now();
        let now_unix = self.clock.now().as_second();
        let recent = [
            (&self.recent, Range::default()),
            (&self.recent_lines, LINE_MINUTES_RANGE),
        ];
        for (cache, range) in recent {
            let (from_unix, to_unix) = range.window(now_unix);
            if let Err(e) = cache
                .totals(&self.db, from_unix, to_unix, now_unix, &[])
                .await
            {
                warn!("failed to compute recent statistics: {e:#}");
                return;
            }
        }
        info!(
            elapsed_secs = started.elapsed().as_secs_f64(),
            "recent statistics ready"
        );
        match self.history_totals(i64::MIN, now_unix, now_unix, &[]).await {
            Ok(_) => info!(
                elapsed_secs = started.elapsed().as_secs_f64(),
                "statistics history ready"
            ),
            Err(e) => {
                warn!("failed to compute statistics history: {e:#}");
                return;
            }
        }
        match self.period_cells(i64::MIN, now_unix, now_unix, &[]).await {
            Ok(_) => info!(
                elapsed_secs = started.elapsed().as_secs_f64(),
                "speed map history ready"
            ),
            Err(e) => warn!("failed to compute speed map history: {e:#}"),
        }
    }

    async fn recent_range(
        &self,
        groups: &[Group],
        range: Range,
        now_unix: i64,
    ) -> Result<RangeTotals> {
        let fetched = self.fetch_recent(groups, range, now_unix).await?;
        Ok(RangeTotals {
            window: fetched.window,
            series: self.skip_idle_line_hours(groups, fetched.series),
        })
    }

    async fn fetch_recent(
        &self,
        groups: &[Group],
        range: Range,
        now_unix: i64,
    ) -> Result<RangeTotals> {
        let source = RecentSource::of(groups, range);
        let window = source.window(range, now_unix);
        let series = match source {
            RecentSource::Minutes => {
                self.recent
                    .totals(&self.db, window.0, window.1, now_unix, groups)
                    .await?
            }
            RecentSource::LineMinutes => {
                self.recent_lines
                    .totals(&self.db, window.0, window.1, now_unix, groups)
                    .await?
            }
            RecentSource::Quarters => {
                self.history_totals(window.0, window.1, now_unix, groups)
                    .await?
            }
        };
        Ok(RangeTotals { window, series })
    }

    async fn recent_ranges(
        &self,
        groups: &[Group],
        now_unix: i64,
    ) -> Result<Vec<(Range, RangeTotals)>> {
        let mut ranges = Vec::with_capacity(Range::ALL.len());
        let mut longest: Option<(RecentSource, RangeTotals)> = None;
        for range in Range::ALL.into_iter().rev() {
            let source = RecentSource::of(groups, range);
            let fetched = match longest.take() {
                Some((fetched_source, fetched)) if fetched_source == source => fetched,
                _ => self.fetch_recent(groups, range, now_unix).await?,
            };
            let in_range = fetched.since(source.window(range, now_unix).0);
            ranges.push((
                range,
                RangeTotals {
                    window: in_range.window,
                    series: self.skip_idle_line_hours(groups, in_range.series),
                },
            ));
            longest = Some((source, fetched));
        }
        ranges.reverse();
        Ok(ranges)
    }

    async fn summary_table(
        &self,
        groups: &[Group],
        now_unix: i64,
    ) -> Result<Option<summary::Table>> {
        if groups.iter().all(|group| matches!(group, Group::Kind(_))) {
            return Ok(self.summary.table(groups));
        }
        let ranges = self.recent_ranges(groups, now_unix).await?;
        let mut rows = vec![Vec::with_capacity(ranges.len()); groups.len()];
        for (range, totals) in ranges {
            for (rows, series) in rows.iter_mut().zip(totals.series) {
                rows.push((range, stats::sum(&series)));
            }
        }
        Ok(Some(summary::Table {
            at_unix: now_unix,
            rows,
        }))
    }

    async fn period_totals(
        &self,
        from_unix: i64,
        to_unix: i64,
        now_unix: i64,
        groups: &[Group],
    ) -> Result<Vec<Vec<TimedTotals>>> {
        let series = self
            .history_totals(from_unix, to_unix, now_unix, groups)
            .await?;
        Ok(self.skip_idle_line_hours(groups, series))
    }

    fn skip_idle_line_hours(
        &self,
        groups: &[Group],
        series: Vec<Vec<TimedTotals>>,
    ) -> Vec<Vec<TimedTotals>> {
        groups
            .iter()
            .zip(series)
            .map(|(group, series)| match group {
                Group::Line(_) => {
                    stats::skip_idle_hours(&series, |unix| HOUR_STEP.start_of(unix, &self.tz))
                }
                Group::Kind(_) => series,
            })
            .collect()
    }

    async fn history_totals(
        &self,
        from_unix: i64,
        to_unix: i64,
        now_unix: i64,
        groups: &[Group],
    ) -> Result<Vec<Vec<TimedTotals>>> {
        let empty = || vec![Vec::new(); groups.len()];
        let Some(first_unix) = self.db.first_seen_unix().await? else {
            return Ok(empty());
        };
        let from_unix = from_unix.max(first_unix);
        let to_unix = to_unix.min(now_unix);
        if from_unix >= to_unix {
            return Ok(empty());
        }
        self.history
            .totals(&self.db, from_unix, to_unix, now_unix, groups)
            .await
    }

    async fn period_cells(
        &self,
        from_unix: i64,
        to_unix: i64,
        now_unix: i64,
        groups: &[Group],
    ) -> Result<Vec<CellSpeeds>> {
        let Some(first_unix) = self.db.first_seen_unix().await? else {
            return Ok(Vec::new());
        };
        let days: Vec<(i64, i64)> = ChartStep::Days
            .starts(from_unix.max(first_unix), to_unix.min(now_unix), &self.tz)
            .into_iter()
            .filter_map(|start| Some((start, ChartStep::Days.next_start(start, &self.tz)?)))
            .collect();
        self.cells.totals(&self.db, &days, now_unix, groups).await
    }

    fn today(&self) -> Date {
        self.clock.now().to_zoned(self.tz.clone()).date()
    }
}

pub fn router(db: Db, clock: Clock) -> Router {
    app(AppState::new(db, clock))
}

fn period_route(kind: PeriodKind) -> String {
    format!("/{}/{{period}}", kind.slug())
}

fn type_and_line_paths(vehicle_type: VehicleType) -> (String, String) {
    let type_path = format!("/{}", vehicle_type.as_str());
    let line_path = format!("{type_path}/{{line}}");
    (type_path, line_path)
}

fn quality_route(path: &str) -> String {
    format!("{path}/{QUALITY_SEGMENT}")
}

#[derive(Debug, Clone, Copy)]
enum PeriodView {
    Statistics,
    Quality,
}

impl PeriodView {
    const ALL: [PeriodView; 2] = [PeriodView::Statistics, PeriodView::Quality];

    fn route(self, path: &str) -> String {
        match self {
            PeriodView::Statistics => path.to_owned(),
            PeriodView::Quality => quality_route(path),
        }
    }

    fn path(self, selection: &Selection, period: Period) -> String {
        match self {
            PeriodView::Statistics => selection.period_path(period),
            PeriodView::Quality => selection.period_quality_path(period),
        }
    }
}

fn recent_period_route(prefix: &str, recent: RecentPeriod, view: PeriodView) -> String {
    view.route(&format!("{prefix}/{}", recent.slug()))
}

fn root_routes() -> Router<AppState> {
    Router::new()
        .route(
            OVERVIEW_PATH,
            get(
                |State(state): State<AppState>,
                 language: Language,
                 Query(query): Query<RangeQuery>| {
                    overview(state, language, SelectionKey::All, query.range)
                },
            ),
        )
        .route(
            OVERVIEW_QUALITY_PATH,
            get(
                |State(state): State<AppState>,
                 language: Language,
                 Query(query): Query<RangeQuery>| {
                    quality::overview(state, language, SelectionKey::All, query.range)
                },
            ),
        )
        .route(
            CHARTS_PATH,
            get(
                |State(state): State<AppState>,
                 language: Language,
                 Path(file): Path<String>,
                 Query(query): Query<RangeQuery>| {
                    chart(state, language, SelectionKey::All, file, query.range)
                },
            ),
        )
        .route(
            MAP_PATH,
            get(|State(state): State<AppState>, language: Language| {
                map::image(state, language, SelectionKey::All)
            }),
        )
        .route(ABOUT_PATH, get(about::page))
        .route(
            BASEMAP_PATH,
            get(|State(state): State<AppState>| async move { map::basemap(&state) }),
        )
}

fn app(state: AppState) -> Router {
    let mut router = root_routes();
    for kind in PeriodKind::ALL {
        router = router
            .route(
                &period_route(kind),
                get(
                    move |State(state): State<AppState>,
                          language: Language,
                          if_none_match: IfNoneMatch,
                          Path(slug): Path<String>| {
                        period_page(
                            state,
                            language,
                            if_none_match,
                            SelectionKey::All,
                            kind,
                            slug,
                        )
                    },
                ),
            )
            .route(
                &quality_route(&period_route(kind)),
                get(
                    move |State(state): State<AppState>,
                          language: Language,
                          if_none_match: IfNoneMatch,
                          Path(slug): Path<String>| {
                        quality::period_page(
                            state,
                            language,
                            if_none_match,
                            SelectionKey::All,
                            kind,
                            slug,
                        )
                    },
                ),
            );
    }
    for recent in RecentPeriod::ALL {
        for view in PeriodView::ALL {
            router = router.route(
                &recent_period_route("", recent, view),
                get(move |State(state): State<AppState>| {
                    recent_period(state, SelectionKey::All, recent, view)
                }),
            );
        }
    }
    for vehicle_type in VehicleType::ALL {
        router = vehicle_type_routes(router, vehicle_type);
        router = vehicle_type_recent_period_routes(router, vehicle_type);
    }
    router
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            page_cache::serve,
        ))
        .with_state(state)
}

fn vehicle_type_routes(
    mut router: Router<AppState>,
    vehicle_type: VehicleType,
) -> Router<AppState> {
    let (type_path, line_path) = type_and_line_paths(vehicle_type);
    router = router
        .route(
            &type_path,
            get(
                move |State(state): State<AppState>,
                      language: Language,
                      Query(query): Query<RangeQuery>| {
                    overview(
                        state,
                        language,
                        SelectionKey::Type(vehicle_type),
                        query.range,
                    )
                },
            ),
        )
        .route(
            &line_path,
            get(
                move |State(state): State<AppState>,
                      language: Language,
                      Path(line): Path<String>,
                      Query(query): Query<RangeQuery>| {
                    overview(
                        state,
                        language,
                        SelectionKey::Line(vehicle_type, line),
                        query.range,
                    )
                },
            ),
        )
        .route(
            &format!("{type_path}{CHARTS_PATH}"),
            get(
                move |State(state): State<AppState>,
                      language: Language,
                      Path(file): Path<String>,
                      Query(query): Query<RangeQuery>| {
                    chart(
                        state,
                        language,
                        SelectionKey::Type(vehicle_type),
                        file,
                        query.range,
                    )
                },
            ),
        )
        .route(
            &format!("{line_path}{CHARTS_PATH}"),
            get(
                move |State(state): State<AppState>,
                      language: Language,
                      Path((line, file)): Path<(String, String)>,
                      Query(query): Query<RangeQuery>| {
                    chart(
                        state,
                        language,
                        SelectionKey::Line(vehicle_type, line),
                        file,
                        query.range,
                    )
                },
            ),
        )
        .route(
            &quality_route(&type_path),
            get(
                move |State(state): State<AppState>,
                      language: Language,
                      Query(query): Query<RangeQuery>| {
                    quality::overview(
                        state,
                        language,
                        SelectionKey::Type(vehicle_type),
                        query.range,
                    )
                },
            ),
        )
        .route(
            &quality_route(&line_path),
            get(
                move |State(state): State<AppState>,
                      language: Language,
                      Path(line): Path<String>,
                      Query(query): Query<RangeQuery>| {
                    quality::overview(
                        state,
                        language,
                        SelectionKey::Line(vehicle_type, line),
                        query.range,
                    )
                },
            ),
        );
    router = vehicle_type_map_routes(router, vehicle_type);
    vehicle_type_period_routes(router, vehicle_type)
}

fn vehicle_type_recent_period_routes(
    mut router: Router<AppState>,
    vehicle_type: VehicleType,
) -> Router<AppState> {
    let (type_path, line_path) = type_and_line_paths(vehicle_type);
    for recent in RecentPeriod::ALL {
        for view in PeriodView::ALL {
            router = router
                .route(
                    &recent_period_route(&type_path, recent, view),
                    get(move |State(state): State<AppState>| {
                        recent_period(state, SelectionKey::Type(vehicle_type), recent, view)
                    }),
                )
                .route(
                    &recent_period_route(&line_path, recent, view),
                    get(
                        move |State(state): State<AppState>, Path(line): Path<String>| {
                            recent_period(
                                state,
                                SelectionKey::Line(vehicle_type, line),
                                recent,
                                view,
                            )
                        },
                    ),
                );
        }
    }
    router
}

fn vehicle_type_map_routes(
    router: Router<AppState>,
    vehicle_type: VehicleType,
) -> Router<AppState> {
    let (type_path, line_path) = type_and_line_paths(vehicle_type);
    router
        .route(
            &format!("{type_path}{MAP_PATH}"),
            get(move |State(state): State<AppState>, language: Language| {
                map::image(state, language, SelectionKey::Type(vehicle_type))
            }),
        )
        .route(
            &format!("{line_path}{MAP_PATH}"),
            get(
                move |State(state): State<AppState>,
                      language: Language,
                      Path(line): Path<String>| {
                    map::image(state, language, SelectionKey::Line(vehicle_type, line))
                },
            ),
        )
}

fn vehicle_type_period_routes(
    mut router: Router<AppState>,
    vehicle_type: VehicleType,
) -> Router<AppState> {
    let (type_path, line_path) = type_and_line_paths(vehicle_type);
    for kind in PeriodKind::ALL {
        router = router
            .route(
                &format!("{type_path}{}", period_route(kind)),
                get(
                    move |State(state): State<AppState>,
                          language: Language,
                          if_none_match: IfNoneMatch,
                          Path(slug): Path<String>| {
                        period_page(
                            state,
                            language,
                            if_none_match,
                            SelectionKey::Type(vehicle_type),
                            kind,
                            slug,
                        )
                    },
                ),
            )
            .route(
                &format!("{line_path}{}", period_route(kind)),
                get(
                    move |State(state): State<AppState>,
                          language: Language,
                          if_none_match: IfNoneMatch,
                          Path((line, slug)): Path<(String, String)>| {
                        period_page(
                            state,
                            language,
                            if_none_match,
                            SelectionKey::Line(vehicle_type, line),
                            kind,
                            slug,
                        )
                    },
                ),
            )
            .route(
                &quality_route(&format!("{type_path}{}", period_route(kind))),
                get(
                    move |State(state): State<AppState>,
                          language: Language,
                          if_none_match: IfNoneMatch,
                          Path(slug): Path<String>| {
                        quality::period_page(
                            state,
                            language,
                            if_none_match,
                            SelectionKey::Type(vehicle_type),
                            kind,
                            slug,
                        )
                    },
                ),
            )
            .route(
                &quality_route(&format!("{line_path}{}", period_route(kind))),
                get(
                    move |State(state): State<AppState>,
                          language: Language,
                          if_none_match: IfNoneMatch,
                          Path((line, slug)): Path<(String, String)>| {
                        quality::period_page(
                            state,
                            language,
                            if_none_match,
                            SelectionKey::Line(vehicle_type, line),
                            kind,
                            slug,
                        )
                    },
                ),
            );
    }
    router
}

pub async fn serve(
    listener: TcpListener,
    db: Db,
    clock: Clock,
    tiles: MapTiles,
    page_cache_capacity: NonZeroUsize,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let state = AppState {
        pages: Arc::new(PageCache::new(page_cache_capacity)),
        ..AppState::new(db, clock)
    };
    let mut background = tokio::task::JoinSet::new();
    background.spawn(state.clone().warm_up());
    if let MapTiles::Download(client) = tiles {
        background.spawn(state.basemap.clone().download(client));
    }
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        error!("request failed: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR).into_response()
    }
}

#[derive(Deserialize)]
struct RangeQuery {
    #[serde(default)]
    range: Range,
}

async fn overview(
    state: AppState,
    language: Language,
    key: SelectionKey,
    selected: Range,
) -> Result<Response, AppError> {
    let Some(selection) = key.resolve(&state.db).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let tr = language.translation();
    let now = state.clock.now();
    let now_unix = now.as_second();
    let subjects = selection.subjects(tr);
    let groups = selection::groups(&subjects);
    let summary = state.summary_table(&groups, now_unix).await?;
    let chart = state.recent_range(&groups, selected, now_unix).await?;
    let shown = selection::shown(&subjects, &chart.series, |series| has_vehicles(series));
    let labels = ChartLabels::new(tr, selected.chart(), &state.tz);
    let figures = chart_figures(
        &shown,
        chart.window,
        &labels,
        tr.range_caption(selected),
        selected.slug(),
    )?;
    let map = map::section(&state, tr, &selection).await?;
    let (lines_from_unix, lines_to_unix) = Range::LONGEST.window(now_unix);
    let lines = state
        .db
        .lines_seen_between(lines_from_unix, lines_to_unix)
        .await?;
    let page = Page::Overview(selected);
    let history = history_links(state.today())?;
    let heading = match selection {
        Selection::All => tr.text(Text::Title).to_owned(),
        Selection::Type(_) | Selection::Line(_) => selection.label(tr),
    };
    let body = html! {
        header {
            h1 { (heading) }
            p #updated { (tr.text(Text::AsOf)) (updated(now, &state.tz)) }
        }
        main {
            @if let Some(map) = &map {
                (map)
            }
            section #charts {
                h2 { (tr.text(Text::Charts)) }
                nav aria-label=(tr.text(Text::ChartRange)) {
                    @for range in Range::ALL {
                        a href=(selection.range_path(range))
                            aria-current=[(range == selected).then_some("page")] {
                            (tr.range(range))
                        }
                    }
                }
                @for figure in &figures {
                    (figure)
                }
            }
            (selection::vehicles_nav(tr, &selection, page, &lines))
            nav #history aria-label=(tr.text(Text::History)) {
                @for (relative, period) in &history {
                    a href=(selection.period_path(*period)) {
                        (tr.relative_period(period.kind(), *relative))
                    }
                }
            }
            nav #pages aria-label=(tr.text(Text::Pages)) {
                a #quality-link href=(selection.quality_path()) { (tr.text(Text::DataQuality)) }
                a #about-link href=(ABOUT_PATH) { (tr.text(Text::About)) }
            }
            (summary::section(tr, &subjects, summary.as_ref(), &state.tz)?)
            (selection::lines_section(tr, &selection, page, &lines))
        }
    };
    let head = Head {
        description: page_description(tr, &heading, Text::StatisticsDescription),
        title: match selection {
            Selection::All => heading,
            Selection::Type(_) | Selection::Line(_) => page_title(tr, &heading),
        },
    };
    let mut response = shell(language, Caching::live()?, &head, &body);
    if summary.is_none() {
        response.extensions_mut().remove::<page_cache::Validity>();
    }
    Ok(response)
}

fn history_links(today: Date) -> Result<Vec<(Relative, Period)>> {
    let day = Period::containing(PeriodKind::Day, today)?;
    let week = Period::containing(PeriodKind::Week, today)?;
    let month = Period::containing(PeriodKind::Month, today)?;
    let year = Period::containing(PeriodKind::Year, today)?;
    Ok(vec![
        (Relative::Current, day),
        (Relative::Previous, day.previous()?),
        (Relative::Current, week),
        (Relative::Previous, week.previous()?),
        (Relative::Current, month),
        (Relative::Previous, month.previous()?),
        (Relative::Current, year),
    ])
}

struct Head {
    title: String,
    description: String,
}

impl Head {
    fn new(tr: &dyn Translation, heading: &str, about: Text) -> Self {
        Self {
            title: page_title(tr, heading),
            description: page_description(tr, heading, about),
        }
    }
}

fn page_title(tr: &dyn Translation, heading: &str) -> String {
    format!("{heading} – {}", tr.text(Text::Title))
}

fn page_description(tr: &dyn Translation, heading: &str, about: Text) -> String {
    format!("{heading}. {}", tr.text(about))
}

fn updated(now: Timestamp, tz: &TimeZone) -> Markup {
    let local = now
        .to_zoned(tz.clone())
        .strftime(UPDATED_FORMAT)
        .to_string();
    html! { time datetime=(now.to_string()) { (local) } }
}

fn shell(language: Language, caching: Caching, head: &Head, body: &Markup) -> Response {
    let markup = html! {
        (DOCTYPE)
        html lang=(language.tag()) {
            head {
                meta charset="utf-8";
                script async src=(format!("{GTAG_SRC}?id={GTAG_ID}")) {}
                script { (PreEscaped(format!("{GTAG_SCRIPT}  gtag('config', '{GTAG_ID}');\n"))) }
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (head.title) }
                meta name="description" content=(head.description);
                style { (PreEscaped(STYLE)) }
            }
            body {
                (body)
                script { (PreEscaped(CHART_SCRIPT)) }
                script { (PreEscaped(MAP_SCRIPT)) }
                script { (PreEscaped(SPEED_MAP_SCRIPT)) }
            }
        }
    };
    let validity = caching.page_validity;
    let mut response = (language_headers(language), caching, markup).into_response();
    response.extensions_mut().insert(validity);
    response
}

fn language_headers(language: Language) -> [(header::HeaderName, HeaderValue); 2] {
    [
        (
            header::CONTENT_LANGUAGE,
            HeaderValue::from_static(language.tag()),
        ),
        (
            header::VARY,
            HeaderValue::from_static(header::ACCEPT_LANGUAGE.as_str()),
        ),
    ]
}

async fn recent_period(
    state: AppState,
    key: SelectionKey,
    recent: RecentPeriod,
    view: PeriodView,
) -> Result<Response, AppError> {
    let Some(selection) = key.resolve(&state.db).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let period = recent.period(state.today())?;
    let target = view.path(&selection, period);
    Ok((Caching::live()?, Redirect::temporary(&target)).into_response())
}

async fn period_has_positions(
    state: &AppState,
    selection: &Selection,
    period: Period,
) -> Result<bool> {
    let (from, to) = period.bounds(&state.tz)?;
    state
        .db
        .has_positions(selection.scope(), from.as_second(), to.as_second())
        .await
}

async fn period_page(
    state: AppState,
    language: Language,
    if_none_match: IfNoneMatch,
    key: SelectionKey,
    kind: PeriodKind,
    slug: String,
) -> Result<Response, AppError> {
    let Some(period) = Period::parse(kind, &slug) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let Some(selection) = key.resolve(&state.db).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    if !period_has_positions(&state, &selection, period).await? {
        return Ok((Caching::live()?, Redirect::temporary(OVERVIEW_PATH)).into_response());
    }
    let now = state.clock.now();
    let now_unix = now.as_second();
    let (from, to) = period.bounds(&state.tz)?;
    let (from_unix, to_unix) = (from.as_second(), to.as_second());
    let caching = Caching::period(state.revision, language, to_unix, now_unix)?;
    if caching.matches(&if_none_match) {
        return Ok(caching.not_modified(language));
    }
    let tr = language.translation();
    let subjects = selection.subjects(tr);
    let series = state
        .period_totals(from_unix, to_unix, now_unix, &selection::groups(&subjects))
        .await?;
    let shown = selection::shown(&subjects, &series, |series| has_vehicles(series));
    let title = period.title(tr);
    let path = selection.period_path(period);
    let labels = ChartLabels::new(tr, period.chart(), &state.tz);
    let figures = chart_figures(&shown, (from_unix, to_unix), &labels, &title, &path)?;
    let speed_map =
        speed_map::section(&state, tr, &subjects, (from_unix, to_unix), now_unix).await?;
    let lines = state.db.lines_seen_between(from_unix, to_unix).await?;
    let page = Page::Period(period);
    let previous = period.previous()?;
    let previous_has_data = period_has_positions(&state, &selection, previous).await?;
    let next = period.next()?;
    let next_started = next.start_date() <= state.today();
    let in_progress = (from_unix..to_unix).contains(&now_unix);
    let parents = period.parents()?;
    let parts = period.parts()?;
    let heading = selection.heading(tr, &title);
    let body = html! {
        header {
            p .breadcrumbs {
                a href=(selection.overview_path()) { (tr.text(Text::Overview)) }
                @for parent in &parents {
                    " · " a href=(selection.period_path(*parent)) { (capitalized(&parent.title(tr))) }
                }
            }
            h1 { (heading) }
            @if let Some(subtitle) = period.subtitle(tr)? {
                p .subtitle { (subtitle) }
            }
            @if in_progress {
                p #updated { (tr.text(Text::SoFarAsOf)) (updated(now, &state.tz)) }
            }
        }
        main {
            @if let Some(speed_map) = &speed_map {
                (speed_map)
            }
            section #charts {
                h2 { (tr.text(Text::Charts)) }
                @for figure in &figures {
                    (figure)
                }
            }
            (selection::vehicles_nav(tr, &selection, page, &lines))
            nav .period-nav aria-label=(tr.text(Text::NeighbouringPeriods)) {
                @if previous_has_data {
                    a #previous rel="prev" href=(selection.period_path(previous)) {
                        (tr.neighbour(kind, Neighbour::Previous))
                    }
                }
                @if next_started {
                    a #next rel="next" href=(selection.period_path(next)) {
                        (tr.neighbour(kind, Neighbour::Next))
                    }
                }
                a #quality-link href=(selection.period_quality_path(period)) { (tr.text(Text::DataQuality)) }
            }
            section #summary {
                h2 { (tr.text(Text::Summary)) }
                (period_table(tr, &shown))
            }
            @if let Some(part) = parts.first() {
                section #parts {
                    h2 { (tr.parts(part.kind())) }
                    nav aria-label=(tr.parts(part.kind())) {
                        @for part in &parts {
                            a href=(selection.period_path(*part)) { (capitalized(&part.short_label(tr))) }
                        }
                    }
                }
            }
            (selection::lines_section(tr, &selection, page, &lines))
        }
    };
    let head = Head::new(tr, &heading, Text::StatisticsDescription);
    Ok(shell(language, caching, &head, &body))
}

fn chart_figures(
    shown: &[(&Subject, &Vec<TimedTotals>)],
    window: (i64, i64),
    labels: &ChartLabels,
    caption: &str,
    range: &str,
) -> Result<Vec<Markup>> {
    Chart::ALL
        .into_iter()
        .map(|chart| {
            let rendered = render_chart(chart, shown, window, labels)?;
            chart_figure(
                chart.slug(),
                labels.tr.metric(chart.metric()),
                caption,
                range,
                &rendered,
            )
        })
        .collect()
}

fn chart_figure(
    slug: &str,
    label: &str,
    caption: &str,
    range: &str,
    rendered: &Rendered,
) -> Result<Markup> {
    let hover = script_json(&rendered.hover)?;
    Ok(html! {
        figure .chart #(format!("chart-{slug}")) data-range=(range) {
            div .chart-plot { (PreEscaped(&rendered.svg)) }
            div .chart-tooltip role="status" hidden {}
            script .chart-data type="application/json" { (PreEscaped(hover)) }
            figcaption { (label) ", " (caption) }
        }
    })
}

fn css_color(RGBColor(red, green, blue): RGBColor) -> String {
    format!("#{red:02x}{green:02x}{blue:02x}")
}

/// JSON that can stand inside a `<script>` element.
fn script_json(value: &impl Serialize) -> Result<String> {
    Ok(serde_json::to_string(value)?.replace(SCRIPT_END_TAG_START, ESCAPED_SCRIPT_END_TAG_START))
}

fn range_table(tr: &dyn Translation, subject: &Subject, rows: &[(Range, Totals)]) -> Markup {
    html! {
        div .table-wrap {
            table .stats #(format!("stats-{}", subject.slug)) {
                caption { (subject.label) }
                thead {
                    tr {
                        th scope="col" { (tr.text(Text::Metric)) }
                        @for (range, _) in rows {
                            th scope="col" { (tr.range(*range)) }
                        }
                    }
                }
                tbody {
                    @for metric in Metric::ALL {
                        tr data-metric=(metric.slug()) {
                            th scope="row" { (tr.metric(metric)) }
                            @for (range, totals) in rows {
                                td data-range=(range.slug()) { (metric.format(totals, tr)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn period_table(tr: &dyn Translation, shown: &[(&Subject, &Vec<TimedTotals>)]) -> Markup {
    let totals: Vec<(&Subject, Totals)> = shown
        .iter()
        .map(|(subject, series)| (*subject, stats::sum(series.iter())))
        .collect();
    html! {
        div .table-wrap {
            table .stats #period-stats {
                thead {
                    tr {
                        th scope="col" { (tr.text(Text::Metric)) }
                        @for (subject, _) in &totals {
                            th scope="col" data-kind=(subject.slug) { (subject.label) }
                        }
                    }
                }
                tbody {
                    @for metric in Metric::ALL {
                        tr data-metric=(metric.slug()) {
                            th scope="row" { (tr.metric(metric)) }
                            @for (subject, total) in &totals {
                                td data-kind=(subject.slug) { (metric.format(total, tr)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn chart(
    state: AppState,
    language: Language,
    key: SelectionKey,
    file: String,
    range: Range,
) -> Result<Response, AppError> {
    let Some(chart) = Chart::from_file(&file) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let Some(selection) = key.resolve(&state.db).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let tr = language.translation();
    let subjects = selection.subjects(tr);
    let groups = selection::groups(&subjects);
    let now_unix = state.clock.now().as_second();
    let totals = state.recent_range(&groups, range, now_unix).await?;
    let shown = selection::shown(&subjects, &totals.series, |series| has_vehicles(series));
    let labels = ChartLabels::new(tr, range.chart(), &state.tz);
    let rendered = render_chart(chart, &shown, totals.window, &labels)?;
    Ok((
        language_headers(language),
        Caching::live()?,
        [(header::CONTENT_TYPE, SVG_CONTENT_TYPE)],
        rendered.svg,
    )
        .into_response())
}

struct ChartLabels<'a> {
    tr: &'a dyn Translation,
    spec: ChartSpec,
    tz: &'a TimeZone,
}

impl<'a> ChartLabels<'a> {
    fn new(tr: &'a dyn Translation, spec: ChartSpec, tz: &'a TimeZone) -> Self {
        Self { tr, spec, tz }
    }

    fn time(&self, unix: i64, format: ChartTime) -> String {
        Timestamp::from_second(unix).map_or_else(
            |_| String::new(),
            |timestamp| {
                self.tr
                    .chart_time(&timestamp.to_zoned(self.tz.clone()), format)
            },
        )
    }
}

impl Labels for ChartLabels<'_> {
    fn tick(&self, unix: i64) -> String {
        self.time(unix, self.spec.tick)
    }

    fn hover(&self, unix: i64) -> String {
        self.time(unix, self.spec.hover)
    }

    fn number(&self, value: f64, decimals: usize) -> String {
        self.tr.number(value, decimals)
    }

    fn no_data(&self) -> &str {
        self.tr.text(Text::NoData)
    }
}

fn render_chart(
    chart: Chart,
    shown: &[(&Subject, &Vec<TimedTotals>)],
    (from_unix, to_unix): (i64, i64),
    labels: &ChartLabels,
) -> Result<Rendered> {
    let step = labels.spec.step;
    let mut series = Vec::new();
    for (subject, timed) in shown {
        let steps = stats::regroup_by(timed, |start_unix| step.start_of(start_unix, labels.tz));
        let points = |metric: Metric| -> Vec<Point> {
            steps
                .iter()
                .map(|timed| {
                    let value = metric.value(&timed.totals);
                    Point {
                        unix: timed.start_unix.max(from_unix),
                        value,
                        text: metric.format_value(value, labels.tr),
                    }
                })
                .collect()
        };
        series.push(Series {
            name: subject.label.clone(),
            color: subject.color,
            stroke: Stroke::Solid,
            points: points(chart.metric()),
        });
        let Some(dashed) = chart.dashed_metric() else {
            continue;
        };
        series.push(Series {
            name: labels.tr.while_moving(&subject.label),
            color: subject.color,
            stroke: Stroke::Dashed,
            points: points(dashed),
        });
    }
    charts::render(&LineChart {
        from_unix,
        to_unix,
        y_label: labels.tr.metric(chart.metric()),
        labels,
        tz: labels.tz.clone(),
        tick_step: labels.spec.tick_step,
        series,
        dashed_legend: chart
            .dashed_metric()
            .map(|_| labels.tr.text(Text::WhileMoving)),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path as FilePath;

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use scraper::{Html, Selector};
    use tower::ServiceExt;

    use super::*;
    use crate::db::{LinePositionCount, OpenMode};
    use crate::fetch::Source;
    use crate::geo::MicroDegrees;
    use crate::map::Basemap;
    use crate::mpk::Position;
    use jiff::civil::Date;

    const IN_MEMORY: &str = ":memory:";
    const NOW: i64 = 1_800_000_000;
    const FETCH_SECS: i64 = 10;
    const HOUR_SECS: i32 = 60 * 60;
    const DRIVE_SECS: i64 = 300;
    const HUNDRED_METERS_NORTH_E6: i64 = 898;
    const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";

    const POLISH_BROWSER: &str = "pl-PL,pl;q=0.9,en-US;q=0.8,en;q=0.7";
    const TEST_COMMIT: &str = "0123abcd";
    const SETTLED_CACHE_CONTROL: &str = "max-age=86400";
    const LIVE_CACHE_CONTROL: &str = "max-age=60";
    const OTHER_ETAG: &str = "\"other\"";

    struct Fetched {
        status: StatusCode,
        content_type: Option<String>,
        headers: header::HeaderMap,
        body: String,
        bytes: Vec<u8>,
    }

    async fn empty_db() -> Db {
        Db::open(FilePath::new(IN_MEMORY), OpenMode::CreateIfMissing)
            .await
            .unwrap()
    }

    async fn db_with_a_bus() -> Db {
        let db = empty_db().await;
        let mut unix = NOW - DRIVE_SECS;
        let mut north_e6 = 0;
        while unix < NOW {
            let bus = Position {
                vehicle_type: VehicleType::Bus,
                line: "a".to_owned(),
                course_id: 1,
                latitude: MicroDegrees(51_100_000 + north_e6),
                longitude: MicroDegrees(17_000_000),
            };
            db.save_positions_with_counts(
                unix,
                &[&bus],
                &[],
                &[LinePositionCount {
                    vehicle_type: bus.vehicle_type,
                    line: bus.line.clone(),
                    received: 1,
                    invalid: 0,
                }],
            )
            .await
            .unwrap();
            unix += FETCH_SECS;
            north_e6 += HUNDRED_METERS_NORTH_E6;
        }
        db
    }

    async fn get(db: &Db, uri: &str) -> Fetched {
        send(db, Request::get(uri)).await
    }

    async fn get_in(db: &Db, uri: &str, accept_language: &str) -> Fetched {
        send(
            db,
            Request::get(uri).header(header::ACCEPT_LANGUAGE, accept_language),
        )
        .await
    }

    async fn record_period(db: &Db, period: Period) {
        let (from, _) = period.bounds(&crate::time::warsaw()).unwrap();
        let bus = Position {
            vehicle_type: VehicleType::Bus,
            line: BUS_LINE.to_owned(),
            course_id: 1,
            latitude: MicroDegrees(51_100_000),
            longitude: MicroDegrees(17_000_000),
        };
        db.save_positions_with_counts(
            from.as_second(),
            &[&bus],
            &[],
            &[LinePositionCount {
                vehicle_type: bus.vehicle_type,
                line: bus.line.clone(),
                received: 1,
                invalid: 0,
            }],
        )
        .await
        .unwrap();
    }

    fn app_at(db: &Db, now_unix: i64, revision: Revision) -> Router {
        let clock = Clock::Fixed(Timestamp::from_second(now_unix).unwrap());
        app(AppState {
            revision,
            ..AppState::new(db.clone(), clock)
        })
    }

    #[tokio::test]
    async fn ui_uses_warsaw_time_in_winter_and_summer() {
        let db = empty_db().await;
        for (utc, local, date) in [
            (
                "2026-01-15T23:30:00Z",
                "2026-01-16 00:30",
                Date::new(2026, 1, 16).unwrap(),
            ),
            (
                "2026-07-15T22:30:00Z",
                "2026-07-16 00:30",
                Date::new(2026, 7, 16).unwrap(),
            ),
        ] {
            let now: Timestamp = utc.parse().unwrap();
            let state = AppState::new(db.clone(), Clock::Fixed(now));
            assert_eq!(state.today(), date);
            let labels = ChartLabels::new(
                Language::English.translation(),
                Range::Hour.chart(),
                &state.tz,
            );
            assert_eq!(labels.tick(now.as_second()), "00:30");
            assert!(labels.hover(now.as_second()).contains("00:30"));
            let fetched = send_to(app(state), Request::get("/")).await;
            assert_eq!(select_texts(&fetched.body, "#updated time"), [local]);
            assert_eq!(
                select_attributes(&fetched.body, "#updated time", "datetime"),
                [now.to_string()]
            );
        }
    }

    async fn send(db: &Db, request: axum::http::request::Builder) -> Fetched {
        let state = AppState {
            revision: Revision::Commit(TEST_COMMIT),
            ..AppState::new(
                db.clone(),
                Clock::Fixed(Timestamp::from_second(NOW).unwrap()),
            )
        };
        state.summary.wait_until_ready().await;
        send_to(app(state), request).await
    }

    async fn send_to(app: Router, request: axum::http::request::Builder) -> Fetched {
        let response = app
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .map(|value| value.to_str().unwrap().to_owned());
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Fetched {
            status,
            content_type,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            bytes: bytes.to_vec(),
        }
    }

    fn header_value(fetched: &Fetched, name: header::HeaderName) -> Option<&str> {
        fetched
            .headers
            .get(name)
            .map(|value| value.to_str().unwrap())
    }

    fn en() -> &'static dyn Translation {
        Language::English.translation()
    }

    fn select_texts(html: &str, selector: &str) -> Vec<String> {
        let selector = Selector::parse(selector).unwrap();
        Html::parse_document(html)
            .select(&selector)
            .map(|element| element.text().collect())
            .collect()
    }

    fn select_attributes(html: &str, selector: &str, attribute: &str) -> Vec<String> {
        let selector = Selector::parse(selector).unwrap();
        Html::parse_document(html)
            .select(&selector)
            .filter_map(|element| element.value().attr(attribute).map(str::to_owned))
            .collect()
    }

    fn cells(html: &str, kind: Kind, metric: Metric) -> Vec<String> {
        select_texts(
            html,
            &format!(
                "#stats-{} tr[data-metric={}] td",
                kind_slug(kind),
                metric.slug()
            ),
        )
    }

    fn tables(html: &str) -> Vec<String> {
        select_attributes(html, "#summary table.stats", "id")
    }

    #[tokio::test]
    async fn shows_statistics_for_every_range_and_only_kinds_with_vehicles() {
        let db = db_with_a_bus().await;
        let page = get(&db, "/").await;
        assert_eq!(page.status, StatusCode::OK);
        assert_eq!(page.content_type.as_deref(), Some(HTML_CONTENT_TYPE));
        let html = &page.body;

        assert_eq!(tables(html), ["stats-all", "stats-bus"]);
        let bus = Kind::Vehicle(VehicleType::Bus);
        let everywhere = |value: &str| vec![value.to_owned(); Range::ALL.len()];
        assert_eq!(cells(html, bus, Metric::AverageSpeed), everywhere("36.0"));
        assert_eq!(cells(html, bus, Metric::MovingSpeed), everywhere("36.0"));
        assert_eq!(cells(html, bus, Metric::AverageVehicles), everywhere("1.0"));
        assert_eq!(cells(html, bus, Metric::PeakVehicles), everywhere("1"));
        assert_eq!(cells(html, bus, Metric::Distance), everywhere("3"));
        assert_eq!(
            cells(html, Kind::All, Metric::AverageSpeed),
            everywhere("36.0")
        );
        assert_eq!(
            select_attributes(html, "#stats-bus td", "data-range").len(),
            Range::ALL.len() * Metric::ALL.len()
        );
    }

    const BUS_LINE: &str = "a";
    const OTHER_BUS_LINE: &str = "b";
    const TRAM_LINE: &str = "1";

    #[tokio::test]
    async fn overview_uses_the_published_summary_while_charts_read_new_positions() {
        let db = db_with_a_bus().await;
        let state = AppState::new(
            db.clone(),
            Clock::Fixed(Timestamp::from_second(NOW).unwrap()),
        );
        state.summary.wait_until_ready().await;
        let tram = Position {
            vehicle_type: VehicleType::Tram,
            line: TRAM_LINE.to_owned(),
            course_id: 2,
            latitude: MicroDegrees(51_100_000),
            longitude: MicroDegrees(17_000_000),
        };
        db.save_positions(NOW - FETCH_SECS, &[&tram], &[])
            .await
            .unwrap();
        let page = send_to(app(state), Request::get(OVERVIEW_PATH)).await;
        assert_eq!(tables(&page.body), ["stats-all", "stats-bus"]);
        assert_eq!(
            series_names(&hover_data(&page.body, Metric::AverageVehicles.slug())),
            ["All vehicles", "Buses", "Trams"]
        );
        assert_eq!(
            select_attributes(&page.body, "#summary-updated time", "datetime"),
            [Timestamp::from_second(NOW).unwrap().to_string()]
        );
    }

    async fn db_with_a_bus_and_a_tram() -> Db {
        let db = db_with_a_bus().await;
        let tram = Position {
            vehicle_type: VehicleType::Tram,
            line: TRAM_LINE.to_owned(),
            course_id: 2,
            latitude: MicroDegrees(51_100_000),
            longitude: MicroDegrees(17_000_000),
        };
        let mut unix = NOW - DRIVE_SECS;
        while unix < NOW {
            db.save_positions(unix, &[&tram], &[]).await.unwrap();
            unix += FETCH_SECS;
        }
        db
    }

    #[tokio::test]
    async fn type_and_line_pages_show_only_their_vehicles() {
        let db = db_with_a_bus_and_a_tram().await;
        let everywhere = |value: &str| vec![value.to_owned(); Range::ALL.len()];
        let all = get(&db, "/").await.body;
        assert_eq!(tables(&all), ["stats-all", "stats-bus", "stats-tram"]);

        let bus_line = get(&db, &format!("/bus/{BUS_LINE}")).await;
        assert_eq!(bus_line.status, StatusCode::OK);
        let html = &bus_line.body;
        assert_eq!(select_texts(html, "h1"), [format!("Bus {BUS_LINE}")]);
        assert_eq!(tables(html), [format!("stats-bus-{BUS_LINE}")]);
        let line_cells = |metric: Metric| {
            select_texts(
                html,
                &format!("#stats-bus-{BUS_LINE} tr[data-metric={}] td", metric.slug()),
            )
        };
        assert_eq!(line_cells(Metric::AverageSpeed), everywhere("36.0"));
        assert_eq!(line_cells(Metric::AverageVehicles), everywhere("1.0"));
        assert_eq!(line_cells(Metric::Distance), everywhere("3"));
        let speed = hover_data(html, Metric::AverageSpeed.slug());
        assert_eq!(
            series_names(&speed),
            [
                format!("Bus {BUS_LINE}"),
                format!("Bus {BUS_LINE}, while moving")
            ]
        );

        let trams = get(&db, "/tram?range=week").await.body;
        assert_eq!(select_texts(&trams, "h1"), ["Trams"]);
        assert_eq!(tables(&trams), ["stats-tram"]);
        let tram = Kind::Vehicle(VehicleType::Tram);
        assert_eq!(cells(&trams, tram, Metric::AverageSpeed), everywhere("0.0"));
        assert_eq!(
            cells(&trams, tram, Metric::MovingSpeed),
            everywhere(MISSING_VALUE)
        );
        assert_eq!(
            series_names(&hover_data(&trams, Metric::AverageVehicles.slug())),
            ["Trams"]
        );

        let chart = get(
            &db,
            &format!("/bus/{BUS_LINE}/charts/average-speed.svg?range=month"),
        )
        .await;
        assert_eq!(chart.status, StatusCode::OK);
        assert_eq!(chart.content_type.as_deref(), Some(SVG_CONTENT_TYPE));
        let buses = get(&db, "/bus/charts/average-speed.svg?range=month").await;
        assert_eq!(
            chart
                .body
                .replace(&format!("\nBus {BUS_LINE}\n"), "\nBuses\n"),
            buses.body,
            "the only bus line draws like all buses, apart from its name"
        );
        assert!(!chart.body.lines().any(|line| line == "All vehicles"));
    }

    #[tokio::test]
    async fn period_pages_of_a_line_total_only_that_line() {
        let db = db_with_a_bus_and_a_tram().await;
        for kind in PeriodKind::ALL {
            let period = containing_the_drive(kind);
            let path = format!("/tram/{TRAM_LINE}{}", period.path());
            let page = get(&db, &path).await;
            assert_eq!(page.status, StatusCode::OK, "{path}");
            let html = &page.body;
            assert_eq!(
                select_texts(html, "h1"),
                [format!("Tram {TRAM_LINE}: {}", period.title(en()))]
            );
            assert_eq!(
                select_attributes(html, "#period-stats thead th[data-kind]", "data-kind"),
                [format!("tram-{TRAM_LINE}")]
            );
            assert_eq!(period_cells(html, Metric::AverageSpeed), ["0.0"]);
            assert_eq!(period_cells(html, Metric::AverageVehicles), ["1.0"]);
            assert_eq!(
                select_attributes(html, "figure.chart", "data-range"),
                vec![path.clone(); Chart::ALL.len()]
            );
        }
        let week = containing_the_drive(PeriodKind::Week);
        let buses = get(&db, &format!("/bus{}", week.path())).await.body;
        assert_eq!(
            select_attributes(&buses, "#period-stats thead th[data-kind]", "data-kind"),
            ["bus"]
        );
        assert_eq!(period_cells(&buses, Metric::Distance), ["3"]);
    }

    #[tokio::test]
    async fn links_keep_the_selection_and_the_period() {
        let db = db_with_a_bus_and_a_tram().await;
        let week = containing_the_drive(PeriodKind::Week);
        let line_prefix = format!("/bus/{BUS_LINE}");
        let html = get(&db, &format!("{line_prefix}{}", week.path()))
            .await
            .body;
        assert_eq!(
            select_attributes(&html, "#vehicles a", "href"),
            [
                week.path(),
                format!("/bus{}", week.path()),
                format!("/tram{}", week.path()),
            ]
        );
        assert!(select_texts(&html, "#vehicles a[aria-current=page]").is_empty());
        assert_eq!(
            select_attributes(&html, "#lines a", "href"),
            [format!("{line_prefix}{}", week.path())]
        );
        assert_eq!(
            select_texts(&html, "#lines a[aria-current=page]"),
            [BUS_LINE]
        );
        assert!(select_attributes(&html, "#previous", "href").is_empty());
        let mut breadcrumbs = vec![line_prefix.clone()];
        breadcrumbs.extend(
            week.parents()
                .unwrap()
                .iter()
                .map(|parent| format!("{line_prefix}{}", parent.path())),
        );
        assert_eq!(
            select_attributes(&html, ".breadcrumbs a", "href"),
            breadcrumbs
        );
        let days = select_attributes(&html, "#parts a", "href");
        assert_eq!(days.len(), 7);
        assert!(days.iter().all(|day| day.starts_with(&line_prefix)));
        assert_eq!(
            select_attributes(&html, "#quality-link", "href"),
            [format!("{line_prefix}{}/quality", week.path())]
        );

        let all = get(&db, &week.path()).await.body;
        assert_eq!(
            select_attributes(&all, "#lines nav", "data-kind"),
            ["bus", "tram"]
        );
        assert_eq!(
            select_attributes(&all, "#lines a", "href"),
            [
                format!("{line_prefix}{}", week.path()),
                format!("/tram/{TRAM_LINE}{}", week.path()),
            ]
        );

        let trams = get(&db, "/tram?range=week").await.body;
        assert_eq!(
            select_attributes(&trams, "#vehicles a", "href"),
            ["/?range=week", "/bus?range=week", "/tram?range=week"]
        );
        assert_eq!(
            select_texts(&trams, "#vehicles a[aria-current=page]"),
            ["Trams"]
        );
        assert_eq!(
            select_attributes(&trams, "#charts nav a", "href"),
            [
                "/tram?range=hour",
                "/tram?range=day",
                "/tram?range=week",
                "/tram?range=month"
            ]
        );
        assert_eq!(
            select_attributes(&trams, "#lines a", "href"),
            [format!("/tram/{TRAM_LINE}?range=week")]
        );
        let history = select_attributes(&trams, "#history a", "href");
        assert_eq!(
            history[0],
            format!("/tram{}", containing(PeriodKind::Day).path())
        );
    }

    fn step_value_texts(hover: &serde_json::Value) -> Vec<String> {
        hover["steps"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(step_texts)
            .collect()
    }

    #[tokio::test]
    async fn line_averages_skip_hours_without_its_vehicles() {
        let db = empty_db().await;
        let tz = crate::time::warsaw();
        let day = local_date(NOW).yesterday().unwrap();
        let at = |hour| {
            day.at(hour, 0, 0, 0)
                .to_zoned(tz.clone())
                .unwrap()
                .timestamp()
                .as_second()
        };
        let parked = |vehicle_type, line: &str, course_id| Position {
            vehicle_type,
            line: line.to_owned(),
            course_id,
            latitude: MicroDegrees(51_100_000),
            longitude: MicroDegrees(17_000_000),
        };
        let tram = parked(VehicleType::Tram, TRAM_LINE, 2);
        let bus = parked(VehicleType::Bus, BUS_LINE, 1);
        let bus_hours = at(10)..at(11);
        let mut unix = at(9);
        while unix < at(12) {
            let seen: Vec<&Position> = if bus_hours.contains(&unix) {
                vec![&tram, &bus]
            } else {
                vec![&tram]
            };
            db.save_positions(unix, &seen, &[]).await.unwrap();
            unix += FETCH_SECS;
        }
        let period = Period::containing(PeriodKind::Day, day).unwrap();

        let line = get(&db, &format!("/bus/{BUS_LINE}{}", period.path()))
            .await
            .body;
        assert_eq!(period_cells(&line, Metric::AverageVehicles), ["1.0"]);
        assert_eq!(period_cells(&line, Metric::PeakVehicles), ["1"]);
        let line_steps = step_value_texts(&hover_data(&line, Metric::AverageVehicles.slug()));
        assert!(line_steps.contains(&"1.0".to_owned()), "{line_steps:?}");
        assert!(!line_steps.contains(&"0.0".to_owned()), "{line_steps:?}");

        let buses = get(&db, &format!("/bus{}", period.path())).await.body;
        assert_eq!(period_cells(&buses, Metric::AverageVehicles), ["0.3"]);
        let bus_steps = step_value_texts(&hover_data(&buses, Metric::AverageVehicles.slug()));
        assert!(bus_steps.contains(&"0.0".to_owned()), "{bus_steps:?}");

        let tram_line = get(&db, &format!("/tram/{TRAM_LINE}{}", period.path()))
            .await
            .body;
        assert_eq!(period_cells(&tram_line, Metric::AverageVehicles), ["1.0"]);
    }

    #[tokio::test]
    async fn unknown_lines_are_not_found() {
        let db = db_with_a_bus().await;
        let day = containing_the_drive(PeriodKind::Day);
        for uri in [
            "/bus/zzz".to_owned(),
            format!("/tram/{BUS_LINE}"),
            format!("/bus/zzz{}", day.path()),
            format!("/bus/{BUS_LINE}/day/2027-13-01"),
            "/bus/zzz/charts/average-speed.svg".to_owned(),
            "/bus/zzz/map.svg".to_owned(),
            format!("/bus/{BUS_LINE}/charts/distance.svg"),
            "/ship".to_owned(),
        ] {
            assert_eq!(get(&db, &uri).await.status, StatusCode::NOT_FOUND, "{uri}");
        }
        assert_eq!(
            get(&db, &format!("/bus/{BUS_LINE}?range=year"))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
        let empty_type = get(&db, "/train").await;
        assert_eq!(empty_type.status, StatusCode::OK);
        assert_eq!(tables(&empty_type.body), ["stats-train"]);
    }

    #[tokio::test]
    async fn shows_missing_values_without_data() {
        let db = empty_db().await;
        let html = get(&db, "/").await.body;
        assert_eq!(tables(&html), ["stats-all"]);
        for metric in Metric::ALL {
            assert_eq!(
                cells(&html, Kind::All, metric),
                vec![MISSING_VALUE.to_owned(); Range::ALL.len()]
            );
        }
    }

    #[tokio::test]
    async fn links_charts_of_the_selected_range() {
        let db = db_with_a_bus().await;
        let default = get(&db, "/").await.body;
        assert_eq!(
            select_texts(&default, "#charts nav a[aria-current=page]"),
            [en().range(Range::default())]
        );

        let html = get(&db, "/?range=week").await.body;
        assert_eq!(
            select_texts(&html, "#charts nav a[aria-current=page]"),
            [en().range(Range::Week)]
        );
        assert_eq!(
            select_attributes(&html, "#charts nav a", "href"),
            [
                "/?range=hour",
                "/?range=day",
                "/?range=week",
                "/?range=month"
            ]
        );
        assert_eq!(
            select_attributes(&html, "figure.chart", "id"),
            ["chart-average-speed", "chart-average-vehicles"]
        );
        assert_eq!(
            select_attributes(&html, "figure.chart", "data-range"),
            ["week", "week"]
        );
        assert_eq!(
            select_texts(&html, "figure.chart .chart-plot > svg").len(),
            Chart::ALL.len()
        );
        assert!(select_texts(&html, "body > script")[0].contains("chart-data"));
    }

    #[tokio::test]
    async fn every_page_starts_with_its_charts() {
        let db = db_with_a_bus().await;
        let day = containing(PeriodKind::Day).slug();
        for uri in [
            "/".to_owned(),
            format!("/day/{day}"),
            "/quality".to_owned(),
            format!("/day/{day}/quality"),
        ] {
            let html = get(&db, &uri).await.body;
            assert_eq!(
                select_attributes(&html, "main > *:first-child", "id"),
                ["charts"],
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn every_page_has_a_description_starting_with_its_heading() {
        let db = db_with_a_bus().await;
        let day = containing(PeriodKind::Day).slug();
        for (uri, text) in [
            ("/".to_owned(), Text::StatisticsDescription),
            ("/bus".to_owned(), Text::StatisticsDescription),
            (format!("/day/{day}"), Text::StatisticsDescription),
            ("/quality".to_owned(), Text::QualityDescription),
            (format!("/day/{day}/quality"), Text::QualityDescription),
            (ABOUT_PATH.to_owned(), Text::AboutDescription),
        ] {
            let html = get(&db, &uri).await.body;
            let [heading] = &select_texts(&html, "h1")[..] else {
                panic!("expected one heading on {uri}");
            };
            assert_eq!(
                select_attributes(&html, "meta[name=description]", "content"),
                [format!("{heading}. {}", en().text(text))],
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn about_page_describes_every_topic_in_the_language_of_the_browser() {
        let db = empty_db().await;
        let overview = get(&db, OVERVIEW_PATH).await.body;
        assert_eq!(
            select_attributes(&overview, "#about-link", "href"),
            [ABOUT_PATH]
        );
        for language in [Language::English, Language::Polish] {
            let tr = language.translation();
            let fetched = get_in(&db, ABOUT_PATH, language.tag()).await;
            assert_eq!(fetched.status, StatusCode::OK);
            assert_eq!(
                header_value(&fetched, header::CACHE_CONTROL),
                Some(LIVE_CACHE_CONTROL)
            );
            assert_eq!(select_texts(&fetched.body, "h1"), [tr.text(Text::About)]);
            assert_eq!(
                select_texts(&fetched.body, "main section h2"),
                about::AboutTopic::ALL.map(|topic| tr.about(topic).heading)
            );
            assert_eq!(
                select_attributes(&fetched.body, "main section a", "href"),
                ["/yesterday", "/prev_week", "/prev_month"]
            );
            for href in select_attributes(&fetched.body, "main a", "href") {
                let status = get(&db, &href).await.status;
                assert!(status.is_success() || status.is_redirection(), "{href}");
            }
        }
    }

    fn hover_data(html: &str, chart: &str) -> serde_json::Value {
        let [data] = &select_texts(html, &format!("#chart-{chart} .chart-data"))[..] else {
            panic!("expected hover data for {chart}");
        };
        serde_json::from_str(data).unwrap()
    }

    fn series_names(hover: &serde_json::Value) -> Vec<String> {
        hover["series"]
            .as_array()
            .unwrap()
            .iter()
            .map(|series| series["name"].as_str().unwrap().to_owned())
            .collect()
    }

    fn step_texts(step: &serde_json::Value) -> Vec<String> {
        step["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value["text"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn embeds_hover_values_for_every_chart_step() {
        let db = db_with_a_bus().await;
        let html = get(&db, "/?range=week").await.body;
        let speed = hover_data(&html, Metric::AverageSpeed.slug());
        let names: Vec<&str> = speed["series"]
            .as_array()
            .unwrap()
            .iter()
            .map(|series| series["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "All vehicles",
                "All vehicles, while moving",
                "Buses",
                "Buses, while moving"
            ]
        );
        let strokes: Vec<&str> = speed["series"]
            .as_array()
            .unwrap()
            .iter()
            .map(|series| series["stroke"].as_str().unwrap())
            .collect();
        assert_eq!(strokes, ["solid", "dashed", "solid", "dashed"]);
        let steps = speed["steps"].as_array().unwrap();
        let offset_secs = crate::time::warsaw()
            .to_offset(Timestamp::from_second(NOW).unwrap())
            .seconds();
        let partial_hours = usize::from(offset_secs % HOUR_SECS != 0);
        assert_eq!(steps.len(), 7 * 24 + partial_hours);
        let texts = |step: &serde_json::Value| -> Vec<String> {
            step["values"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value["text"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(texts(&steps[0]), [MISSING_VALUE; 4]);
        assert_eq!(steps[0]["values"][0]["y"], serde_json::Value::Null);
        let last = steps.last().unwrap();
        assert_eq!(texts(last), ["36.0"; 4]);
        assert!(last["values"][1]["y"].is_i64());
        let vehicles = hover_data(&html, Metric::AverageVehicles.slug());
        assert_eq!(
            texts(vehicles["steps"].as_array().unwrap().last().unwrap()),
            ["1.0", "1.0"]
        );
    }

    #[test]
    fn hover_data_cannot_close_its_script_early() {
        let rendered = Rendered {
            svg: String::new(),
            hover: charts::Hover {
                width: charts::WIDTH,
                height: charts::HEIGHT,
                plot: charts::PlotArea {
                    left: 0,
                    top: 0,
                    right: 1,
                    bottom: 1,
                },
                series: vec![charts::HoverSeries {
                    name: "</script><b>".to_owned(),
                    color: String::new(),
                    stroke: Stroke::Solid,
                }],
                steps: Vec::new(),
            },
        };
        let html = chart_figure(
            Metric::AverageSpeed.slug(),
            en().metric(Metric::AverageSpeed),
            "last day",
            Range::Day.slug(),
            &rendered,
        )
        .unwrap()
        .into_string();
        assert_eq!(html.matches("</script>").count(), 1, "{html}");
        let [data] = &select_texts(&html, ".chart-data")[..] else {
            panic!("{html}");
        };
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(parsed["series"][0]["name"], "</script><b>");
    }

    #[tokio::test]
    async fn rejects_unknown_ranges() {
        let db = empty_db().await;
        assert_eq!(
            get(&db, "/?range=year").await.status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            get(&db, "/charts/average-speed.svg?range=year")
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn draws_charts_with_a_line_per_kind() {
        let db = db_with_a_bus().await;
        for chart_kind in Chart::ALL {
            for range in Range::ALL {
                let uri = format!(
                    "/charts/{}{SVG_EXTENSION}?range={}",
                    chart_kind.slug(),
                    range.slug()
                );
                let chart = get(&db, &uri).await;
                assert_eq!(chart.status, StatusCode::OK, "{uri}");
                assert_eq!(chart.content_type.as_deref(), Some(SVG_CONTENT_TYPE));
                let lines: Vec<&str> = chart.body.lines().collect();
                assert!(lines.contains(&"All vehicles"), "{uri}");
                assert!(lines.contains(&"Buses"), "{uri}");
                assert!(!lines.contains(&"Trams"), "{uri}");
            }
        }
    }

    fn local_date(unix: i64) -> Date {
        Timestamp::from_second(unix)
            .unwrap()
            .to_zoned(crate::time::warsaw())
            .date()
    }

    fn containing(kind: PeriodKind) -> Period {
        Period::containing(kind, local_date(NOW)).unwrap()
    }

    fn containing_the_drive(kind: PeriodKind) -> Period {
        Period::containing(kind, local_date(NOW - FETCH_SECS)).unwrap()
    }

    fn contains_now(period: Period) -> bool {
        let (from, to) = period.bounds(&crate::time::warsaw()).unwrap();
        (from.as_second()..to.as_second()).contains(&NOW)
    }

    fn period_cells(html: &str, metric: Metric) -> Vec<String> {
        select_texts(
            html,
            &format!("#period-stats tr[data-metric={}] td", metric.slug()),
        )
    }

    #[tokio::test]
    async fn period_pages_total_everything_in_the_period() {
        let db = db_with_a_bus().await;
        for kind in PeriodKind::ALL {
            let period = containing_the_drive(kind);
            let page = get(&db, &period.path()).await;
            assert_eq!(page.status, StatusCode::OK, "{}", period.path());
            let html = &page.body;
            assert_eq!(select_texts(html, "h1"), [period.title(en())]);
            assert_eq!(
                select_attributes(html, "#period-stats thead th[data-kind]", "data-kind"),
                ["all", "bus"]
            );
            assert_eq!(period_cells(html, Metric::AverageSpeed), ["36.0", "36.0"]);
            assert_eq!(period_cells(html, Metric::AverageVehicles), ["1.0", "1.0"]);
            assert_eq!(period_cells(html, Metric::PeakVehicles), ["1", "1"]);
            assert_eq!(period_cells(html, Metric::Distance), ["3", "3"]);
            let so_far = select_texts(html, "#updated");
            if contains_now(period) {
                assert!(so_far[0].starts_with("So far"), "{so_far:?}");
                assert!(select_attributes(html, "#next", "href").is_empty());
            } else {
                assert!(so_far.is_empty(), "{so_far:?}");
                assert_eq!(
                    select_attributes(html, "#next", "href"),
                    [period.next().unwrap().path()]
                );
            }
            assert_eq!(
                select_attributes(html, "figure.chart", "data-range"),
                vec![period.path(); Chart::ALL.len()]
            );
        }
    }

    #[tokio::test]
    async fn previous_period_links_require_data_for_the_selection() {
        for kind in PeriodKind::ALL {
            let db = db_with_a_bus().await;
            let bus = Selection::Line(db.line(VehicleType::Bus, BUS_LINE).await.unwrap().unwrap());
            let period = containing_the_drive(kind);
            record_period(&db, period.next().unwrap()).await;
            for selection in [&Selection::All, &Selection::Type(VehicleType::Bus), &bus] {
                let html = get(&db, &selection.period_path(period)).await.body;
                assert!(select_attributes(&html, "#previous", "href").is_empty());
                let next = period.next().unwrap();
                let html = get(&db, &selection.period_path(next)).await.body;
                assert_eq!(
                    select_attributes(&html, "#previous", "href"),
                    [selection.period_path(period)]
                );
                assert_root_redirect(&get(&db, &selection.period_path(next.next().unwrap())).await);
            }
            let tram = Selection::Type(VehicleType::Tram);
            assert_root_redirect(&get(&db, &tram.period_path(period.next().unwrap())).await);
        }
    }

    #[tokio::test]
    async fn previous_quality_period_links_use_quality_records() {
        for kind in PeriodKind::ALL {
            let db = db_with_quality_counts().await;
            let bus = Selection::Line(db.line(VehicleType::Bus, BUS_LINE).await.unwrap().unwrap());
            let period = Period::containing(kind, local_date(QUALITY_RECENTLY)).unwrap();
            record_period(&db, period.next().unwrap()).await;
            for selection in [&Selection::All, &Selection::Type(VehicleType::Bus), &bus] {
                let html = get(&db, &selection.period_quality_path(period)).await.body;
                assert!(select_attributes(&html, "#previous", "href").is_empty());
                let next = period.next().unwrap();
                let html = get(&db, &selection.period_quality_path(next)).await.body;
                assert_eq!(
                    select_attributes(&html, "#previous", "href"),
                    [selection.period_quality_path(period)]
                );
                assert_root_redirect(
                    &get(&db, &selection.period_quality_path(next.next().unwrap())).await,
                );
            }
        }
    }

    fn assert_root_redirect(fetched: &Fetched) {
        assert_eq!(fetched.status, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(header_value(fetched, header::LOCATION), Some(OVERVIEW_PATH));
        assert_eq!(header_value(fetched, header::ETAG), None);
        assert_eq!(
            header_value(fetched, header::CACHE_CONTROL),
            Some(LIVE_CACHE_CONTROL)
        );
    }

    #[tokio::test]
    async fn empty_periods_redirect_to_root_even_with_conditional_requests() {
        let db = db_with_a_bus().await;
        let bus = Selection::Line(db.line(VehicleType::Bus, BUS_LINE).await.unwrap().unwrap());
        for kind in PeriodKind::ALL {
            let period = containing_the_drive(kind);
            for selection in [&Selection::All, &Selection::Type(VehicleType::Bus), &bus] {
                for empty in [period.previous().unwrap(), period.next().unwrap()] {
                    for path in [
                        selection.period_path(empty),
                        selection.period_quality_path(empty),
                    ] {
                        assert_root_redirect(&get(&db, &path).await);
                        assert_root_redirect(&get_if_none_match(&db, &path, "*").await);
                    }
                }
            }
            let tram = Selection::Type(VehicleType::Tram);
            for path in [tram.period_path(period), tram.period_quality_path(period)] {
                assert_root_redirect(&get(&db, &path).await);
            }
        }
        assert_root_redirect(&get(&db, "/year/1900").await);
        assert_eq!(get(&db, OVERVIEW_PATH).await.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn period_pages_link_to_larger_and_smaller_periods() {
        let db = db_with_a_bus().await;
        let day = containing_the_drive(PeriodKind::Day);
        let html = get(&db, &day.path()).await.body;
        let mut expected = vec![OVERVIEW_PATH.to_owned()];
        expected.extend(day.parents().unwrap().iter().map(|parent| parent.path()));
        assert_eq!(select_attributes(&html, ".breadcrumbs a", "href"), expected);
        assert!(select_texts(&html, "#parts").is_empty());

        let month = containing_the_drive(PeriodKind::Month);
        let html = get(&db, &month.path()).await.body;
        let days: Vec<String> = month
            .parts()
            .unwrap()
            .iter()
            .map(|day| day.path())
            .collect();
        assert_eq!(select_attributes(&html, "#parts a", "href"), days);
        assert!(days.contains(&day.path()));

        let html = get(&db, &containing(PeriodKind::Year).path()).await.body;
        assert_eq!(select_attributes(&html, "#parts a", "href").len(), 12);
    }

    #[tokio::test]
    async fn overview_links_to_recent_periods() {
        let db = empty_db().await;
        let html = get(&db, "/").await.body;
        let day = containing(PeriodKind::Day);
        let week = containing(PeriodKind::Week);
        let month = containing(PeriodKind::Month);
        assert_eq!(
            select_attributes(&html, "#history a", "href"),
            [
                day.path(),
                day.previous().unwrap().path(),
                week.path(),
                week.previous().unwrap().path(),
                month.path(),
                month.previous().unwrap().path(),
                containing(PeriodKind::Year).path(),
            ]
        );
    }

    #[tokio::test]
    async fn recent_periods_redirect_to_the_calendar_page_before_today() {
        let db = db_with_a_bus().await;
        let day = containing(PeriodKind::Day).previous().unwrap();
        let week = containing(PeriodKind::Week).previous().unwrap();
        let month = containing(PeriodKind::Month).previous().unwrap();
        for (uri, target) in [
            ("/yesterday", day.path()),
            ("/prev_week", week.path()),
            ("/prev_month", month.path()),
            ("/prev_week/quality", format!("{}/quality", week.path())),
            ("/tram/yesterday", format!("/tram{}", day.path())),
            ("/bus/a/prev_month", format!("/bus/a{}", month.path())),
            (
                "/bus/a/yesterday/quality",
                format!("/bus/a{}/quality", day.path()),
            ),
        ] {
            let fetched = get(&db, uri).await;
            assert_eq!(fetched.status, StatusCode::TEMPORARY_REDIRECT, "{uri}");
            assert_eq!(
                header_value(&fetched, header::LOCATION),
                Some(target.as_str()),
                "{uri}"
            );
            assert_eq!(
                header_value(&fetched, header::CACHE_CONTROL),
                Some(LIVE_CACHE_CONTROL),
                "{uri}"
            );
            assert_root_redirect(&get(&db, &target).await);
        }
        assert_eq!(
            get(&db, "/bus/unknown/yesterday").await.status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn rejects_malformed_periods() {
        let db = empty_db().await;
        for uri in [
            "/day/2027-13-01",
            "/day/2027-1-15",
            "/day/today",
            "/week/2027-W60",
            "/week/2027-02",
            "/month/2027-1",
            "/year/27",
            "/decade/2020",
        ] {
            assert_eq!(get(&db, uri).await.status, StatusCode::NOT_FOUND, "{uri}");
        }
    }

    async fn db_with_quality_counts() -> Db {
        let db = empty_db().await;
        let url = crate::fetch::tests::refused_url();
        let error = crate::fetch::send(Source::MpkPositions, reqwest::Client::new().get(&url))
            .await
            .err()
            .unwrap();
        let recently = NOW - 2 * FETCH_SECS;
        for failed in [None, None, Some(&error)] {
            db.save_fetch_outcome(Source::MpkPositions, recently, failed)
                .await
                .unwrap();
        }
        db.save_fetch_outcome(
            Source::OpenMeteo,
            NOW - 2 * i64::from(HOUR_SECS),
            Some(&error),
        )
        .await
        .unwrap();
        let count = |vehicle_type, line: &str, received, invalid| LinePositionCount {
            vehicle_type,
            line: line.to_owned(),
            received,
            invalid,
        };
        db.save_positions_with_counts(
            recently,
            &[],
            &[],
            &[
                count(VehicleType::Bus, BUS_LINE, 6, 2),
                count(VehicleType::Bus, OTHER_BUS_LINE, 4, 0),
                count(VehicleType::Tram, TRAM_LINE, 5, 0),
            ],
        )
        .await
        .unwrap();
        db
    }

    const QUALITY_RECENTLY: i64 = NOW - 2 * FETCH_SECS;

    fn text(html: &str, selector: &str) -> String {
        select_texts(html, selector).join("")
    }

    #[tokio::test]
    async fn statistics_pages_link_to_data_quality() {
        let db = db_with_quality_counts().await;
        record_period(&db, containing_the_drive(PeriodKind::Day)).await;
        let html = get(&db, "/").await.body;
        assert!(select_texts(&html, "#fetch-failures").is_empty());
        assert_eq!(
            select_attributes(&html, "#quality-link", "href"),
            [OVERVIEW_QUALITY_PATH]
        );
        let day = Period::containing(PeriodKind::Day, local_date(QUALITY_RECENTLY)).unwrap();
        let html = get(&db, &day.path()).await.body;
        assert!(select_texts(&html, "#invalid-positions").is_empty());
        assert_eq!(
            select_attributes(&html, "#quality-link", "href"),
            [format!("{}/quality", day.path())]
        );
    }

    #[tokio::test]
    async fn quality_overview_shows_only_percentages() {
        let db = db_with_quality_counts().await;
        let html = get(&db, OVERVIEW_QUALITY_PATH).await.body;
        let fetch = |source: &str, range: &str| {
            text(
                &html,
                &format!("#fetch-failures tr[data-source={source}] td[data-range={range}]"),
            )
        };
        assert_eq!(fetch("mpk-positions", "hour"), "33.3%");
        assert_eq!(fetch("mpk-positions", "month"), "33.3%");
        assert_eq!(fetch("open-meteo", "hour"), MISSING_VALUE);
        assert_eq!(fetch("open-meteo", "day"), "100.0%");
        assert_eq!(fetch("mpk-lines", "week"), MISSING_VALUE);
        assert_eq!(
            select_attributes(&html, "#fetch-failures tbody tr", "data-source"),
            ["mpk-positions", "mpk-lines", "open-meteo", "open-data"]
        );
        let positions = |kind: &str| {
            text(
                &html,
                &format!("#invalid-positions tr[data-kind={kind}] td[data-range=hour]"),
            )
        };
        assert_eq!(positions("all"), "13.3%");
        assert_eq!(positions("bus"), "20.0%");
        assert_eq!(positions("tram"), "0%");
        assert_eq!(
            select_attributes(&html, "#invalid-positions tbody tr", "data-kind"),
            ["all", "bus", "tram"]
        );
        assert!(
            !text(&html, "main").contains(" of "),
            "counts must not be shown"
        );
    }

    #[tokio::test]
    async fn quality_charts_show_when_failures_happened() {
        let db = db_with_quality_counts().await;
        let html = get(&db, &format!("{OVERVIEW_QUALITY_PATH}?range=hour"))
            .await
            .body;
        let failed = hover_data(&html, "failed-downloads");
        assert_eq!(series_names(&failed), ["MPK positions"]);
        let steps = failed["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 60);
        let last = steps.last().unwrap();
        assert_eq!(step_texts(last), ["33.3%"]);
        assert!(last["values"][0]["y"].is_i64());
        assert_eq!(step_texts(&steps[0]), [MISSING_VALUE]);
        assert!(steps[0]["values"][0]["y"].is_null());

        let invalid = hover_data(&html, "invalid-positions");
        assert_eq!(series_names(&invalid), ["All vehicles", "Buses", "Trams"]);
        let last = invalid["steps"].as_array().unwrap().last().unwrap();
        assert_eq!(step_texts(last), ["13.3%", "20.0%", "0%"]);

        let day = get(&db, &format!("{OVERVIEW_QUALITY_PATH}?range=day"))
            .await
            .body;
        assert_eq!(
            series_names(&hover_data(&day, "failed-downloads")),
            ["MPK positions", "Open-Meteo weather"]
        );
    }

    #[tokio::test]
    async fn period_quality_pages_show_the_period() {
        let db = db_with_quality_counts().await;
        let day = Period::containing(PeriodKind::Day, local_date(QUALITY_RECENTLY)).unwrap();
        let html = get(&db, &Selection::All.period_quality_path(day))
            .await
            .body;
        assert_eq!(
            select_texts(&html, "h1"),
            [format!("Data quality: {}", day.title(en()))]
        );
        assert_eq!(
            text(&html, "#fetch-failures tr[data-source=mpk-positions] td"),
            "33.3%"
        );
        assert_eq!(
            text(&html, "#fetch-failures tr[data-source=open-data] td"),
            MISSING_VALUE
        );
        assert_eq!(
            text(&html, "#invalid-positions tr[data-kind=all] td"),
            "13.3%"
        );
        assert_eq!(
            select_attributes(&html, "#statistics-link", "href"),
            [day.path()]
        );
        assert!(select_attributes(&html, "#previous", "href").is_empty());
        let mut breadcrumbs = vec![OVERVIEW_QUALITY_PATH.to_owned()];
        breadcrumbs.extend(
            day.parents()
                .unwrap()
                .into_iter()
                .map(|period| Selection::All.period_quality_path(period)),
        );
        assert_eq!(
            select_attributes(&html, ".breadcrumbs a", "href"),
            breadcrumbs
        );
        let failed = hover_data(&html, "failed-downloads");
        assert_eq!(
            select_attributes(&html, "#chart-failed-downloads", "data-range"),
            [Selection::All.period_quality_path(day)]
        );
        assert_eq!(series_names(&failed)[0], "MPK positions");
        assert!(
            failed["steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step_texts(step)[0] == "33.3%")
        );

        let month = Period::containing(PeriodKind::Month, local_date(QUALITY_RECENTLY)).unwrap();
        let html = get(&db, &Selection::All.period_quality_path(month))
            .await
            .body;
        let days: Vec<String> = month
            .parts()
            .unwrap()
            .into_iter()
            .map(|period| Selection::All.period_quality_path(period))
            .collect();
        assert_eq!(select_attributes(&html, "#parts a", "href"), days);

        let before = day.previous().unwrap().previous().unwrap();
        assert_root_redirect(&get(&db, &Selection::All.period_quality_path(before)).await);
        assert_eq!(
            get(&db, "/day/2027-13-01/quality").await.status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn quality_pages_of_a_type_or_line_show_only_their_positions() {
        let db = db_with_quality_counts().await;
        let invalid = |html: &str, range: &str| {
            text(
                html,
                &format!("#invalid-positions tbody tr td[data-range={range}]"),
            )
        };
        let failed = |html: &str| {
            text(
                html,
                "#fetch-failures tr[data-source=mpk-positions] td[data-range=hour]",
            )
        };

        let line = get(&db, &format!("/bus/{BUS_LINE}/quality?range=hour")).await;
        assert_eq!(line.status, StatusCode::OK);
        let html = &line.body;
        assert_eq!(
            select_texts(html, "h1"),
            [format!("Data quality: Bus {BUS_LINE}")]
        );
        assert_eq!(
            select_attributes(html, "#invalid-positions tbody tr", "data-kind"),
            [format!("bus-{BUS_LINE}")]
        );
        assert_eq!(invalid(html, "hour"), "33.3%");
        assert!(select_texts(html, "#fetch-failures").is_empty());
        assert_eq!(
            select_attributes(html, "figure.chart", "id"),
            ["chart-invalid-positions"]
        );
        assert_eq!(
            series_names(&hover_data(html, "invalid-positions")),
            [format!("Bus {BUS_LINE}")]
        );
        assert_eq!(
            select_attributes(html, "#charts nav a", "href")[0],
            format!("/bus/{BUS_LINE}/quality?range=hour")
        );
        assert_eq!(
            select_attributes(html, ".breadcrumbs a", "href"),
            [format!("/bus/{BUS_LINE}")]
        );

        let buses = get(&db, "/bus/quality").await.body;
        assert_eq!(
            select_attributes(&buses, "#invalid-positions tbody tr", "data-kind"),
            ["bus"]
        );
        assert_eq!(invalid(&buses, "day"), "20.0%");
        assert!(select_texts(&buses, "#fetch-failures").is_empty());
        let all = get(&db, OVERVIEW_QUALITY_PATH).await.body;
        assert_eq!(failed(&all), "33.3%");
        assert!(!select_texts(&all, "#chart-failed-downloads").is_empty());

        let day = Period::containing(PeriodKind::Day, local_date(QUALITY_RECENTLY)).unwrap();
        let other = Selection::Line(
            db.line(VehicleType::Bus, OTHER_BUS_LINE)
                .await
                .unwrap()
                .unwrap(),
        );
        let html = get(&db, &other.period_quality_path(day)).await.body;
        assert_eq!(
            select_texts(&html, "h1"),
            [format!(
                "Data quality: Bus {OTHER_BUS_LINE}, {}",
                day.title(en())
            )]
        );
        assert_eq!(text(&html, "#invalid-positions tbody tr td"), "0%");
        assert!(select_texts(&html, "#fetch-failures").is_empty());
        assert_eq!(
            select_attributes(&html, "figure.chart", "id"),
            ["chart-invalid-positions"]
        );
        assert_eq!(
            select_attributes(&html, "#statistics-link", "href"),
            [other.period_path(day)]
        );
        assert!(select_attributes(&html, "#previous", "href").is_empty());
        assert_eq!(
            select_attributes(&html, "#vehicles a", "href")[0],
            Selection::All.period_quality_path(day)
        );

        assert_root_redirect(&get(&db, &other.period_path(day)).await);
    }

    #[test]
    fn lists_chart_steps_in_local_time() {
        let tz = crate::time::warsaw();
        let start = |year, month, day| {
            jiff::civil::date(year, month, day)
                .to_zoned(tz.clone())
                .unwrap()
                .timestamp()
                .as_second()
        };
        let local_hours = |starts: &[i64]| -> Vec<String> {
            starts
                .iter()
                .map(|unix| {
                    Timestamp::from_second(*unix)
                        .unwrap()
                        .to_zoned(tz.clone())
                        .strftime("%d %H:%M")
                        .to_string()
                })
                .collect()
        };
        let days = ChartStep::Days.starts(start(2026, 10, 24), start(2026, 10, 27), &tz);
        assert_eq!(local_hours(&days), ["24 00:00", "25 00:00", "26 00:00"]);
        let quarters = ChartStep::Hours(6).starts(start(2026, 10, 25), start(2026, 10, 26), &tz);
        assert_eq!(
            local_hours(&quarters),
            ["25 00:00", "25 06:00", "25 12:00", "25 18:00"]
        );
        let spring = ChartStep::Hours(6).starts(start(2026, 3, 29), start(2026, 3, 30), &tz);
        assert_eq!(
            local_hours(&spring),
            ["29 00:00", "29 06:00", "29 12:00", "29 18:00"]
        );
        assert_eq!(
            ChartStep::Minutes(15)
                .starts(start(2026, 3, 29), start(2026, 3, 30), &tz)
                .len(),
            23 * 4
        );
        assert_eq!(
            ChartStep::Hours(1)
                .starts(start(2026, 10, 25), start(2026, 10, 26), &tz)
                .len(),
            25
        );
        let unaligned =
            ChartStep::Minutes(1).starts(start(2026, 1, 1) + 30, start(2026, 1, 1) + 150, &tz);
        assert_eq!(unaligned.len(), 3);
    }

    #[tokio::test]
    async fn speaks_the_language_the_browser_prefers() {
        let db = db_with_a_bus().await;
        let page = get_in(&db, "/", POLISH_BROWSER).await;
        assert_eq!(page.status, StatusCode::OK);
        assert_eq!(header_value(&page, header::CONTENT_LANGUAGE), Some("pl"));
        assert_eq!(
            header_value(&page, header::VARY),
            Some(header::ACCEPT_LANGUAGE.as_str())
        );
        let html = &page.body;
        assert_eq!(select_attributes(html, "html", "lang"), ["pl"]);
        assert_eq!(select_texts(html, "h1"), ["Statystyki MPK Wrocław"]);
        assert_eq!(
            select_texts(html, "#charts nav a[aria-current=page]"),
            ["Ostatnia doba"]
        );
        let bus = Kind::Vehicle(VehicleType::Bus);
        assert_eq!(
            cells(html, bus, Metric::AverageSpeed),
            vec!["36,0"; Range::ALL.len()]
        );
        assert_eq!(select_texts(html, "#stats-bus caption"), ["Autobusy"]);
        assert_eq!(
            series_names(&hover_data(html, Metric::AverageSpeed.slug())),
            [
                "Wszystkie pojazdy",
                "Wszystkie pojazdy, w ruchu",
                "Autobusy",
                "Autobusy, w ruchu"
            ]
        );
        assert_eq!(
            select_texts(html, "#history a")[..2],
            ["Dzisiaj", "Wczoraj"]
        );

        for accept_language in ["", "de-DE,de;q=0.9", "pl;q=0"] {
            let page = get_in(&db, "/", accept_language).await;
            assert_eq!(
                header_value(&page, header::CONTENT_LANGUAGE),
                Some("en"),
                "{accept_language}"
            );
            assert_eq!(select_texts(&page.body, "h1"), [en().text(Text::Title)]);
        }
    }

    #[tokio::test]
    async fn translates_period_pages_and_charts() {
        let db = db_with_quality_counts().await;
        record_period(&db, containing_the_drive(PeriodKind::Day)).await;
        let pl = Language::Polish.translation();
        let day = Period::containing(PeriodKind::Day, local_date(QUALITY_RECENTLY)).unwrap();
        let html = get_in(&db, &day.path(), POLISH_BROWSER).await.body;
        let title = day.title(pl);
        assert_eq!(select_texts(&html, "h1"), [capitalized(&title)]);
        assert_eq!(
            select_texts(&html, "#chart-average-speed figcaption"),
            [format!("Średnia prędkość (km/h), {title}")]
        );
        assert!(select_texts(&html, "#previous").is_empty());
        assert_eq!(select_texts(&html, "#quality-link"), ["Jakość danych"]);

        let quality = get_in(
            &db,
            &format!("{OVERVIEW_QUALITY_PATH}?range=hour"),
            POLISH_BROWSER,
        )
        .await
        .body;
        assert_eq!(
            text(
                &quality,
                "#fetch-failures tr[data-source=mpk-positions] td[data-range=hour]"
            ),
            "33,3%"
        );
        let failed = hover_data(&quality, "failed-downloads");
        assert_eq!(series_names(&failed), ["Pozycje MPK"]);
        let last = failed["steps"].as_array().unwrap().last().unwrap();
        assert_eq!(step_texts(last), ["33,3%"]);

        let chart = get_in(&db, "/charts/average-speed.svg?range=week", POLISH_BROWSER).await;
        assert_eq!(chart.status, StatusCode::OK);
        assert_eq!(header_value(&chart, header::CONTENT_LANGUAGE), Some("pl"));
        for expected in ["Średnia prędkość (km/h)", "Brak danych"] {
            assert!(
                chart.body.lines().any(|line| line == expected),
                "{expected}: {}",
                chart.body
            );
        }
    }

    #[tokio::test]
    async fn serves_only_known_charts() {
        let db = empty_db().await;
        for uri in [
            "/charts/distance.svg",
            "/charts/average-speed.png",
            "/charts/average-speed",
        ] {
            assert_eq!(get(&db, uri).await.status, StatusCode::NOT_FOUND, "{uri}");
        }
        assert_eq!(
            get(&db, "/charts/average-speed.svg").await.status,
            StatusCode::OK
        );
    }

    async fn get_if_none_match(db: &Db, uri: &str, if_none_match: &str) -> Fetched {
        send(
            db,
            Request::get(uri).header(header::IF_NONE_MATCH, if_none_match),
        )
        .await
    }

    #[tokio::test]
    async fn ended_periods_are_not_sent_again_until_the_next_deploy() {
        let db = db_with_a_bus().await;
        let yesterday = containing_the_drive(PeriodKind::Day).previous().unwrap();
        record_period(&db, yesterday).await;
        for path in [
            yesterday.path(),
            Selection::All.period_quality_path(yesterday),
        ] {
            let fetched = get(&db, &path).await;
            assert_eq!(fetched.status, StatusCode::OK);
            assert_eq!(
                header_value(&fetched, header::CACHE_CONTROL),
                Some(SETTLED_CACHE_CONTROL)
            );
            let etag = header_value(&fetched, header::ETAG).unwrap().to_owned();
            assert!(etag.contains(TEST_COMMIT));
            for if_none_match in [
                etag.clone(),
                format!("W/{etag}"),
                format!("{OTHER_ETAG}, {etag}"),
                "*".to_owned(),
            ] {
                let not_modified = get_if_none_match(&db, &path, &if_none_match).await;
                assert_eq!(not_modified.status, StatusCode::NOT_MODIFIED);
                assert!(not_modified.body.is_empty());
                assert_eq!(header_value(&not_modified, header::ETAG), Some(&*etag));
                assert_eq!(
                    header_value(&not_modified, header::CACHE_CONTROL),
                    Some(SETTLED_CACHE_CONTROL)
                );
                assert_eq!(
                    header_value(&not_modified, header::VARY),
                    Some(header::ACCEPT_LANGUAGE.as_str())
                );
            }
            let changed = get_if_none_match(&db, &path, OTHER_ETAG).await;
            assert_eq!(changed.status, StatusCode::OK);
            assert!(!changed.body.is_empty());
        }
    }

    #[tokio::test]
    async fn each_language_has_its_own_etag() {
        let db = db_with_a_bus().await;
        record_period(
            &db,
            containing_the_drive(PeriodKind::Day).previous().unwrap(),
        )
        .await;
        let path = containing_the_drive(PeriodKind::Day)
            .previous()
            .unwrap()
            .path();
        let english = get(&db, &path).await;
        let polish = get_in(&db, &path, POLISH_BROWSER).await;
        let english_etag = header_value(&english, header::ETAG).unwrap();
        assert_ne!(Some(english_etag), header_value(&polish, header::ETAG));
        let switched = send(
            &db,
            Request::get(&path)
                .header(header::ACCEPT_LANGUAGE, POLISH_BROWSER)
                .header(header::IF_NONE_MATCH, english_etag),
        )
        .await;
        assert_eq!(switched.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn live_pages_are_kept_for_a_minute_only() {
        let db = db_with_a_bus().await;
        let today = Period::containing(PeriodKind::Day, local_date(NOW)).unwrap();
        for path in [
            OVERVIEW_PATH.to_owned(),
            format!("{OVERVIEW_PATH}?range=week"),
            OVERVIEW_QUALITY_PATH.to_owned(),
            "/charts/average-speed.svg".to_owned(),
            "/bus".to_owned(),
            today.path(),
            Selection::All.period_quality_path(today),
        ] {
            let fetched = get_if_none_match(&db, &path, "*").await;
            assert_eq!(fetched.status, StatusCode::OK, "{path}");
            assert_eq!(
                header_value(&fetched, header::CACHE_CONTROL),
                Some(LIVE_CACHE_CONTROL),
                "{path}"
            );
            assert_eq!(header_value(&fetched, header::ETAG), None, "{path}");
        }
    }

    #[tokio::test]
    async fn periods_are_kept_only_once_their_data_is_final() {
        let db = db_with_a_bus().await;
        let day = containing_the_drive(PeriodKind::Day);
        let (_, to) = day.bounds(&crate::time::warsaw()).unwrap();
        let final_unix = to.as_second() + db::duration_secs(stats::final_delay());
        let at =
            |now_unix, revision| send_to(app_at(&db, now_unix, revision), Request::get(day.path()));
        let commit = Revision::Commit(TEST_COMMIT);
        for (fetched, cache_control, has_etag) in [
            (at(final_unix - 1, commit).await, LIVE_CACHE_CONTROL, false),
            (at(final_unix, commit).await, SETTLED_CACHE_CONTROL, true),
            (
                at(final_unix, Revision::Unknown).await,
                LIVE_CACHE_CONTROL,
                false,
            ),
        ] {
            assert_eq!(
                header_value(&fetched, header::CACHE_CONTROL),
                Some(cache_control)
            );
            assert_eq!(header_value(&fetched, header::ETAG).is_some(), has_etag);
        }
    }

    const JPEG_CONTENT_TYPE: &str = "image/jpeg";
    const LATER_SECS: i64 = 3600;

    fn app_with_map(db: &Db, now_unix: i64) -> Router {
        let clock = Clock::Fixed(Timestamp::from_second(now_unix).unwrap());
        app(AppState {
            basemap: SharedBasemap::ready(Basemap::empty()),
            ..AppState::new(db.clone(), clock)
        })
    }

    async fn get_with_map(db: &Db, uri: &str) -> Fetched {
        send_to(app_with_map(db, NOW), Request::get(uri)).await
    }

    fn map_vehicles(html: &str, field: &str) -> Vec<String> {
        let [data] = &select_texts(html, "#map .map-data")[..] else {
            panic!("expected map data");
        };
        let data: serde_json::Value = serde_json::from_str(data).unwrap();
        data["vehicles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|vehicle| vehicle[field].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn shows_current_vehicles_on_a_map_above_the_charts() {
        let db = db_with_a_bus_and_a_tram().await;
        let html = get_with_map(&db, "/").await.body;
        assert_eq!(
            select_attributes(&html, "main > section", "id")[..2],
            ["map", "charts"]
        );
        assert_eq!(
            select_attributes(&html, "#map .map-plot svg g", "data-kind"),
            ["bus", "tram"]
        );
        assert_eq!(
            select_attributes(&html, "#map .map-plot svg image", "href"),
            [BASEMAP_PATH]
        );
        assert!(select_texts(&html, "#map svg title").is_empty());
        assert_eq!(map_vehicles(&html, "name"), ["Bus a", "Tram 1"]);
        assert_eq!(map_vehicles(&html, "speed"), ["36 km/h", "0 km/h"]);
        assert_eq!(
            select_attributes(&html, "#map .map-legend li", "data-kind"),
            ["bus", "tram"]
        );
        assert_eq!(select_texts(&html, "#map .map-count"), ["1", "1"]);
        assert_eq!(
            select_attributes(&html, "#map figure", "data-src"),
            [MAP_PATH]
        );

        let line = get_with_map(&db, &format!("/bus/{BUS_LINE}")).await.body;
        assert_eq!(map_vehicles(&line, "name"), [format!("Bus {BUS_LINE}")]);
        assert_eq!(
            select_texts(&line, "#map .map-legend li span:not(.map-count)"),
            [format!("Bus {BUS_LINE}")]
        );

        let polish = send_to(
            app_with_map(&db, NOW),
            Request::get("/tram").header(header::ACCEPT_LANGUAGE, POLISH_BROWSER),
        )
        .await
        .body;
        assert_eq!(
            map_vehicles(&polish, "name"),
            [format!("Tramwaj {TRAM_LINE}")]
        );
    }

    #[tokio::test]
    async fn says_when_no_vehicles_are_running() {
        let db = db_with_a_bus().await;
        let html = send_to(app_with_map(&db, NOW + LATER_SECS), Request::get("/"))
            .await
            .body;
        assert!(select_texts(&html, "#map .map-plot svg g").is_empty());
        assert_eq!(
            select_texts(&html, "#map .map-empty"),
            [en().text(Text::NoVehiclesNow)]
        );
    }

    #[tokio::test]
    async fn serves_the_map_as_images() {
        let db = db_with_a_bus_and_a_tram().await;
        let map = get_with_map(&db, MAP_PATH).await;
        assert_eq!(map.status, StatusCode::OK);
        assert_eq!(map.content_type.as_deref(), Some(SVG_CONTENT_TYPE));
        assert_eq!(
            header_value(&map, header::CACHE_CONTROL),
            Some(LIVE_CACHE_CONTROL)
        );
        assert_eq!(
            select_texts(&map.body, "svg title"),
            ["Bus a, 36 km/h", "Tram 1, 0 km/h"]
        );
        let trams = get_with_map(&db, &format!("/tram{MAP_PATH}")).await.body;
        assert_eq!(select_attributes(&trams, "svg g", "data-kind"), ["tram"]);
        let line = get_with_map(&db, &format!("/bus/{BUS_LINE}{MAP_PATH}"))
            .await
            .body;
        assert_eq!(select_attributes(&line, "svg g", "data-kind"), ["bus"]);

        let basemap = get_with_map(&db, BASEMAP_PATH).await;
        assert_eq!(basemap.status, StatusCode::OK);
        assert_eq!(basemap.content_type.as_deref(), Some(JPEG_CONTENT_TYPE));
        assert_eq!(basemap.bytes, Basemap::empty().jpeg());
        assert_eq!(
            header_value(&basemap, header::CACHE_CONTROL),
            Some(SETTLED_CACHE_CONTROL)
        );
    }

    const STOP_SECS: i64 = 20 * 60;

    async fn db_with_a_bus_and_a_long_tram_stop() -> Db {
        let db = db_with_a_bus().await;
        let tram = Position {
            vehicle_type: VehicleType::Tram,
            line: TRAM_LINE.to_owned(),
            course_id: 2,
            latitude: MicroDegrees(51_100_000),
            longitude: MicroDegrees(17_000_000),
        };
        let mut unix = NOW - STOP_SECS;
        while unix < NOW {
            db.save_positions(unix, &[&tram], &[]).await.unwrap();
            unix += FETCH_SECS;
        }
        db
    }

    fn speed_map_data(html: &str) -> serde_json::Value {
        let [data] = &select_texts(html, "#speed-map .speed-map-data")[..] else {
            panic!("expected speed map data");
        };
        serde_json::from_str(data).unwrap()
    }

    #[tokio::test]
    async fn shows_the_average_speed_of_each_area_on_period_pages() {
        let db = db_with_a_bus_and_a_long_tram_stop().await;
        let day = containing_the_drive(PeriodKind::Day);
        let html = get_with_map(&db, &day.path()).await.body;
        assert_eq!(
            select_attributes(&html, "main > section", "id")[..2],
            ["speed-map", "charts"]
        );
        assert_eq!(
            select_attributes(&html, "#speed-map svg polygon", "fill"),
            [speed_map::slowest_color()]
        );
        let data = speed_map_data(&html);
        let names: Vec<&str> = data["subjects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|subject| subject["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["All vehicles", "Trams"]);
        assert_eq!(
            data["cells"],
            serde_json::json!([[["0.6", "36.0"], ["0.0", MISSING_VALUE]]])
        );
        assert_eq!(select_texts(&html, "#speed-map .speed-legend li").len(), 7);

        let bus_line = get_with_map(&db, &format!("/bus/{BUS_LINE}{}", day.path()))
            .await
            .body;
        assert!(select_texts(&bus_line, "#speed-map svg polygon").is_empty());
        assert_eq!(
            select_texts(&bus_line, "#speed-map .map-empty"),
            [en().text(Text::NoData)]
        );

        let without_map = get(&db, &day.path()).await.body;
        assert!(select_texts(&without_map, "#speed-map").is_empty());
    }

    #[tokio::test]
    async fn leaves_the_map_out_until_it_is_downloaded() {
        let db = db_with_a_bus().await;
        let html = get(&db, "/").await.body;
        assert!(select_texts(&html, "#map").is_empty());
        for uri in [MAP_PATH, BASEMAP_PATH] {
            assert_eq!(
                get(&db, uri).await.status,
                StatusCode::SERVICE_UNAVAILABLE,
                "{uri}"
            );
        }
    }
}

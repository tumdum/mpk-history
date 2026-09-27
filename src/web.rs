use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{RoundMode, Timestamp, Unit, ZonedRound};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde::Deserialize;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::charts::{self, LineChart, Point, RGBColor, Rendered, Series, TickStep};
use crate::db::{self, Db};
use crate::mpk::VehicleType;
use crate::stats::{self, Bucket, Kind, Retention, StatsCache, TimedBucket, Totals};

mod period;
mod quality;

use period::{Period, PeriodKind};
use quality::{OVERVIEW_QUALITY_PATH, quality_path};

const TITLE: &str = "MPK Wrocław statistics";
const OVERVIEW_PATH: &str = "/";
const OVERVIEW_LABEL: &str = "Overview";
const SVG_EXTENSION: &str = ".svg";
const SVG_CONTENT_TYPE: &str = "image/svg+xml";
const MISSING_VALUE: &str = "–";
const UPDATED_FORMAT: &str = "%Y-%m-%d %H:%M";
const INTERNAL_ERROR: &str = "internal error";
const CHART_SCRIPT: &str = include_str!("chart_hover.js");
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
.chart-tooltip {
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
.breadcrumbs { color: var(--text-secondary); margin: 0 0 8px; }
.breadcrumbs a, .subtitle a { color: var(--text-secondary); }
.subtitle { color: var(--text-secondary); margin: 0; }
.period-nav { margin-top: 16px; }
";

const BLUE: RGBColor = RGBColor(0x2a, 0x78, 0xd6);
const ORANGE: RGBColor = RGBColor(0xeb, 0x68, 0x34);
const AQUA: RGBColor = RGBColor(0x1b, 0xaf, 0x7a);
const YELLOW: RGBColor = RGBColor(0xed, 0xa1, 0x00);

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

    fn starts(self, from_unix: i64, to_unix: i64, tz: &TimeZone) -> Vec<i64> {
        let nominal_secs = self.nominal_secs();
        let mut starts = Vec::new();
        let mut start = self.start_of(from_unix, tz);
        while start < to_unix {
            starts.push(start);
            let mut next = self.start_of(start + nominal_secs, tz);
            if next <= start {
                next = self.start_of(start + nominal_secs + nominal_secs / 2, tz);
            }
            if next <= start {
                break;
            }
            start = next;
        }
        starts
    }
}

#[derive(Debug, Clone, Copy)]
struct ChartSpec {
    step: ChartStep,
    tick_step: TickStep,
    tick_format: &'static str,
    hover_format: &'static str,
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
                tick_format: "%H:%M",
                hover_format: "%H:%M",
            },
            Range::Day => ChartSpec {
                step: ChartStep::Minutes(15),
                tick_step: TickStep::Hours(3),
                tick_format: "%H:%M",
                hover_format: "%a %H:%M",
            },
            Range::Week => ChartSpec {
                step: ChartStep::Hours(1),
                tick_step: TickStep::Days(1),
                tick_format: "%a %d.%m",
                hover_format: "%a %d.%m %H:%M",
            },
            Range::Month => ChartSpec {
                step: ChartStep::Hours(6),
                tick_step: TickStep::Days(5),
                tick_format: "%d.%m",
                hover_format: "%a %d.%m %H:%M",
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

    fn label(self) -> &'static str {
        match self {
            Range::Hour => "Last hour",
            Range::Day => "Last day",
            Range::Week => "Last week",
            Range::Month => "Last month",
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
    AverageVehicles,
    PeakVehicles,
    Distance,
}

impl Metric {
    const ALL: [Metric; 4] = [
        Metric::AverageSpeed,
        Metric::AverageVehicles,
        Metric::PeakVehicles,
        Metric::Distance,
    ];
    const CHARTED: [Metric; 2] = [Metric::AverageVehicles, Metric::AverageSpeed];

    fn slug(self) -> &'static str {
        match self {
            Metric::AverageSpeed => "average-speed",
            Metric::AverageVehicles => "average-vehicles",
            Metric::PeakVehicles => "peak-vehicles",
            Metric::Distance => "distance",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Metric::AverageSpeed => "Average speed (km/h)",
            Metric::AverageVehicles => "Average active vehicles",
            Metric::PeakVehicles => "Peak active vehicles",
            Metric::Distance => "Distance driven (km)",
        }
    }

    fn decimals(self) -> usize {
        match self {
            Metric::AverageSpeed | Metric::AverageVehicles => 1,
            Metric::PeakVehicles | Metric::Distance => 0,
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn value(self, totals: &Totals) -> Option<f64> {
        match self {
            Metric::AverageSpeed => totals.average_speed_kmh(),
            Metric::AverageVehicles => totals.average_vehicles(),
            Metric::PeakVehicles => totals.peak_vehicles().map(|peak| peak as f64),
            Metric::Distance => totals.distance_km(),
        }
    }

    fn format(self, totals: &Totals) -> String {
        self.format_value(self.value(totals))
    }

    fn format_value(self, value: Option<f64>) -> String {
        value.map_or_else(
            || MISSING_VALUE.to_owned(),
            |value| format!("{value:.*}", self.decimals()),
        )
    }

    fn from_chart_file(file: &str) -> Option<Metric> {
        let slug = file.strip_suffix(SVG_EXTENSION)?;
        Metric::CHARTED
            .into_iter()
            .find(|metric| metric.slug() == slug)
    }
}

fn kind_slug(kind: Kind) -> &'static str {
    match kind {
        Kind::All => "all",
        Kind::Vehicle(vehicle_type) => vehicle_type.as_str(),
    }
}

fn kind_label(kind: Kind) -> &'static str {
    match kind {
        Kind::All => "All vehicles",
        Kind::Vehicle(VehicleType::Bus) => "Buses",
        Kind::Vehicle(VehicleType::Tram) => "Trams",
        Kind::Vehicle(VehicleType::Train) => "Trains",
    }
}

fn kind_color(kind: Kind) -> RGBColor {
    match kind {
        Kind::All => BLUE,
        Kind::Vehicle(VehicleType::Bus) => ORANGE,
        Kind::Vehicle(VehicleType::Tram) => AQUA,
        Kind::Vehicle(VehicleType::Train) => YELLOW,
    }
}

fn visible_kinds(total: &Bucket) -> Vec<Kind> {
    Kind::ALL
        .into_iter()
        .filter(|kind| matches!(kind, Kind::All) || total.get(*kind).has_vehicles())
        .collect()
}

#[derive(Clone)]
struct AppState {
    db: Db,
    recent: Arc<StatsCache>,
    history: Arc<StatsCache>,
    clock: Clock,
    tz: TimeZone,
}

impl AppState {
    fn new(db: Db, clock: Clock) -> Self {
        Self {
            db,
            recent: Arc::new(StatsCache::new(
                stats::BUCKET,
                Retention::Last(Range::LONGEST.duration()),
            )),
            history: Arc::new(StatsCache::new(HISTORY_RESOLUTION, Retention::Forever)),
            clock,
            tz: TimeZone::system(),
        }
    }

    async fn warm_up(self) {
        let started = Instant::now();
        let now_unix = self.clock.now().as_second();
        if let Err(e) = self.recent_buckets(Range::LONGEST, now_unix).await {
            warn!("failed to compute recent statistics: {e:#}");
            return;
        }
        info!(
            elapsed_secs = started.elapsed().as_secs_f64(),
            "recent statistics ready"
        );
        match self.history_buckets(i64::MIN, now_unix, now_unix).await {
            Ok(_) => info!(
                elapsed_secs = started.elapsed().as_secs_f64(),
                "statistics history ready"
            ),
            Err(e) => warn!("failed to compute statistics history: {e:#}"),
        }
    }

    async fn recent_buckets(&self, range: Range, now_unix: i64) -> Result<Vec<TimedBucket>> {
        let (from_unix, to_unix) = range.window(now_unix);
        self.recent
            .buckets(&self.db, from_unix, to_unix, now_unix)
            .await
    }

    async fn history_buckets(
        &self,
        from_unix: i64,
        to_unix: i64,
        now_unix: i64,
    ) -> Result<Vec<TimedBucket>> {
        let Some(first_unix) = self.db.first_seen_unix().await? else {
            return Ok(Vec::new());
        };
        let from_unix = from_unix.max(first_unix);
        let to_unix = to_unix.min(now_unix);
        if from_unix >= to_unix {
            return Ok(Vec::new());
        }
        self.history
            .buckets(&self.db, from_unix, to_unix, now_unix)
            .await
    }

    fn today(&self) -> Date {
        self.clock.now().to_zoned(self.tz.clone()).date()
    }
}

pub fn router(db: Db, clock: Clock) -> Router {
    app(AppState::new(db, clock))
}

fn app(state: AppState) -> Router {
    let mut router = Router::new()
        .route(OVERVIEW_PATH, get(index))
        .route(OVERVIEW_QUALITY_PATH, get(quality::overview))
        .route("/charts/{file}", get(chart));
    for kind in PeriodKind::ALL {
        router = router
            .route(
                &format!("/{}/{{period}}", kind.slug()),
                get(
                    move |State(state): State<AppState>, Path(slug): Path<String>| {
                        period_page(state, kind, slug)
                    },
                ),
            )
            .route(
                &format!("/{}/{{period}}/quality", kind.slug()),
                get(
                    move |State(state): State<AppState>, Path(slug): Path<String>| {
                        quality::period_page(state, kind, slug)
                    },
                ),
            );
    }
    router.with_state(state)
}

pub async fn serve(
    listener: TcpListener,
    db: Db,
    clock: Clock,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let state = AppState::new(db, clock);
    tokio::spawn(state.clone().warm_up());
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

async fn index(
    State(state): State<AppState>,
    Query(query): Query<RangeQuery>,
) -> Result<Markup, AppError> {
    let now = state.clock.now();
    let now_unix = now.as_second();
    let longest = state.recent_buckets(Range::LONGEST, now_unix).await?;
    let totals = Range::ALL.map(|range| {
        let (from_unix, _) = range.window(now_unix);
        let buckets = longest.iter().filter(|b| b.start_unix >= from_unix);
        (range, stats::sum(buckets))
    });
    let kinds = visible_kinds(&stats::sum(&longest));
    let selected = query.range;
    let (from_unix, to_unix) = selected.window(now_unix);
    let buckets = state.recent_buckets(selected, now_unix).await?;
    let caption = selected.label().to_lowercase();
    let mut figures = Vec::with_capacity(Metric::CHARTED.len());
    for metric in Metric::CHARTED {
        let rendered = render_chart(
            metric,
            &buckets,
            (from_unix, to_unix),
            selected.chart(),
            &state.tz,
        )?;
        figures.push(chart_figure(
            metric.slug(),
            metric.label(),
            &caption,
            selected.slug(),
            &rendered,
        )?);
    }
    let history = history_links(state.today())?;
    let body = html! {
        header {
            h1 { (TITLE) }
            p #updated { "As of " (updated(now, &state.tz)) }
        }
        nav #history aria-label="History" {
            @for (label, period) in &history {
                a href=(period.path()) { (label) }
            }
        }
        nav #pages aria-label="Pages" {
            a #quality-link href=(OVERVIEW_QUALITY_PATH) { "Data quality" }
        }
        main {
            section #summary {
                h2 { "Summary" }
                @for kind in &kinds {
                    (range_table(*kind, &totals))
                }
            }
            section #charts {
                h2 { "Charts" }
                nav aria-label="Chart range" {
                    @for range in Range::ALL {
                        a href=(format!("/?range={}", range.slug()))
                            aria-current=[(range == selected).then_some("page")] {
                            (range.label())
                        }
                    }
                }
                @for figure in &figures {
                    (figure)
                }
            }
        }
    };
    Ok(shell(TITLE, &body))
}

fn history_links(today: Date) -> Result<Vec<(&'static str, Period)>> {
    let day = Period::containing(PeriodKind::Day, today)?;
    let week = Period::containing(PeriodKind::Week, today)?;
    let month = Period::containing(PeriodKind::Month, today)?;
    let year = Period::containing(PeriodKind::Year, today)?;
    Ok(vec![
        ("Today", day),
        ("Yesterday", day.previous()?),
        ("This week", week),
        ("Previous week", week.previous()?),
        ("This month", month),
        ("Previous month", month.previous()?),
        ("This year", year),
    ])
}

fn updated(now: Timestamp, tz: &TimeZone) -> Markup {
    let local = now
        .to_zoned(tz.clone())
        .strftime(UPDATED_FORMAT)
        .to_string();
    html! { time datetime=(now.to_string()) { (local) } }
}

fn shell(title: &str, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                style { (PreEscaped(STYLE)) }
            }
            body {
                (body)
                script { (PreEscaped(CHART_SCRIPT)) }
            }
        }
    }
}

async fn period_page(
    state: AppState,
    kind: PeriodKind,
    slug: String,
) -> Result<Response, AppError> {
    let Some(period) = Period::parse(kind, &slug) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let now = state.clock.now();
    let now_unix = now.as_second();
    let (from, to) = period.bounds(&state.tz)?;
    let (from_unix, to_unix) = (from.as_second(), to.as_second());
    let buckets = state.history_buckets(from_unix, to_unix, now_unix).await?;
    let total = stats::sum(&buckets);
    let kinds = visible_kinds(&total);
    let caption = period.title();
    let mut figures = Vec::with_capacity(Metric::CHARTED.len());
    for metric in Metric::CHARTED {
        let rendered = render_chart(
            metric,
            &buckets,
            (from_unix, to_unix),
            period.chart(),
            &state.tz,
        )?;
        figures.push(chart_figure(
            metric.slug(),
            metric.label(),
            &caption,
            &period.path(),
            &rendered,
        )?);
    }
    let previous = period.previous()?;
    let next = period.next()?;
    let next_started = next.start_date() <= state.today();
    let in_progress = (from_unix..to_unix).contains(&now_unix);
    let parents = period.parents()?;
    let parts = period.parts()?;
    let noun = kind.slug();
    let body = html! {
        header {
            p .breadcrumbs {
                a href=(OVERVIEW_PATH) { (OVERVIEW_LABEL) }
                @for parent in &parents {
                    " · " a href=(parent.path()) { (parent.title()) }
                }
            }
            h1 { (period.title()) }
            @if let Some(subtitle) = period.subtitle()? {
                p .subtitle { (subtitle) }
            }
            @if in_progress {
                p #updated { "So far, as of " (updated(now, &state.tz)) }
            }
            nav .period-nav aria-label="Neighbouring periods" {
                a #previous rel="prev" href=(previous.path()) { "← Previous " (noun) }
                @if next_started {
                    a #next rel="next" href=(next.path()) { "Next " (noun) " →" }
                }
                a #quality-link href=(quality_path(period)) { "Data quality" }
            }
        }
        main {
            section #summary {
                h2 { "Summary" }
                (period_table(&kinds, &total))
            }
            section #charts {
                h2 { "Charts" }
                @for figure in &figures {
                    (figure)
                }
            }
            @if let Some(part) = parts.first() {
                section #parts {
                    h2 { (part.kind().plural_label()) }
                    nav aria-label=(part.kind().plural_label()) {
                        @for part in &parts {
                            a href=(part.path()) { (part.short_label()) }
                        }
                    }
                }
            }
        }
    };
    Ok(shell(&format!("{} – {TITLE}", period.title()), &body).into_response())
}

fn chart_figure(
    slug: &str,
    label: &str,
    caption: &str,
    range: &str,
    rendered: &Rendered,
) -> Result<Markup> {
    let hover = serde_json::to_string(&rendered.hover)?
        .replace(SCRIPT_END_TAG_START, ESCAPED_SCRIPT_END_TAG_START);
    Ok(html! {
        figure .chart #(format!("chart-{slug}")) data-range=(range) {
            div .chart-plot { (PreEscaped(&rendered.svg)) }
            div .chart-tooltip role="status" hidden {}
            script .chart-data type="application/json" { (PreEscaped(hover)) }
            figcaption { (label) ", " (caption) }
        }
    })
}

fn range_table(kind: Kind, totals: &[(Range, Bucket)]) -> Markup {
    html! {
        div .table-wrap {
            table .stats #(format!("stats-{}", kind_slug(kind))) {
                caption { (kind_label(kind)) }
                thead {
                    tr {
                        th scope="col" { "Metric" }
                        @for (range, _) in totals {
                            th scope="col" { (range.label()) }
                        }
                    }
                }
                tbody {
                    @for metric in Metric::ALL {
                        tr data-metric=(metric.slug()) {
                            th scope="row" { (metric.label()) }
                            @for (range, total) in totals {
                                td data-range=(range.slug()) { (metric.format(total.get(kind))) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn period_table(kinds: &[Kind], total: &Bucket) -> Markup {
    html! {
        div .table-wrap {
            table .stats #period-stats {
                thead {
                    tr {
                        th scope="col" { "Metric" }
                        @for kind in kinds {
                            th scope="col" data-kind=(kind_slug(*kind)) { (kind_label(*kind)) }
                        }
                    }
                }
                tbody {
                    @for metric in Metric::ALL {
                        tr data-metric=(metric.slug()) {
                            th scope="row" { (metric.label()) }
                            @for kind in kinds {
                                td data-kind=(kind_slug(*kind)) { (metric.format(total.get(*kind))) }
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn chart(
    State(state): State<AppState>,
    Path(file): Path<String>,
    Query(query): Query<RangeQuery>,
) -> Result<Response, AppError> {
    let Some(metric) = Metric::from_chart_file(&file) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let range = query.range;
    let now_unix = state.clock.now().as_second();
    let buckets = state.recent_buckets(range, now_unix).await?;
    let rendered = render_chart(
        metric,
        &buckets,
        range.window(now_unix),
        range.chart(),
        &state.tz,
    )?;
    Ok(([(header::CONTENT_TYPE, SVG_CONTENT_TYPE)], rendered.svg).into_response())
}

fn render_chart(
    metric: Metric,
    buckets: &[TimedBucket],
    (from_unix, to_unix): (i64, i64),
    spec: ChartSpec,
    tz: &TimeZone,
) -> Result<Rendered> {
    let grouped = stats::regroup_by(buckets, |start_unix| spec.step.start_of(start_unix, tz));
    let series = visible_kinds(&stats::sum(buckets))
        .into_iter()
        .map(|kind| Series {
            name: kind_label(kind).to_owned(),
            color: kind_color(kind),
            points: grouped
                .iter()
                .map(|timed| {
                    let value = metric.value(timed.bucket.get(kind));
                    Point {
                        unix: timed.start_unix.max(from_unix),
                        value,
                        text: metric.format_value(value),
                    }
                })
                .collect(),
        })
        .collect();
    charts::render(&LineChart {
        from_unix,
        to_unix,
        y_label: metric.label(),
        tick_format: spec.tick_format,
        hover_format: spec.hover_format,
        tick_step: spec.tick_step,
        series,
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
    use crate::db::{OpenMode, PositionCount};
    use crate::fetch::Source;
    use crate::geo::MicroDegrees;
    use crate::mpk::Position;
    use jiff::civil::Date;

    const IN_MEMORY: &str = ":memory:";
    const NOW: i64 = 1_800_000_000;
    const FETCH_SECS: i64 = 10;
    const HOUR_SECS: i32 = 60 * 60;
    const DRIVE_SECS: i64 = 300;
    const HUNDRED_METERS_NORTH_E6: i64 = 898;
    const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";

    struct Fetched {
        status: StatusCode,
        content_type: Option<String>,
        body: String,
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
            db.save_positions(unix, &[&bus], &[]).await.unwrap();
            unix += FETCH_SECS;
            north_e6 += HUNDRED_METERS_NORTH_E6;
        }
        db
    }

    async fn get(db: &Db, uri: &str) -> Fetched {
        let clock = Clock::Fixed(Timestamp::from_second(NOW).unwrap());
        let response = router(db.clone(), clock)
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|value| value.to_str().unwrap().to_owned());
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Fetched {
            status,
            content_type,
            body: String::from_utf8(body.to_vec()).unwrap(),
        }
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
            select_texts(&default, "nav a[aria-current=page]"),
            [Range::default().label()]
        );

        let html = get(&db, "/?range=week").await.body;
        assert_eq!(
            select_texts(&html, "nav a[aria-current=page]"),
            [Range::Week.label()]
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
            ["chart-average-vehicles", "chart-average-speed"]
        );
        assert_eq!(
            select_attributes(&html, "figure.chart", "data-range"),
            ["week", "week"]
        );
        assert_eq!(
            select_texts(&html, "figure.chart .chart-plot > svg").len(),
            2
        );
        assert!(select_texts(&html, "body > script")[0].contains("chart-data"));
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
        assert_eq!(names, ["All vehicles", "Buses"]);
        let steps = speed["steps"].as_array().unwrap();
        let offset_secs = TimeZone::system()
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
        assert_eq!(texts(&steps[0]), [MISSING_VALUE, MISSING_VALUE]);
        assert_eq!(steps[0]["values"][0]["y"], serde_json::Value::Null);
        let last = steps.last().unwrap();
        assert_eq!(texts(last), ["36.0", "36.0"]);
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
                }],
                steps: Vec::new(),
            },
        };
        let html = chart_figure(
            Metric::AverageSpeed.slug(),
            Metric::AverageSpeed.label(),
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
        for metric in Metric::CHARTED {
            for range in Range::ALL {
                let uri = format!(
                    "/charts/{}{SVG_EXTENSION}?range={}",
                    metric.slug(),
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
            .to_zoned(TimeZone::system())
            .date()
    }

    fn containing(kind: PeriodKind) -> Period {
        Period::containing(kind, local_date(NOW)).unwrap()
    }

    fn containing_the_drive(kind: PeriodKind) -> Period {
        Period::containing(kind, local_date(NOW - FETCH_SECS)).unwrap()
    }

    fn contains_now(period: Period) -> bool {
        let (from, to) = period.bounds(&TimeZone::system()).unwrap();
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
            assert_eq!(select_texts(html, "h1"), [period.title()]);
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
                [period.path(), period.path()]
            );
        }
    }

    #[tokio::test]
    async fn earlier_periods_show_missing_values_and_link_forward() {
        let db = db_with_a_bus().await;
        let drive_day = containing_the_drive(PeriodKind::Day);
        let yesterday = drive_day.previous().unwrap();
        let html = get(&db, &yesterday.path()).await.body;
        assert_eq!(
            select_attributes(&html, "#period-stats thead th[data-kind]", "data-kind"),
            ["all"]
        );
        for metric in Metric::ALL {
            assert_eq!(period_cells(&html, metric), [MISSING_VALUE]);
        }
        assert!(select_texts(&html, "#updated").is_empty());
        assert_eq!(
            select_attributes(&html, "#next", "href"),
            [drive_day.path()]
        );
        assert_eq!(
            select_attributes(&html, "#previous", "href"),
            [yesterday.previous().unwrap().path()]
        );

        let long_ago = get(&db, "/year/1900").await;
        assert_eq!(long_ago.status, StatusCode::OK);
        assert_eq!(
            period_cells(&long_ago.body, Metric::Distance),
            [MISSING_VALUE]
        );
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
        let count = |vehicle_type, received, invalid| PositionCount {
            vehicle_type,
            received,
            invalid,
        };
        db.save_positions_with_counts(
            recently,
            &[],
            &[],
            &[
                count(VehicleType::Bus, 10, 2),
                count(VehicleType::Tram, 5, 0),
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
        let html = get(&db, &quality_path(day)).await.body;
        assert_eq!(
            select_texts(&html, "h1"),
            [format!("Data quality: {}", day.title())]
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
        assert_eq!(
            select_attributes(&html, "#previous", "href"),
            [quality_path(day.previous().unwrap())]
        );
        let mut breadcrumbs = vec![OVERVIEW_QUALITY_PATH.to_owned()];
        breadcrumbs.extend(day.parents().unwrap().into_iter().map(quality_path));
        assert_eq!(
            select_attributes(&html, ".breadcrumbs a", "href"),
            breadcrumbs
        );
        let failed = hover_data(&html, "failed-downloads");
        assert_eq!(
            select_attributes(&html, "#chart-failed-downloads", "data-range"),
            [quality_path(day)]
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
        let html = get(&db, &quality_path(month)).await.body;
        let days: Vec<String> = month
            .parts()
            .unwrap()
            .into_iter()
            .map(quality_path)
            .collect();
        assert_eq!(select_attributes(&html, "#parts a", "href"), days);

        let before = day.previous().unwrap().previous().unwrap();
        let html = get(&db, &quality_path(before)).await.body;
        assert_eq!(
            select_attributes(&html, "#invalid-positions tbody tr", "data-kind"),
            ["all"]
        );
        assert_eq!(
            get(&db, "/day/2027-13-01/quality").await.status,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn lists_chart_steps_in_local_time() {
        let tz = TimeZone::get("Europe/Warsaw").unwrap();
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
}

use anyhow::Result;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};

use super::period::{Period, PeriodKind};
use super::{
    AppError, AppState, ChartSpec, MISSING_VALUE, OVERVIEW_LABEL, OVERVIEW_PATH, Range, RangeQuery,
    TITLE, chart_figure, kind_color, kind_label, kind_slug, shell, updated,
};
use crate::charts::{self, LineChart, Point, RGBColor, Rendered, Series};
use crate::db::{Db, FetchCount, PositionCount};
use crate::fetch::Source;
use crate::stats::Kind;

pub(super) const OVERVIEW_QUALITY_PATH: &str = "/quality";
const QUALITY_SEGMENT: &str = "quality";
const PAGE_TITLE: &str = "Data quality";
const PERCENT: f64 = 100.0;
const SMALLEST_SHOWN_PERCENT: f64 = 0.1;
const SOURCES: [Source; 4] = [
    Source::MpkPositions,
    Source::MpkLines,
    Source::OpenMeteo,
    Source::OpenData,
];

const MAGENTA: RGBColor = RGBColor(0xe8, 0x7b, 0xa4);
const GREEN: RGBColor = RGBColor(0x00, 0x83, 0x00);
const VIOLET: RGBColor = RGBColor(0x4a, 0x3a, 0xa7);
const RED: RGBColor = RGBColor(0xe3, 0x49, 0x48);

fn source_slug(source: Source) -> &'static str {
    match source {
        Source::MpkPositions => "mpk-positions",
        Source::MpkLines => "mpk-lines",
        Source::OpenMeteo => "open-meteo",
        Source::OpenData => "open-data",
    }
}

fn source_label(source: Source) -> &'static str {
    match source {
        Source::MpkPositions => "MPK positions",
        Source::MpkLines => "MPK line list",
        Source::OpenMeteo => "Open-Meteo weather",
        Source::OpenData => "Open data fleet",
    }
}

fn source_color(source: Source) -> RGBColor {
    match source {
        Source::MpkPositions => MAGENTA,
        Source::MpkLines => GREEN,
        Source::OpenMeteo => VIOLET,
        Source::OpenData => RED,
    }
}

pub(super) fn quality_path(period: Period) -> String {
    format!("{}/{QUALITY_SEGMENT}", period.path())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Share {
    part: i64,
    total: i64,
}

impl Share {
    fn add(self, part: i64, total: i64) -> Self {
        Self {
            part: self.part + part,
            total: self.total + total,
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn percent(self) -> Option<f64> {
        (self.total > 0).then(|| self.part as f64 / self.total as f64 * PERCENT)
    }

    fn format(self) -> String {
        let Some(percent) = self.percent() else {
            return MISSING_VALUE.to_owned();
        };
        if self.part == 0 {
            return "0%".to_owned();
        }
        if percent < SMALLEST_SHOWN_PERCENT {
            return format!("<{SMALLEST_SHOWN_PERCENT}%");
        }
        format!("{percent:.1}%")
    }
}

fn failed_fetches<'a>(counts: impl IntoIterator<Item = &'a FetchCount>, source: Source) -> Share {
    counts
        .into_iter()
        .filter(|count| count.source == source)
        .fold(Share::default(), |share, count| {
            share.add(count.failures, count.attempts)
        })
}

fn invalid_positions<'a>(counts: impl IntoIterator<Item = &'a PositionCount>, kind: Kind) -> Share {
    counts
        .into_iter()
        .filter(|count| match kind {
            Kind::All => true,
            Kind::Vehicle(vehicle_type) => count.vehicle_type == vehicle_type,
        })
        .fold(Share::default(), |share, count| {
            share.add(count.invalid, count.received)
        })
}

struct Quality {
    fetches: Vec<FetchCount>,
    positions: Vec<PositionCount>,
}

impl Quality {
    async fn load(db: &Db, (from_unix, to_unix): (i64, i64)) -> Result<Self> {
        Ok(Self {
            fetches: db.fetch_counts(from_unix, to_unix).await?,
            positions: db.position_counts(from_unix, to_unix).await?,
        })
    }

    fn failed_fetches(&self, source: Source) -> Share {
        failed_fetches(&self.fetches, source)
    }

    fn invalid_positions(&self, kind: Kind) -> Share {
        invalid_positions(&self.positions, kind)
    }

    fn sources(&self) -> Vec<Source> {
        SOURCES
            .into_iter()
            .filter(|source| self.failed_fetches(*source).total > 0)
            .collect()
    }

    fn position_kinds(&self) -> Vec<Kind> {
        Kind::ALL
            .into_iter()
            .filter(|kind| matches!(kind, Kind::All) || self.invalid_positions(*kind).total > 0)
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QualityChart {
    FailedDownloads,
    InvalidPositions,
}

impl QualityChart {
    const ALL: [QualityChart; 2] = [
        QualityChart::FailedDownloads,
        QualityChart::InvalidPositions,
    ];

    fn slug(self) -> &'static str {
        match self {
            QualityChart::FailedDownloads => "failed-downloads",
            QualityChart::InvalidPositions => "invalid-positions",
        }
    }

    fn label(self) -> &'static str {
        match self {
            QualityChart::FailedDownloads => "Failed downloads (%)",
            QualityChart::InvalidPositions => "Invalid GPS positions (%)",
        }
    }
}

async fn charts(
    state: &AppState,
    window: (i64, i64),
    spec: ChartSpec,
    caption: &str,
    range: &str,
    totals: &Quality,
) -> Result<Vec<Markup>> {
    let fetches = state.db.fetch_counts_by_minute(window.0, window.1).await?;
    let positions = state
        .db
        .position_counts_by_minute(window.0, window.1)
        .await?;
    let starts = spec.step.starts(window.0, window.1, &state.tz);
    let mut figures = Vec::with_capacity(QualityChart::ALL.len());
    for chart in QualityChart::ALL {
        let series = match chart {
            QualityChart::FailedDownloads => totals
                .sources()
                .into_iter()
                .map(|source| ShareSeries {
                    name: source_label(source),
                    color: source_color(source),
                    minutes: fetches
                        .iter()
                        .filter(|(_, count)| count.source == source)
                        .map(|(minute, count)| (*minute, failed_fetches([count], source)))
                        .collect(),
                })
                .collect(),
            QualityChart::InvalidPositions => totals
                .position_kinds()
                .into_iter()
                .map(|kind| ShareSeries {
                    name: kind_label(kind),
                    color: kind_color(kind),
                    minutes: positions
                        .iter()
                        .map(|(minute, count)| (*minute, invalid_positions([count], kind)))
                        .filter(|(_, share)| share.total > 0)
                        .collect(),
                })
                .collect(),
        };
        let rendered = render_share_chart(chart.label(), series, &starts, window, spec)?;
        figures.push(chart_figure(
            chart.slug(),
            chart.label(),
            caption,
            range,
            &rendered,
        )?);
    }
    Ok(figures)
}

struct ShareSeries {
    name: &'static str,
    color: RGBColor,
    minutes: Vec<(i64, Share)>,
}

fn per_step(minutes: &[(i64, Share)], starts: &[i64]) -> Vec<Share> {
    let mut shares = vec![Share::default(); starts.len()];
    let mut step = 0;
    for (minute, share) in minutes {
        while starts.get(step + 1).is_some_and(|next| next <= minute) {
            step += 1;
        }
        if starts.get(step).is_some_and(|start| start <= minute) {
            shares[step] = shares[step].add(share.part, share.total);
        }
    }
    shares
}

fn render_share_chart(
    label: &str,
    series: Vec<ShareSeries>,
    starts: &[i64],
    (from_unix, to_unix): (i64, i64),
    spec: ChartSpec,
) -> Result<Rendered> {
    let series = series
        .into_iter()
        .map(|series| Series {
            name: series.name.to_owned(),
            color: series.color,
            points: starts
                .iter()
                .zip(per_step(&series.minutes, starts))
                .map(|(start_unix, share)| Point {
                    unix: (*start_unix).max(from_unix),
                    value: share.percent(),
                    text: share.format(),
                })
                .collect(),
        })
        .collect();
    charts::render(&LineChart {
        from_unix,
        to_unix,
        y_label: label,
        tick_format: spec.tick_format,
        hover_format: spec.hover_format,
        tick_step: spec.tick_step,
        series,
    })
}

fn range_tables(quality: &[(Range, Quality)], position_kinds: &[Kind]) -> Markup {
    html! {
        div .table-wrap {
            table .stats #fetch-failures {
                caption { "Failed downloads" }
                thead {
                    tr {
                        th scope="col" { "Source" }
                        @for (range, _) in quality {
                            th scope="col" { (range.label()) }
                        }
                    }
                }
                tbody {
                    @for source in SOURCES {
                        tr data-source=(source_slug(source)) {
                            th scope="row" { (source_label(source)) }
                            @for (range, counts) in quality {
                                td data-range=(range.slug()) { (counts.failed_fetches(source).format()) }
                            }
                        }
                    }
                }
            }
        }
        div .table-wrap {
            table .stats #invalid-positions {
                caption { "Invalid GPS positions" }
                thead {
                    tr {
                        th scope="col" { "Vehicles" }
                        @for (range, _) in quality {
                            th scope="col" { (range.label()) }
                        }
                    }
                }
                tbody {
                    @for kind in position_kinds {
                        tr data-kind=(kind_slug(*kind)) {
                            th scope="row" { (kind_label(*kind)) }
                            @for (range, counts) in quality {
                                td data-range=(range.slug()) { (counts.invalid_positions(*kind).format()) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn period_tables(quality: &Quality) -> Markup {
    html! {
        div .table-wrap {
            table .stats #fetch-failures {
                thead {
                    tr {
                        th scope="col" { "Source" }
                        th scope="col" { "Failed downloads" }
                    }
                }
                tbody {
                    @for source in SOURCES {
                        tr data-source=(source_slug(source)) {
                            th scope="row" { (source_label(source)) }
                            td { (quality.failed_fetches(source).format()) }
                        }
                    }
                }
            }
        }
        div .table-wrap {
            table .stats #invalid-positions {
                thead {
                    tr {
                        th scope="col" { "Vehicles" }
                        th scope="col" { "Invalid GPS positions" }
                    }
                }
                tbody {
                    @for kind in quality.position_kinds() {
                        tr data-kind=(kind_slug(kind)) {
                            th scope="row" { (kind_label(kind)) }
                            td { (quality.invalid_positions(kind).format()) }
                        }
                    }
                }
            }
        }
    }
}

pub(super) async fn overview(
    State(state): State<AppState>,
    Query(query): Query<RangeQuery>,
) -> Result<Markup, AppError> {
    let now = state.clock.now();
    let now_unix = now.as_second();
    let mut quality = Vec::with_capacity(Range::ALL.len());
    for range in Range::ALL {
        quality.push((
            range,
            Quality::load(&state.db, range.window(now_unix)).await?,
        ));
    }
    let position_kinds = quality
        .last()
        .map(|(_, longest)| longest.position_kinds())
        .unwrap_or_default();
    let selected = query.range;
    let window = selected.window(now_unix);
    let selected_totals = Quality::load(&state.db, window).await?;
    let figures = charts(
        &state,
        window,
        selected.chart(),
        &selected.label().to_lowercase(),
        selected.slug(),
        &selected_totals,
    )
    .await?;
    let body = html! {
        header {
            p .breadcrumbs { a href=(OVERVIEW_PATH) { (OVERVIEW_LABEL) } }
            h1 { (PAGE_TITLE) }
            p #updated { "As of " (updated(now, &state.tz)) }
        }
        main {
            section #summary {
                h2 { "Summary" }
                (range_tables(&quality, &position_kinds))
            }
            section #charts {
                h2 { "Charts" }
                nav aria-label="Chart range" {
                    @for range in Range::ALL {
                        a href=(format!("{OVERVIEW_QUALITY_PATH}?range={}", range.slug()))
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
    Ok(shell(&format!("{PAGE_TITLE} – {TITLE}"), &body))
}

pub(super) async fn period_page(
    state: AppState,
    kind: PeriodKind,
    slug: String,
) -> Result<Response, AppError> {
    let Some(period) = Period::parse(kind, &slug) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let now = state.clock.now();
    let (from, to) = period.bounds(&state.tz)?;
    let window = (from.as_second(), to.as_second());
    let quality = Quality::load(&state.db, window).await?;
    let figures = charts(
        &state,
        window,
        period.chart(),
        &period.title(),
        &quality_path(period),
        &quality,
    )
    .await?;
    let previous = period.previous()?;
    let next = period.next()?;
    let next_started = next.start_date() <= state.today();
    let in_progress = (window.0..window.1).contains(&now.as_second());
    let parents = period.parents()?;
    let parts = period.parts()?;
    let noun = kind.slug();
    let body = html! {
        header {
            p .breadcrumbs {
                a href=(OVERVIEW_QUALITY_PATH) { (PAGE_TITLE) }
                @for parent in &parents {
                    " · " a href=(quality_path(*parent)) { (parent.title()) }
                }
            }
            h1 { (PAGE_TITLE) ": " (period.title()) }
            p .subtitle { a #statistics-link href=(period.path()) { "Statistics of this " (noun) } }
            @if in_progress {
                p #updated { "So far, as of " (updated(now, &state.tz)) }
            }
            nav .period-nav aria-label="Neighbouring periods" {
                a #previous rel="prev" href=(quality_path(previous)) { "← Previous " (noun) }
                @if next_started {
                    a #next rel="next" href=(quality_path(next)) { "Next " (noun) " →" }
                }
            }
        }
        main {
            section #summary {
                h2 { "Summary" }
                (period_tables(&quality))
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
                            a href=(quality_path(*part)) { (part.short_label()) }
                        }
                    }
                }
            }
        }
    };
    let title = format!("{PAGE_TITLE}: {} – {TITLE}", period.title());
    Ok(shell(&title, &body).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_shares_readably() {
        let share = |part, total| Share { part, total }.format();
        assert_eq!(share(0, 0), MISSING_VALUE);
        assert_eq!(share(0, 5), "0%");
        assert_eq!(share(1, 2000), "<0.1%");
        assert_eq!(share(1, 3), "33.3%");
        assert_eq!(share(5, 5), "100.0%");
    }

    #[test]
    fn sums_minutes_into_their_steps() {
        let share = |part, total| Share { part, total };
        let minutes = [
            (0, share(1, 2)),
            (60, share(0, 2)),
            (180, share(1, 1)),
            (600, share(5, 5)),
        ];
        assert_eq!(
            per_step(&minutes, &[0, 120, 240]),
            [share(1, 4), share(1, 1), share(5, 5)]
        );
        assert_eq!(per_step(&minutes, &[100, 200]), [share(1, 1), share(5, 5)]);
        assert_eq!(per_step(&[], &[0]), [Share::default()]);
    }
}

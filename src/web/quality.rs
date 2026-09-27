use anyhow::Result;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};

use super::period::{Period, PeriodKind};
use super::selection::{self, Page, Selection, SelectionKey, Subject};
use super::{
    AppError, AppState, ChartSpec, GREEN, MAGENTA, MISSING_VALUE, OVERVIEW_LABEL, Range, TITLE,
    VIOLET, YELLOW, chart_figure, shell, updated,
};
use crate::charts::{self, LineChart, Point, RGBColor, Rendered, Series};
use crate::db::{Db, FetchCount, PositionCount};
use crate::fetch::Source;
use crate::stats::{Group, Kind};

pub(super) const OVERVIEW_QUALITY_PATH: &str = "/quality";
pub(super) const QUALITY_SEGMENT: &str = "quality";
const PAGE_TITLE: &str = "Data quality";
const PERCENT: f64 = 100.0;
const SMALLEST_SHOWN_PERCENT: f64 = 0.1;
const SOURCES: [Source; 4] = [
    Source::MpkPositions,
    Source::MpkLines,
    Source::OpenMeteo,
    Source::OpenData,
];

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
        Source::OpenData => YELLOW,
    }
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

fn invalid_positions<'a>(
    counts: impl IntoIterator<Item = &'a PositionCount>,
    group: Group,
) -> Share {
    counts
        .into_iter()
        .filter(|count| match group {
            Group::Kind(Kind::All) | Group::Line(_) => true,
            Group::Kind(Kind::Vehicle(vehicle_type)) => count.vehicle_type == vehicle_type,
        })
        .fold(Share::default(), |share, count| {
            share.add(count.invalid, count.received)
        })
}

async fn position_counts(
    db: &Db,
    selection: &Selection,
    (from_unix, to_unix): (i64, i64),
) -> Result<Vec<PositionCount>> {
    match selection {
        Selection::Line(line) => db.line_position_counts(line, from_unix, to_unix).await,
        Selection::All | Selection::Type(_) => db.position_counts(from_unix, to_unix).await,
    }
}

async fn position_counts_by_minute(
    db: &Db,
    selection: &Selection,
    (from_unix, to_unix): (i64, i64),
) -> Result<Vec<(i64, PositionCount)>> {
    match selection {
        Selection::Line(line) => {
            db.line_position_counts_by_minute(line, from_unix, to_unix)
                .await
        }
        Selection::All | Selection::Type(_) => {
            db.position_counts_by_minute(from_unix, to_unix).await
        }
    }
}

struct Quality {
    fetches: Vec<FetchCount>,
    positions: Vec<PositionCount>,
}

impl Quality {
    /// `positions` holds only the counts of the selection's line when it is one.
    async fn load(db: &Db, selection: &Selection, window: (i64, i64)) -> Result<Self> {
        let fetches = if QualityChart::of(selection).contains(&QualityChart::FailedDownloads) {
            db.fetch_counts(window.0, window.1).await?
        } else {
            Vec::new()
        };
        Ok(Self {
            fetches,
            positions: position_counts(db, selection, window).await?,
        })
    }

    fn failed_fetches(&self, source: Source) -> Share {
        failed_fetches(&self.fetches, source)
    }

    fn invalid_positions(&self, group: Group) -> Share {
        invalid_positions(&self.positions, group)
    }

    fn sources(&self) -> Vec<Source> {
        SOURCES
            .into_iter()
            .filter(|source| self.failed_fetches(*source).total > 0)
            .collect()
    }

    fn position_shares(&self, subjects: &[Subject]) -> Vec<Share> {
        subjects
            .iter()
            .map(|subject| self.invalid_positions(subject.group))
            .collect()
    }
}

fn shown_positions<'a>(subjects: &'a [Subject], shares: &'a [Share]) -> Vec<&'a Subject> {
    selection::shown(subjects, shares, |share| share.total > 0)
        .into_iter()
        .map(|(subject, _)| subject)
        .collect()
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

    fn of(selection: &Selection) -> &'static [QualityChart] {
        match selection {
            Selection::All => &Self::ALL,
            Selection::Type(_) | Selection::Line(_) => &[QualityChart::InvalidPositions],
        }
    }

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

struct ChartContext<'a> {
    selection: &'a Selection,
    window: (i64, i64),
    spec: ChartSpec,
    caption: &'a str,
    range: &'a str,
}

async fn charts(
    state: &AppState,
    context: ChartContext<'_>,
    totals: &Quality,
    position_subjects: &[&Subject],
) -> Result<Vec<Markup>> {
    let ChartContext {
        selection,
        window,
        spec,
        caption,
        range,
    } = context;
    let shown = QualityChart::of(selection);
    let fetches = if shown.contains(&QualityChart::FailedDownloads) {
        state.db.fetch_counts_by_minute(window.0, window.1).await?
    } else {
        Vec::new()
    };
    let positions = position_counts_by_minute(&state.db, selection, window).await?;
    let starts = spec.step.starts(window.0, window.1, &state.tz);
    let mut figures = Vec::with_capacity(shown.len());
    for chart in shown.iter().copied() {
        let series = match chart {
            QualityChart::FailedDownloads => totals
                .sources()
                .into_iter()
                .map(|source| ShareSeries {
                    name: source_label(source).to_owned(),
                    color: source_color(source),
                    minutes: fetches
                        .iter()
                        .filter(|(_, count)| count.source == source)
                        .map(|(minute, count)| (*minute, failed_fetches([count], source)))
                        .collect(),
                })
                .collect(),
            QualityChart::InvalidPositions => position_subjects
                .iter()
                .map(|subject| ShareSeries {
                    name: subject.label.clone(),
                    color: subject.color,
                    minutes: positions
                        .iter()
                        .map(|(minute, count)| (*minute, invalid_positions([count], subject.group)))
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
    name: String,
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
            name: series.name,
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

fn range_tables(
    selection: &Selection,
    quality: &[(Range, Quality)],
    position_subjects: &[&Subject],
) -> Markup {
    let charts = QualityChart::of(selection);
    html! {
        @if charts.contains(&QualityChart::FailedDownloads) {
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
                    @for subject in position_subjects {
                        tr data-kind=(subject.slug) {
                            th scope="row" { (subject.label) }
                            @for (range, counts) in quality {
                                td data-range=(range.slug()) { (counts.invalid_positions(subject.group).format()) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn period_tables(
    selection: &Selection,
    quality: &Quality,
    position_subjects: &[&Subject],
) -> Markup {
    let charts = QualityChart::of(selection);
    html! {
        @if charts.contains(&QualityChart::FailedDownloads) {
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
                    @for subject in position_subjects {
                        tr data-kind=(subject.slug) {
                            th scope="row" { (subject.label) }
                            td { (quality.invalid_positions(subject.group).format()) }
                        }
                    }
                }
            }
        }
    }
}

fn page_heading(selection: &Selection, period: Option<Period>) -> String {
    let what = match (selection, period) {
        (Selection::All, None) => return PAGE_TITLE.to_owned(),
        (Selection::All, Some(period)) => period.title(),
        (Selection::Type(_) | Selection::Line(_), None) => selection.label(),
        (Selection::Type(_) | Selection::Line(_), Some(period)) => {
            format!("{}, {}", selection.label(), period.title())
        }
    };
    format!("{PAGE_TITLE}: {what}")
}

pub(super) async fn overview(
    state: AppState,
    key: SelectionKey,
    selected: Range,
) -> Result<Response, AppError> {
    let Some(selection) = key.resolve(&state.db).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let now = state.clock.now();
    let now_unix = now.as_second();
    let mut quality = Vec::with_capacity(Range::ALL.len());
    for range in Range::ALL {
        quality.push((
            range,
            Quality::load(&state.db, &selection, range.window(now_unix)).await?,
        ));
    }
    let subjects = selection.subjects();
    let longest_shares = quality
        .last()
        .map(|(_, longest)| longest.position_shares(&subjects))
        .unwrap_or_default();
    let position_subjects = shown_positions(&subjects, &longest_shares);
    let window = selected.window(now_unix);
    let selected_totals = Quality::load(&state.db, &selection, window).await?;
    let selected_shares = selected_totals.position_shares(&subjects);
    let caption = selected.label().to_lowercase();
    let figures = charts(
        &state,
        ChartContext {
            selection: &selection,
            window,
            spec: selected.chart(),
            caption: &caption,
            range: selected.slug(),
        },
        &selected_totals,
        &shown_positions(&subjects, &selected_shares),
    )
    .await?;
    let (lines_from_unix, lines_to_unix) = Range::LONGEST.window(now_unix);
    let lines = state
        .db
        .lines_seen_between(lines_from_unix, lines_to_unix)
        .await?;
    let page = Page::QualityOverview(selected);
    let heading = page_heading(&selection, None);
    let body = html! {
        header {
            p .breadcrumbs { a href=(selection.overview_path()) { (OVERVIEW_LABEL) } }
            h1 { (heading) }
            p #updated { "As of " (updated(now, &state.tz)) }
        }
        (selection::vehicles_nav(&selection, page, &lines))
        main {
            section #summary {
                h2 { "Summary" }
                (range_tables(&selection, &quality, &position_subjects))
            }
            section #charts {
                h2 { "Charts" }
                nav aria-label="Chart range" {
                    @for range in Range::ALL {
                        a href=(selection.quality_range_path(range))
                            aria-current=[(range == selected).then_some("page")] {
                            (range.label())
                        }
                    }
                }
                @for figure in &figures {
                    (figure)
                }
            }
            (selection::lines_section(&selection, page, &lines))
        }
    };
    Ok(shell(&format!("{heading} – {TITLE}"), &body).into_response())
}

pub(super) async fn period_page(
    state: AppState,
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
    let now = state.clock.now();
    let (from, to) = period.bounds(&state.tz)?;
    let window = (from.as_second(), to.as_second());
    let quality = Quality::load(&state.db, &selection, window).await?;
    let subjects = selection.subjects();
    let shares = quality.position_shares(&subjects);
    let position_subjects = shown_positions(&subjects, &shares);
    let figures = charts(
        &state,
        ChartContext {
            selection: &selection,
            window,
            spec: period.chart(),
            caption: &period.title(),
            range: &selection.period_quality_path(period),
        },
        &quality,
        &position_subjects,
    )
    .await?;
    let lines = state.db.lines_seen_between(window.0, window.1).await?;
    let page = Page::QualityPeriod(period);
    let previous = period.previous()?;
    let next = period.next()?;
    let next_started = next.start_date() <= state.today();
    let in_progress = (window.0..window.1).contains(&now.as_second());
    let parents = period.parents()?;
    let parts = period.parts()?;
    let noun = kind.slug();
    let heading = page_heading(&selection, Some(period));
    let body = html! {
        header {
            p .breadcrumbs {
                a href=(selection.quality_path()) { (PAGE_TITLE) }
                @for parent in &parents {
                    " · " a href=(selection.period_quality_path(*parent)) { (parent.title()) }
                }
            }
            h1 { (heading) }
            p .subtitle { a #statistics-link href=(selection.period_path(period)) { "Statistics of this " (noun) } }
            @if in_progress {
                p #updated { "So far, as of " (updated(now, &state.tz)) }
            }
            (selection::vehicles_nav(&selection, page, &lines))
            nav .period-nav aria-label="Neighbouring periods" {
                a #previous rel="prev" href=(selection.period_quality_path(previous)) { "← Previous " (noun) }
                @if next_started {
                    a #next rel="next" href=(selection.period_quality_path(next)) { "Next " (noun) " →" }
                }
            }
        }
        main {
            section #summary {
                h2 { "Summary" }
                (period_tables(&selection, &quality, &position_subjects))
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
                            a href=(selection.period_quality_path(*part)) { (part.short_label()) }
                        }
                    }
                }
            }
            (selection::lines_section(&selection, page, &lines))
        }
    };
    Ok(shell(&format!("{heading} – {TITLE}"), &body).into_response())
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

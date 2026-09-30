use anyhow::Result;
use jiff::tz::TimeZone;
use jiff::{RoundMode, Span, Timestamp, Unit, ZonedRound};
use plotters::coord::Shift;
use plotters::coord::cartesian::Cartesian2d;
use plotters::coord::combinators::WithKeyPoints;
use plotters::coord::types::{RangedCoordf64, RangedCoordi64};
use plotters::element::DashedPathElement;
use plotters::prelude::*;
use plotters::style::text_anchor::{HPos, Pos, VPos};
use serde::Serialize;

pub use plotters::style::RGBColor;

pub const WIDTH: u32 = 960;
pub const HEIGHT: u32 = 320;
const MARGIN: u32 = 16;
const X_LABEL_AREA: u32 = 32;
const Y_LABEL_AREA: u32 = 64;
const Y_LABELS: usize = 6;
const FONT: &str = "sans-serif";
const FONT_SIZE: u32 = 16;
const NO_DATA_FONT_SIZE: u32 = 22;
const LINE_WIDTH: u32 = 2;
const MARKER_RADIUS: i32 = 4;
const LEGEND_LINE_LENGTH: i32 = 16;
const DASH_LENGTH: i32 = 6;
const DASH_GAP: i32 = 4;
const MIN_SERIES_FOR_LEGEND: usize = 2;
const HEADROOM: f64 = 1.4;
const LEGEND_ROW_PX: u32 = 20;
const LEGEND_OFFSET_AND_PADDING_PX: u32 = 17;
const LEGEND_GAP_PX: u32 = 8;
const MIN_Y_MAX_FOR_WHOLE_LABELS: f64 = 10.0;
const EMPTY_Y_MAX: f64 = 1.0;

const SURFACE: RGBColor = RGBColor(0xfc, 0xfc, 0xfb);
const TEXT_SECONDARY: RGBColor = RGBColor(0x52, 0x51, 0x4e);
const GRIDLINE: RGBColor = RGBColor(0xe1, 0xe0, 0xd9);
const AXIS: RGBColor = RGBColor(0xc3, 0xc2, 0xb7);

pub struct Point {
    pub unix: i64,
    pub value: Option<f64>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stroke {
    Solid,
    Dashed,
}

pub struct Series {
    pub name: String,
    pub color: RGBColor,
    pub stroke: Stroke,
    pub points: Vec<Point>,
}

#[derive(Debug, Clone, Copy)]
pub enum TickStep {
    Minutes(i64),
    Hours(i64),
    Days(i64),
    Months(i64),
}

impl TickStep {
    fn rounding(self) -> ZonedRound {
        let (unit, increment) = match self {
            TickStep::Minutes(minutes) => (Unit::Minute, minutes),
            TickStep::Hours(hours) => (Unit::Hour, hours),
            TickStep::Days(_) | TickStep::Months(_) => (Unit::Day, 1),
        };
        ZonedRound::new()
            .smallest(unit)
            .increment(increment)
            .mode(RoundMode::Ceil)
    }

    fn span(self) -> Span {
        match self {
            TickStep::Minutes(minutes) => Span::new().minutes(minutes),
            TickStep::Hours(hours) => Span::new().hours(hours),
            TickStep::Days(days) => Span::new().days(days),
            TickStep::Months(months) => Span::new().months(months),
        }
    }
}

/// The text a chart draws, in the reader's language and time zone.
pub trait Labels {
    fn tick(&self, unix: i64) -> String;
    fn hover(&self, unix: i64) -> String;
    fn number(&self, value: f64, decimals: usize) -> String;
    fn no_data(&self) -> &str;
}

pub struct LineChart<'a> {
    pub from_unix: i64,
    pub to_unix: i64,
    pub y_label: &'a str,
    pub labels: &'a dyn Labels,
    pub tz: TimeZone,
    pub tick_step: TickStep,
    pub series: Vec<Series>,
    /// The one legend entry for all dashed series; dashed series get no entry of their own.
    pub dashed_legend: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hover {
    pub width: u32,
    pub height: u32,
    pub plot: PlotArea,
    pub series: Vec<HoverSeries>,
    pub steps: Vec<HoverStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PlotArea {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HoverSeries {
    pub name: String,
    pub color: String,
    pub stroke: Stroke,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HoverStep {
    pub x: i32,
    pub label: String,
    pub values: Vec<HoverValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HoverValue {
    pub y: Option<i32>,
    pub text: String,
}

type Plot<'a, 'b> =
    ChartContext<'a, SVGBackend<'b>, Cartesian2d<WithKeyPoints<RangedCoordi64>, RangedCoordf64>>;

pub struct Rendered {
    pub svg: String,
    pub hover: Hover,
}

pub fn render(chart: &LineChart) -> Result<Rendered> {
    let mut svg = String::new();
    let hover = {
        let root = SVGBackend::with_string(&mut svg, (WIDTH, HEIGHT)).into_drawing_area();
        let hover = draw(&root, chart)?;
        root.present()?;
        hover
    };
    Ok(Rendered { svg, hover })
}

fn draw(root: &DrawingArea<SVGBackend, Shift>, chart: &LineChart) -> Result<Hover> {
    root.fill(&SURFACE)?;
    let max_value = chart
        .series
        .iter()
        .flat_map(|series| series.points.iter().filter_map(|point| point.value))
        .fold(None, |max: Option<f64>, value| {
            Some(max.map_or(value, |max| max.max(value)))
        });
    let legend_entries = legend_entries(chart);
    let y_max = max_value
        .filter(|max| *max > 0.0)
        .map_or(EMPTY_Y_MAX, |max| max * headroom(legend_entries));
    let label_style = (FONT, FONT_SIZE).into_font().color(&TEXT_SECONDARY);
    let ticks = time_ticks(chart.from_unix, chart.to_unix, chart.tick_step, &chart.tz)?;
    let tick_count = ticks.len();
    let mut plot = ChartBuilder::on(root)
        .margin(MARGIN)
        .x_label_area_size(X_LABEL_AREA)
        .y_label_area_size(Y_LABEL_AREA)
        .build_cartesian_2d(
            (chart.from_unix..chart.to_unix).with_key_points(ticks),
            0.0..y_max,
        )?;
    let format_time = |unix: &i64| chart.labels.tick(*unix);
    let y_decimals = usize::from(y_max < MIN_Y_MAX_FOR_WHOLE_LABELS);
    let format_value = |value: &f64| chart.labels.number(*value, y_decimals);
    plot.configure_mesh()
        .x_labels(tick_count)
        .y_labels(Y_LABELS)
        .x_label_formatter(&format_time)
        .y_label_formatter(&format_value)
        .y_desc(chart.y_label)
        .label_style(label_style.clone())
        .axis_desc_style(label_style.clone())
        .axis_style(AXIS)
        .bold_line_style(GRIDLINE)
        .light_line_style(TRANSPARENT)
        .draw()?;

    draw_series(&mut plot, chart)?;

    if max_value.is_none() {
        let center = (
            i32::try_from(WIDTH / 2).unwrap_or_default(),
            i32::try_from(HEIGHT / 2).unwrap_or_default(),
        );
        let style = (FONT, NO_DATA_FONT_SIZE)
            .into_font()
            .color(&TEXT_SECONDARY)
            .pos(Pos::new(HPos::Center, VPos::Center));
        root.draw(&Text::new(chart.labels.no_data(), center, style))?;
        return Ok(hover(&plot, chart, HoverSteps::Skip));
    }
    if legend_entries >= MIN_SERIES_FOR_LEGEND {
        plot.configure_series_labels()
            .position(SeriesLabelPosition::UpperLeft)
            .background_style(SURFACE)
            .border_style(GRIDLINE)
            .label_font(label_style)
            .draw()?;
    }
    Ok(hover(&plot, chart, HoverSteps::Include))
}

fn dashed_legend<'a>(chart: &LineChart<'a>) -> Option<&'a str> {
    let has_dashed = chart
        .series
        .iter()
        .any(|series| series.stroke == Stroke::Dashed);
    chart.dashed_legend.filter(|_| has_dashed)
}

fn legend_entries(chart: &LineChart) -> usize {
    let solid = chart
        .series
        .iter()
        .filter(|series| series.stroke == Stroke::Solid)
        .count();
    solid + usize::from(dashed_legend(chart).is_some())
}

#[allow(clippy::cast_precision_loss)]
fn headroom(legend_entries: usize) -> f64 {
    if legend_entries < MIN_SERIES_FOR_LEGEND {
        return HEADROOM;
    }
    let plot_px = f64::from(HEIGHT - 2 * MARGIN - X_LABEL_AREA);
    let legend_px = f64::from(LEGEND_ROW_PX) * legend_entries as f64
        + f64::from(LEGEND_OFFSET_AND_PADDING_PX + LEGEND_GAP_PX);
    if legend_px >= plot_px {
        return HEADROOM;
    }
    HEADROOM.max(plot_px / (plot_px - legend_px))
}

fn draw_series(plot: &mut Plot, chart: &LineChart) -> Result<()> {
    for series in &chart.series {
        let style = ShapeStyle {
            color: series.color.to_rgba(),
            filled: true,
            stroke_width: LINE_WIDTH,
        };
        for (index, run) in runs(&series.points).into_iter().enumerate() {
            let drawn = match (&run[..], series.stroke) {
                ([single], _) => plot.draw_series([Circle::new(*single, MARKER_RADIUS, style)])?,
                (_, Stroke::Solid) => plot.draw_series(LineSeries::new(run, style))?,
                (_, Stroke::Dashed) => {
                    plot.draw_series(DashedLineSeries::new(run, DASH_LENGTH, DASH_GAP, style))?
                }
            };
            if index > 0 || series.stroke == Stroke::Dashed {
                continue;
            }
            drawn.label(&series.name).legend(move |(x, y)| {
                PathElement::new([(x, y), (x + LEGEND_LINE_LENGTH, y)], style)
            });
        }
    }
    if let Some(label) = dashed_legend(chart) {
        let style = ShapeStyle {
            color: TEXT_SECONDARY.to_rgba(),
            filled: true,
            stroke_width: LINE_WIDTH,
        };
        plot.draw_series(std::iter::empty::<Circle<(i64, f64), i32>>())?
            .label(label)
            .legend(move |(x, y)| {
                DashedPathElement::new(
                    [(x, y), (x + LEGEND_LINE_LENGTH, y)],
                    DASH_LENGTH,
                    DASH_GAP,
                    style,
                )
            });
    }
    Ok(())
}

enum HoverSteps {
    Include,
    Skip,
}

fn hover(plot: &Plot, chart: &LineChart, steps: HoverSteps) -> Hover {
    let (x_pixels, y_pixels) = plot.plotting_area().get_pixel_range();
    let series = chart
        .series
        .iter()
        .map(|series| HoverSeries {
            name: series.name.clone(),
            color: hex(series.color),
            stroke: series.stroke,
        })
        .collect();
    let steps = match (steps, chart.series.first()) {
        (HoverSteps::Include, Some(first)) => first
            .points
            .iter()
            .enumerate()
            .map(|(index, point)| hover_step(plot, chart, index, point.unix))
            .collect(),
        _ => Vec::new(),
    };
    Hover {
        width: WIDTH,
        height: HEIGHT,
        plot: PlotArea {
            left: x_pixels.start,
            top: y_pixels.start,
            right: x_pixels.end,
            bottom: y_pixels.end,
        },
        series,
        steps,
    }
}

fn hover_step(plot: &Plot, chart: &LineChart, index: usize, unix: i64) -> HoverStep {
    HoverStep {
        x: plot.backend_coord(&(unix, 0.0)).0,
        label: chart.labels.hover(unix),
        values: chart
            .series
            .iter()
            .map(|series| {
                let point = series.points.get(index);
                HoverValue {
                    y: point
                        .and_then(|point| point.value)
                        .map(|value| plot.backend_coord(&(unix, value)).1),
                    text: point.map(|point| point.text.clone()).unwrap_or_default(),
                }
            })
            .collect(),
    }
}

fn hex(color: RGBColor) -> String {
    format!("#{:02x}{:02x}{:02x}", color.0, color.1, color.2)
}

fn runs(points: &[Point]) -> Vec<Vec<(i64, f64)>> {
    let mut runs = Vec::new();
    let mut current = Vec::new();
    for point in points {
        match point.value {
            Some(value) => current.push((point.unix, value)),
            None if !current.is_empty() => runs.push(std::mem::take(&mut current)),
            None => {}
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

fn time_ticks(from_unix: i64, to_unix: i64, step: TickStep, tz: &TimeZone) -> Result<Vec<i64>> {
    let start = Timestamp::from_second(from_unix)?.to_zoned(tz.clone());
    let mut tick = start.round(step.rounding())?;
    if let TickStep::Months(_) = step {
        let month_start = start.first_of_month()?.start_of_day()?;
        tick = if month_start < start {
            month_start.checked_add(Span::new().months(1))?
        } else {
            month_start
        };
    }
    let mut ticks = Vec::new();
    while tick.timestamp().as_second() < to_unix {
        ticks.push(tick.timestamp().as_second());
        tick = tick.checked_add(step.span())?;
    }
    Ok(ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: RGBColor = RGBColor(0xe3, 0x49, 0x48);
    const BLUE: RGBColor = RGBColor(0x2a, 0x78, 0xd6);
    const TIME_FORMAT: &str = "%H:%M";
    const TICK_MINUTES: i64 = 2;
    const MINUTE_SECS: i64 = 60;
    const HOUR_SECS: i64 = 60 * MINUTE_SECS;
    const MISSING: &str = "-";
    const NO_DATA: &str = "No data";

    struct TestLabels;

    impl Labels for TestLabels {
        fn tick(&self, unix: i64) -> String {
            local_time(unix, TIME_FORMAT)
        }

        fn hover(&self, unix: i64) -> String {
            local_time(unix, TIME_FORMAT)
        }

        fn number(&self, value: f64, decimals: usize) -> String {
            format!("{value:.decimals$}")
        }

        fn no_data(&self) -> &str {
            NO_DATA
        }
    }

    fn local_time(unix: i64, format: &str) -> String {
        Timestamp::from_second(unix).map_or_else(
            |_| String::new(),
            |timestamp| {
                timestamp
                    .to_zoned(TimeZone::UTC)
                    .strftime(format)
                    .to_string()
            },
        )
    }

    fn chart(series: Vec<Series>) -> LineChart<'static> {
        LineChart {
            from_unix: 0,
            to_unix: 600,
            y_label: "km/h",
            labels: &TestLabels,
            tz: TimeZone::UTC,
            tick_step: TickStep::Minutes(TICK_MINUTES),
            series,
            dashed_legend: None,
        }
    }

    fn points(values: &[(i64, Option<f64>)]) -> Vec<Point> {
        values
            .iter()
            .map(|(unix, value)| Point {
                unix: *unix,
                value: *value,
                text: value.map_or_else(|| MISSING.to_owned(), |value| format!("{value:.1}")),
            })
            .collect()
    }

    fn two_series() -> LineChart<'static> {
        chart(vec![
            Series {
                name: "Buses".to_owned(),
                color: RED,
                stroke: Stroke::Solid,
                points: points(&[
                    (0, Some(10.0)),
                    (60, Some(20.0)),
                    (120, None),
                    (180, Some(5.0)),
                ]),
            },
            Series {
                name: "Trams".to_owned(),
                color: BLUE,
                stroke: Stroke::Solid,
                points: points(&[(0, Some(15.0)), (60, Some(16.0)), (120, None), (180, None)]),
            },
        ])
    }

    fn svg_color(color: RGBColor) -> String {
        format!("#{:02X}{:02X}{:02X}", color.0, color.1, color.2)
    }

    fn has_text(svg: &str, text: &str) -> bool {
        svg.lines().any(|line| line == text)
    }

    #[test]
    fn splits_points_into_runs_at_gaps() {
        let points = points(&[
            (0, Some(1.0)),
            (60, Some(2.0)),
            (120, None),
            (180, None),
            (240, Some(3.0)),
            (300, None),
        ]);
        assert_eq!(runs(&points), [vec![(0, 1.0), (60, 2.0)], vec![(240, 3.0)]]);
        assert!(runs(&self::points(&[(0, None)])).is_empty());
    }

    #[test]
    fn puts_ticks_on_round_times() {
        let from_unix = 7 * MINUTE_SECS + 5;
        let ticks = time_ticks(
            from_unix,
            from_unix + HOUR_SECS,
            TickStep::Minutes(10),
            &TimeZone::UTC,
        )
        .unwrap();
        assert_eq!(
            ticks,
            [10, 20, 30, 40, 50, 60].map(|minutes| minutes * MINUTE_SECS)
        );
        let exact = time_ticks(0, 2 * MINUTE_SECS, TickStep::Minutes(1), &TimeZone::UTC).unwrap();
        assert_eq!(exact, [0, MINUTE_SECS]);

        let year_start = jiff::civil::date(2026, 1, 1)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        let year_end = year_start.checked_add(Span::new().years(1)).unwrap();
        let months = time_ticks(
            year_start.timestamp().as_second() + 1,
            year_end.timestamp().as_second(),
            TickStep::Months(1),
            &TimeZone::UTC,
        )
        .unwrap();
        let labels: Vec<String> = months
            .iter()
            .map(|unix| local_time(*unix, "%m-%d %H"))
            .collect();
        assert_eq!(labels.len(), 11);
        assert_eq!(labels[0], "02-01 00");
        assert_eq!(labels[10], "12-01 00");
    }

    #[test]
    fn warsaw_daily_ticks_follow_daylight_saving_transitions() {
        let tz = crate::time::warsaw();
        for (from, to, hours) in [
            ("2026-03-28T23:00:00Z", "2026-03-30T22:00:00Z", 23),
            ("2026-10-24T22:00:00Z", "2026-10-26T23:00:00Z", 25),
        ] {
            let from: Timestamp = from.parse().unwrap();
            let to: Timestamp = to.parse().unwrap();
            let ticks =
                time_ticks(from.as_second(), to.as_second(), TickStep::Days(1), &tz).unwrap();
            assert_eq!(ticks.len(), 2);
            assert_eq!(ticks[1] - ticks[0], hours * HOUR_SECS);
            for tick in ticks {
                assert_eq!(
                    Timestamp::from_second(tick)
                        .unwrap()
                        .to_zoned(tz.clone())
                        .time(),
                    jiff::civil::Time::midnight()
                );
            }
        }
    }

    #[test]
    fn draws_every_series_with_its_color_and_a_legend() {
        let svg = render(&two_series()).unwrap().svg;
        assert!(svg.starts_with("<svg"), "{svg}");
        assert!(svg.contains("km/h"));
        assert!(has_text(&svg, "Buses") && has_text(&svg, "Trams"), "{svg}");
        assert!(
            svg.contains(&svg_color(RED)) && svg.contains(&svg_color(BLUE)),
            "{svg}"
        );
        assert!(svg.contains("<circle"), "single points need a marker");
        assert!(!svg.contains(NO_DATA));
    }

    #[test]
    fn describes_every_step_for_hovering() {
        let hover = render(&two_series()).unwrap().hover;
        assert_eq!((hover.width, hover.height), (WIDTH, HEIGHT));
        assert_eq!(
            hover.series,
            [
                HoverSeries {
                    name: "Buses".to_owned(),
                    color: "#e34948".to_owned(),
                    stroke: Stroke::Solid,
                },
                HoverSeries {
                    name: "Trams".to_owned(),
                    color: "#2a78d6".to_owned(),
                    stroke: Stroke::Solid,
                },
            ]
        );
        let plot = hover.plot;
        assert!(
            0 < plot.left && plot.left < plot.right && plot.right <= i32::try_from(WIDTH).unwrap()
        );
        assert!(
            0 < plot.top && plot.top < plot.bottom && plot.bottom <= i32::try_from(HEIGHT).unwrap()
        );

        let xs: Vec<i32> = hover.steps.iter().map(|step| step.x).collect();
        assert_eq!(xs.len(), 4);
        assert_eq!(xs[0], plot.left);
        assert!(xs.windows(2).all(|pair| pair[0] < pair[1]), "{xs:?}");
        assert!(xs.iter().all(|x| (plot.left..=plot.right).contains(x)));

        let first = &hover.steps[0];
        assert_eq!(first.label, local_time(0, TIME_FORMAT));
        assert_eq!(
            first
                .values
                .iter()
                .map(|v| v.text.as_str())
                .collect::<Vec<_>>(),
            ["10.0", "15.0"]
        );
        let [bus, tram] = [first.values[0].y.unwrap(), first.values[1].y.unwrap()];
        assert!(tram < bus, "the higher value is drawn higher");
        assert!((plot.top..=plot.bottom).contains(&bus));

        let gap = &hover.steps[2];
        assert!(gap.values.iter().all(|value| value.y.is_none()));
        assert!(gap.values.iter().all(|value| value.text == MISSING));
        assert_eq!(hover.steps[3].values[1].y, None);
        assert!(hover.steps[3].values[0].y.is_some());
    }

    #[test]
    fn draws_dashed_series_with_one_shared_legend_entry() {
        const WHILE_MOVING: &str = "While moving";
        let mut chart = two_series();
        chart.series[1].stroke = Stroke::Dashed;
        chart.series[1].color = RED;
        chart.dashed_legend = Some(WHILE_MOVING);
        let rendered = render(&chart).unwrap();
        let svg = &rendered.svg;
        assert!(
            has_text(svg, "Buses") && has_text(svg, WHILE_MOVING),
            "{svg}"
        );
        assert!(!has_text(svg, "Trams"), "{svg}");
        let solid = render(&two_series()).unwrap().svg;
        assert!(
            svg.matches("<polyline").count() > solid.matches("<polyline").count(),
            "dashes are drawn as separate lines"
        );
        let strokes: Vec<Stroke> = rendered.hover.series.iter().map(|s| s.stroke).collect();
        assert_eq!(strokes, [Stroke::Solid, Stroke::Dashed]);
        assert_eq!(rendered.hover.series[1].name, "Trams");
    }

    #[test]
    fn leaves_room_above_the_data_for_the_legend() {
        assert!((headroom(1) - HEADROOM).abs() < f64::EPSILON);
        assert!((headroom(2) - HEADROOM).abs() < f64::EPSILON);
        assert!(headroom(3) > HEADROOM);
        assert!(headroom(4) > headroom(3));
    }

    #[test]
    fn says_when_there_is_no_data() {
        let rendered = render(&chart(vec![Series {
            name: "Buses".to_owned(),
            color: RED,
            stroke: Stroke::Solid,
            points: points(&[(0, None)]),
        }]))
        .unwrap();
        assert!(has_text(&rendered.svg, NO_DATA), "{}", rendered.svg);
        assert!(rendered.hover.steps.is_empty());
    }
}

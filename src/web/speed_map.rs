use std::time::Duration;

use anyhow::Result;
use maud::{Markup, PreEscaped, html};
use serde::Serialize;

use super::i18n::{Text, Translation};
use super::map::{ATTRIBUTION, ATTRIBUTION_URL, SVG_NAMESPACE, rounded};
use super::selection::{self, Subject};
use super::{AppState, BASEMAP_PATH, Metric, css_color, script_json};
use crate::cells::{self, CellSpeeds, CellTotals};
use crate::map::Basemap;

const MIN_CELL_TRAVEL: Duration = Duration::from_mins(5);
const SPEED_DECIMALS: usize = 0;
const BAND_LABEL_BELOW: &str = "< ";
const BAND_LABEL_FROM: &str = "≥ ";
const BAND_LABEL_RANGE: &str = "–";

struct Band {
    from_kmh: f64,
    color: &'static str,
}

const BANDS: [Band; 7] = [
    Band {
        from_kmh: 0.0,
        color: "#cde2fb",
    },
    Band {
        from_kmh: 10.0,
        color: "#9ec5f4",
    },
    Band {
        from_kmh: 15.0,
        color: "#6da7ec",
    },
    Band {
        from_kmh: 20.0,
        color: "#3987e5",
    },
    Band {
        from_kmh: 25.0,
        color: "#256abf",
    },
    Band {
        from_kmh: 30.0,
        color: "#184f95",
    },
    Band {
        from_kmh: 35.0,
        color: "#0d366b",
    },
];

fn band(speed_kmh: f64) -> &'static Band {
    BANDS
        .iter()
        .rev()
        .find(|band| speed_kmh >= band.from_kmh)
        .unwrap_or(&BANDS[0])
}

#[cfg(test)]
pub(super) fn slowest_color() -> &'static str {
    BANDS[0].color
}

fn band_label(tr: &dyn Translation, index: usize) -> String {
    let number = |kmh| tr.number(kmh, SPEED_DECIMALS);
    let from_kmh = BANDS[index].from_kmh;
    match BANDS.get(index + 1) {
        None => format!("{BAND_LABEL_FROM}{}", number(from_kmh)),
        Some(next) if index == 0 => format!("{BAND_LABEL_BELOW}{}", number(next.from_kmh)),
        Some(next) => format!(
            "{}{BAND_LABEL_RANGE}{}",
            number(from_kmh),
            number(next.from_kmh)
        ),
    }
}

fn enough(totals: &CellTotals) -> bool {
    totals.travel() >= MIN_CELL_TRAVEL
}

struct Shown {
    points: String,
    color: &'static str,
    speeds: Vec<Option<[String; 2]>>,
}

#[derive(Serialize)]
struct HoverData<'a> {
    unit: &'a str,
    columns: [&'a str; 2],
    subjects: Vec<HoverSubject<'a>>,
    cells: Vec<&'a [Option<[String; 2]>]>,
}

#[derive(Serialize)]
struct HoverSubject<'a> {
    name: &'a str,
    color: String,
}

fn polygon(basemap: &Basemap, cell: &CellSpeeds) -> Option<String> {
    let (latitude, longitude) = cells::center(cell.cell);
    basemap.pixel(latitude, longitude)?;
    Some(
        cells::boundary(cell.cell)
            .into_iter()
            .map(|(latitude, longitude)| {
                let (x, y) = basemap.unclipped_pixel(latitude, longitude);
                format!("{},{}", rounded(x), rounded(y))
            })
            .collect::<Vec<String>>()
            .join(" "),
    )
}

pub(super) async fn section(
    state: &AppState,
    tr: &dyn Translation,
    subjects: &[Subject],
    (from_unix, to_unix): (i64, i64),
    now_unix: i64,
) -> Result<Option<Markup>> {
    let Some(basemap) = state.basemap.get() else {
        return Ok(None);
    };
    let cells = state
        .period_cells(from_unix, to_unix, now_unix, &selection::groups(subjects))
        .await?;
    let figure = figure(tr, basemap, subjects, &cells)?;
    Ok(Some(html! {
        section #speed-map {
            h2 { (tr.text(Text::SpeedMap)) }
            (figure)
        }
    }))
}

/// `cells` has the totals of every subject, in the order of `subjects`. The first
/// subject colors the map.
fn figure(
    tr: &dyn Translation,
    basemap: &Basemap,
    subjects: &[Subject],
    cells: &[CellSpeeds],
) -> Result<Markup> {
    let speed = |totals: &CellTotals| {
        let format = |metric: Metric, value| metric.format_value(value, tr);
        enough(totals).then(|| {
            [
                format(Metric::AverageSpeed, totals.average_speed_kmh()),
                format(Metric::MovingSpeed, totals.moving_speed_kmh()),
            ]
        })
    };
    let kept: Vec<usize> = (0..subjects.len())
        .filter(|index| cells.iter().any(|cell| enough(&cell.totals[*index])))
        .collect();
    let shown: Vec<Shown> = cells
        .iter()
        .filter_map(|cell| {
            let main = cell.totals.first().filter(|totals| enough(totals))?;
            let speed_kmh = main.average_speed_kmh()?;
            Some(Shown {
                points: polygon(basemap, cell)?,
                color: band(speed_kmh).color,
                speeds: kept
                    .iter()
                    .map(|index| speed(&cell.totals[*index]))
                    .collect(),
            })
        })
        .collect();
    let hover = script_json(&HoverData {
        unit: tr.text(Text::SpeedKmh),
        columns: [tr.text(Text::Average), tr.text(Text::WhileMoving)],
        subjects: kept
            .iter()
            .map(|index| HoverSubject {
                name: &subjects[*index].label,
                color: css_color(subjects[*index].color),
            })
            .collect(),
        cells: shown.iter().map(|cell| &cell.speeds[..]).collect(),
    })?;
    let (width, height) = (basemap.width(), basemap.height());
    Ok(html! {
        figure .speed-map {
            div .map-plot {
                svg xmlns=(SVG_NAMESPACE) viewBox=(format!("0 0 {width} {height}"))
                    width=(width) height=(height) role="img"
                    aria-label=(tr.text(Text::SpeedMapDescription)) {
                    image href=(BASEMAP_PATH) width=(width) height=(height) {}
                    g .speed-cells {
                        @for (index, cell) in shown.iter().enumerate() {
                            polygon points=(cell.points) fill=(cell.color) data-cell=(index) {}
                        }
                    }
                }
            }
            div .map-tooltip role="status" hidden {}
            script .speed-map-data type="application/json" { (PreEscaped(hover)) }
            figcaption {
                @if shown.is_empty() {
                    p .map-empty { (tr.text(Text::NoData)) }
                } @else {
                    p .speed-legend-title { (tr.metric(Metric::AverageSpeed)) }
                    ul .speed-legend {
                        @for (index, band) in BANDS.iter().enumerate() {
                            li {
                                span .speed-swatch style=(format!("background: {}", band.color)) {}
                                span { (band_label(tr, index)) }
                            }
                        }
                    }
                }
                p .map-attribution { a href=(ATTRIBUTION_URL) { (ATTRIBUTION) } }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::i18n::Language;

    #[test]
    fn puts_speeds_in_bands_and_names_them() {
        let colors: Vec<&str> = [0.0, 9.9, 10.0, 120.0]
            .into_iter()
            .map(|speed_kmh| band(speed_kmh).color)
            .collect();
        assert_eq!(
            colors,
            [
                BANDS[0].color,
                BANDS[0].color,
                BANDS[1].color,
                BANDS[6].color
            ]
        );
        let en = Language::English.translation();
        let labels: Vec<String> = (0..BANDS.len())
            .map(|index| band_label(en, index))
            .collect();
        assert_eq!(
            labels,
            ["< 10", "10–15", "15–20", "20–25", "25–30", "30–35", "≥ 35"]
        );
    }
}

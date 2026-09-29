use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, PreEscaped, html};
use serde::Serialize;
use tracing::{info, warn};

use super::caching::Caching;
use super::i18n::{Language, Text, Translation};
use super::selection::{Selection, SelectionKey};
use super::{
    AppError, AppState, MISSING_VALUE, SVG_CONTENT_TYPE, css_color, kind_color, language_headers,
    script_json,
};
use crate::db::{PositionFilter, StoredPosition};
use crate::map::{Basemap, Symbol, TileClient};
use crate::mpk::VehicleType;
use crate::speed;
use crate::stats::Kind;

pub(super) const BASEMAP_PATH: &str = "/map/wroclaw.jpg";
pub(super) const MAP_PATH: &str = "/map.svg";
const JPEG_CONTENT_TYPE: &str = "image/jpeg";
pub(super) const SVG_NAMESPACE: &str = "http://www.w3.org/2000/svg";
const ACTIVE_MAX_AGE: Duration = Duration::from_secs(30);
const DOWNLOAD_RETRY: Duration = Duration::from_mins(15);
const SPEED_UNIT: &str = "km/h";
const SPEED_DECIMALS: usize = 0;
const LABEL_SEPARATOR: &str = ", ";
const LEGEND_SYMBOL_PX: u32 = 14;
const STEPS_PER_PIXEL: f64 = 10.0;
pub(super) const ATTRIBUTION: &str = "© OpenStreetMap contributors";
pub(super) const ATTRIBUTION_URL: &str = "https://www.openstreetmap.org/copyright";

pub enum MapTiles {
    Download(TileClient),
    Disabled,
}

#[derive(Clone, Default)]
pub(super) struct SharedBasemap(Arc<OnceLock<Basemap>>);

impl SharedBasemap {
    #[cfg(test)]
    pub(super) fn ready(basemap: Basemap) -> Self {
        Self(Arc::new(OnceLock::from(basemap)))
    }

    pub(super) fn get(&self) -> Option<&Basemap> {
        self.0.get()
    }

    pub(super) async fn download(self, client: TileClient) {
        loop {
            match Basemap::download(&client).await {
                Ok(basemap) => {
                    info!(
                        width = basemap.width(),
                        height = basemap.height(),
                        "map ready"
                    );
                    self.0.set(basemap).ok();
                    return;
                }
                Err(e) => warn!("failed to download the map: {e:#}"),
            }
            tokio::time::sleep(DOWNLOAD_RETRY).await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerLabels {
    Titles,
    HoverData,
}

struct Marker {
    vehicle_type: VehicleType,
    x: f64,
    y: f64,
    name: String,
    speed: String,
}

#[derive(Serialize)]
struct HoverData<'a> {
    width: u32,
    height: u32,
    vehicles: Vec<HoverVehicle<'a>>,
}

#[derive(Serialize)]
struct HoverVehicle<'a> {
    x: f64,
    y: f64,
    name: &'a str,
    speed: &'a str,
    color: String,
}

fn symbol(vehicle_type: VehicleType) -> Symbol {
    match vehicle_type {
        VehicleType::Bus => Symbol::Circle,
        VehicleType::Tram => Symbol::Square,
        VehicleType::Train => Symbol::Triangle,
    }
}

fn color(vehicle_type: VehicleType) -> String {
    css_color(kind_color(Kind::Vehicle(vehicle_type)))
}

async fn vehicles(
    state: &AppState,
    tr: &dyn Translation,
    selection: &Selection,
    basemap: &Basemap,
) -> Result<Vec<Marker>> {
    let now_unix = state.clock.now().as_second();
    let positions: Vec<StoredPosition> = state
        .db
        .positions_at(PositionFilter::All, now_unix, ACTIVE_MAX_AGE)
        .await?
        .into_iter()
        .filter(|position| selection.includes(position.vehicle_type, position.line_id))
        .collect();
    let speeds = speed::speeds_kmh(&state.db, &positions, now_unix).await?;
    Ok(positions
        .iter()
        .filter_map(|position| {
            let (x, y) = basemap.pixel(position.latitude, position.longitude)?;
            let speed = speeds
                .get(&(position.line_id, position.course_id))
                .map_or_else(
                    || MISSING_VALUE.to_owned(),
                    |speed| format!("{} {SPEED_UNIT}", tr.number(*speed, SPEED_DECIMALS)),
                );
            Some(Marker {
                vehicle_type: position.vehicle_type,
                x: rounded(x),
                y: rounded(y),
                name: tr.line(position.vehicle_type, &position.line),
                speed,
            })
        })
        .collect())
}

pub(super) fn rounded(pixel: f64) -> f64 {
    (pixel * STEPS_PER_PIXEL).round() / STEPS_PER_PIXEL
}

fn svg(
    tr: &dyn Translation,
    basemap: &Basemap,
    vehicles: &[Marker],
    labels: MarkerLabels,
) -> Markup {
    let (width, height) = (basemap.width(), basemap.height());
    html! {
        svg xmlns=(SVG_NAMESPACE) viewBox=(format!("0 0 {width} {height}"))
            width=(width) height=(height) role="img" aria-label=(tr.text(Text::MapDescription)) {
            image href=(BASEMAP_PATH) width=(width) height=(height) {}
            @for vehicle in vehicles {
                g data-kind=(vehicle.vehicle_type.as_str()) {
                    (PreEscaped(symbol(vehicle.vehicle_type).svg(vehicle.x, vehicle.y, &color(vehicle.vehicle_type))))
                    @if labels == MarkerLabels::Titles {
                        title { (vehicle.name) (LABEL_SEPARATOR) (vehicle.speed) }
                    }
                }
            }
        }
    }
}

fn legend_symbol(vehicle_type: VehicleType) -> Markup {
    let extent = Symbol::extent();
    html! {
        svg .map-symbol xmlns=(SVG_NAMESPACE) aria-hidden="true"
            viewBox=(format!("{} {} {} {}", -extent, -extent, 2.0 * extent, 2.0 * extent))
            width=(LEGEND_SYMBOL_PX) height=(LEGEND_SYMBOL_PX) {
            (PreEscaped(symbol(vehicle_type).svg(0.0, 0.0, &color(vehicle_type))))
        }
    }
}

fn legend(
    tr: &dyn Translation,
    selection: &Selection,
    vehicles: &[Marker],
) -> Vec<(VehicleType, String, usize)> {
    VehicleType::ALL
        .into_iter()
        .filter_map(|vehicle_type| {
            let count = vehicles
                .iter()
                .filter(|vehicle| vehicle.vehicle_type == vehicle_type)
                .count();
            if count == 0 {
                return None;
            }
            let label = match selection {
                Selection::Line(_) => selection.label(tr),
                Selection::All | Selection::Type(_) => {
                    tr.kind(Kind::Vehicle(vehicle_type)).to_owned()
                }
            };
            Some((vehicle_type, label, count))
        })
        .collect()
}

pub(super) async fn section(
    state: &AppState,
    tr: &dyn Translation,
    selection: &Selection,
) -> Result<Option<Markup>> {
    let Some(basemap) = state.basemap.get() else {
        return Ok(None);
    };
    let vehicles = vehicles(state, tr, selection, basemap).await?;
    let hover = HoverData {
        width: basemap.width(),
        height: basemap.height(),
        vehicles: vehicles
            .iter()
            .map(|vehicle| HoverVehicle {
                x: vehicle.x,
                y: vehicle.y,
                name: &vehicle.name,
                speed: &vehicle.speed,
                color: color(vehicle.vehicle_type),
            })
            .collect(),
    };
    let hover = script_json(&hover)?;
    let legend = legend(tr, selection, &vehicles);
    Ok(Some(html! {
        section #map {
            h2 { (tr.text(Text::VehiclesNow)) }
            figure .map data-src=(selection.map_path()) {
                div .map-plot { (svg(tr, basemap, &vehicles, MarkerLabels::HoverData)) }
                div .map-tooltip role="status" hidden {}
                script .map-data type="application/json" { (PreEscaped(hover)) }
                figcaption {
                    @if legend.is_empty() {
                        p .map-empty { (tr.text(Text::NoVehiclesNow)) }
                    } @else {
                        ul .map-legend {
                            @for (vehicle_type, label, count) in &legend {
                                li data-kind=(vehicle_type.as_str()) {
                                    (legend_symbol(*vehicle_type))
                                    span { (label) }
                                    span .map-count { (count) }
                                }
                            }
                        }
                    }
                    p .map-attribution { a href=(ATTRIBUTION_URL) { (ATTRIBUTION) } }
                }
            }
        }
    }))
}

pub(super) async fn image(
    state: AppState,
    language: Language,
    key: SelectionKey,
) -> Result<Response, AppError> {
    let Some(selection) = key.resolve(&state.db).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let Some(basemap) = state.basemap.get() else {
        return Ok(StatusCode::SERVICE_UNAVAILABLE.into_response());
    };
    let tr = language.translation();
    let vehicles = vehicles(&state, tr, &selection, basemap).await?;
    Ok((
        language_headers(language),
        Caching::live()?,
        [(header::CONTENT_TYPE, SVG_CONTENT_TYPE)],
        svg(tr, basemap, &vehicles, MarkerLabels::Titles).into_string(),
    )
        .into_response())
}

pub(super) fn basemap(state: &AppState) -> Result<Response, AppError> {
    let Some(basemap) = state.basemap.get() else {
        return Ok(StatusCode::SERVICE_UNAVAILABLE.into_response());
    };
    Ok((
        Caching::stable()?,
        [(header::CONTENT_TYPE, JPEG_CONTENT_TYPE)],
        basemap.jpeg().to_vec(),
    )
        .into_response())
}

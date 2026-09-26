use std::fmt;
use std::str::FromStr;

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;

use crate::geo::{self, MicroDegrees};

const FORECAST_URL: &str = "https://api.open-meteo.com/v1/forecast";
const CURRENT_VARIABLES: &[&str] = &[
    "temperature_2m",
    "relative_humidity_2m",
    "precipitation",
    "rain",
    "showers",
    "snowfall",
    "weather_code",
    "cloud_cover",
    "surface_pressure",
    "wind_speed_10m",
    "wind_direction_10m",
    "wind_gusts_10m",
];
const TIME_FORMAT: &str = "unixtime";
const FORECAST_DAYS: &str = "1";
const AREA_PARTS: usize = 4;

/*
bounding box of 548545 position(s) in all data
latitude:  51.014816 .. 51.198370
longitude: 16.843672 .. 17.166170
*/
pub const DEFAULT_AREA: &str = "51.014816,16.843672,51.198370,17.166170";
pub const DEFAULT_MODEL: &str = "icon_d2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Area {
    pub south: MicroDegrees,
    pub west: MicroDegrees,
    pub north: MicroDegrees,
    pub east: MicroDegrees,
}

impl FromStr for Area {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let parts = s
            .split(',')
            .map(|part| part.trim().parse::<f64>().map(MicroDegrees::from))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("invalid area {s:?}: {e}"))?;
        let [south, west, north, east] = parts[..] else {
            bail!("invalid area {s:?}: expected {AREA_PARTS} numbers \"south,west,north,east\"");
        };
        if !geo::is_in_range(south, west) || !geo::is_in_range(north, east) {
            bail!("invalid area {s:?}: coordinates out of range");
        }
        if south >= north || west >= east {
            bail!("invalid area {s:?}: south must be below north and west below east");
        }
        Ok(Self {
            south,
            west,
            north,
            east,
        })
    }
}

impl fmt::Display for Area {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:.6},{:.6},{:.6},{:.6}",
            self.south.degrees(),
            self.west.degrees(),
            self.north.degrees(),
            self.east.degrees()
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Current {
    #[serde(rename = "time")]
    pub observed_unix: i64,
    pub temperature_2m: Option<f64>,
    pub relative_humidity_2m: Option<f64>,
    pub precipitation: Option<f64>,
    pub rain: Option<f64>,
    pub showers: Option<f64>,
    pub snowfall: Option<f64>,
    pub weather_code: Option<i64>,
    pub cloud_cover: Option<f64>,
    pub surface_pressure: Option<f64>,
    pub wind_speed_10m: Option<f64>,
    pub wind_direction_10m: Option<f64>,
    pub wind_gusts_10m: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GridPoint {
    pub latitude: MicroDegrees,
    pub longitude: MicroDegrees,
    pub current: Current,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Response {
    Many(Vec<GridPoint>),
    One(GridPoint),
}

#[derive(Deserialize)]
struct ErrorBody {
    reason: String,
}

pub struct Client {
    http: reqwest::Client,
}

impl Client {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    pub async fn fetch_current(&self, area: &Area, model: &str) -> Result<Vec<GridPoint>> {
        let variables = CURRENT_VARIABLES.join(",");
        let bounding_box = area.to_string();
        let response = self
            .http
            .get(FORECAST_URL)
            .query(&[
                ("current", variables.as_str()),
                ("bounding_box", bounding_box.as_str()),
                ("models", model),
                ("timeformat", TIME_FORMAT),
                ("forecast_days", FORECAST_DAYS),
            ])
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            let reason = serde_json::from_str::<ErrorBody>(&body)
                .map(|error| error.reason)
                .unwrap_or(body);
            bail!("open-meteo returned {status}: {reason}");
        }
        let points = match serde_json::from_str(&body)? {
            Response::Many(points) => points,
            Response::One(point) => vec![point],
        };
        Ok(points)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_area() {
        let area: Area = DEFAULT_AREA.parse().unwrap();
        assert_eq!(area.south, MicroDegrees(51_014_816));
        assert_eq!(area.west, MicroDegrees(16_843_672));
        assert_eq!(area.north, MicroDegrees(51_198_370));
        assert_eq!(area.east, MicroDegrees(17_166_170));
        assert_eq!(area.to_string(), DEFAULT_AREA);
    }

    #[test]
    fn rejects_invalid_areas() {
        for input in [
            "51.0,16.8,51.2",
            "51.0,16.8,51.2,17.1,1.0",
            "51.2,16.8,51.0,17.1",
            "51.0,17.1,51.2,16.8",
            "51.0,16.8,95.0,17.1",
            "a,16.8,51.2,17.1",
        ] {
            assert!(input.parse::<Area>().is_err(), "{input}");
        }
    }

    #[test]
    fn parses_grid_response() {
        let json = r#"[{"latitude":51.02,"longitude":16.839998,"current":{"time":1790342100,"interval":900,
            "temperature_2m":14.8,"relative_humidity_2m":73,"precipitation":0.0,"rain":0.0,"showers":0.0,
            "snowfall":0.0,"weather_code":3,"cloud_cover":98,"surface_pressure":1021.7,"wind_speed_10m":10.3,
            "wind_direction_10m":299,"wind_gusts_10m":null}}]"#;
        let Response::Many(points) = serde_json::from_str(json).unwrap() else {
            panic!("expected a list");
        };
        let point = &points[0];
        assert_eq!(point.latitude, MicroDegrees(51_020_000));
        assert_eq!(point.longitude, MicroDegrees(16_839_998));
        assert_eq!(point.current.observed_unix, 1_790_342_100);
        assert_eq!(point.current.relative_humidity_2m, Some(73.0));
        assert_eq!(point.current.weather_code, Some(3));
        assert_eq!(point.current.wind_gusts_10m, None);
    }

    #[test]
    fn parses_single_point_response() {
        let json = r#"{"latitude":51.1,"longitude":17.0,"current":{"time":1790342100}}"#;
        assert!(matches!(
            serde_json::from_str(json).unwrap(),
            Response::One(_)
        ));
    }
}

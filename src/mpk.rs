use anyhow::{Result, anyhow};
use scraper::{Html, Selector};
use serde::Deserialize;
use tracing::warn;

use crate::fetch::{self, ErrorKind, FetchError, Source};
use crate::geo::{self, MicroDegrees};

const MAP_PAGE_URL: &str =
    "https://mpk.wroc.pl/strefa-pasazera/zaplanuj-podroz/mapa-pozycji-pojazdow";
const POSITIONS_URL: &str = "https://mpk.wroc.pl/bus_position";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VehicleType {
    Bus = 0,
    Tram = 1,
    Train = 2,
}

impl VehicleType {
    pub const ALL: [VehicleType; 3] = [VehicleType::Bus, VehicleType::Tram, VehicleType::Train];

    pub fn id(self) -> i64 {
        self as i64
    }

    pub fn as_str(self) -> &'static str {
        match self {
            VehicleType::Bus => "bus",
            VehicleType::Tram => "tram",
            VehicleType::Train => "train",
        }
    }

    fn lines_selector(self) -> &'static str {
        match self {
            VehicleType::Bus => ".bus-gps-lines.bus li.line",
            VehicleType::Tram => ".bus-gps-lines.tram li.line",
            VehicleType::Train => ".bus-gps-lines.train li.line",
        }
    }

    fn form_key(self) -> &'static str {
        match self {
            VehicleType::Bus => "busList[bus][]",
            VehicleType::Tram => "busList[tram][]",
            VehicleType::Train => "busList[train][]",
        }
    }
}

impl TryFrom<i64> for VehicleType {
    type Error = anyhow::Error;

    fn try_from(id: i64) -> Result<Self> {
        VehicleType::ALL
            .into_iter()
            .find(|vehicle_type| vehicle_type.id() == id)
            .ok_or_else(|| anyhow!("unknown vehicle type id: {id}"))
    }
}

#[derive(Debug, Clone)]
pub struct Line {
    pub vehicle_type: VehicleType,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Position {
    #[serde(rename = "type")]
    pub vehicle_type: VehicleType,
    #[serde(rename = "name")]
    pub line: String,
    #[serde(rename = "k")]
    pub course_id: i64,
    #[serde(rename = "x")]
    pub latitude: MicroDegrees,
    #[serde(rename = "y")]
    pub longitude: MicroDegrees,
}

impl Position {
    pub fn has_valid_coordinates(&self) -> bool {
        geo::is_plausible_fix(self.latitude, self.longitude)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Entry {
    Position(Position),
    Unrecognized(serde_json::Value),
}

type LineSelectors = Vec<(VehicleType, Selector)>;

pub struct Client {
    http: reqwest::Client,
    line_selectors: LineSelectors,
}

impl Client {
    pub fn new(http: reqwest::Client) -> Result<Self> {
        let line_selectors = VehicleType::ALL
            .into_iter()
            .map(|vehicle_type| {
                Selector::parse(vehicle_type.lines_selector())
                    .map(|selector| (vehicle_type, selector))
                    .map_err(|e| anyhow!("invalid selector: {e}"))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            http,
            line_selectors,
        })
    }

    pub async fn fetch_lines(&self) -> Result<Vec<Line>, FetchError> {
        let fetched = fetch::send(Source::MpkLines, self.http.get(MAP_PAGE_URL))
            .await?
            .require_success()?;
        let lines = parse_lines(&fetched.body, &self.line_selectors);
        if lines.is_empty() {
            return Err(fetched.error(
                ErrorKind::Empty,
                format!("no lines found on {MAP_PAGE_URL}"),
            ));
        }
        Ok(lines)
    }

    pub async fn fetch_positions(&self, lines: &[Line]) -> Result<Vec<Position>, FetchError> {
        let form: Vec<(&str, &str)> = lines
            .iter()
            .map(|line| (line.vehicle_type.form_key(), line.name.as_str()))
            .collect();
        let request = self.http.post(POSITIONS_URL).form(&form);
        let entries: Vec<Entry> = fetch::send(Source::MpkPositions, request)
            .await?
            .require_success()?
            .json()?;
        Ok(split_entries(entries))
    }
}

fn split_entries(entries: Vec<Entry>) -> Vec<Position> {
    let mut positions = Vec::with_capacity(entries.len());
    let mut unrecognized = Vec::new();
    for entry in entries {
        match entry {
            Entry::Position(position) => positions.push(position),
            Entry::Unrecognized(value) => unrecognized.push(value),
        }
    }
    if let Some(example) = unrecognized.first() {
        warn!(count = unrecognized.len(), %example, "skipped unrecognized positions");
    }
    positions
}

fn parse_lines(html: &str, selectors: &LineSelectors) -> Vec<Line> {
    let document = Html::parse_document(html);
    let mut lines = Vec::new();
    for (vehicle_type, selector) in selectors {
        for element in document.select(selector) {
            let name = element.text().collect::<String>().trim().to_lowercase();
            if name.is_empty() {
                continue;
            }
            lines.push(Line {
                vehicle_type: *vehicle_type,
                name,
            });
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines_from_map_page() {
        let html = r#"
            <div class="bus-gps-lines bus"><ul><li class="line">A</li><li class="line">710<br></li></ul></div>
            <div id="tramwaje" class="bus-gps-lines tram"><ul><li class="line">0</li><li class="line"> </li></ul></div>
            <div class="bus-gps-lines train"><ul><li class="line">S1</li></ul></div>
        "#;
        let client = Client::new(reqwest::Client::new()).unwrap();
        let lines = parse_lines(html, &client.line_selectors);
        let got: Vec<_> = lines
            .iter()
            .map(|l| (l.vehicle_type, l.name.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (VehicleType::Bus, "a"),
                (VehicleType::Bus, "710"),
                (VehicleType::Tram, "0"),
                (VehicleType::Train, "s1"),
            ]
        );
    }

    #[test]
    fn parses_positions_with_swapped_coordinates() {
        let json = r#"[{"name":"a","type":"bus","y":17.07566,"x":51.148666,"k":29073702}]"#;
        let positions: Vec<Position> = serde_json::from_str(json).unwrap();
        let p = &positions[0];
        assert_eq!(p.vehicle_type, VehicleType::Bus);
        assert_eq!(p.line, "a");
        assert_eq!(p.course_id, 29073702);
        assert_eq!(p.latitude, MicroDegrees(51_148_666));
        assert_eq!(p.longitude, MicroDegrees(17_075_660));
        assert!(p.has_valid_coordinates());
    }

    #[test]
    fn skips_unrecognized_entries() {
        let json = r#"[
            {"name":"s1","type":"train","y":17.0,"x":51.1,"k":1},
            {"name":"x","type":"ferry","y":17.0,"x":51.1,"k":2},
            {"name":"a","type":"bus","x":51.1,"k":3},
            {"name":"a","type":"bus","y":17.0,"x":51.1,"k":4}
        ]"#;
        let entries: Vec<Entry> = serde_json::from_str(json).unwrap();
        let got: Vec<_> = split_entries(entries)
            .iter()
            .map(|p| (p.vehicle_type, p.course_id))
            .collect();
        assert_eq!(got, [(VehicleType::Train, 1), (VehicleType::Bus, 4)]);
    }

    #[test]
    fn vehicle_type_ids_round_trip() {
        for vehicle_type in VehicleType::ALL {
            assert_eq!(
                VehicleType::try_from(vehicle_type.id()).unwrap(),
                vehicle_type
            );
        }
        assert_eq!(VehicleType::ALL.map(VehicleType::id), [0, 1, 2]);
        assert!(VehicleType::try_from(3).is_err());
    }

    #[test]
    fn rejects_out_of_range_coordinates() {
        let json = r#"[
            {"name":"19","type":"tram","y":7158.279,"x":7158.279,"k":28797438},
            {"name":"13","type":"tram","y":6416.1504,"x":4921.0796,"k":28800321},
            {"name":"a","type":"bus","y":17.07566,"x":91.0,"k":1},
            {"name":"a","type":"bus","y":-180.5,"x":51.1,"k":2},
            {"name":"142","type":"bus","y":0.0,"x":0.0,"k":29072067}
        ]"#;
        let positions: Vec<Position> = serde_json::from_str(json).unwrap();
        assert!(positions.iter().all(|p| !p.has_valid_coordinates()));
    }

    #[test]
    fn accepts_single_zero_coordinate() {
        let json = r#"[
            {"name":"a","type":"bus","y":17.0,"x":0.0,"k":1},
            {"name":"a","type":"bus","y":0.0,"x":51.1,"k":2}
        ]"#;
        let positions: Vec<Position> = serde_json::from_str(json).unwrap();
        assert!(positions.iter().all(Position::has_valid_coordinates));
    }
}

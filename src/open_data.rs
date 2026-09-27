use anyhow::{Context, Result};
use jiff::civil;
use jiff::tz::TimeZone;
use serde::Deserialize;
use tracing::warn;

use crate::geo::{self, MicroDegrees};

const VEHICLES_URL: &str = "https://open-data.cui.wroclaw.pl/hdb/db/14?download=json";
const TIME_ZONE: &str = "Europe/Warsaw";
const MISSING_PLATE: &str = "-";
const MISSING_SIDE_NUMBER: i64 = 0;

#[derive(Debug, Clone, PartialEq)]
pub struct VehicleRecord {
    pub side_number: i64,
    pub plate: Option<String>,
    pub brigade: Option<String>,
    pub line: String,
    pub latitude: MicroDegrees,
    pub longitude: MicroDegrees,
    pub updated_local: civil::DateTime,
    pub updated_unix_candidates: Vec<i64>,
}

#[derive(Deserialize)]
struct RawRecord {
    #[serde(rename = "Nr_Boczny")]
    side_number: i64,
    #[serde(rename = "Nr_Rej")]
    plate: Option<String>,
    #[serde(rename = "Brygada")]
    brigade: Option<String>,
    #[serde(rename = "Nazwa_Linii")]
    line: Option<String>,
    #[serde(rename = "Ostatnia_Pozycja_Szerokosc")]
    latitude: MicroDegrees,
    #[serde(rename = "Ostatnia_Pozycja_Dlugosc")]
    longitude: MicroDegrees,
    #[serde(rename = "Data_Aktualizacji")]
    updated: civil::DateTime,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Entry {
    Record(RawRecord),
    Unrecognized(serde_json::Value),
}

#[derive(Deserialize)]
struct Dataset {
    #[serde(rename = "dane")]
    entries: Vec<Entry>,
}

pub struct Client {
    http: reqwest::Client,
    time_zone: TimeZone,
}

impl Client {
    pub fn new(http: reqwest::Client) -> Result<Self> {
        let time_zone = TimeZone::get(TIME_ZONE)
            .with_context(|| format!("time zone {TIME_ZONE} is not available"))?;
        Ok(Self { http, time_zone })
    }

    pub async fn fetch_vehicles(&self) -> Result<Vec<VehicleRecord>> {
        let dataset: Dataset = self
            .http
            .get(VEHICLES_URL)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("invalid open data vehicles response")?;
        Ok(self.on_route(dataset.entries))
    }

    fn on_route(&self, entries: Vec<Entry>) -> Vec<VehicleRecord> {
        let mut records = Vec::with_capacity(entries.len());
        let mut unrecognized = Vec::new();
        for entry in entries {
            let raw = match entry {
                Entry::Record(raw) => raw,
                Entry::Unrecognized(value) => {
                    unrecognized.push(value);
                    continue;
                }
            };
            let Some(record) = self.normalize(raw) else {
                continue;
            };
            records.push(record);
        }
        if let Some(example) = unrecognized.first() {
            warn!(count = unrecognized.len(), %example, "skipped unrecognized open data records");
        }
        records
    }

    fn normalize(&self, raw: RawRecord) -> Option<VehicleRecord> {
        let line = non_empty(raw.line)?.to_lowercase();
        if raw.side_number == MISSING_SIDE_NUMBER
            || !geo::is_plausible_fix(raw.latitude, raw.longitude)
        {
            return None;
        }
        let updated_unix_candidates = self.unix_candidates(raw.updated);
        if updated_unix_candidates.is_empty() {
            return None;
        }
        Some(VehicleRecord {
            side_number: raw.side_number,
            plate: non_empty(raw.plate)
                .filter(|plate| plate != MISSING_PLATE)
                .map(|plate| normalize_plate(&plate)),
            brigade: non_empty(raw.brigade),
            line,
            latitude: raw.latitude,
            longitude: raw.longitude,
            updated_local: raw.updated,
            updated_unix_candidates,
        })
    }

    fn unix_candidates(&self, local: civil::DateTime) -> Vec<i64> {
        let ambiguous = self.time_zone.to_ambiguous_timestamp(local);
        let mut candidates: Vec<i64> = [ambiguous.earlier(), ambiguous.later()]
            .into_iter()
            .filter_map(|timestamp| timestamp.ok())
            .map(|timestamp| timestamp.as_second())
            .collect();
        candidates.sort_unstable();
        candidates.dedup();
        candidates
    }
}

pub fn normalize_plate(plate: &str) -> String {
    plate
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_uppercase()
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Client {
        Client::new(reqwest::Client::new()).unwrap()
    }

    #[test]
    fn keeps_only_usable_records_on_route() {
        let json = r#"{"meta": {}, "dane": [
            {"Nr_Rej": "dw 916kf", "Brygada": "00104", "Nr_Boczny": 8335, "Nazwa_Linii": "A",
             "Data_Aktualizacji": "2026-09-26T14:59:55.370000",
             "Ostatnia_Pozycja_Dlugosc": 17.02387046813965, "Ostatnia_Pozycja_Szerokosc": 51.146968841552734},
            {"Nr_Rej": "-", "Brygada": null, "Nr_Boczny": 3109, "Nazwa_Linii": "18",
             "Data_Aktualizacji": "2026-09-26T15:00:02.640000",
             "Ostatnia_Pozycja_Dlugosc": 17.02, "Ostatnia_Pozycja_Szerokosc": 51.14},
            {"Nr_Rej": null, "Brygada": null, "Nr_Boczny": 3401, "Nazwa_Linii": "",
             "Data_Aktualizacji": "2026-09-26T15:00:02.640000",
             "Ostatnia_Pozycja_Dlugosc": 17.02, "Ostatnia_Pozycja_Szerokosc": 51.14},
            {"Nr_Rej": null, "Brygada": null, "Nr_Boczny": 8315, "Nazwa_Linii": "15",
             "Data_Aktualizacji": "2026-09-26T15:00:02.640000",
             "Ostatnia_Pozycja_Dlugosc": 0.0, "Ostatnia_Pozycja_Szerokosc": 0.0},
            {"Nr_Rej": null, "Brygada": null, "Nr_Boczny": 0, "Nazwa_Linii": "15",
             "Data_Aktualizacji": "2026-09-26T15:00:02.640000",
             "Ostatnia_Pozycja_Dlugosc": 17.02, "Ostatnia_Pozycja_Szerokosc": 51.14},
            {"Nr_Boczny": "broken"}
        ]}"#;
        let dataset: Dataset = serde_json::from_str(json).unwrap();
        let records = client().on_route(dataset.entries);
        assert_eq!(
            records,
            [
                VehicleRecord {
                    side_number: 8335,
                    plate: Some("DW916KF".to_owned()),
                    brigade: Some("00104".to_owned()),
                    line: "a".to_owned(),
                    latitude: MicroDegrees(51_146_969),
                    longitude: MicroDegrees(17_023_870),
                    updated_local: "2026-09-26T14:59:55.37".parse().unwrap(),
                    updated_unix_candidates: vec![1_790_427_595],
                },
                VehicleRecord {
                    side_number: 3109,
                    plate: None,
                    brigade: None,
                    line: "18".to_owned(),
                    latitude: MicroDegrees(51_140_000),
                    longitude: MicroDegrees(17_020_000),
                    updated_local: "2026-09-26T15:00:02.64".parse().unwrap(),
                    updated_unix_candidates: vec![1_790_427_602],
                },
            ]
        );
    }

    #[test]
    fn gives_both_readings_of_a_repeated_or_skipped_local_time() {
        let client = client();
        let candidates = |local: &str| client.unix_candidates(local.parse().unwrap());
        const FALL_BACK_0030_UTC: i64 = 1_792_888_200;
        const FALL_BACK_0130_UTC: i64 = 1_792_891_800;
        const SPRING_FORWARD_0030_UTC: i64 = 1_774_744_200;
        const SPRING_FORWARD_0130_UTC: i64 = 1_774_747_800;
        assert_eq!(
            candidates("2026-10-25T02:30:00"),
            [FALL_BACK_0030_UTC, FALL_BACK_0130_UTC]
        );
        assert_eq!(
            candidates("2026-03-29T02:30:00"),
            [SPRING_FORWARD_0030_UTC, SPRING_FORWARD_0130_UTC]
        );
        assert_eq!(candidates("2026-09-26T14:59:55"), [1_790_427_595]);
    }
}

use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::tz::TimeZone;
use jiff::{Timestamp, civil};

use crate::db::{BoundingBox, Db, FleetVehicle, MatchSource, PositionFilter, StoredPosition};
use crate::{open_data, speed};

const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";
const KM_PER_DEGREE_LATITUDE: f64 = 111.32;
const UNKNOWN_FLEET_VALUE: &str = "-";
const FLEET_VALUE_SEPARATOR: &str = "/";
const UNKNOWN_SPEED: &str = "-";
const INFERRED_MARKER: &str = "*";

pub fn parse_time(input: &str) -> Result<Timestamp> {
    if let Ok(timestamp) = input.parse::<Timestamp>() {
        return Ok(timestamp);
    }
    let local: civil::DateTime = input.parse().with_context(|| {
        format!("invalid time {input:?}, expected e.g. \"2026-09-25 12:40:00\"")
    })?;
    Ok(local.to_zoned(TimeZone::system())?.timestamp())
}

pub enum Target {
    Line(String),
    Plate(String),
}

pub async fn print_positions(
    db: &Db,
    target: &Target,
    at: Timestamp,
    max_age: Duration,
) -> Result<()> {
    let value = match target {
        Target::Line(line) => line.to_lowercase(),
        Target::Plate(plate) => open_data::normalize_plate(plate),
    };
    let (filter, kind) = match target {
        Target::Line(_) => (PositionFilter::Line(&value), "line"),
        Target::Plate(_) => (PositionFilter::Plate(&value), "plate"),
    };
    let description = format!("{kind} {value}");
    let positions = db.positions_at(filter, at.as_second(), max_age).await?;
    let at_local = format_time(at.as_second())?;
    if positions.is_empty() {
        println!("no vehicles for {description} at {at_local}");
        return Ok(());
    }
    println!(
        "{} vehicle(s) for {description} at {at_local}",
        positions.len()
    );
    let course_ids: Vec<i64> = positions.iter().map(|p| p.course_id).collect();
    let fleet = db.fleet_vehicles(&course_ids).await?;
    let speeds = speed::speeds_kmh(db, &positions, at.as_second()).await?;
    println!(
        "{:<5} {:<5} {:>10} {:>9} {:<9} {:<7} {:>6} {:>10} {:>10}  {:<19}  {:<19}",
        "TYPE",
        "LINE",
        "COURSE",
        "SIDE",
        "PLATE",
        "BRIGADE",
        "KM/H",
        "LATITUDE",
        "LONGITUDE",
        "HERE SINCE",
        "LAST SEEN"
    );
    let mut any_inferred = false;
    for position in positions {
        let speed = speeds.get(&(position.line_id, position.course_id)).copied();
        let vehicles = fleet.get(&position.course_id);
        any_inferred |= vehicles
            .is_some_and(|vehicles| vehicles.iter().any(|v| v.source == MatchSource::Inferred));
        print_position(&position, vehicles, speed)?;
    }
    if any_inferred {
        println!("{INFERRED_MARKER} inferred from the brigade's previous or next course");
    }
    Ok(())
}

fn print_position(
    position: &StoredPosition,
    fleet: Option<&Vec<FleetVehicle>>,
    speed_kmh: Option<f64>,
) -> Result<()> {
    println!(
        "{:<5} {:<5} {:>10} {:>9} {:<9} {:<7} {:>6} {:>10.6} {:>10.6}  {:<19}  {:<19}",
        position.vehicle_type.as_str(),
        position.line,
        position.course_id,
        format_fleet(fleet, |vehicle| Some(format_side_number(vehicle))),
        format_fleet(fleet, |vehicle| vehicle.plate.clone()),
        format_fleet(fleet, |vehicle| vehicle.brigade.clone()),
        speed_kmh.map_or_else(|| UNKNOWN_SPEED.to_owned(), |speed| format!("{speed:.1}")),
        position.latitude.degrees(),
        position.longitude.degrees(),
        format_time(position.first_seen_unix)?,
        format_time(position.last_seen_unix)?,
    );
    Ok(())
}

pub async fn print_bounding_box(
    db: &Db,
    from: Option<Timestamp>,
    to: Option<Timestamp>,
) -> Result<()> {
    if let (Some(from), Some(to)) = (from, to)
        && from > to
    {
        bail!("--from must not be later than --to");
    }
    let range = describe_range(from, to)?;
    let Some(bbox) = db
        .bounding_box(
            from.map(jiff::Timestamp::as_second),
            to.map(jiff::Timestamp::as_second),
        )
        .await?
    else {
        println!("no positions {range}");
        return Ok(());
    };
    println!("bounding box of {} position(s) {range}", bbox.positions);
    println!(
        "latitude:  {:.6} .. {:.6}",
        bbox.south.degrees(),
        bbox.north.degrees()
    );
    println!(
        "longitude: {:.6} .. {:.6}",
        bbox.west.degrees(),
        bbox.east.degrees()
    );
    println!(
        "size:      {:.1} km north-south x {:.1} km east-west",
        height_km(&bbox),
        width_km(&bbox)
    );
    Ok(())
}

fn describe_range(from: Option<Timestamp>, to: Option<Timestamp>) -> Result<String> {
    let format = |t: Timestamp| format_time(t.as_second());
    Ok(match (from, to) {
        (None, None) => "in all data".to_owned(),
        (Some(from), None) => format!("since {}", format(from)?),
        (None, Some(to)) => format!("until {}", format(to)?),
        (Some(from), Some(to)) => format!("between {} and {}", format(from)?, format(to)?),
    })
}

fn height_km(bbox: &BoundingBox) -> f64 {
    (bbox.north.degrees() - bbox.south.degrees()) * KM_PER_DEGREE_LATITUDE
}

fn width_km(bbox: &BoundingBox) -> f64 {
    let middle_latitude = f64::midpoint(bbox.north.degrees(), bbox.south.degrees());
    (bbox.east.degrees() - bbox.west.degrees())
        * KM_PER_DEGREE_LATITUDE
        * middle_latitude.to_radians().cos()
}

fn format_side_number(vehicle: &FleetVehicle) -> String {
    match vehicle.source {
        MatchSource::Matched => vehicle.side_number.to_string(),
        MatchSource::Inferred => format!("{}{INFERRED_MARKER}", vehicle.side_number),
    }
}

fn format_fleet(
    fleet: Option<&Vec<FleetVehicle>>,
    value: impl Fn(&FleetVehicle) -> Option<String>,
) -> String {
    let Some(fleet) = fleet else {
        return UNKNOWN_FLEET_VALUE.to_owned();
    };
    fleet
        .iter()
        .map(|vehicle| value(vehicle).unwrap_or_else(|| UNKNOWN_FLEET_VALUE.to_owned()))
        .collect::<Vec<_>>()
        .join(FLEET_VALUE_SEPARATOR)
}

fn format_time(unix: i64) -> Result<String> {
    let zoned = Timestamp::from_second(unix)?.to_zoned(TimeZone::system());
    Ok(zoned.strftime(TIME_FORMAT).to_string())
}

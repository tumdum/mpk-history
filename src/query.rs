use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::tz::TimeZone;
use jiff::{Timestamp, civil};

use crate::db::{BoundingBox, Db, StoredPosition};

const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";
const KM_PER_DEGREE_LATITUDE: f64 = 111.32;

pub fn parse_time(input: &str) -> Result<Timestamp> {
    if let Ok(timestamp) = input.parse::<Timestamp>() {
        return Ok(timestamp);
    }
    let local: civil::DateTime = input.parse().with_context(|| {
        format!("invalid time {input:?}, expected e.g. \"2026-09-25 12:40:00\"")
    })?;
    Ok(local.to_zoned(TimeZone::system())?.timestamp())
}

pub async fn print_positions(db: &Db, line: &str, at: Timestamp, max_age: Duration) -> Result<()> {
    let line = line.to_lowercase();
    let positions = db.positions_at(&line, at.as_second(), max_age).await?;
    let at_local = format_time(at.as_second())?;
    if positions.is_empty() {
        println!("no vehicles of line {line} at {at_local}");
        return Ok(());
    }
    println!(
        "{} vehicle(s) of line {line} at {at_local}",
        positions.len()
    );
    println!(
        "{:<5} {:<5} {:>10} {:>10} {:>10}  {:<19}  {:<19}",
        "TYPE", "LINE", "COURSE", "LATITUDE", "LONGITUDE", "HERE SINCE", "LAST SEEN"
    );
    for position in positions {
        print_position(&position)?;
    }
    Ok(())
}

fn print_position(position: &StoredPosition) -> Result<()> {
    println!(
        "{:<5} {:<5} {:>10} {:>10.6} {:>10.6}  {:<19}  {:<19}",
        position.vehicle_type.as_str(),
        position.line,
        position.course_id,
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
        .bounding_box(from.map(|t| t.as_second()), to.map(|t| t.as_second()))
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
    let middle_latitude = (bbox.north.degrees() + bbox.south.degrees()) / 2.0;
    (bbox.east.degrees() - bbox.west.degrees())
        * KM_PER_DEGREE_LATITUDE
        * middle_latitude.to_radians().cos()
}

fn format_time(unix: i64) -> Result<String> {
    let zoned = Timestamp::from_second(unix)?.to_zoned(TimeZone::system());
    Ok(zoned.strftime(TIME_FORMAT).to_string())
}

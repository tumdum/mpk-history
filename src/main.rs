mod backup;
mod collector;
mod db;
mod fleet;
mod geo;
mod mpk;
mod open_data;
mod open_meteo;
mod query;
mod speed;
mod weather;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use jiff::Timestamp;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::collector::VehicleCollector;
use crate::db::{Db, OpenMode};
use crate::fleet::FleetCollector;
use crate::open_meteo::Area;
use crate::weather::WeatherCollector;

const DEFAULT_LOG_FILTER: &str = "info";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser)]
#[command(about = "Records realtime positions of Wrocław MPK buses and trams into SQLite")]
struct Args {
    #[arg(
        long,
        global = true,
        env = "MPK_DATABASE",
        default_value = "mpk-history.sqlite"
    )]
    database: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Periodically fetch vehicle positions and weather and store them
    Collect {
        #[command(flatten)]
        vehicles: VehicleArgs,

        #[command(flatten)]
        weather: WeatherArgs,

        #[command(flatten)]
        fleet: FleetArgs,
    },
    /// Show positions of all vehicles of a line, or of one vehicle by plate, at a given time
    Query {
        #[command(flatten)]
        target: QueryTarget,

        /// Local time like "2026-09-25 12:40:00" or RFC 3339 like "2026-09-25T10:40:00Z"; defaults to now
        #[arg(long, value_parser = query::parse_time)]
        at: Option<Timestamp>,

        /// How long after its last sighting a vehicle is still shown; keep it above the collect interval
        #[arg(long, default_value_t = 30)]
        max_age_secs: u64,
    },
    /// Show the bounding box of stored positions, optionally limited to a time range
    Bbox {
        /// Only positions seen at or after this time; same formats as `query --at`
        #[arg(long, value_parser = query::parse_time)]
        from: Option<Timestamp>,

        /// Only positions seen at or before this time; same formats as `query --at`
        #[arg(long, value_parser = query::parse_time)]
        to: Option<Timestamp>,
    },
    /// Write a consistent, compacted copy of the database to a new file; safe while collecting
    Backup {
        /// File to create; it must not exist yet
        target: PathBuf,
    },
}

#[derive(clap::Args)]
#[group(required = true, multiple = false)]
struct QueryTarget {
    /// Line name, e.g. 18 or A
    #[arg(long)]
    line: Option<String>,

    /// Plate number, e.g. DW916KF; only courses already matched to fleet data can be found
    #[arg(long)]
    plate: Option<String>,
}

impl TryFrom<QueryTarget> for query::Target {
    type Error = anyhow::Error;

    fn try_from(target: QueryTarget) -> Result<Self> {
        match (target.line, target.plate) {
            (Some(line), None) => Ok(query::Target::Line(line)),
            (None, Some(plate)) => Ok(query::Target::Plate(plate)),
            _ => bail!("pass exactly one of --line or --plate"),
        }
    }
}

#[derive(clap::Args)]
struct VehicleArgs {
    #[arg(long, env = "MPK_INTERVAL_SECS", default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..))]
    interval_secs: u64,

    #[arg(long, env = "MPK_LINES_REFRESH_SECS", default_value_t = 3600)]
    lines_refresh_secs: u64,

    /// Moves of at most this many meters from where a vehicle's current row started are treated as GPS jitter; 0 stores every change
    #[arg(long, env = "MPK_MIN_MOVE_METERS", default_value_t = 2.0, value_parser = parse_meters)]
    min_move_meters: f64,
}

fn parse_meters(input: &str) -> Result<f64, String> {
    let meters: f64 = input.parse().map_err(|e| format!("{e}"))?;
    if !meters.is_finite() || meters < 0.0 {
        return Err("must be a non-negative number of meters".to_owned());
    }
    Ok(meters)
}

#[derive(clap::Args)]
struct WeatherArgs {
    /// Whether to also collect weather from open-meteo
    #[arg(long, env = "MPK_WEATHER", value_enum, default_value_t = CollectorMode::Enabled)]
    weather: CollectorMode,

    /// open-meteo counts every grid point as an API call; the free tier allows 10000 calls per day
    #[arg(long, env = "MPK_WEATHER_INTERVAL_SECS", default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..))]
    weather_interval_secs: u64,

    /// Weather grid area as "south,west,north,east" in degrees
    #[arg(long, env = "MPK_WEATHER_AREA", default_value = open_meteo::DEFAULT_AREA)]
    weather_area: Area,

    /// open-meteo weather model; it must support bounding boxes
    #[arg(long, env = "MPK_WEATHER_MODEL", default_value = open_meteo::DEFAULT_MODEL)]
    weather_model: String,
}

#[derive(clap::Args)]
struct FleetArgs {
    /// Whether to match courses to side and plate numbers from Wrocław open data
    #[arg(long, env = "MPK_FLEET", value_enum, default_value_t = CollectorMode::Enabled)]
    fleet: CollectorMode,

    /// The open data vehicle list is refreshed about every 15 minutes
    #[arg(long, env = "MPK_FLEET_INTERVAL_SECS", default_value_t = 900, value_parser = clap::value_parser!(u64).range(1..))]
    fleet_interval_secs: u64,
}

#[derive(Clone, Copy, ValueEnum)]
enum CollectorMode {
    Enabled,
    Disabled,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER)),
        )
        .init();

    let args = Args::parse();
    match args.command {
        Command::Collect {
            vehicles,
            weather,
            fleet,
        } => {
            let db = Db::open(&args.database, OpenMode::CreateIfMissing).await?;
            info!(database = %args.database.display(), "database ready");
            let result = collect(&db, vehicles, weather, fleet).await;
            db.close().await;
            result
        }
        Command::Query {
            target,
            at,
            max_age_secs,
        } => {
            let target = query::Target::try_from(target)?;
            let db = Db::open(&args.database, OpenMode::MustExist).await?;
            let at = at.unwrap_or_else(Timestamp::now);
            let result =
                query::print_positions(&db, &target, at, Duration::from_secs(max_age_secs)).await;
            db.close().await;
            result
        }
        Command::Bbox { from, to } => {
            let db = Db::open(&args.database, OpenMode::MustExist).await?;
            let result = query::print_bounding_box(&db, from, to).await;
            db.close().await;
            result
        }
        Command::Backup { target } => {
            let db = Db::open(&args.database, OpenMode::ExistingAsIs).await?;
            let result = backup::backup(&db, &target).await;
            db.close().await;
            result
        }
    }
}

async fn collect(
    db: &Db,
    vehicle_args: VehicleArgs,
    weather_args: WeatherArgs,
    fleet_args: FleetArgs,
) -> Result<()> {
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .build()?;
    let (stop_tx, stop_rx) = watch::channel(());

    let vehicle_interval = Duration::from_secs(vehicle_args.interval_secs);
    let mut vehicles = VehicleCollector::new(
        mpk::Client::new(http.clone()),
        db.clone(),
        vehicle_interval,
        Duration::from_secs(vehicle_args.lines_refresh_secs),
        vehicle_args.min_move_meters,
    );
    let vehicles_loop = run_every("vehicle", vehicle_interval, stop_rx.clone(), async || {
        vehicles.collect().await
    });

    let weather_loop = async {
        if let CollectorMode::Disabled = weather_args.weather {
            return;
        }
        info!(area = %weather_args.weather_area, model = weather_args.weather_model, "collecting weather");
        let mut weather = WeatherCollector::new(
            open_meteo::Client::new(http.clone()),
            db.clone(),
            weather_args.weather_area,
            weather_args.weather_model,
        );
        run_every(
            "weather",
            Duration::from_secs(weather_args.weather_interval_secs),
            stop_rx.clone(),
            async || weather.collect().await,
        )
        .await;
    };

    let fleet_loop = async {
        if let CollectorMode::Disabled = fleet_args.fleet {
            return Ok(());
        }
        info!("matching courses to fleet open data");
        let mut fleet = FleetCollector::new(open_data::Client::new(http.clone())?, db.clone());
        run_every(
            "fleet",
            Duration::from_secs(fleet_args.fleet_interval_secs),
            stop_rx.clone(),
            async || fleet.collect().await,
        )
        .await;
        Ok(())
    };

    let shutdown = async {
        let result = shutdown_signal().await;
        info!("shutting down");
        stop_tx.send_replace(());
        result
    };

    let (result, (), (), fleet_result) =
        tokio::join!(shutdown, vehicles_loop, weather_loop, fleet_loop);
    result.and(fleet_result)
}

async fn run_every(
    name: &str,
    interval: Duration,
    mut stop: watch::Receiver<()>,
    mut job: impl AsyncFnMut() -> Result<()>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = stop.changed() => return,
        }
        if let Err(e) = job().await {
            warn!("{name} collection failed: {e:#}");
        }
    }
}

async fn shutdown_signal() -> Result<()> {
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_is_valid() {
        Args::command().debug_assert();
    }
}

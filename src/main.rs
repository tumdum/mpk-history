mod backup;
mod collector;
mod db;
mod geo;
mod mpk;
mod open_meteo;
mod query;
mod weather;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use jiff::Timestamp;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::collector::VehicleCollector;
use crate::db::{Db, OpenMode};
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
    },
    /// Show positions of all vehicles of a line at a given time
    Query {
        #[arg(long)]
        line: String,

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
struct VehicleArgs {
    #[arg(long, env = "MPK_INTERVAL_SECS", default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..))]
    interval_secs: u64,

    #[arg(long, env = "MPK_LINES_REFRESH_SECS", default_value_t = 3600)]
    lines_refresh_secs: u64,
}

#[derive(clap::Args)]
struct WeatherArgs {
    /// Whether to also collect weather from open-meteo
    #[arg(long, env = "MPK_WEATHER", value_enum, default_value_t = WeatherMode::Enabled)]
    weather: WeatherMode,

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

#[derive(Clone, Copy, ValueEnum)]
enum WeatherMode {
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
        Command::Collect { vehicles, weather } => {
            let db = Db::open(&args.database, OpenMode::CreateIfMissing).await?;
            info!(database = %args.database.display(), "database ready");
            let result = collect(&db, vehicles, weather).await;
            db.close().await;
            result
        }
        Command::Query {
            line,
            at,
            max_age_secs,
        } => {
            let db = Db::open(&args.database, OpenMode::MustExist).await?;
            let at = at.unwrap_or_else(Timestamp::now);
            let result =
                query::print_positions(&db, &line, at, Duration::from_secs(max_age_secs)).await;
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

async fn collect(db: &Db, vehicle_args: VehicleArgs, weather_args: WeatherArgs) -> Result<()> {
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
    );
    let vehicles_loop = run_every("vehicle", vehicle_interval, stop_rx.clone(), async || {
        vehicles.collect().await
    });

    let weather_loop = async {
        if let WeatherMode::Disabled = weather_args.weather {
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

    let shutdown = async {
        let result = shutdown_signal().await;
        info!("shutting down");
        stop_tx.send_replace(());
        result
    };

    let (result, (), ()) = tokio::join!(shutdown, vehicles_loop, weather_loop);
    result
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

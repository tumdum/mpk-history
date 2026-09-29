#![deny(
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used
)]
#![allow(clippy::missing_errors_doc)]

pub mod backup;
pub mod build_info;
pub mod charts;
pub mod collector;
pub mod db;
pub mod fetch;
mod fetch_errors;
pub mod fleet;
pub mod geo;
pub mod inference;
pub mod map;
pub mod mpk;
pub mod open_data;
pub mod open_meteo;
pub mod query;
pub mod speed;
pub mod stats;
pub mod weather;
pub mod web;

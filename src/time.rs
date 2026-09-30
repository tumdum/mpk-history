use std::sync::LazyLock;

use jiff::tz::TimeZone;

pub const WARSAW_TIME_ZONE_NAME: &str = "Europe/Warsaw";

static WARSAW_TIME_ZONE: LazyLock<TimeZone> = LazyLock::new(|| {
    TimeZone::get(WARSAW_TIME_ZONE_NAME).unwrap_or_else(|error| {
        panic!("time zone {WARSAW_TIME_ZONE_NAME} is not available: {error}")
    })
});

pub fn warsaw() -> TimeZone {
    WARSAW_TIME_ZONE.clone()
}

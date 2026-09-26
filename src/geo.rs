use serde::Deserialize;

const MICRO_DEGREES_PER_DEGREE: f64 = 1_000_000.0;
const MAX_LATITUDE: MicroDegrees = MicroDegrees(90_000_000);
const MAX_LONGITUDE: MicroDegrees = MicroDegrees(180_000_000);
const NULL_ISLAND: (MicroDegrees, MicroDegrees) = (MicroDegrees(0), MicroDegrees(0));
const METERS_PER_DEGREE_LATITUDE: f64 = 111_320.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(from = "f64")]
pub struct MicroDegrees(pub i64);

impl MicroDegrees {
    pub fn degrees(self) -> f64 {
        self.0 as f64 / MICRO_DEGREES_PER_DEGREE
    }
}

impl From<f64> for MicroDegrees {
    fn from(degrees: f64) -> Self {
        Self((degrees * MICRO_DEGREES_PER_DEGREE).round() as i64)
    }
}

pub fn is_in_range(latitude: MicroDegrees, longitude: MicroDegrees) -> bool {
    latitude.0.abs() <= MAX_LATITUDE.0 && longitude.0.abs() <= MAX_LONGITUDE.0
}

pub fn is_plausible_fix(latitude: MicroDegrees, longitude: MicroDegrees) -> bool {
    is_in_range(latitude, longitude) && (latitude, longitude) != NULL_ISLAND
}

pub fn distance_m(a: (MicroDegrees, MicroDegrees), b: (MicroDegrees, MicroDegrees)) -> f64 {
    let middle_latitude = (a.0.degrees() + b.0.degrees()) / 2.0;
    let north = (a.0.degrees() - b.0.degrees()) * METERS_PER_DEGREE_LATITUDE;
    let east = (a.1.degrees() - b.1.degrees())
        * METERS_PER_DEGREE_LATITUDE
        * middle_latitude.to_radians().cos();
    north.hypot(east)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_short_distances() {
        let a = (MicroDegrees(51_100_000), MicroDegrees(17_000_000));
        let north = (MicroDegrees(51_101_000), MicroDegrees(17_000_000));
        let east = (MicroDegrees(51_100_000), MicroDegrees(17_001_000));
        assert!((distance_m(a, north) - 111.32).abs() < 0.01);
        assert!((distance_m(a, east) - 69.9).abs() < 0.1);
        assert_eq!(distance_m(a, a), 0.0);
    }
}

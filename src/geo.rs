use serde::Deserialize;

const MICRO_DEGREES_PER_DEGREE: f64 = 1_000_000.0;
const MAX_LATITUDE: MicroDegrees = MicroDegrees(90_000_000);
const MAX_LONGITUDE: MicroDegrees = MicroDegrees(180_000_000);
const NULL_ISLAND: (MicroDegrees, MicroDegrees) = (MicroDegrees(0), MicroDegrees(0));

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

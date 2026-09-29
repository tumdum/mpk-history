use std::convert::Infallible;
use std::future::{self, Future};

use axum::extract::FromRequestParts;
use axum::http::header::ACCEPT_LANGUAGE;
use axum::http::request::Parts;
use jiff::Zoned;
use jiff::civil::{Date, ISOWeekDate};

use super::period::PeriodKind;
use super::quality::QualityChart;
use super::{Metric, Range};
use crate::fetch::Source;
use crate::mpk::VehicleType;
use crate::stats::Kind;

mod en;
mod pl;

const RANGE_SEPARATOR: char = ',';
const PARAMETER_SEPARATOR: char = ';';
const VALUE_SEPARATOR: char = '=';
const SUBTAG_SEPARATOR: char = '-';
const QUALITY_PARAMETER: &str = "q";
const WILDCARD: &str = "*";
const NOT_ACCEPTABLE: f32 = 0.0;
const FULL_QUALITY: f32 = 1.0;
const DECIMAL_POINT: char = '.';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Language {
    English,
    Polish,
}

impl Language {
    const ALL: [Language; 2] = [Language::English, Language::Polish];
    const FALLBACK: Language = Language::English;

    pub(super) fn tag(self) -> &'static str {
        match self {
            Language::English => "en",
            Language::Polish => "pl",
        }
    }

    pub(super) fn translation(self) -> &'static dyn Translation {
        match self {
            Language::English => &en::English,
            Language::Polish => &pl::Polish,
        }
    }

    /// Picks the language of `Accept-Language` with the highest weight, the first one on
    /// a tie.
    pub(super) fn preferred(accept_language: &str) -> Language {
        let mut best: Option<(f32, Language)> = None;
        for range in accept_language.split(RANGE_SEPARATOR) {
            let Some((tag, quality)) = language_range(range) else {
                continue;
            };
            if !(quality > NOT_ACCEPTABLE && quality <= FULL_QUALITY) {
                continue;
            }
            let Some(language) = Language::matching(tag) else {
                continue;
            };
            if best.is_none_or(|(best_quality, _)| quality > best_quality) {
                best = Some((quality, language));
            }
        }
        best.map_or(Language::FALLBACK, |(_, language)| language)
    }

    fn matching(tag: &str) -> Option<Language> {
        if tag == WILDCARD {
            return Some(Language::FALLBACK);
        }
        let primary = tag.split(SUBTAG_SEPARATOR).next()?;
        Language::ALL
            .into_iter()
            .find(|language| language.tag().eq_ignore_ascii_case(primary))
    }
}

fn language_range(range: &str) -> Option<(&str, f32)> {
    let mut parts = range.split(PARAMETER_SEPARATOR);
    let tag = parts.next()?.trim();
    if tag.is_empty() {
        return None;
    }
    let mut quality = FULL_QUALITY;
    for parameter in parts {
        let Some((name, value)) = parameter.split_once(VALUE_SEPARATOR) else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case(QUALITY_PARAMETER) {
            quality = value.trim().parse().ok()?;
        }
    }
    Some((tag, quality))
}

impl<S: Send + Sync> FromRequestParts<S> for Language {
    type Rejection = Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let ranges: Vec<&str> = parts
            .headers
            .get_all(ACCEPT_LANGUAGE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        future::ready(Ok(Language::preferred(
            &ranges.join(&RANGE_SEPARATOR.to_string()),
        )))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Text {
    Title,
    StatisticsDescription,
    QualityDescription,
    Overview,
    AsOf,
    SoFarAsOf,
    Charts,
    ChartRange,
    History,
    Pages,
    DataQuality,
    Summary,
    NeighbouringPeriods,
    Metric,
    Vehicles,
    Lines,
    Source,
    FailedDownloads,
    InvalidPositions,
    NoData,
    VehiclesNow,
    MapDescription,
    NoVehiclesNow,
    WhileMoving,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Relative {
    Current,
    Previous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Neighbour {
    Previous,
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChartTime {
    Time,
    WeekdayTime,
    DayMonth,
    WeekdayDayMonth,
    WeekdayDayMonthTime,
    WeekdayDate,
    Month,
}

/// Everything the web page shows in a language. Titles and labels of periods are written
/// as they read inside a sentence; see [`capitalized`].
pub(super) trait Translation: Sync {
    fn text(&self, text: Text) -> &'static str;
    fn decimal_separator(&self) -> char;
    fn range(&self, range: Range) -> &'static str;
    /// The range after a comma, as in "Average speed (km/h), last day".
    fn range_caption(&self, range: Range) -> &'static str;
    fn metric(&self, metric: Metric) -> &'static str;
    fn kind(&self, kind: Kind) -> &'static str;
    fn line(&self, vehicle_type: VehicleType, name: &str) -> String;
    /// The name of a moving speed line in a chart, as in "Buses, while moving".
    fn while_moving(&self, name: &str) -> String;
    fn source(&self, source: Source) -> &'static str;
    fn quality_chart(&self, chart: QualityChart) -> &'static str;
    fn relative_period(&self, kind: PeriodKind, relative: Relative) -> &'static str;
    fn neighbour(&self, kind: PeriodKind, neighbour: Neighbour) -> &'static str;
    fn statistics_of(&self, kind: PeriodKind) -> &'static str;
    fn parts(&self, kind: PeriodKind) -> &'static str;
    fn day(&self, date: Date) -> String;
    fn week(&self, week: ISOWeekDate) -> String;
    fn month(&self, first_day: Date) -> String;
    fn week_days(&self, first: Date, last: Date) -> String;
    fn day_short(&self, date: Date) -> String;
    fn week_short(&self, week: ISOWeekDate) -> String;
    fn month_short(&self, first_day: Date) -> String;
    fn chart_time(&self, time: &Zoned, format: ChartTime) -> String;

    fn number(&self, value: f64, decimals: usize) -> String {
        let text = format!("{value:.decimals$}");
        let separator = self.decimal_separator();
        if separator == DECIMAL_POINT {
            return text;
        }
        text.replace(DECIMAL_POINT, &separator.to_string())
    }
}

/// For text that starts a heading, a link or a title.
pub(super) fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_uppercase().chain(chars).collect()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("", Language::English)]
    #[case("pl", Language::Polish)]
    #[case("pl-PL,pl;q=0.9,en-US;q=0.8,en;q=0.7", Language::Polish)]
    #[case("PL-pl", Language::Polish)]
    #[case("en-US,en;q=0.9,pl;q=0.8", Language::English)]
    #[case("de-DE,de;q=0.9,pl;q=0.5", Language::Polish)]
    #[case("de-DE,de;q=0.9,pl;q=0.5,en;q=0.6", Language::English)]
    #[case("pl;q=0.5, en;q=0.4", Language::Polish)]
    #[case("en;q=0.4 , pl ; q=0.5", Language::Polish)]
    #[case("pl, en", Language::Polish)]
    #[case("de, fr", Language::English)]
    #[case("pl;q=0", Language::English)]
    #[case("pl;q=abc", Language::English)]
    #[case("pl;q=2", Language::English)]
    #[case("pl;q=NaN", Language::English)]
    #[case("de, *;q=0.5, pl;q=0.4", Language::English)]
    #[case("de, *;q=0.1, pl;q=0.4", Language::Polish)]
    #[case("plx, polish", Language::English)]
    #[case(",;,pl", Language::Polish)]
    fn picks_the_most_preferred_supported_language(
        #[case] accept_language: &str,
        #[case] expected: Language,
    ) {
        assert_eq!(Language::preferred(accept_language), expected);
    }

    #[test]
    fn formats_numbers_with_the_decimal_separator_of_the_language() {
        let english = Language::English.translation();
        let polish = Language::Polish.translation();
        assert_eq!(english.number(36.04, 1), "36.0");
        assert_eq!(polish.number(36.04, 1), "36,0");
        assert_eq!(polish.number(1234.4, 0), "1234");
    }

    #[test]
    fn capitalizes_only_the_first_letter() {
        assert_eq!(
            capitalized("środa, 30 września 2026"),
            "Środa, 30 września 2026"
        );
        assert_eq!(capitalized("Week 39"), "Week 39");
        assert_eq!(capitalized(""), "");
    }
}

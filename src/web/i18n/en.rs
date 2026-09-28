use jiff::Zoned;
use jiff::civil::{Date, ISOWeekDate};

use super::{ChartTime, Neighbour, Relative, Text, Translation};
use crate::fetch::Source;
use crate::mpk::VehicleType;
use crate::stats::Kind;
use crate::web::period::PeriodKind;
use crate::web::quality::QualityChart;
use crate::web::{Metric, Range};

pub(in crate::web) struct English;

impl Translation for English {
    fn text(&self, text: Text) -> &'static str {
        match text {
            Text::Title => "MPK Wrocław statistics",
            Text::Overview => "Overview",
            Text::AsOf => "As of ",
            Text::SoFarAsOf => "So far, as of ",
            Text::Charts => "Charts",
            Text::ChartRange => "Chart range",
            Text::History => "History",
            Text::Pages => "Pages",
            Text::DataQuality => "Data quality",
            Text::Summary => "Summary",
            Text::NeighbouringPeriods => "Neighbouring periods",
            Text::Metric => "Metric",
            Text::Vehicles => "Vehicles",
            Text::Lines => "Lines",
            Text::Source => "Source",
            Text::FailedDownloads => "Failed downloads",
            Text::InvalidPositions => "Invalid GPS positions",
            Text::NoData => "No data",
        }
    }

    fn decimal_separator(&self) -> char {
        '.'
    }

    fn range(&self, range: Range) -> &'static str {
        match range {
            Range::Hour => "Last hour",
            Range::Day => "Last day",
            Range::Week => "Last week",
            Range::Month => "Last month",
        }
    }

    fn range_caption(&self, range: Range) -> &'static str {
        match range {
            Range::Hour => "last hour",
            Range::Day => "last day",
            Range::Week => "last week",
            Range::Month => "last month",
        }
    }

    fn metric(&self, metric: Metric) -> &'static str {
        match metric {
            Metric::AverageSpeed => "Average speed (km/h)",
            Metric::AverageVehicles => "Average active vehicles",
            Metric::PeakVehicles => "Peak active vehicles",
            Metric::Distance => "Distance driven (km)",
        }
    }

    fn kind(&self, kind: Kind) -> &'static str {
        match kind {
            Kind::All => "All vehicles",
            Kind::Vehicle(VehicleType::Bus) => "Buses",
            Kind::Vehicle(VehicleType::Tram) => "Trams",
            Kind::Vehicle(VehicleType::Train) => "Trains",
        }
    }

    fn line(&self, vehicle_type: VehicleType, name: &str) -> String {
        let noun = match vehicle_type {
            VehicleType::Bus => "Bus",
            VehicleType::Tram => "Tram",
            VehicleType::Train => "Train",
        };
        format!("{noun} {name}")
    }

    fn source(&self, source: Source) -> &'static str {
        match source {
            Source::MpkPositions => "MPK positions",
            Source::MpkLines => "MPK line list",
            Source::OpenMeteo => "Open-Meteo weather",
            Source::OpenData => "Open data fleet",
        }
    }

    fn quality_chart(&self, chart: QualityChart) -> &'static str {
        match chart {
            QualityChart::FailedDownloads => "Failed downloads (%)",
            QualityChart::InvalidPositions => "Invalid GPS positions (%)",
        }
    }

    fn relative_period(&self, kind: PeriodKind, relative: Relative) -> &'static str {
        match (kind, relative) {
            (PeriodKind::Day, Relative::Current) => "Today",
            (PeriodKind::Day, Relative::Previous) => "Yesterday",
            (PeriodKind::Week, Relative::Current) => "This week",
            (PeriodKind::Week, Relative::Previous) => "Previous week",
            (PeriodKind::Month, Relative::Current) => "This month",
            (PeriodKind::Month, Relative::Previous) => "Previous month",
            (PeriodKind::Year, Relative::Current) => "This year",
            (PeriodKind::Year, Relative::Previous) => "Previous year",
        }
    }

    fn neighbour(&self, kind: PeriodKind, neighbour: Neighbour) -> &'static str {
        match (kind, neighbour) {
            (PeriodKind::Day, Neighbour::Previous) => "← Previous day",
            (PeriodKind::Day, Neighbour::Next) => "Next day →",
            (PeriodKind::Week, Neighbour::Previous) => "← Previous week",
            (PeriodKind::Week, Neighbour::Next) => "Next week →",
            (PeriodKind::Month, Neighbour::Previous) => "← Previous month",
            (PeriodKind::Month, Neighbour::Next) => "Next month →",
            (PeriodKind::Year, Neighbour::Previous) => "← Previous year",
            (PeriodKind::Year, Neighbour::Next) => "Next year →",
        }
    }

    fn statistics_of(&self, kind: PeriodKind) -> &'static str {
        match kind {
            PeriodKind::Day => "Statistics of this day",
            PeriodKind::Week => "Statistics of this week",
            PeriodKind::Month => "Statistics of this month",
            PeriodKind::Year => "Statistics of this year",
        }
    }

    fn parts(&self, kind: PeriodKind) -> &'static str {
        match kind {
            PeriodKind::Day => "Days",
            PeriodKind::Week => "Weeks",
            PeriodKind::Month => "Months",
            PeriodKind::Year => "Years",
        }
    }

    fn day(&self, date: Date) -> String {
        date.strftime("%A, %-d %B %Y").to_string()
    }

    fn week(&self, week: ISOWeekDate) -> String {
        format!("Week {}, {}", week.week(), week.year())
    }

    fn month(&self, first_day: Date) -> String {
        first_day.strftime("%B %Y").to_string()
    }

    fn week_days(&self, first: Date, last: Date) -> String {
        format!(
            "{} – {}",
            first.strftime("%-d %B"),
            last.strftime("%-d %B %Y")
        )
    }

    fn day_short(&self, date: Date) -> String {
        date.strftime("%a %-d").to_string()
    }

    fn week_short(&self, week: ISOWeekDate) -> String {
        format!("Week {}", week.week())
    }

    fn month_short(&self, first_day: Date) -> String {
        first_day.strftime("%B").to_string()
    }

    fn chart_time(&self, time: &Zoned, format: ChartTime) -> String {
        let pattern = match format {
            ChartTime::Time => "%H:%M",
            ChartTime::WeekdayTime => "%a %H:%M",
            ChartTime::DayMonth => "%d.%m",
            ChartTime::WeekdayDayMonth => "%a %d.%m",
            ChartTime::WeekdayDayMonthTime => "%a %d.%m %H:%M",
            ChartTime::WeekdayDate => "%a %d.%m.%Y",
            ChartTime::Month => "%b",
        };
        time.strftime(pattern).to_string()
    }
}

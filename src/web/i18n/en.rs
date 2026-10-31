use jiff::Zoned;
use jiff::civil::{Date, ISOWeekDate};

use super::{AboutPart, AboutText, ChartTime, Neighbour, Relative, Text, Translation};
use crate::fetch::Source;
use crate::mpk::VehicleType;
use crate::stats::Kind;
use crate::web::about::AboutTopic;
use crate::web::period::{PeriodKind, RecentPeriod};
use crate::web::quality::QualityChart;
use crate::web::{Metric, Range};

pub(in crate::web) struct English;

impl Translation for English {
    fn text(&self, text: Text) -> &'static str {
        match text {
            Text::Title => "MPK Wrocław statistics",
            Text::StatisticsDescription => {
                "Average speed, active vehicles and distance driven by MPK Wrocław buses and trams, with charts."
            }
            Text::QualityDescription => {
                "Failed downloads and invalid GPS positions in the MPK Wrocław vehicle data."
            }
            Text::Overview => "Overview",
            Text::AsOf => "As of ",
            Text::SoFarAsOf => "So far, as of ",
            Text::Charts => "Charts",
            Text::ChartRange => "Chart range",
            Text::History => "History",
            Text::Pages => "Pages",
            Text::DataQuality => "Data quality",
            Text::Summary => "Summary",
            Text::PreparingStatistics => "Summary statistics are being prepared.",
            Text::NeighbouringPeriods => "Neighbouring periods",
            Text::Metric => "Metric",
            Text::Vehicles => "Vehicles",
            Text::Lines => "Lines",
            Text::Source => "Source",
            Text::FailedDownloads => "Failed downloads",
            Text::InvalidPositions => "Invalid GPS positions",
            Text::NoData => "No data",
            Text::VehiclesNow => "Vehicles now",
            Text::MapDescription => "Map of Wrocław with the vehicles seen in the last 30 seconds",
            Text::NoVehiclesNow => "No vehicles right now",
            Text::WhileMoving => "While moving",
            Text::SpeedMap => "Average speed by area",
            Text::SpeedMapDescription => {
                "Map of Wrocław divided into hexagons, coloured by the average speed of the vehicles in each"
            }
            Text::Average => "Average",
            Text::SpeedKmh => "Speed (km/h)",
            Text::About => "About",
            Text::AboutDescription => {
                "What you can find on the MPK Wrocław statistics pages: a live map of vehicles, statistics, charts, calendar pages and data quality."
            }
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
            Metric::MovingSpeed => "Average speed while moving (km/h)",
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

    fn while_moving(&self, name: &str) -> String {
        format!("{name}, while moving")
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

    fn about(&self, topic: AboutTopic) -> AboutText {
        match topic {
            AboutTopic::Service => AboutText {
                heading: "What this is",
                paragraphs: &[
                    &[AboutPart::Words(
                        "This site shows statistics of MPK Wrocław buses and trams: how fast they go, how many of them are on the road and how far they drive. It is built from the public vehicle positions published by MPK Wrocław, kept since the collecting started.",
                    )],
                    &[AboutPart::Words(
                        "It is not an official MPK Wrocław service.",
                    )],
                ],
            },
            AboutTopic::CurrentVehicles => AboutText {
                heading: "Vehicles now",
                paragraphs: &[&[AboutPart::Words(
                    "The overview starts with a map of Wrocław with every vehicle seen in the last 30 seconds. Buses, trams and trains each have their own shape and colour. Point at a vehicle, or tap it, to see its line and current speed.",
                )]],
            },
            AboutTopic::Statistics => AboutText {
                heading: "Statistics and charts",
                paragraphs: &[
                    &[AboutPart::Words(
                        "The overview has tables for the last hour, day, week and month with the average speed, the average speed while moving, the average and peak number of active vehicles, and the distance driven. There is one table for all vehicles and one for each vehicle type.",
                    )],
                    &[AboutPart::Words(
                        "The charts show how the average speed and the number of active vehicles changed over the chosen range. Point at a chart to see the values at that time. The charts also work with the keyboard.",
                    )],
                ],
            },
            AboutTopic::VehiclesAndLines => AboutText {
                heading: "Vehicle types and lines",
                paragraphs: &[&[AboutPart::Words(
                    "Every page can be limited to buses, trams or a single line, with the links under the charts and the list of lines at the bottom of the page. A line's pages show the same statistics, charts and maps, for that line only.",
                )]],
            },
            AboutTopic::CalendarPages => AboutText {
                heading: "Days, weeks, months and years",
                paragraphs: &[
                    &[AboutPart::Words(
                        "Every day, week, month and year has its own page with its totals and charts, links to the period before and after, and links to the days or months in it. The overview links to today, yesterday, this and the previous week and month, and this year.",
                    )],
                    &[
                        AboutPart::Words("The addresses "),
                        AboutPart::Address(RecentPeriod::Yesterday),
                        AboutPart::Words(", "),
                        AboutPart::Address(RecentPeriod::PreviousWeek),
                        AboutPart::Words(" and "),
                        AboutPart::Address(RecentPeriod::PreviousMonth),
                        AboutPart::Words(
                            " always lead to the day, week or month before the current one, so they work well as bookmarks.",
                        ),
                    ],
                ],
            },
            AboutTopic::SpeedMap => AboutText {
                heading: "Speed by area",
                paragraphs: &[&[AboutPart::Words(
                    "Calendar pages start with a map of the city split into small hexagons, coloured by how fast the vehicles went there in that period. Point at a hexagon to see its average speed, also for each vehicle type. The colours mean the same on every page, so different periods can be compared.",
                )]],
            },
            AboutTopic::DataQuality => AboutText {
                heading: "Data quality",
                paragraphs: &[&[AboutPart::Words(
                    "The data quality pages show how often downloading the data failed and what share of the GPS positions was invalid, for the same ranges and periods as the statistics. They help to tell whether an unusual number comes from the vehicles or from gaps in the data. Where there is no data at all, the pages show “–”.",
                )]],
            },
            AboutTopic::Languages => AboutText {
                heading: "Languages",
                paragraphs: &[&[AboutPart::Words(
                    "The pages are in English and Polish. The language follows the settings of your browser.",
                )]],
            },
        }
    }
}

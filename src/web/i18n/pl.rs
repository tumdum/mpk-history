use jiff::Zoned;
use jiff::civil::{Date, ISOWeekDate, Weekday};

use super::{AboutPart, AboutText, ChartTime, Neighbour, Relative, Text, Translation};
use crate::fetch::Source;
use crate::mpk::VehicleType;
use crate::stats::Kind;
use crate::web::about::AboutTopic;
use crate::web::period::{PeriodKind, RecentPeriod};
use crate::web::quality::QualityChart;
use crate::web::{Metric, Range};

const MONTHS: [&str; 12] = [
    "styczeń",
    "luty",
    "marzec",
    "kwiecień",
    "maj",
    "czerwiec",
    "lipiec",
    "sierpień",
    "wrzesień",
    "październik",
    "listopad",
    "grudzień",
];
const MONTHS_OF_DATE: [&str; 12] = [
    "stycznia",
    "lutego",
    "marca",
    "kwietnia",
    "maja",
    "czerwca",
    "lipca",
    "sierpnia",
    "września",
    "października",
    "listopada",
    "grudnia",
];
const MONTHS_SHORT: [&str; 12] = [
    "sty", "lut", "mar", "kwi", "maj", "cze", "lip", "sie", "wrz", "paź", "lis", "gru",
];
const DAY_MONTH: &str = "%d.%m";
const TIME: &str = "%H:%M";
const DATE: &str = "%d.%m.%Y";

fn month_name(names: &[&'static str; 12], date: Date) -> &'static str {
    usize::try_from(date.month() - 1)
        .ok()
        .and_then(|index| names.get(index))
        .copied()
        .unwrap_or_default()
}

fn weekday(weekday: Weekday) -> &'static str {
    match weekday {
        Weekday::Monday => "poniedziałek",
        Weekday::Tuesday => "wtorek",
        Weekday::Wednesday => "środa",
        Weekday::Thursday => "czwartek",
        Weekday::Friday => "piątek",
        Weekday::Saturday => "sobota",
        Weekday::Sunday => "niedziela",
    }
}

fn weekday_short(weekday: Weekday) -> &'static str {
    match weekday {
        Weekday::Monday => "pon.",
        Weekday::Tuesday => "wt.",
        Weekday::Wednesday => "śr.",
        Weekday::Thursday => "czw.",
        Weekday::Friday => "pt.",
        Weekday::Saturday => "sob.",
        Weekday::Sunday => "niedz.",
    }
}

fn day_of_month(date: Date) -> String {
    format!("{} {}", date.day(), month_name(&MONTHS_OF_DATE, date))
}

pub(in crate::web) struct Polish;

impl Translation for Polish {
    fn text(&self, text: Text) -> &'static str {
        match text {
            Text::Title => "Statystyki MPK Wrocław",
            Text::StatisticsDescription => {
                "Średnia prędkość, liczba aktywnych pojazdów i przejechany dystans autobusów i tramwajów MPK Wrocław, z wykresami."
            }
            Text::QualityDescription => {
                "Nieudane pobrania i błędne pozycje GPS w danych o pojazdach MPK Wrocław."
            }
            Text::Overview => "Przegląd",
            Text::AsOf => "Stan na ",
            Text::SoFarAsOf => "Do tej pory, stan na ",
            Text::Charts => "Wykresy",
            Text::ChartRange => "Zakres wykresów",
            Text::History => "Historia",
            Text::Pages => "Strony",
            Text::DataQuality => "Jakość danych",
            Text::Summary => "Podsumowanie",
            Text::NeighbouringPeriods => "Sąsiednie okresy",
            Text::Metric => "Wskaźnik",
            Text::Vehicles => "Pojazdy",
            Text::Lines => "Linie",
            Text::Source => "Źródło",
            Text::FailedDownloads => "Nieudane pobrania",
            Text::InvalidPositions => "Błędne pozycje GPS",
            Text::NoData => "Brak danych",
            Text::VehiclesNow => "Pojazdy teraz",
            Text::MapDescription => {
                "Mapa Wrocławia z pojazdami widzianymi w ostatnich 30 sekundach"
            }
            Text::NoVehiclesNow => "W tej chwili brak pojazdów",
            Text::WhileMoving => "W ruchu",
            Text::SpeedMap => "Średnia prędkość według obszarów",
            Text::SpeedMapDescription => {
                "Mapa Wrocławia podzielona na sześciokąty, pokolorowane według średniej prędkości pojazdów w każdym z nich"
            }
            Text::Average => "Średnia",
            Text::SpeedKmh => "Prędkość (km/h)",
            Text::About => "O stronie",
            Text::AboutDescription => {
                "Co można znaleźć na stronach statystyk MPK Wrocław: mapę pojazdów na żywo, statystyki, wykresy, strony kalendarzowe i jakość danych."
            }
        }
    }

    fn decimal_separator(&self) -> char {
        ','
    }

    fn range(&self, range: Range) -> &'static str {
        match range {
            Range::Hour => "Ostatnia godzina",
            Range::Day => "Ostatnia doba",
            Range::Week => "Ostatni tydzień",
            Range::Month => "Ostatni miesiąc",
        }
    }

    fn range_caption(&self, range: Range) -> &'static str {
        match range {
            Range::Hour => "ostatnia godzina",
            Range::Day => "ostatnia doba",
            Range::Week => "ostatni tydzień",
            Range::Month => "ostatni miesiąc",
        }
    }

    fn metric(&self, metric: Metric) -> &'static str {
        match metric {
            Metric::AverageSpeed => "Średnia prędkość (km/h)",
            Metric::MovingSpeed => "Średnia prędkość w ruchu (km/h)",
            Metric::AverageVehicles => "Średnia liczba aktywnych pojazdów",
            Metric::PeakVehicles => "Największa liczba aktywnych pojazdów",
            Metric::Distance => "Przejechany dystans (km)",
        }
    }

    fn kind(&self, kind: Kind) -> &'static str {
        match kind {
            Kind::All => "Wszystkie pojazdy",
            Kind::Vehicle(VehicleType::Bus) => "Autobusy",
            Kind::Vehicle(VehicleType::Tram) => "Tramwaje",
            Kind::Vehicle(VehicleType::Train) => "Pociągi",
        }
    }

    fn line(&self, vehicle_type: VehicleType, name: &str) -> String {
        let noun = match vehicle_type {
            VehicleType::Bus => "Autobus",
            VehicleType::Tram => "Tramwaj",
            VehicleType::Train => "Pociąg",
        };
        format!("{noun} {name}")
    }

    fn while_moving(&self, name: &str) -> String {
        format!("{name}, w ruchu")
    }

    fn source(&self, source: Source) -> &'static str {
        match source {
            Source::MpkPositions => "Pozycje MPK",
            Source::MpkLines => "Lista linii MPK",
            Source::OpenMeteo => "Pogoda Open-Meteo",
            Source::OpenData => "Flota z otwartych danych",
        }
    }

    fn quality_chart(&self, chart: QualityChart) -> &'static str {
        match chart {
            QualityChart::FailedDownloads => "Nieudane pobrania (%)",
            QualityChart::InvalidPositions => "Błędne pozycje GPS (%)",
        }
    }

    fn relative_period(&self, kind: PeriodKind, relative: Relative) -> &'static str {
        match (kind, relative) {
            (PeriodKind::Day, Relative::Current) => "Dzisiaj",
            (PeriodKind::Day, Relative::Previous) => "Wczoraj",
            (PeriodKind::Week, Relative::Current) => "Ten tydzień",
            (PeriodKind::Week, Relative::Previous) => "Poprzedni tydzień",
            (PeriodKind::Month, Relative::Current) => "Ten miesiąc",
            (PeriodKind::Month, Relative::Previous) => "Poprzedni miesiąc",
            (PeriodKind::Year, Relative::Current) => "Ten rok",
            (PeriodKind::Year, Relative::Previous) => "Poprzedni rok",
        }
    }

    fn neighbour(&self, kind: PeriodKind, neighbour: Neighbour) -> &'static str {
        match (kind, neighbour) {
            (PeriodKind::Day, Neighbour::Previous) => "← Poprzedni dzień",
            (PeriodKind::Day, Neighbour::Next) => "Następny dzień →",
            (PeriodKind::Week, Neighbour::Previous) => "← Poprzedni tydzień",
            (PeriodKind::Week, Neighbour::Next) => "Następny tydzień →",
            (PeriodKind::Month, Neighbour::Previous) => "← Poprzedni miesiąc",
            (PeriodKind::Month, Neighbour::Next) => "Następny miesiąc →",
            (PeriodKind::Year, Neighbour::Previous) => "← Poprzedni rok",
            (PeriodKind::Year, Neighbour::Next) => "Następny rok →",
        }
    }

    fn statistics_of(&self, kind: PeriodKind) -> &'static str {
        match kind {
            PeriodKind::Day => "Statystyki tego dnia",
            PeriodKind::Week => "Statystyki tego tygodnia",
            PeriodKind::Month => "Statystyki tego miesiąca",
            PeriodKind::Year => "Statystyki tego roku",
        }
    }

    fn parts(&self, kind: PeriodKind) -> &'static str {
        match kind {
            PeriodKind::Day => "Dni",
            PeriodKind::Week => "Tygodnie",
            PeriodKind::Month => "Miesiące",
            PeriodKind::Year => "Lata",
        }
    }

    fn day(&self, date: Date) -> String {
        format!(
            "{}, {} {}",
            weekday(date.weekday()),
            day_of_month(date),
            date.year()
        )
    }

    fn week(&self, week: ISOWeekDate) -> String {
        format!("tydzień {}, {}", week.week(), week.year())
    }

    fn month(&self, first_day: Date) -> String {
        format!("{} {}", month_name(&MONTHS, first_day), first_day.year())
    }

    fn week_days(&self, first: Date, last: Date) -> String {
        format!(
            "{} – {} {}",
            day_of_month(first),
            day_of_month(last),
            last.year()
        )
    }

    fn day_short(&self, date: Date) -> String {
        format!("{} {}", weekday_short(date.weekday()), date.day())
    }

    fn week_short(&self, week: ISOWeekDate) -> String {
        format!("tydzień {}", week.week())
    }

    fn month_short(&self, first_day: Date) -> String {
        month_name(&MONTHS, first_day).to_owned()
    }

    fn chart_time(&self, time: &Zoned, format: ChartTime) -> String {
        let weekday = weekday_short(time.weekday());
        match format {
            ChartTime::Time => time.strftime(TIME).to_string(),
            ChartTime::WeekdayTime => format!("{weekday} {}", time.strftime(TIME)),
            ChartTime::DayMonth => time.strftime(DAY_MONTH).to_string(),
            ChartTime::WeekdayDayMonth => format!("{weekday} {}", time.strftime(DAY_MONTH)),
            ChartTime::WeekdayDayMonthTime => format!(
                "{weekday} {} {}",
                time.strftime(DAY_MONTH),
                time.strftime(TIME)
            ),
            ChartTime::WeekdayDate => format!("{weekday} {}", time.strftime(DATE)),
            ChartTime::Month => month_name(&MONTHS_SHORT, time.date()).to_owned(),
        }
    }

    fn about(&self, topic: AboutTopic) -> AboutText {
        match topic {
            AboutTopic::Service => AboutText {
                heading: "Czym jest ta strona",
                paragraphs: &[
                    &[AboutPart::Words(
                        "Ta strona pokazuje statystyki autobusów i tramwajów MPK Wrocław: jak szybko jeżdżą, ile ich jest na trasach i jaki dystans przejeżdżają. Powstaje z publicznych pozycji pojazdów udostępnianych przez MPK Wrocław, przechowywanych od początku zbierania.",
                    )],
                    &[AboutPart::Words(
                        "To nie jest oficjalny serwis MPK Wrocław.",
                    )],
                ],
            },
            AboutTopic::CurrentVehicles => AboutText {
                heading: "Pojazdy teraz",
                paragraphs: &[&[AboutPart::Words(
                    "Przegląd zaczyna się od mapy Wrocławia ze wszystkimi pojazdami widzianymi w ostatnich 30 sekundach. Autobusy, tramwaje i pociągi mają własny kształt i kolor. Najedź na pojazd albo go dotknij, aby zobaczyć jego linię i aktualną prędkość.",
                )]],
            },
            AboutTopic::Statistics => AboutText {
                heading: "Statystyki i wykresy",
                paragraphs: &[
                    &[AboutPart::Words(
                        "Przegląd zawiera tabele dla ostatniej godziny, dnia, tygodnia i miesiąca ze średnią prędkością, średnią prędkością w ruchu, średnią i największą liczbą aktywnych pojazdów oraz przejechanym dystansem. Jest jedna tabela dla wszystkich pojazdów i osobna dla każdego rodzaju pojazdu.",
                    )],
                    &[AboutPart::Words(
                        "Wykresy pokazują, jak zmieniały się średnia prędkość i liczba aktywnych pojazdów w wybranym zakresie. Najedź na wykres, aby zobaczyć wartości w danej chwili. Wykresy działają też z klawiatury.",
                    )],
                ],
            },
            AboutTopic::VehiclesAndLines => AboutText {
                heading: "Rodzaje pojazdów i linie",
                paragraphs: &[&[AboutPart::Words(
                    "Każdą stronę można ograniczyć do autobusów, tramwajów albo jednej linii za pomocą odnośników pod wykresami i listy linii na dole strony. Strony linii pokazują te same statystyki, wykresy i mapy, ale tylko dla tej linii.",
                )]],
            },
            AboutTopic::CalendarPages => AboutText {
                heading: "Dni, tygodnie, miesiące i lata",
                paragraphs: &[
                    &[AboutPart::Words(
                        "Każdy dzień, tydzień, miesiąc i rok ma własną stronę z podsumowaniem i wykresami, odnośnikami do okresu poprzedniego i następnego oraz do dni lub miesięcy, z których się składa. Przegląd ma odnośniki do dnia dzisiejszego i wczorajszego, bieżącego i poprzedniego tygodnia i miesiąca oraz bieżącego roku.",
                    )],
                    &[
                        AboutPart::Words("Adresy "),
                        AboutPart::Address(RecentPeriod::Yesterday),
                        AboutPart::Words(", "),
                        AboutPart::Address(RecentPeriod::PreviousWeek),
                        AboutPart::Words(" i "),
                        AboutPart::Address(RecentPeriod::PreviousMonth),
                        AboutPart::Words(
                            " zawsze prowadzą do dnia, tygodnia lub miesiąca przed bieżącym, więc dobrze nadają się na zakładki.",
                        ),
                    ],
                ],
            },
            AboutTopic::SpeedMap => AboutText {
                heading: "Prędkość według obszarów",
                paragraphs: &[&[AboutPart::Words(
                    "Strony kalendarzowe zaczynają się od mapy miasta podzielonej na małe sześciokąty, pokolorowane według tego, jak szybko jeździły tam pojazdy w danym okresie. Najedź na sześciokąt, aby zobaczyć jego średnią prędkość, także dla każdego rodzaju pojazdu. Kolory znaczą to samo na każdej stronie, więc można porównywać różne okresy.",
                )]],
            },
            AboutTopic::DataQuality => AboutText {
                heading: "Jakość danych",
                paragraphs: &[&[AboutPart::Words(
                    "Strony jakości danych pokazują, jak często nie udało się pobrać danych i jaka część pozycji GPS była błędna, dla tych samych zakresów i okresów co statystyki. Pomagają ocenić, czy nietypowa liczba wynika z ruchu pojazdów, czy z braków w danych. Tam, gdzie danych nie ma wcale, strony pokazują „–”.",
                )]],
            },
            AboutTopic::Languages => AboutText {
                heading: "Języki",
                paragraphs: &[&[AboutPart::Words(
                    "Strony są po angielsku i po polsku. Język wynika z ustawień przeglądarki.",
                )]],
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;
    use jiff::tz::TimeZone;

    use super::*;

    #[test]
    fn names_dates_in_the_right_case() {
        let sunday = date(2026, 9, 27);
        assert_eq!(Polish.day(sunday), "niedziela, 27 września 2026");
        assert_eq!(Polish.month(date(2026, 9, 1)), "wrzesień 2026");
        assert_eq!(Polish.month_short(date(2026, 2, 1)), "luty");
        assert_eq!(Polish.week(sunday.iso_week_date()), "tydzień 39, 2026");
        assert_eq!(
            Polish.week_days(date(2026, 12, 28), date(2027, 1, 3)),
            "28 grudnia – 3 stycznia 2027"
        );
        assert_eq!(Polish.day_short(sunday), "niedz. 27");
    }

    #[test]
    fn names_chart_times() {
        let time = date(2026, 1, 5)
            .at(7, 30, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        let text = |format| Polish.chart_time(&time, format);
        assert_eq!(text(ChartTime::Time), "07:30");
        assert_eq!(text(ChartTime::WeekdayTime), "pon. 07:30");
        assert_eq!(text(ChartTime::DayMonth), "05.01");
        assert_eq!(text(ChartTime::WeekdayDayMonth), "pon. 05.01");
        assert_eq!(text(ChartTime::WeekdayDayMonthTime), "pon. 05.01 07:30");
        assert_eq!(text(ChartTime::WeekdayDate), "pon. 05.01.2026");
        assert_eq!(text(ChartTime::Month), "sty");
    }
}

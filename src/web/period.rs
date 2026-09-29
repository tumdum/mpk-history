use anyhow::Result;
use jiff::civil::{Date, ISOWeekDate, Weekday};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};

use super::i18n::{ChartTime, Translation};
use super::{ChartSpec, ChartStep};
use crate::charts::TickStep;

const WEEK_MARKER: &str = "-W";
const MONTH_SEPARATOR: char = '-';
const DAY_FORMAT: &str = "%Y-%m-%d";
const MONTH_FORMAT: &str = "%Y-%m";
const YEAR_FORMAT: &str = "%Y";
const THURSDAY_OFFSET_DAYS: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeriodKind {
    Day,
    Week,
    Month,
    Year,
}

impl PeriodKind {
    pub const ALL: [PeriodKind; 4] = [
        PeriodKind::Day,
        PeriodKind::Week,
        PeriodKind::Month,
        PeriodKind::Year,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            PeriodKind::Day => "day",
            PeriodKind::Week => "week",
            PeriodKind::Month => "month",
            PeriodKind::Year => "year",
        }
    }

    fn span(self) -> Span {
        match self {
            PeriodKind::Day => Span::new().days(1),
            PeriodKind::Week => Span::new().weeks(1),
            PeriodKind::Month => Span::new().months(1),
            PeriodKind::Year => Span::new().years(1),
        }
    }

    fn part(self) -> Option<PeriodKind> {
        match self {
            PeriodKind::Day => None,
            PeriodKind::Week | PeriodKind::Month => Some(PeriodKind::Day),
            PeriodKind::Year => Some(PeriodKind::Month),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecentPeriod {
    Yesterday,
    PreviousWeek,
    PreviousMonth,
}

impl RecentPeriod {
    pub const ALL: [RecentPeriod; 3] = [
        RecentPeriod::Yesterday,
        RecentPeriod::PreviousWeek,
        RecentPeriod::PreviousMonth,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            RecentPeriod::Yesterday => "yesterday",
            RecentPeriod::PreviousWeek => "prev_week",
            RecentPeriod::PreviousMonth => "prev_month",
        }
    }

    fn kind(self) -> PeriodKind {
        match self {
            RecentPeriod::Yesterday => PeriodKind::Day,
            RecentPeriod::PreviousWeek => PeriodKind::Week,
            RecentPeriod::PreviousMonth => PeriodKind::Month,
        }
    }

    pub fn period(self, today: Date) -> Result<Period> {
        Period::containing(self.kind(), today)?.previous()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    kind: PeriodKind,
    start: Date,
}

impl Period {
    pub fn containing(kind: PeriodKind, date: Date) -> Result<Self> {
        let start = match kind {
            PeriodKind::Day => date,
            PeriodKind::Week => date
                .checked_sub(Span::new().days(i64::from(date.weekday().to_monday_zero_offset())))?,
            PeriodKind::Month => date.first_of_month(),
            PeriodKind::Year => date.first_of_year(),
        };
        Ok(Self { kind, start })
    }

    pub fn parse(kind: PeriodKind, text: &str) -> Option<Self> {
        let start = match kind {
            PeriodKind::Day => Date::strptime(DAY_FORMAT, text).ok()?,
            PeriodKind::Week => {
                let (year, week) = text.split_once(WEEK_MARKER)?;
                ISOWeekDate::new(year.parse().ok()?, week.parse().ok()?, Weekday::Monday)
                    .ok()?
                    .date()
            }
            PeriodKind::Month => {
                let (year, month) = text.split_once(MONTH_SEPARATOR)?;
                Date::new(year.parse().ok()?, month.parse().ok()?, 1).ok()?
            }
            PeriodKind::Year => Date::new(text.parse().ok()?, 1, 1).ok()?,
        };
        let period = Self { kind, start };
        (period.slug() == text).then_some(period)
    }

    pub fn kind(self) -> PeriodKind {
        self.kind
    }

    pub fn slug(self) -> String {
        match self.kind {
            PeriodKind::Day => self.start.strftime(DAY_FORMAT).to_string(),
            PeriodKind::Week => {
                let week = self.start.iso_week_date();
                format!("{:04}{WEEK_MARKER}{:02}", week.year(), week.week())
            }
            PeriodKind::Month => self.start.strftime(MONTH_FORMAT).to_string(),
            PeriodKind::Year => self.start.strftime(YEAR_FORMAT).to_string(),
        }
    }

    pub fn path(self) -> String {
        format!("/{}/{}", self.kind.slug(), self.slug())
    }

    pub fn start_date(self) -> Date {
        self.start
    }

    fn end_date(self) -> Result<Date> {
        Ok(self.start.checked_add(self.kind.span())?)
    }

    pub fn bounds(self, tz: &TimeZone) -> Result<(Timestamp, Timestamp)> {
        let start = self.start.to_zoned(tz.clone())?.timestamp();
        let end = self.end_date()?.to_zoned(tz.clone())?.timestamp();
        Ok((start, end))
    }

    pub fn previous(self) -> Result<Self> {
        Ok(Self {
            kind: self.kind,
            start: self.start.checked_sub(self.kind.span())?,
        })
    }

    pub fn next(self) -> Result<Self> {
        Ok(Self {
            kind: self.kind,
            start: self.end_date()?,
        })
    }

    /// As it reads inside a sentence, which in some languages starts in lower case.
    pub fn title(self, tr: &dyn Translation) -> String {
        match self.kind {
            PeriodKind::Day => tr.day(self.start),
            PeriodKind::Week => tr.week(self.start.iso_week_date()),
            PeriodKind::Month => tr.month(self.start),
            PeriodKind::Year => self.start.strftime(YEAR_FORMAT).to_string(),
        }
    }

    pub fn subtitle(self, tr: &dyn Translation) -> Result<Option<String>> {
        let PeriodKind::Week = self.kind else {
            return Ok(None);
        };
        let last_day = self.end_date()?.yesterday()?;
        Ok(Some(tr.week_days(self.start, last_day)))
    }

    pub fn short_label(self, tr: &dyn Translation) -> String {
        match self.kind {
            PeriodKind::Day => tr.day_short(self.start),
            PeriodKind::Week => tr.week_short(self.start.iso_week_date()),
            PeriodKind::Month => tr.month_short(self.start),
            PeriodKind::Year => self.start.strftime(YEAR_FORMAT).to_string(),
        }
    }

    pub fn parents(self) -> Result<Vec<Period>> {
        let parent_kinds: &[PeriodKind] = match self.kind {
            PeriodKind::Day => &[PeriodKind::Week, PeriodKind::Month, PeriodKind::Year],
            PeriodKind::Week => &[PeriodKind::Month, PeriodKind::Year],
            PeriodKind::Month => &[PeriodKind::Year],
            PeriodKind::Year => &[],
        };
        let anchor = match self.kind {
            PeriodKind::Week => self
                .start
                .checked_add(Span::new().days(THURSDAY_OFFSET_DAYS))?,
            PeriodKind::Day | PeriodKind::Month | PeriodKind::Year => self.start,
        };
        parent_kinds
            .iter()
            .map(|kind| Period::containing(*kind, anchor))
            .collect()
    }

    pub fn parts(self) -> Result<Vec<Period>> {
        let Some(kind) = self.kind.part() else {
            return Ok(Vec::new());
        };
        let end = self.end_date()?;
        let mut parts = Vec::new();
        let mut part = Period {
            kind,
            start: self.start,
        };
        while part.start < end {
            parts.push(part);
            part = part.next()?;
        }
        Ok(parts)
    }

    pub fn chart(self) -> ChartSpec {
        match self.kind {
            PeriodKind::Day => ChartSpec {
                step: ChartStep::Minutes(15),
                tick_step: TickStep::Hours(3),
                tick: ChartTime::Time,
                hover: ChartTime::Time,
            },
            PeriodKind::Week => ChartSpec {
                step: ChartStep::Hours(1),
                tick_step: TickStep::Days(1),
                tick: ChartTime::WeekdayDayMonth,
                hover: ChartTime::WeekdayDayMonthTime,
            },
            PeriodKind::Month => ChartSpec {
                step: ChartStep::Hours(6),
                tick_step: TickStep::Days(5),
                tick: ChartTime::DayMonth,
                hover: ChartTime::WeekdayDayMonthTime,
            },
            PeriodKind::Year => ChartSpec {
                step: ChartStep::Days,
                tick_step: TickStep::Months(1),
                tick: ChartTime::Month,
                hover: ChartTime::WeekdayDate,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::*;
    use crate::web::i18n::Language;

    const WARSAW: &str = "Europe/Warsaw";
    const HOUR_SECS: i64 = 60 * 60;

    fn warsaw() -> TimeZone {
        TimeZone::get(WARSAW).unwrap()
    }

    fn period(kind: PeriodKind, text: &str) -> Period {
        Period::parse(kind, text).unwrap_or_else(|| panic!("{text} is not a {kind:?}"))
    }

    fn slugs(periods: &[Period]) -> Vec<String> {
        periods.iter().map(|period| period.slug()).collect()
    }

    #[test]
    fn parses_only_canonical_slugs() {
        let valid = [
            (PeriodKind::Day, "2026-09-27"),
            (PeriodKind::Week, "2026-W39"),
            (PeriodKind::Week, "2026-W53"),
            (PeriodKind::Month, "2026-09"),
            (PeriodKind::Year, "2026"),
        ];
        for (kind, text) in valid {
            assert_eq!(period(kind, text).slug(), text);
            assert_eq!(
                period(kind, text).path(),
                format!("/{}/{text}", kind.slug())
            );
        }
        let invalid = [
            (PeriodKind::Day, "2026-9-27"),
            (PeriodKind::Day, "2026-02-30"),
            (PeriodKind::Day, "27.09.2026"),
            (PeriodKind::Week, "2026-W54"),
            (PeriodKind::Week, "2025-W53"),
            (PeriodKind::Week, "2026-W9"),
            (PeriodKind::Week, "2026-39"),
            (PeriodKind::Month, "2026-13"),
            (PeriodKind::Month, "2026-9"),
            (PeriodKind::Year, "26"),
            (PeriodKind::Year, "year"),
            (PeriodKind::Year, ""),
        ];
        for (kind, text) in invalid {
            assert_eq!(Period::parse(kind, text), None, "{kind:?} {text}");
        }
    }

    #[test]
    fn finds_the_period_containing_a_date() {
        let sunday = date(2026, 9, 27);
        let slug = |kind| Period::containing(kind, sunday).unwrap().slug();
        assert_eq!(slug(PeriodKind::Day), "2026-09-27");
        assert_eq!(slug(PeriodKind::Week), "2026-W39");
        assert_eq!(slug(PeriodKind::Month), "2026-09");
        assert_eq!(slug(PeriodKind::Year), "2026");
        let week = Period::containing(PeriodKind::Week, sunday).unwrap();
        assert_eq!(week.start_date(), date(2026, 9, 21));
    }

    #[test]
    fn uses_local_midnight_and_follows_daylight_saving_time() {
        let tz = warsaw();
        let length_hours = |kind, text| {
            let (start, end) = period(kind, text).bounds(&tz).unwrap();
            (end.as_second() - start.as_second()) / HOUR_SECS
        };
        assert_eq!(length_hours(PeriodKind::Day, "2026-09-27"), 24);
        assert_eq!(length_hours(PeriodKind::Day, "2026-03-29"), 23);
        assert_eq!(length_hours(PeriodKind::Day, "2026-10-25"), 25);
        assert_eq!(length_hours(PeriodKind::Week, "2026-W13"), 7 * 24 - 1);
        assert_eq!(length_hours(PeriodKind::Year, "2026"), 365 * 24);

        let (start, _) = period(PeriodKind::Day, "2026-09-27").bounds(&tz).unwrap();
        assert_eq!(start.to_string(), "2026-09-26T22:00:00Z");
    }

    #[test]
    fn steps_to_neighbouring_periods_across_year_ends() {
        let step = |kind, text: &str| {
            let current = period(kind, text);
            (
                current.previous().unwrap().slug(),
                current.next().unwrap().slug(),
            )
        };
        assert_eq!(
            step(PeriodKind::Day, "2027-01-01"),
            ("2026-12-31".to_owned(), "2027-01-02".to_owned())
        );
        assert_eq!(
            step(PeriodKind::Week, "2026-W53"),
            ("2026-W52".to_owned(), "2027-W01".to_owned())
        );
        assert_eq!(
            step(PeriodKind::Week, "2027-W01"),
            ("2026-W53".to_owned(), "2027-W02".to_owned())
        );
        assert_eq!(
            step(PeriodKind::Month, "2026-01"),
            ("2025-12".to_owned(), "2026-02".to_owned())
        );
        assert_eq!(
            step(PeriodKind::Year, "2026"),
            ("2025".to_owned(), "2027".to_owned())
        );
    }

    #[test]
    fn finds_recent_periods_before_today() {
        let path = |recent: RecentPeriod, today| recent.period(today).unwrap().path();
        let new_year = date(2027, 1, 1);
        assert_eq!(path(RecentPeriod::Yesterday, new_year), "/day/2026-12-31");
        assert_eq!(path(RecentPeriod::PreviousWeek, new_year), "/week/2026-W52");
        assert_eq!(
            path(RecentPeriod::PreviousMonth, new_year),
            "/month/2026-12"
        );
        let monday = date(2026, 9, 28);
        assert_eq!(path(RecentPeriod::PreviousWeek, monday), "/week/2026-W39");
        let leap_day = date(2028, 3, 1);
        assert_eq!(path(RecentPeriod::Yesterday, leap_day), "/day/2028-02-29");
        assert_eq!(
            path(RecentPeriod::PreviousMonth, leap_day),
            "/month/2028-02"
        );
    }

    #[test]
    fn links_to_larger_and_smaller_periods() {
        let day = period(PeriodKind::Day, "2026-09-27");
        assert_eq!(
            slugs(&day.parents().unwrap()),
            ["2026-W39", "2026-09", "2026"]
        );
        assert!(day.parts().unwrap().is_empty());

        let week_across_years = period(PeriodKind::Week, "2026-W53");
        assert_eq!(week_across_years.start_date(), date(2026, 12, 28));
        assert_eq!(
            slugs(&week_across_years.parents().unwrap()),
            ["2026-12", "2026"]
        );
        let days = week_across_years.parts().unwrap();
        assert_eq!(days.len(), 7);
        assert_eq!(days[0].slug(), "2026-12-28");
        assert_eq!(days[6].slug(), "2027-01-03");

        assert_eq!(
            period(PeriodKind::Month, "2028-02").parts().unwrap().len(),
            29
        );
        let months = period(PeriodKind::Year, "2026").parts().unwrap();
        assert_eq!(months.len(), 12);
        assert_eq!(months[11].slug(), "2026-12");
        assert!(
            period(PeriodKind::Year, "2026")
                .parents()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn names_periods_for_people() {
        let en = Language::English.translation();
        assert_eq!(
            period(PeriodKind::Day, "2026-09-27").title(en),
            "Sunday, 27 September 2026"
        );
        let week = period(PeriodKind::Week, "2026-W39");
        assert_eq!(week.title(en), "Week 39, 2026");
        assert_eq!(
            week.subtitle(en).unwrap().as_deref(),
            Some("21 September – 27 September 2026")
        );
        assert_eq!(
            period(PeriodKind::Month, "2026-09").title(en),
            "September 2026"
        );
        assert_eq!(period(PeriodKind::Year, "2026").title(en), "2026");
        assert_eq!(
            period(PeriodKind::Day, "2026-09-27").short_label(en),
            "Sun 27"
        );
        assert_eq!(
            period(PeriodKind::Month, "2026-09").subtitle(en).unwrap(),
            None
        );
    }

    #[test]
    fn names_periods_in_polish() {
        let pl = Language::Polish.translation();
        assert_eq!(
            period(PeriodKind::Day, "2026-09-27").title(pl),
            "niedziela, 27 września 2026"
        );
        let week = period(PeriodKind::Week, "2026-W39");
        assert_eq!(week.title(pl), "tydzień 39, 2026");
        assert_eq!(
            week.subtitle(pl).unwrap().as_deref(),
            Some("21 września – 27 września 2026")
        );
        assert_eq!(
            period(PeriodKind::Month, "2026-09").title(pl),
            "wrzesień 2026"
        );
        assert_eq!(period(PeriodKind::Year, "2026").title(pl), "2026");
        assert_eq!(
            period(PeriodKind::Month, "2026-09").short_label(pl),
            "wrzesień"
        );
    }
}

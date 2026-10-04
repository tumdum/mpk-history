use anyhow::Result;
use maud::{Markup, html};

use super::i18n::{Text, Translation, capitalized};
use super::map::MAP_PATH;
use super::period::Period;
use super::quality::{OVERVIEW_QUALITY_PATH, QUALITY_SEGMENT};
use super::{OVERVIEW_PATH, Range, kind_color, kind_slug};
use crate::charts::RGBColor;
use crate::db::{Db, StoredLine, VehicleScope};
use crate::mpk::VehicleType;
use crate::stats::{Group, Kind};

const UNRESERVED_PATH_MARKS: &[u8] = b"-._~";

#[derive(Debug, Clone)]
pub(super) enum SelectionKey {
    All,
    Type(VehicleType),
    Line(VehicleType, String),
}

impl SelectionKey {
    pub(super) async fn resolve(self, db: &Db) -> Result<Option<Selection>> {
        Ok(match self {
            SelectionKey::All => Some(Selection::All),
            SelectionKey::Type(vehicle_type) => Some(Selection::Type(vehicle_type)),
            SelectionKey::Line(vehicle_type, name) => {
                db.line(vehicle_type, &name).await?.map(Selection::Line)
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Selection {
    All,
    Type(VehicleType),
    Line(StoredLine),
}

impl Selection {
    pub(super) fn scope(&self) -> VehicleScope {
        match self {
            Selection::All => VehicleScope::All,
            Selection::Type(vehicle_type) => VehicleScope::Type(*vehicle_type),
            Selection::Line(line) => VehicleScope::Line(line.id),
        }
    }

    fn prefix(&self) -> String {
        match self {
            Selection::All => String::new(),
            Selection::Type(vehicle_type) => format!("/{}", vehicle_type.as_str()),
            Selection::Line(line) => format!(
                "/{}/{}",
                line.vehicle_type.as_str(),
                path_segment(&line.name)
            ),
        }
    }

    pub(super) fn overview_path(&self) -> String {
        match self {
            Selection::All => OVERVIEW_PATH.to_owned(),
            Selection::Type(_) | Selection::Line(_) => self.prefix(),
        }
    }

    pub(super) fn range_path(&self, range: Range) -> String {
        format!("{}?range={}", self.overview_path(), range.slug())
    }

    pub(super) fn period_path(&self, period: Period) -> String {
        format!("{}{}", self.prefix(), period.path())
    }

    pub(super) fn quality_path(&self) -> String {
        match self {
            Selection::All => OVERVIEW_QUALITY_PATH.to_owned(),
            Selection::Type(_) | Selection::Line(_) => {
                format!("{}/{QUALITY_SEGMENT}", self.prefix())
            }
        }
    }

    pub(super) fn quality_range_path(&self, range: Range) -> String {
        format!("{}?range={}", self.quality_path(), range.slug())
    }

    pub(super) fn period_quality_path(&self, period: Period) -> String {
        format!("{}/{QUALITY_SEGMENT}", self.period_path(period))
    }

    pub(super) fn map_path(&self) -> String {
        format!("{}{MAP_PATH}", self.prefix())
    }

    pub(super) fn includes(&self, vehicle_type: VehicleType, line_id: i64) -> bool {
        match self {
            Selection::All => true,
            Selection::Type(selected) => *selected == vehicle_type,
            Selection::Line(line) => line.id == line_id,
        }
    }

    pub(super) fn label(&self, tr: &dyn Translation) -> String {
        match self {
            Selection::All => tr.kind(Kind::All).to_owned(),
            Selection::Type(vehicle_type) => tr.kind(Kind::Vehicle(*vehicle_type)).to_owned(),
            Selection::Line(line) => tr.line(line.vehicle_type, &line.name),
        }
    }

    pub(super) fn heading(&self, tr: &dyn Translation, what: &str) -> String {
        match self {
            Selection::All => capitalized(what),
            Selection::Type(_) | Selection::Line(_) => format!("{}: {what}", self.label(tr)),
        }
    }

    fn vehicle_type(&self) -> Option<VehicleType> {
        match self {
            Selection::All => None,
            Selection::Type(vehicle_type) => Some(*vehicle_type),
            Selection::Line(line) => Some(line.vehicle_type),
        }
    }

    pub(super) fn subjects(&self, tr: &dyn Translation) -> Vec<Subject> {
        match self {
            Selection::All => std::iter::once(Subject::kind(tr, Kind::All, Visibility::Always))
                .chain(VehicleType::ALL.into_iter().map(|vehicle_type| {
                    Subject::kind(tr, Kind::Vehicle(vehicle_type), Visibility::WithVehicles)
                }))
                .collect(),
            Selection::Type(vehicle_type) => vec![Subject::kind(
                tr,
                Kind::Vehicle(*vehicle_type),
                Visibility::Always,
            )],
            Selection::Line(line) => vec![Subject {
                group: Group::Line(line.id),
                slug: format!("{}-{}", line.vehicle_type.as_str(), line.name),
                label: self.label(tr),
                color: kind_color(Kind::Vehicle(line.vehicle_type)),
                visibility: Visibility::Always,
            }],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Always,
    WithVehicles,
}

#[derive(Debug, Clone)]
pub(super) struct Subject {
    pub(super) group: Group,
    pub(super) slug: String,
    pub(super) label: String,
    pub(super) color: RGBColor,
    visibility: Visibility,
}

impl Subject {
    fn kind(tr: &dyn Translation, kind: Kind, visibility: Visibility) -> Self {
        Self {
            group: Group::Kind(kind),
            slug: kind_slug(kind).to_owned(),
            label: tr.kind(kind).to_owned(),
            color: kind_color(kind),
            visibility,
        }
    }
}

pub(super) fn groups(subjects: &[Subject]) -> Vec<Group> {
    subjects.iter().map(|subject| subject.group).collect()
}

/// `values` has one entry per subject, in the same order.
pub(super) fn shown<'a, T>(
    subjects: &'a [Subject],
    values: &'a [T],
    has_vehicles: impl Fn(&T) -> bool,
) -> Vec<(&'a Subject, &'a T)> {
    subjects
        .iter()
        .zip(values)
        .filter(|(subject, value)| subject.visibility == Visibility::Always || has_vehicles(value))
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Page {
    Overview(Range),
    Period(Period),
    QualityOverview(Range),
    QualityPeriod(Period),
}

impl Page {
    fn path(self, selection: &Selection) -> String {
        match self {
            Page::Overview(range) => selection.range_path(range),
            Page::Period(period) => selection.period_path(period),
            Page::QualityOverview(range) => selection.quality_range_path(range),
            Page::QualityPeriod(period) => selection.period_quality_path(period),
        }
    }
}

pub(super) fn vehicles_nav(
    tr: &dyn Translation,
    selection: &Selection,
    page: Page,
    lines: &[StoredLine],
) -> Markup {
    let types = VehicleType::ALL.into_iter().filter(|vehicle_type| {
        selection.vehicle_type() == Some(*vehicle_type)
            || lines.iter().any(|line| line.vehicle_type == *vehicle_type)
    });
    let options: Vec<Selection> = std::iter::once(Selection::All)
        .chain(types.map(Selection::Type))
        .collect();
    html! {
        nav #vehicles aria-label=(tr.text(Text::Vehicles)) {
            @for option in &options {
                a href=(page.path(option)) aria-current=[(option == selection).then_some("page")] {
                    (option.label(tr))
                }
            }
        }
    }
}

pub(super) fn lines_section(
    tr: &dyn Translation,
    selection: &Selection,
    page: Page,
    lines: &[StoredLine],
) -> Markup {
    let mut lines: Vec<StoredLine> = lines
        .iter()
        .filter(|line| {
            selection
                .vehicle_type()
                .is_none_or(|vehicle_type| vehicle_type == line.vehicle_type)
        })
        .cloned()
        .collect();
    if let Selection::Line(current) = selection
        && !lines.contains(current)
    {
        lines.push(current.clone());
    }
    lines.sort_by_key(line_order);
    if lines.is_empty() {
        return html! {};
    }
    html! {
        section #lines {
            h2 { (tr.text(Text::Lines)) }
            @for of_type in lines.chunk_by(|a, b| a.vehicle_type == b.vehicle_type) {
                @let kind = Kind::Vehicle(of_type[0].vehicle_type);
                h3 { (tr.kind(kind)) }
                nav aria-label=(tr.kind(kind)) data-kind=(kind_slug(kind)) {
                    @for line in of_type {
                        @let option = Selection::Line(line.clone());
                        a href=(page.path(&option)) aria-current=[(&option == selection).then_some("page")] {
                            (line.name)
                        }
                    }
                }
            }
        }
    }
}

fn line_order(line: &StoredLine) -> (i64, bool, u64, String) {
    let number = line.name.parse::<u64>();
    (
        line.vehicle_type.id(),
        number.is_err(),
        number.unwrap_or_default(),
        line.name.clone(),
    )
}

fn path_segment(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || UNRESERVED_PATH_MARKS.contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::i18n::Language;
    use crate::web::period::PeriodKind;

    const LINE_ID: i64 = 7;

    fn line(vehicle_type: VehicleType, name: &str) -> StoredLine {
        StoredLine {
            id: LINE_ID,
            vehicle_type,
            name: name.to_owned(),
        }
    }

    #[test]
    fn builds_a_unique_path_for_every_selection_and_period() {
        let week = Period::parse(PeriodKind::Week, "2026-W39").unwrap();
        let bus_line = Selection::Line(line(VehicleType::Bus, "136"));
        let cases = [
            (Selection::All, "/", "/week/2026-W39"),
            (
                Selection::Type(VehicleType::Bus),
                "/bus",
                "/bus/week/2026-W39",
            ),
            (
                Selection::Type(VehicleType::Tram),
                "/tram",
                "/tram/week/2026-W39",
            ),
            (bus_line.clone(), "/bus/136", "/bus/136/week/2026-W39"),
        ];
        for (selection, overview, period) in cases {
            assert_eq!(selection.overview_path(), overview);
            assert_eq!(selection.period_path(week), period);
        }
        assert_eq!(bus_line.range_path(Range::Week), "/bus/136?range=week");
        assert_eq!(Selection::All.quality_path(), "/quality");
        assert_eq!(bus_line.quality_path(), "/bus/136/quality");
        assert_eq!(
            bus_line.quality_range_path(Range::Day),
            "/bus/136/quality?range=day"
        );
        assert_eq!(
            Selection::Type(VehicleType::Tram).period_quality_path(week),
            "/tram/week/2026-W39/quality"
        );
        assert_eq!(
            Selection::All.period_quality_path(week),
            "/week/2026-W39/quality"
        );
        assert_eq!(Selection::All.range_path(Range::Hour), "/?range=hour");
        assert_eq!(Selection::All.map_path(), "/map.svg");
        assert_eq!(bus_line.map_path(), "/bus/136/map.svg");
        let odd = Selection::Line(line(VehicleType::Tram, "n 1/2"));
        assert_eq!(odd.overview_path(), "/tram/n%201%2F2");
    }

    #[test]
    fn names_selections_for_people() {
        let en = Language::English.translation();
        assert_eq!(Selection::All.label(en), "All vehicles");
        assert_eq!(Selection::Type(VehicleType::Tram).label(en), "Trams");
        let bus_line = Selection::Line(line(VehicleType::Bus, "a"));
        assert_eq!(bus_line.label(en), "Bus a");
        assert_eq!(bus_line.heading(en, "May 2027"), "Bus a: May 2027");
        assert_eq!(Selection::All.heading(en, "May 2027"), "May 2027");

        let pl = Language::Polish.translation();
        assert_eq!(bus_line.label(pl), "Autobus a");
        assert_eq!(bus_line.heading(pl, "maj 2027"), "Autobus a: maj 2027");
        assert_eq!(Selection::All.heading(pl, "maj 2027"), "Maj 2027");
    }

    #[test]
    fn sorts_numbered_lines_before_lettered_ones() {
        let mut lines: Vec<StoredLine> = ["a", "136", "k", "9", "20", "d"]
            .into_iter()
            .map(|name| line(VehicleType::Bus, name))
            .chain(std::iter::once(line(VehicleType::Tram, "0")))
            .collect();
        lines.sort_by_key(line_order);
        let names: Vec<&str> = lines.iter().map(|line| line.name.as_str()).collect();
        assert_eq!(names, ["9", "20", "136", "a", "d", "k", "0"]);
    }
}

use anyhow::Result;
use axum::response::Response;
use maud::html;

use super::caching::Caching;
use super::i18n::{AboutPart, Language, Relative, Text};
use super::period::{PeriodKind, RecentPeriod};
use super::quality::OVERVIEW_QUALITY_PATH;
use super::{AppError, Head, OVERVIEW_PATH, PeriodView, recent_period_route, shell};

pub(super) const ABOUT_PATH: &str = "/about";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AboutTopic {
    Service,
    CurrentVehicles,
    Statistics,
    VehiclesAndLines,
    CalendarPages,
    SpeedMap,
    DataQuality,
    Languages,
}

impl AboutTopic {
    pub(super) const ALL: [AboutTopic; 8] = [
        AboutTopic::Service,
        AboutTopic::CurrentVehicles,
        AboutTopic::Statistics,
        AboutTopic::VehiclesAndLines,
        AboutTopic::CalendarPages,
        AboutTopic::SpeedMap,
        AboutTopic::DataQuality,
        AboutTopic::Languages,
    ];
}

pub(super) async fn page(language: Language) -> Result<Response, AppError> {
    let tr = language.translation();
    let heading = tr.text(Text::About);
    let yesterday = recent_path(RecentPeriod::Yesterday);
    let body = html! {
        header {
            p .breadcrumbs { a href=(OVERVIEW_PATH) { (tr.text(Text::Overview)) } }
            h1 { (heading) }
        }
        main .about {
            @for topic in AboutTopic::ALL {
                @let about = tr.about(topic);
                section {
                    h2 { (about.heading) }
                    @for paragraph in about.paragraphs {
                        p {
                            @for part in *paragraph {
                                @match part {
                                    AboutPart::Words(words) => (words),
                                    AboutPart::Address(recent) => {
                                        @let path = recent_path(*recent);
                                        a href=(path) { (path) }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            nav #pages aria-label=(tr.text(Text::Pages)) {
                a href=(OVERVIEW_PATH) { (tr.text(Text::Overview)) }
                a href=(yesterday) { (tr.relative_period(PeriodKind::Day, Relative::Previous)) }
                a href=(OVERVIEW_QUALITY_PATH) { (tr.text(Text::DataQuality)) }
            }
        }
    };
    let head = Head::new(tr, heading, Text::AboutDescription);
    Ok(shell(language, Caching::live()?, &head, &body))
}

fn recent_path(recent: RecentPeriod) -> String {
    recent_period_route("", recent, PeriodView::Statistics)
}

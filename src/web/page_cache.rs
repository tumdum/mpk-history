use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::Response;
use lru::LruCache;
use parking_lot::Mutex;

use super::caching::IfNoneMatch;
use super::i18n::Language;
use super::{AppError, AppState};
use crate::{db, stats};

pub const DEFAULT_PAGE_CACHE_CAPACITY: NonZeroUsize = NonZeroUsize::new(128).unwrap();
const RECENT_HISTORY: Duration = Duration::from_hours(24);
const PAGE_MAX_AGE: Duration = Duration::from_mins(1);

#[derive(Clone, Copy)]
pub(super) enum Validity {
    Forever,
    Recent,
}

impl Validity {
    pub(super) fn period(to_unix: i64, now_unix: i64) -> Self {
        let delay = RECENT_HISTORY.max(stats::final_delay());
        if now_unix < to_unix + db::duration_secs(delay) {
            return Self::Recent;
        }
        Self::Forever
    }

    fn deadline(self, started: Instant) -> Option<Instant> {
        match self {
            Self::Forever => None,
            Self::Recent => Some(started + PAGE_MAX_AGE),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct PageKey {
    uri: Uri,
    language: Language,
}

#[derive(Clone)]
struct RenderedPage {
    headers: HeaderMap,
    body: Bytes,
    valid_until: Option<Instant>,
}

impl RenderedPage {
    fn response(self, if_none_match: &IfNoneMatch) -> Response {
        let matches = self
            .headers
            .get(header::ETAG)
            .and_then(|tag| tag.to_str().ok())
            .is_some_and(|tag| if_none_match.matches(tag));
        let mut response = if matches {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::NOT_MODIFIED;
            response
        } else {
            Response::new(Body::from(self.body))
        };
        *response.headers_mut() = self.headers;
        if matches {
            response.headers_mut().remove(header::CONTENT_LENGTH);
            response.headers_mut().remove(header::CONTENT_TYPE);
        }
        response
    }
}

pub(super) struct PageCache(Mutex<LruCache<PageKey, RenderedPage>>);

impl PageCache {
    pub(super) fn new(capacity: NonZeroUsize) -> Self {
        Self(Mutex::new(LruCache::new(capacity)))
    }

    fn get(&self, key: &PageKey, now: Instant) -> Option<RenderedPage> {
        let mut pages = self.0.lock();
        let page = pages.get(key)?;
        if page.valid_until.is_some_and(|until| now >= until) {
            pages.pop(key);
            return None;
        }
        Some(page.clone())
    }
}

pub(super) async fn serve(
    State(state): State<AppState>,
    language: Language,
    if_none_match: IfNoneMatch,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    if request.method() != Method::GET || state.basemap.get().is_none() {
        return Ok(next.run(request).await);
    }
    let started = Instant::now();
    let key = PageKey {
        uri: request.uri().clone(),
        language,
    };
    if let Some(page) = state.pages.get(&key, started) {
        return Ok(page.response(&if_none_match));
    }
    let response = next.run(request).await;
    if response.status() != StatusCode::OK {
        return Ok(response);
    }
    let Some(validity) = response.extensions().get::<Validity>().copied() else {
        return Ok(response);
    };
    let (parts, body) = response.into_parts();
    let body = to_bytes(body, usize::MAX).await?;
    state.pages.0.lock().put(
        key,
        RenderedPage {
            headers: parts.headers.clone(),
            body: body.clone(),
            valid_until: validity.deadline(started),
        },
    );
    Ok(Response::from_parts(parts, Body::from(body)))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use axum::Router;
    use jiff::Timestamp;
    use rstest::rstest;
    use tower::ServiceExt;

    use super::*;
    use crate::db::{Db, OpenMode};
    use crate::fetch::Source;
    use crate::geo::MicroDegrees;
    use crate::map::Basemap;
    use crate::mpk::{Position, VehicleType};
    use crate::web::caching::{Caching, Revision};
    use crate::web::map::SharedBasemap;
    use crate::web::{Clock, app};

    const NOW: &str = "2026-09-30T12:00:00Z";
    const HISTORICAL_PAGE: &str = "/day/2026-09-27";
    const HISTORICAL_SEEN: &str = "2026-09-27T12:00:00Z";
    const TEST_COMMIT: &str = "test-commit";
    const CAPACITY: NonZeroUsize = NonZeroUsize::new(2).unwrap();

    async fn state() -> AppState {
        let db = Db::open(Path::new(":memory:"), OpenMode::CreateIfMissing)
            .await
            .unwrap();
        let bus = Position {
            vehicle_type: VehicleType::Bus,
            line: "a".to_owned(),
            course_id: 1,
            latitude: MicroDegrees(51_100_000),
            longitude: MicroDegrees(17_000_000),
        };
        db.save_positions(
            HISTORICAL_SEEN.parse::<Timestamp>().unwrap().as_second(),
            &[&bus],
            &[],
        )
        .await
        .unwrap();
        db.save_fetch_outcome(
            Source::MpkPositions,
            HISTORICAL_SEEN.parse::<Timestamp>().unwrap().as_second(),
            None,
        )
        .await
        .unwrap();
        let state = AppState {
            basemap: SharedBasemap::ready(Basemap::empty()),
            revision: Revision::Commit(TEST_COMMIT),
            pages: Arc::new(PageCache::new(CAPACITY)),
            ..AppState::new(db, Clock::Fixed(NOW.parse().unwrap()))
        };
        state.summary.wait_until_ready().await;
        state
    }

    fn key(uri: &str, language: Language) -> PageKey {
        PageKey {
            uri: uri.parse().unwrap(),
            language,
        }
    }

    async fn request(app: Router, uri: &str, language: Language) -> Response {
        app.oneshot(
            Request::get(uri)
                .header(header::ACCEPT_LANGUAGE, language.tag())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    }

    async fn body(response: Response) -> Bytes {
        to_bytes(response.into_body(), usize::MAX).await.unwrap()
    }

    #[rstest]
    #[case(-1, Validity::Recent)]
    #[case(0, Validity::Recent)]
    #[case(db::duration_secs(RECENT_HISTORY) - 1, Validity::Recent)]
    #[case(db::duration_secs(RECENT_HISTORY), Validity::Forever)]
    fn period_validity_is_independent_of_build_revision(
        #[case] age: i64,
        #[case] expected: Validity,
    ) {
        let now = NOW.parse::<Timestamp>().unwrap().as_second();
        let started = Instant::now();
        for revision in [Revision::Unknown, Revision::Commit(TEST_COMMIT)] {
            let caching = Caching::period(revision, Language::English, now - age, now).unwrap();
            assert_eq!(
                caching.page_validity.deadline(started),
                expected.deadline(started)
            );
        }
    }

    #[tokio::test]
    async fn does_not_cache_an_overview_before_its_summary_is_ready() {
        let mut state = state().await;
        state.summary = super::super::summary::Cache::new(state.db.clone(), state.clock);
        let first = request(app(state.clone()), "/", Language::English).await;
        assert!(state.pages.0.lock().is_empty());
        let first = body(first).await;
        assert!(
            std::str::from_utf8(&first)
                .unwrap()
                .contains("Summary statistics are being prepared.")
        );
        state.summary.wait_until_ready().await;
        let ready = request(app(state.clone()), "/", Language::English).await;
        assert!(state.pages.0.lock().contains(&key("/", Language::English)));
        assert_ne!(body(ready).await, first);
    }

    #[tokio::test]
    async fn hits_skip_rendering_and_expired_pages_are_rendered_again() {
        let mut state = state().await;
        assert!(state.pages.0.lock().is_empty());
        let first = body(request(app(state.clone()), "/", Language::English).await).await;
        state.clock = Clock::Fixed(state.clock.now() + Duration::from_hours(1));
        let cached = body(request(app(state.clone()), "/", Language::English).await).await;
        assert_eq!(first, cached);
        state
            .pages
            .0
            .lock()
            .get_mut(&key("/", Language::English))
            .unwrap()
            .valid_until = Some(Instant::now());
        let refreshed = body(request(app(state.clone()), "/", Language::English).await).await;
        assert_ne!(first, refreshed);
        let key = key("/", Language::English);
        let until = state
            .pages
            .0
            .lock()
            .peek(&key)
            .unwrap()
            .valid_until
            .unwrap();
        assert!(
            state
                .pages
                .get(&key, until.checked_sub(Duration::from_nanos(1)).unwrap())
                .is_some()
        );
        assert!(state.pages.get(&key, until).is_none());
        assert!(state.pages.0.lock().is_empty());
    }

    #[tokio::test]
    async fn evicts_the_least_recently_used_page_and_separates_languages() {
        let state = state().await;
        let app = app(state.clone());
        let en = body(request(app.clone(), "/", Language::English).await).await;
        let pl = body(request(app.clone(), "/", Language::Polish).await).await;
        assert_ne!(en, pl);
        assert_eq!(
            body(request(app.clone(), "/", Language::English).await).await,
            en
        );
        request(app, "/?range=hour", Language::English).await;
        let pages = state.pages.0.lock();
        assert_eq!(pages.len(), CAPACITY.get());
        assert!(pages.contains(&key("/", Language::English)));
        assert!(pages.contains(&key("/?range=hour", Language::English)));
        assert!(!pages.contains(&key("/", Language::Polish)));
    }

    #[tokio::test]
    async fn historical_hits_keep_headers_and_support_conditional_requests() {
        let state = state().await;
        let app = app(state.clone());
        let first = request(app.clone(), HISTORICAL_PAGE, Language::English).await;
        let headers = first.headers().clone();
        let first = body(first).await;
        let cached = request(app.clone(), HISTORICAL_PAGE, Language::English).await;
        assert_eq!(cached.headers(), &headers);
        assert_eq!(body(cached).await, first);
        let key = key(HISTORICAL_PAGE, Language::English);
        assert!(
            state
                .pages
                .get(&key, Instant::now() + RECENT_HISTORY)
                .is_some()
        );
        assert!(
            state
                .pages
                .0
                .lock()
                .peek(&key)
                .unwrap()
                .valid_until
                .is_none()
        );
        for tag in [
            headers[header::ETAG].to_str().unwrap().to_owned(),
            format!("W/{}", headers[header::ETAG].to_str().unwrap()),
            "*".to_owned(),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::get(HISTORICAL_PAGE)
                        .header(header::IF_NONE_MATCH, tag)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
            assert_eq!(response.headers()[header::ETAG], headers[header::ETAG]);
            assert!(body(response).await.is_empty());
        }
    }

    #[tokio::test]
    async fn waits_for_basemap_and_caches_only_successful_html_pages() {
        let mut state = state().await;
        let basemap = state.basemap.clone();
        state.basemap = SharedBasemap::default();
        request(app(state.clone()), HISTORICAL_PAGE, Language::English).await;
        assert!(state.pages.0.lock().is_empty());
        state.basemap = basemap;
        let app = app(state.clone());
        for uri in [
            "/yesterday",
            "/day/1900-01-01",
            "/day/1900-01-01/quality",
            "/missing",
            "/?range=invalid",
            "/bus/unknown",
            "/map/wroclaw.jpg",
            "/map.svg",
            "/charts/average-speed.svg",
        ] {
            request(app.clone(), uri, Language::English).await;
            assert!(state.pages.0.lock().is_empty(), "{uri}");
        }
        let head = app
            .clone()
            .oneshot(Request::head(HISTORICAL_PAGE).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(head.status(), StatusCode::OK);
        assert!(state.pages.0.lock().is_empty());
        for uri in [
            HISTORICAL_PAGE,
            "/day/2026-09-27/quality",
            "/quality",
            "/about",
            "/bus",
        ] {
            assert_eq!(
                request(app.clone(), uri, Language::English).await.status(),
                StatusCode::OK
            );
            assert!(
                state.pages.0.lock().contains(&key(uri, Language::English)),
                "{uri}"
            );
        }
    }
}

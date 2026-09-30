use std::convert::Infallible;
use std::future::{self, Future};
use std::time::Duration;

use anyhow::Result;
use axum::extract::FromRequestParts;
use axum::http::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, IntoResponseParts, Response, ResponseParts};

use super::i18n::Language;
use super::language_headers;
use super::page_cache::Validity;
use crate::{build_info, db, stats};

const SETTLED_MAX_AGE: Duration = Duration::from_hours(24);
const LIVE_MAX_AGE: Duration = stats::BUCKET;
const TAG_SEPARATOR: char = ',';
const ANY_TAG: &str = "*";
const WEAK_PREFIX: &str = "W/";

#[derive(Debug, Clone, Copy)]
pub(super) enum Revision {
    Commit(&'static str),
    Unknown,
}

impl Revision {
    pub(super) fn current() -> Self {
        build_info::clean_commit().map_or(Revision::Unknown, Revision::Commit)
    }
}

pub(super) struct Caching {
    cache_control: HeaderValue,
    etag: Option<HeaderValue>,
    pub(super) page_validity: Validity,
}

impl Caching {
    pub(super) fn live() -> Result<Self> {
        Ok(Self {
            cache_control: cache_control(LIVE_MAX_AGE)?,
            etag: None,
            page_validity: Validity::Recent,
        })
    }

    pub(super) fn stable() -> Result<Self> {
        Ok(Self {
            cache_control: cache_control(SETTLED_MAX_AGE)?,
            etag: None,
            page_validity: Validity::Forever,
        })
    }

    pub(super) fn period(
        revision: Revision,
        language: Language,
        to_unix: i64,
        now_unix: i64,
    ) -> Result<Self> {
        let mut caching = Self::live()?;
        caching.page_validity = Validity::period(to_unix, now_unix);
        if now_unix < to_unix + db::duration_secs(stats::final_delay()) {
            return Ok(caching);
        }
        let Revision::Commit(commit) = revision else {
            return Ok(caching);
        };
        Ok(Self {
            cache_control: cache_control(SETTLED_MAX_AGE)?,
            etag: Some(HeaderValue::try_from(format!(
                "\"{commit}-{}\"",
                language.tag()
            ))?),
            page_validity: caching.page_validity,
        })
    }

    pub(super) fn matches(&self, request: &IfNoneMatch) -> bool {
        let Some(etag) = &self.etag else {
            return false;
        };
        let Ok(etag) = etag.to_str() else {
            return false;
        };
        request.matches(etag)
    }

    pub(super) fn not_modified(self, language: Language) -> Response {
        (
            StatusCode::NOT_MODIFIED,
            language_headers(language),
            self,
            (),
        )
            .into_response()
    }
}

fn cache_control(max_age: Duration) -> Result<HeaderValue> {
    Ok(HeaderValue::try_from(format!(
        "max-age={}",
        max_age.as_secs()
    ))?)
}

impl IntoResponseParts for Caching {
    type Error = Infallible;

    fn into_response_parts(self, mut parts: ResponseParts) -> Result<ResponseParts, Self::Error> {
        let headers = parts.headers_mut();
        headers.insert(CACHE_CONTROL, self.cache_control);
        if let Some(etag) = self.etag {
            headers.insert(ETAG, etag);
        }
        Ok(parts)
    }
}

pub(super) struct IfNoneMatch(Vec<String>);

impl IfNoneMatch {
    pub(super) fn matches(&self, etag: &str) -> bool {
        self.0
            .iter()
            .flat_map(|value| value.split(TAG_SEPARATOR))
            .map(str::trim)
            .any(|tag| tag == ANY_TAG || tag.strip_prefix(WEAK_PREFIX).unwrap_or(tag) == etag)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for IfNoneMatch {
    type Rejection = Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let tags = parts
            .headers
            .get_all(IF_NONE_MATCH)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect();
        future::ready(Ok(IfNoneMatch(tags)))
    }
}

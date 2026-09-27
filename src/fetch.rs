use std::fmt;
use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;
use reqwest::{RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;

const MAX_STORED_TEXT_BYTES: usize = 16 * 1024;
const HEADER_SEPARATOR: &str = "\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    MpkLines = 0,
    MpkPositions = 1,
    OpenMeteo = 2,
    OpenData = 3,
}

impl Source {
    #[must_use]
    pub fn id(self) -> i64 {
        self as i64
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Source::MpkLines => "MPK line list",
            Source::MpkPositions => "MPK positions",
            Source::OpenMeteo => "open-meteo",
            Source::OpenData => "open data fleet",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Timeout = 0,
    Connect = 1,
    Request = 2,
    Status = 3,
    Body = 4,
    Invalid = 5,
    Empty = 6,
}

impl ErrorKind {
    #[must_use]
    pub fn id(self) -> i64 {
        self as i64
    }

    fn of(error: &reqwest::Error) -> Self {
        if error.is_timeout() {
            return ErrorKind::Timeout;
        }
        if error.is_connect() {
            return ErrorKind::Connect;
        }
        if error.is_body() || error.is_decode() {
            return ErrorKind::Body;
        }
        ErrorKind::Request
    }
}

#[derive(Debug)]
pub struct Attempt {
    pub source: Source,
    pub method: Option<String>,
    pub url: Option<String>,
    pub request_body: Option<String>,
    pub started_unix: i64,
    started: Instant,
}

#[derive(Debug)]
pub struct StoredBody {
    pub total_bytes: usize,
    pub text: String,
}

impl StoredBody {
    fn of(text: String) -> Self {
        Self {
            total_bytes: text.len(),
            text: truncated(text),
        }
    }
}

#[derive(Debug)]
pub struct ReceivedResponse {
    pub status: StatusCode,
    pub headers: String,
    pub body: Option<StoredBody>,
}

#[derive(Debug)]
pub struct FetchError {
    pub attempt: Box<Attempt>,
    pub elapsed: Duration,
    pub kind: ErrorKind,
    pub message: String,
    pub response: Option<Box<ReceivedResponse>>,
}

impl FetchError {
    fn transport(
        attempt: Box<Attempt>,
        error: reqwest::Error,
        response: Option<ReceivedResponse>,
    ) -> Self {
        Self {
            elapsed: attempt.started.elapsed(),
            attempt,
            kind: ErrorKind::of(&error),
            message: format!("{:#}", anyhow::Error::from(error)),
            response: response.map(Box::new),
        }
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.attempt.source.as_str(), self.message)
    }
}

impl std::error::Error for FetchError {}

pub struct Fetched {
    attempt: Box<Attempt>,
    elapsed: Duration,
    status: StatusCode,
    headers: HeaderMap,
    pub body: String,
}

impl Fetched {
    pub fn require_success(self) -> Result<Self, FetchError> {
        self.require_success_with_reason(|_| None)
    }

    pub fn require_success_with_reason(
        self,
        reason: impl FnOnce(&str) -> Option<String>,
    ) -> Result<Self, FetchError> {
        if self.status.is_success() {
            return Ok(self);
        }
        let message = match reason(&self.body) {
            Some(reason) => format!("HTTP {}: {reason}", self.status),
            None => format!("HTTP {}", self.status),
        };
        Err(self.error(ErrorKind::Status, message))
    }

    pub fn json<T: DeserializeOwned>(self) -> Result<T, FetchError> {
        let parsed = serde_json::from_str(&self.body);
        parsed.map_err(|e| self.error(ErrorKind::Invalid, format!("invalid JSON: {e}")))
    }

    pub fn error(self, kind: ErrorKind, message: impl Into<String>) -> FetchError {
        FetchError {
            attempt: self.attempt,
            elapsed: self.elapsed,
            kind,
            message: message.into(),
            response: Some(Box::new(ReceivedResponse {
                status: self.status,
                headers: format_headers(&self.headers),
                body: Some(StoredBody::of(self.body)),
            })),
        }
    }
}

pub async fn send(source: Source, request: RequestBuilder) -> Result<Fetched, FetchError> {
    let started = Instant::now();
    let started_unix = jiff::Timestamp::now().as_second();
    let (http, request) = request.build_split();
    let request = match request {
        Ok(request) => request,
        Err(e) => {
            let attempt = Box::new(Attempt {
                source,
                method: None,
                url: e.url().map(ToString::to_string),
                request_body: None,
                started_unix,
                started,
            });
            return Err(FetchError::transport(attempt, e, None));
        }
    };
    let attempt = Box::new(Attempt {
        source,
        method: Some(request.method().to_string()),
        url: Some(request.url().to_string()),
        request_body: request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .map(|bytes| truncated(String::from_utf8_lossy(bytes).into_owned())),
        started_unix,
        started,
    });
    let response = match http.execute(request).await {
        Ok(response) => response,
        Err(e) => return Err(FetchError::transport(attempt, e, None)),
    };
    let status = response.status();
    let headers = response.headers().clone();
    match response.text().await {
        Ok(body) => Ok(Fetched {
            elapsed: attempt.started.elapsed(),
            attempt,
            status,
            headers,
            body,
        }),
        Err(e) => {
            let received = ReceivedResponse {
                status,
                headers: format_headers(&headers),
                body: None,
            };
            Err(FetchError::transport(attempt, e, Some(received)))
        }
    }
}

fn format_headers(headers: &HeaderMap) -> String {
    headers
        .iter()
        .map(|(name, value)| format!("{name}: {}", String::from_utf8_lossy(value.as_bytes())))
        .collect::<Vec<_>>()
        .join(HEADER_SEPARATOR)
}

fn truncated(mut text: String) -> String {
    text.truncate(text.floor_char_boundary(MAX_STORED_TEXT_BYTES));
    text
}

#[cfg(test)]
pub mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};

    use rstest::rstest;

    use super::*;

    const LOCALHOST: &str = "127.0.0.1:0";
    const PATH: &str = "/data";
    const CONTENT_LENGTH: &str = "content-length:";
    const END_OF_HEADERS: &str = "\r\n";
    const SHORT_TIMEOUT: Duration = Duration::from_millis(200);
    const CONTENT_TYPE_HEADER: &str = "content-type: text/plain";
    const PARAMETER: (&str, &str) = ("busList[bus][]", "a");
    const ENCODED_PARAMETER: &str = "busList%5Bbus%5D%5B%5D=a";
    const SHORT_BODY: &str = "down for maintenance";
    const TWO_BYTE_CHAR: char = 'ł';
    const EMPTY_MESSAGE: &str = "no lines found";
    const INVALID_JSON_PREFIX: &str = "invalid JSON: ";

    fn read_request(stream: &TcpStream) {
        let mut reader = BufReader::new(stream);
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == END_OF_HEADERS {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix(CONTENT_LENGTH) {
                content_length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
    }

    fn serve_once(status: StatusCode, body: &str) -> String {
        let response = format!(
            "HTTP/1.1 {status}\r\n{CONTENT_TYPE_HEADER}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let listener = TcpListener::bind(LOCALHOST).unwrap();
        let url = format!("http://{}{PATH}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&stream);
            stream.write_all(response.as_bytes()).unwrap();
        });
        url
    }

    fn never_answer() -> String {
        let listener = TcpListener::bind(LOCALHOST).unwrap();
        let url = format!("http://{}{PATH}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            read_request(&stream);
            std::thread::sleep(10 * SHORT_TIMEOUT);
        });
        url
    }

    pub(crate) fn refused_url() -> String {
        let listener = TcpListener::bind(LOCALHOST).unwrap();
        format!("http://{}{PATH}", listener.local_addr().unwrap())
    }

    #[derive(Debug, Clone, Copy)]
    enum Failure {
        Status(StatusCode),
        InvalidJson,
        Empty,
    }

    impl Failure {
        fn status(self) -> StatusCode {
            match self {
                Failure::Status(status) => status,
                Failure::InvalidJson | Failure::Empty => StatusCode::OK,
            }
        }

        fn kind(self) -> ErrorKind {
            match self {
                Failure::Status(_) => ErrorKind::Status,
                Failure::InvalidJson => ErrorKind::Invalid,
                Failure::Empty => ErrorKind::Empty,
            }
        }

        fn apply(self, fetched: Fetched) -> FetchError {
            match self {
                Failure::Status(_) => fetched.require_success().err().unwrap(),
                Failure::InvalidJson => fetched
                    .require_success()
                    .unwrap()
                    .json::<Vec<i64>>()
                    .err()
                    .unwrap(),
                Failure::Empty => fetched
                    .require_success()
                    .unwrap()
                    .error(ErrorKind::Empty, EMPTY_MESSAGE),
            }
        }

        fn assert_message(self, message: &str) {
            match self {
                Failure::Status(status) => assert_eq!(message, format!("HTTP {status}")),
                Failure::InvalidJson => {
                    assert!(message.starts_with(INVALID_JSON_PREFIX), "{message}");
                }
                Failure::Empty => assert_eq!(message, EMPTY_MESSAGE),
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum RequestShape {
        GetWithQuery,
        PostWithForm,
    }

    impl RequestShape {
        fn build(self, http: &reqwest::Client, url: &str) -> RequestBuilder {
            match self {
                RequestShape::GetWithQuery => http.get(url).query(&[PARAMETER]),
                RequestShape::PostWithForm => http.post(url).form(&[PARAMETER]),
            }
        }

        fn method(self) -> &'static str {
            match self {
                RequestShape::GetWithQuery => "GET",
                RequestShape::PostWithForm => "POST",
            }
        }

        fn expected_url(self, url: &str) -> String {
            match self {
                RequestShape::GetWithQuery => format!("{url}?{ENCODED_PARAMETER}"),
                RequestShape::PostWithForm => url.to_owned(),
            }
        }

        fn expected_body(self) -> Option<&'static str> {
            match self {
                RequestShape::GetWithQuery => None,
                RequestShape::PostWithForm => Some(ENCODED_PARAMETER),
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum BodySize {
        Short,
        OverLimit,
    }

    impl BodySize {
        fn text(self) -> String {
            match self {
                BodySize::Short => SHORT_BODY.to_owned(),
                BodySize::OverLimit => TWO_BYTE_CHAR.to_string().repeat(MAX_STORED_TEXT_BYTES),
            }
        }

        fn expected_stored(self) -> String {
            match self {
                BodySize::Short => SHORT_BODY.to_owned(),
                BodySize::OverLimit => TWO_BYTE_CHAR
                    .to_string()
                    .repeat(MAX_STORED_TEXT_BYTES / TWO_BYTE_CHAR.len_utf8()),
            }
        }
    }

    #[rstest]
    #[case::bad_request(Failure::Status(StatusCode::BAD_REQUEST))]
    #[case::not_found(Failure::Status(StatusCode::NOT_FOUND))]
    #[case::too_many_requests(Failure::Status(StatusCode::TOO_MANY_REQUESTS))]
    #[case::internal_server_error(Failure::Status(StatusCode::INTERNAL_SERVER_ERROR))]
    #[case::service_unavailable(Failure::Status(StatusCode::SERVICE_UNAVAILABLE))]
    #[case::invalid_json(Failure::InvalidJson)]
    #[case::empty(Failure::Empty)]
    #[tokio::test]
    async fn keeps_request_and_response_of_a_failed_fetch(
        #[case] failure: Failure,
        #[values(
            Source::MpkLines,
            Source::MpkPositions,
            Source::OpenMeteo,
            Source::OpenData
        )]
        source: Source,
        #[values(RequestShape::GetWithQuery, RequestShape::PostWithForm)] shape: RequestShape,
        #[values(BodySize::Short, BodySize::OverLimit)] body_size: BodySize,
    ) {
        let body = body_size.text();
        let url = serve_once(failure.status(), &body);
        let fetched = send(source, shape.build(&reqwest::Client::new(), &url))
            .await
            .unwrap();
        let error = failure.apply(fetched);

        assert_eq!(error.kind, failure.kind());
        failure.assert_message(&error.message);
        assert_eq!(
            error.to_string(),
            format!("{}: {}", source.as_str(), error.message)
        );
        assert_eq!(error.attempt.source, source);
        assert_eq!(error.attempt.method.as_deref(), Some(shape.method()));
        assert_eq!(
            error.attempt.url.as_deref(),
            Some(shape.expected_url(&url).as_str())
        );
        assert_eq!(error.attempt.request_body.as_deref(), shape.expected_body());
        let response = error.response.unwrap();
        assert_eq!(response.status, failure.status());
        assert!(response.headers.contains(CONTENT_TYPE_HEADER));
        let stored = response.body.unwrap();
        assert_eq!(stored.text, body_size.expected_stored());
        assert_eq!(stored.total_bytes, body.len());
    }

    #[tokio::test]
    async fn adds_the_reason_to_a_failed_status() {
        let url = serve_once(StatusCode::BAD_REQUEST, "too many points");
        let error = send(Source::OpenMeteo, reqwest::Client::new().get(&url))
            .await
            .unwrap()
            .require_success_with_reason(|body| Some(body.to_owned()))
            .err()
            .unwrap();
        assert_eq!(error.message, "HTTP 400 Bad Request: too many points");
        assert_eq!(
            error.to_string(),
            "open-meteo: HTTP 400 Bad Request: too many points"
        );
    }

    #[tokio::test]
    async fn reports_refused_connection() {
        let url = refused_url();
        let error = send(Source::MpkLines, reqwest::Client::new().get(&url))
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::Connect);
        assert_eq!(error.attempt.url.as_deref(), Some(url.as_str()));
        assert!(error.response.is_none());
        assert!(error.message.contains(&url), "{}", error.message);
    }

    #[tokio::test]
    async fn reports_timeout() {
        let url = never_answer();
        let http = reqwest::Client::builder()
            .timeout(SHORT_TIMEOUT)
            .build()
            .unwrap();
        let error = send(Source::OpenMeteo, http.get(&url)).await.err().unwrap();
        assert_eq!(error.kind, ErrorKind::Timeout);
        assert!(error.elapsed >= SHORT_TIMEOUT);
        assert!(error.response.is_none());
    }

    #[test]
    fn truncates_long_text_on_a_character_boundary() {
        let text = TWO_BYTE_CHAR.to_string().repeat(MAX_STORED_TEXT_BYTES);
        let body = StoredBody::of(text);
        assert_eq!(body.total_bytes, 2 * MAX_STORED_TEXT_BYTES);
        assert_eq!(body.text.len(), MAX_STORED_TEXT_BYTES);
        assert!(body.text.chars().all(|c| c == TWO_BYTE_CHAR));

        let shifted = format!(
            "x{}",
            TWO_BYTE_CHAR.to_string().repeat(MAX_STORED_TEXT_BYTES)
        );
        let body = StoredBody::of(shifted);
        assert_eq!(body.text.len(), MAX_STORED_TEXT_BYTES - 1);
    }

    #[test]
    fn keeps_short_text() {
        assert_eq!(StoredBody::of("ok".to_owned()).text, "ok");
    }
}

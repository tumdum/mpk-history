CREATE TABLE fetch_errors (
    id INTEGER PRIMARY KEY,
    source INTEGER NOT NULL,
    kind INTEGER NOT NULL,
    started_unix INTEGER NOT NULL,
    elapsed_ms INTEGER NOT NULL,
    method TEXT,
    url TEXT,
    request_body TEXT,
    http_status INTEGER,
    response_headers TEXT,
    response_bytes INTEGER,
    response_body TEXT,
    message TEXT NOT NULL
);

CREATE INDEX fetch_errors_source_started ON fetch_errors (source, started_unix);

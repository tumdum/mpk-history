CREATE TABLE snapshots (
    id INTEGER PRIMARY KEY,
    fetched_at_ms INTEGER NOT NULL
);

CREATE INDEX snapshots_fetched_at_ms ON snapshots (fetched_at_ms);

CREATE TABLE positions (
    snapshot_id INTEGER NOT NULL REFERENCES snapshots (id),
    vehicle_type TEXT NOT NULL,
    line TEXT NOT NULL,
    course_id INTEGER NOT NULL,
    latitude REAL NOT NULL,
    longitude REAL NOT NULL
);

CREATE INDEX positions_snapshot_id ON positions (snapshot_id);
CREATE INDEX positions_line ON positions (line, snapshot_id);
CREATE INDEX positions_course_id ON positions (course_id);

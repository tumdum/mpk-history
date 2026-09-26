CREATE TABLE vehicle_positions (
    id INTEGER PRIMARY KEY,
    vehicle_type TEXT NOT NULL,
    line TEXT NOT NULL,
    course_id INTEGER NOT NULL,
    latitude REAL NOT NULL,
    longitude REAL NOT NULL,
    first_seen_ms INTEGER NOT NULL,
    last_seen_ms INTEGER NOT NULL
);

INSERT INTO vehicle_positions (vehicle_type, line, course_id, latitude, longitude, first_seen_ms, last_seen_ms)
SELECT p.vehicle_type, p.line, p.course_id, p.latitude, p.longitude, s.fetched_at_ms, s.fetched_at_ms
FROM positions p
JOIN snapshots s ON s.id = p.snapshot_id
ORDER BY s.fetched_at_ms;

DROP TABLE positions;
DROP TABLE snapshots;

CREATE INDEX vehicle_positions_line_first_seen ON vehicle_positions (line, first_seen_ms);

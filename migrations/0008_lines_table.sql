CREATE TABLE lines (
    id INTEGER PRIMARY KEY,
    vehicle_type INTEGER NOT NULL,
    name TEXT NOT NULL,
    UNIQUE (vehicle_type, name)
);

INSERT INTO lines (vehicle_type, name)
SELECT DISTINCT vehicle_type, line
FROM vehicle_positions
ORDER BY vehicle_type, line;

CREATE TABLE vehicle_positions_new (
    id INTEGER PRIMARY KEY,
    line_id INTEGER NOT NULL REFERENCES lines (id),
    course_id INTEGER NOT NULL,
    latitude_e6 INTEGER NOT NULL,
    longitude_e6 INTEGER NOT NULL,
    first_seen_unix INTEGER NOT NULL,
    seen_for_secs INTEGER NOT NULL
);

INSERT INTO vehicle_positions_new (id, line_id, course_id, latitude_e6, longitude_e6, first_seen_unix, seen_for_secs)
SELECT p.id, l.id, p.course_id, p.latitude_e6, p.longitude_e6, p.first_seen_unix, p.seen_for_secs
FROM vehicle_positions p
JOIN lines l ON l.vehicle_type = p.vehicle_type AND l.name = p.line
ORDER BY p.id;

DROP TABLE vehicle_positions;
ALTER TABLE vehicle_positions_new RENAME TO vehicle_positions;

CREATE INDEX vehicle_positions_line_first_seen ON vehicle_positions (line_id, first_seen_unix);

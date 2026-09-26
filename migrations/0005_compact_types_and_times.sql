CREATE TABLE vehicle_positions_new (
    id INTEGER PRIMARY KEY,
    vehicle_type INTEGER NOT NULL,
    line TEXT NOT NULL,
    course_id INTEGER NOT NULL,
    latitude_e6 INTEGER NOT NULL,
    longitude_e6 INTEGER NOT NULL,
    first_seen_unix INTEGER NOT NULL,
    seen_for_secs INTEGER NOT NULL
);

INSERT INTO vehicle_positions_new (id, vehicle_type, line, course_id, latitude_e6, longitude_e6, first_seen_unix, seen_for_secs)
SELECT id,
       CASE vehicle_type WHEN 'bus' THEN 0 WHEN 'tram' THEN 1 WHEN 'train' THEN 2 END,
       line, course_id, latitude_e6, longitude_e6,
       first_seen_ms / 1000,
       last_seen_ms / 1000 - first_seen_ms / 1000
FROM vehicle_positions
ORDER BY id;

DROP TABLE vehicle_positions;
ALTER TABLE vehicle_positions_new RENAME TO vehicle_positions;

CREATE INDEX vehicle_positions_line_first_seen ON vehicle_positions (line, first_seen_unix);

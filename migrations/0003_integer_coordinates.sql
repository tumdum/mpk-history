CREATE TABLE vehicle_positions_new (
    id INTEGER PRIMARY KEY,
    vehicle_type TEXT NOT NULL,
    line TEXT NOT NULL,
    course_id INTEGER NOT NULL,
    latitude_e6 INTEGER NOT NULL,
    longitude_e6 INTEGER NOT NULL,
    first_seen_ms INTEGER NOT NULL,
    last_seen_ms INTEGER NOT NULL
);

INSERT INTO vehicle_positions_new (id, vehicle_type, line, course_id, latitude_e6, longitude_e6, first_seen_ms, last_seen_ms)
SELECT id, vehicle_type, line, course_id,
       CAST(ROUND(latitude * 1000000) AS INTEGER),
       CAST(ROUND(longitude * 1000000) AS INTEGER),
       first_seen_ms, last_seen_ms
FROM vehicle_positions
ORDER BY id;

DROP TABLE vehicle_positions;
ALTER TABLE vehicle_positions_new RENAME TO vehicle_positions;

CREATE INDEX vehicle_positions_line_first_seen ON vehicle_positions (line, first_seen_ms);

CREATE TABLE course_vehicles (
    course_id INTEGER NOT NULL,
    side_number INTEGER NOT NULL,
    plate TEXT,
    brigade TEXT,
    observed_unix INTEGER NOT NULL,
    distance_m REAL NOT NULL,
    PRIMARY KEY (course_id, side_number)
);

CREATE INDEX course_vehicles_side_number ON course_vehicles (side_number);

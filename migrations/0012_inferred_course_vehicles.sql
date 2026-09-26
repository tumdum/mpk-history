ALTER TABLE course_vehicles ADD COLUMN source INTEGER NOT NULL DEFAULT 0;
ALTER TABLE course_vehicles ADD COLUMN inferred_from_course_id INTEGER;

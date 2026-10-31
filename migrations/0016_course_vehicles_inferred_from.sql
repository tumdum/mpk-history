CREATE INDEX course_vehicles_inferred_from
ON course_vehicles (inferred_from_course_id)
WHERE inferred_from_course_id IS NOT NULL;

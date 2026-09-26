ALTER TABLE course_vehicles ADD COLUMN line_id INTEGER REFERENCES lines (id);

UPDATE course_vehicles
SET line_id = p.line_id
FROM (
    SELECT course_id, MIN(line_id) AS line_id
    FROM vehicle_positions
    WHERE course_id IN (SELECT course_id FROM course_vehicles)
    GROUP BY course_id
) AS p
WHERE p.course_id = course_vehicles.course_id;

UPDATE course_vehicles
SET plate = upper(replace(plate, ' ', ''))
WHERE plate IS NOT NULL;

CREATE INDEX course_vehicles_plate ON course_vehicles (plate);

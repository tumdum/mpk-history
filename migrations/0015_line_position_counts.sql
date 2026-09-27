CREATE TABLE line_position_counts (
    line_id INTEGER NOT NULL REFERENCES lines (id),
    minute_unix INTEGER NOT NULL,
    received INTEGER NOT NULL,
    invalid INTEGER NOT NULL,
    PRIMARY KEY (line_id, minute_unix)
) WITHOUT ROWID;

CREATE TABLE fetch_attempts (
    source INTEGER NOT NULL,
    minute_unix INTEGER NOT NULL,
    attempts INTEGER NOT NULL,
    failures INTEGER NOT NULL,
    PRIMARY KEY (source, minute_unix)
) WITHOUT ROWID;

CREATE TABLE position_counts (
    vehicle_type INTEGER NOT NULL,
    minute_unix INTEGER NOT NULL,
    received INTEGER NOT NULL,
    invalid INTEGER NOT NULL,
    PRIMARY KEY (vehicle_type, minute_unix)
) WITHOUT ROWID;

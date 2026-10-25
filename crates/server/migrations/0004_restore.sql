CREATE TABLE IF NOT EXISTS ff_restore (
    singleton SMALLINT PRIMARY KEY CHECK (singleton=1),
    fingerprint TEXT NOT NULL,
    completed BOOLEAN NOT NULL
);

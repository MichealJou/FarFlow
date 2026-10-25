CREATE TABLE IF NOT EXISTS ff_storage (
    singleton SMALLINT PRIMARY KEY CHECK (singleton=1),
    id UUID NOT NULL UNIQUE,
    path_hash TEXT NOT NULL,
    ready BOOLEAN NOT NULL
);

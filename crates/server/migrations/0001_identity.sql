CREATE TABLE IF NOT EXISTS ff_accounts (
    id UUID PRIMARY KEY,
    account TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    department TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    active BOOLEAN NOT NULL,
    is_admin BOOLEAN NOT NULL,
    must_change_password BOOLEAN NOT NULL,
    version BIGINT NOT NULL CHECK (version > 0)
);

CREATE TABLE IF NOT EXISTS ff_sessions (
    token_hash TEXT PRIMARY KEY,
    account_id UUID NOT NULL REFERENCES ff_accounts(id),
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL CHECK (expires_at > created_at)
);
CREATE INDEX IF NOT EXISTS ff_sessions_account ON ff_sessions(account_id);
CREATE INDEX IF NOT EXISTS ff_sessions_expiry ON ff_sessions(expires_at);

CREATE TABLE IF NOT EXISTS ff_login_attempts (
    account TEXT PRIMARY KEY,
    failures INTEGER NOT NULL CHECK (failures >= 0),
    window_start BIGINT NOT NULL,
    blocked_until BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS ff_login_attempts_age ON ff_login_attempts(window_start);

CREATE TABLE IF NOT EXISTS ff_account_events (
    sequence BIGSERIAL PRIMARY KEY,
    account_id UUID NOT NULL REFERENCES ff_accounts(id),
    actor_id UUID NOT NULL REFERENCES ff_accounts(id),
    action TEXT NOT NULL,
    at BIGINT NOT NULL,
    data TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS ff_account_events_target ON ff_account_events(account_id, sequence);

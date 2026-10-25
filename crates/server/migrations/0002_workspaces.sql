-- 核心指纹使用原始 JSON 编码；JSONB 会规范化负零，不能作为这些载荷的存储类型。
CREATE TABLE IF NOT EXISTS ff_workspaces (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    creator_id UUID NOT NULL REFERENCES ff_accounts(id),
    revision BIGINT NOT NULL CHECK (revision >= 0),
    genesis TEXT NOT NULL,
    state TEXT NOT NULL,
    checksum TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ff_workspace_members (
    workspace_id UUID NOT NULL REFERENCES ff_workspaces(id),
    user_id UUID NOT NULL REFERENCES ff_accounts(id),
    active BOOLEAN NOT NULL,
    owner BOOLEAN NOT NULL,
    PRIMARY KEY (workspace_id, user_id)
);
CREATE INDEX IF NOT EXISTS ff_workspace_members_user ON ff_workspace_members(user_id, active);

CREATE TABLE IF NOT EXISTS ff_workspace_operations (
    workspace_id UUID NOT NULL REFERENCES ff_workspaces(id),
    id UUID NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0),
    client TEXT NOT NULL,
    request TEXT NOT NULL,
    receipt TEXT NOT NULL,
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, revision)
);

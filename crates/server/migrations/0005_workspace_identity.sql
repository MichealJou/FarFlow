CREATE TABLE IF NOT EXISTS ff_workspace_identities (
    workspace_id UUID NOT NULL REFERENCES ff_workspaces(id),
    member_id UUID NOT NULL,
    account_id UUID NOT NULL REFERENCES ff_accounts(id),
    PRIMARY KEY (workspace_id, member_id),
    UNIQUE (workspace_id, account_id)
);
CREATE TABLE IF NOT EXISTS ff_workspace_origins (
    workspace_id UUID PRIMARY KEY REFERENCES ff_workspaces(id),
    data TEXT NOT NULL,
    checksum TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS ff_workspace_origin_operations (
    workspace_id UUID NOT NULL REFERENCES ff_workspaces(id),
    id UUID NOT NULL,
    request TEXT,
    client TEXT,
    receipt TEXT,
    PRIMARY KEY (workspace_id,id)
);
INSERT INTO ff_workspace_identities
SELECT m.workspace_id,m.user_id,m.user_id FROM ff_workspace_members m
WHERE NOT EXISTS(SELECT 1 FROM ff_workspace_origins o WHERE o.workspace_id=m.workspace_id)
AND NOT EXISTS(SELECT 1 FROM ff_workspace_identities i WHERE i.workspace_id=m.workspace_id AND i.member_id=m.user_id)
ON CONFLICT (workspace_id,member_id) DO NOTHING;

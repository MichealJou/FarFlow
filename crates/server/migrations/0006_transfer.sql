CREATE TABLE IF NOT EXISTS ff_workspace_transfer (
    workspace_id UUID PRIMARY KEY REFERENCES ff_workspaces(id),
    data TEXT NOT NULL,
    checksum TEXT NOT NULL,
    writable BOOLEAN NOT NULL
);
CREATE TABLE IF NOT EXISTS ff_workspace_transfer_events (
    sequence BIGSERIAL PRIMARY KEY,
    workspace_id UUID NOT NULL REFERENCES ff_workspaces(id),
    account_id UUID NOT NULL REFERENCES ff_accounts(id),
    data TEXT NOT NULL,
    checksum TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS ff_transfer_events_workspace ON ff_workspace_transfer_events(workspace_id,sequence);

DO $farflow_install$
BEGIN
    IF to_regprocedure('ff_transfer_guard()') IS NULL THEN
        EXECUTE $farflow_definition$
CREATE FUNCTION ff_transfer_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $farflow_guard$
DECLARE
    previous_id UUID;
    current_id UUID;
    workspace UUID;
BEGIN
    IF TG_TABLE_NAME = 'ff_workspaces' THEN
        IF TG_OP <> 'INSERT' THEN previous_id := OLD.id; END IF;
        IF TG_OP <> 'DELETE' THEN current_id := NEW.id; END IF;
    ELSE
        IF TG_OP <> 'INSERT' THEN previous_id := OLD.workspace_id; END IF;
        IF TG_OP <> 'DELETE' THEN current_id := NEW.workspace_id; END IF;
    END IF;
    FOR workspace IN SELECT DISTINCT value FROM unnest(ARRAY[previous_id,current_id]) AS value WHERE value IS NOT NULL LOOP
        IF TG_TABLE_NAME <> 'ff_workspaces' THEN
            PERFORM id FROM ff_workspaces WHERE id=workspace FOR KEY SHARE;
        END IF;
        IF EXISTS(SELECT 1 FROM ff_workspace_transfer WHERE workspace_id=workspace AND NOT writable)
            OR (NOT EXISTS(SELECT 1 FROM ff_workspace_transfer WHERE workspace_id=workspace)
                AND EXISTS(SELECT 1 FROM ff_workspace_transfer_events WHERE workspace_id=workspace)) THEN
            RAISE EXCEPTION '工作空间正在迁移或已交接，当前只读' USING ERRCODE='55000';
        END IF;
    END LOOP;
    IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END;
$farflow_guard$;
        $farflow_definition$;
    END IF;
END;
$farflow_install$;

DO $farflow_triggers$
DECLARE
    relation TEXT;
BEGIN
    FOREACH relation IN ARRAY ARRAY['ff_workspaces','ff_workspace_operations','ff_workspace_members','ff_workspace_identities'] LOOP
        IF NOT EXISTS(SELECT 1 FROM pg_trigger WHERE tgrelid=to_regclass(relation) AND tgname='ff_transfer_fence') THEN
            EXECUTE format('CREATE TRIGGER ff_transfer_fence BEFORE INSERT OR UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION ff_transfer_guard()',relation);
        END IF;
    END LOOP;
END;
$farflow_triggers$;

-- Company-level settings, visible and editable by the company's assigned users and AR admins.
-- One row per company and setting key; value is JSON. First key: account_owners (array of names).

CREATE TABLE company_settings (
    company_id  BIGINT NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    key         TEXT NOT NULL,
    value       JSONB NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by  TEXT,
    PRIMARY KEY (company_id, key)
);

-- Permanent record of every settings change (append-only, like the other history tables).
CREATE TABLE company_settings_log (
    id          BIGSERIAL PRIMARY KEY,
    company_id  BIGINT NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    key         TEXT NOT NULL,
    old_value   JSONB,
    new_value   JSONB,
    changed_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    changed_by  TEXT NOT NULL
);
CREATE INDEX company_settings_log_company ON company_settings_log(company_id, id);

CREATE FUNCTION company_settings_history() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND OLD.value IS NOT DISTINCT FROM NEW.value THEN RETURN NULL; END IF;
    INSERT INTO company_settings_log (company_id, key, old_value, new_value, changed_by)
    VALUES (COALESCE(NEW.company_id, OLD.company_id), COALESCE(NEW.key, OLD.key),
            CASE WHEN TG_OP = 'INSERT' THEN NULL ELSE OLD.value END,
            CASE WHEN TG_OP = 'DELETE' THEN NULL ELSE NEW.value END, ar_actor());
    RETURN NULL;
END $$;
CREATE TRIGGER company_settings_history AFTER INSERT OR UPDATE OR DELETE ON company_settings
    FOR EACH ROW EXECUTE FUNCTION company_settings_history();

CREATE TRIGGER company_settings_log_append_only BEFORE UPDATE OR DELETE ON company_settings_log
    FOR EACH ROW EXECUTE FUNCTION refuse_change();
CREATE TRIGGER company_settings_log_no_truncate BEFORE TRUNCATE ON company_settings_log
    FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();

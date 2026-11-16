-- AR Tracker schema (PostgreSQL). Runs inside the app's own schema (default "ar"),
-- which the app creates and puts first on the search_path before migrating.

-- Who is making the current change. The app sets it per transaction with
--   SELECT set_config('ar.user', '<user>', true)
-- and the history triggers below record it.
CREATE FUNCTION ar_actor() RETURNS text LANGUAGE sql STABLE AS $$
    SELECT COALESCE(NULLIF(current_setting('ar.user', true), ''), current_user::text)
$$;

CREATE TABLE companies (
    id          BIGSERIAL PRIMARY KEY,
    code        TEXT NOT NULL UNIQUE,              -- e.g. "1001" from "1001--Example Co"
    name        TEXT NOT NULL,
    legal_name  TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Master list of global attributes. options (select only): JSON text [{"label": "...", "color": "#rrggbb"}]
CREATE TABLE attributes (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    kind        TEXT NOT NULL DEFAULT 'text' CHECK (kind IN ('text','longtext','date','number','money','select')),
    options     TEXT NOT NULL DEFAULT '[]',
    sort        INTEGER NOT NULL DEFAULT 100,
    default_on  BOOLEAN NOT NULL DEFAULT false,    -- enabled automatically for newly created companies
    archived    BOOLEAN NOT NULL DEFAULT false
);

CREATE TABLE company_attributes (
    company_id   BIGINT NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    attribute_id BIGINT NOT NULL REFERENCES attributes(id) ON DELETE CASCADE,
    enabled      BOOLEAN NOT NULL DEFAULT true,
    PRIMARY KEY (company_id, attribute_id)
);

-- status: active (on the latest report) | archived (dropped off = paid off / cleared)
CREATE TABLE customers (
    id          BIGSERIAL PRIMARY KEY,
    company_id  BIGINT NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    ext_id      TEXT NOT NULL,                     -- accounting system Customer ID, e.g. PD_5029963
    name        TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','archived')),
    first_seen  DATE NOT NULL,
    last_seen   DATE NOT NULL,
    archived_on DATE,
    follow_up   DATE,
    UNIQUE (company_id, ext_id)
);
CREATE INDEX customers_ext ON customers(ext_id);
CREATE INDEX customers_company ON customers(company_id, status);

CREATE TABLE imports (
    id           BIGSERIAL PRIMARY KEY,
    company_id   BIGINT NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    as_of        DATE NOT NULL,
    filename     TEXT NOT NULL,
    imported_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    imported_by  TEXT,
    line_count   INTEGER NOT NULL DEFAULT 0,
    total        NUMERIC(14,2) NOT NULL DEFAULT 0,
    added        INTEGER NOT NULL DEFAULT 0,
    archived     INTEGER NOT NULL DEFAULT 0,
    reactivated  INTEGER NOT NULL DEFAULT 0,
    UNIQUE (company_id, as_of)
);

-- Per-customer aging for one weekly snapshot, in the standard display buckets.
CREATE TABLE balances (
    import_id   BIGINT NOT NULL REFERENCES imports(id) ON DELETE CASCADE,
    customer_id BIGINT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    b0   NUMERIC(14,2) NOT NULL DEFAULT 0,   -- -0 (current)
    b30  NUMERIC(14,2) NOT NULL DEFAULT 0,   -- 1-30
    b60  NUMERIC(14,2) NOT NULL DEFAULT 0,   -- 31-60
    b90  NUMERIC(14,2) NOT NULL DEFAULT 0,   -- 61-90
    b182 NUMERIC(14,2) NOT NULL DEFAULT 0,   -- 91-182
    b365 NUMERIC(14,2) NOT NULL DEFAULT 0,   -- 183-365
    b366 NUMERIC(14,2) NOT NULL DEFAULT 0,   -- 366-
    total NUMERIC(14,2) NOT NULL DEFAULT 0,
    PRIMARY KEY (import_id, customer_id)
);
CREATE INDEX balances_customer ON balances(customer_id);

-- Raw report lines, kept for drill-down.
CREATE TABLE lines (
    id            BIGSERIAL PRIMARY KEY,
    import_id     BIGINT NOT NULL REFERENCES imports(id) ON DELETE CASCADE,
    customer_id   BIGINT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    posting_date  DATE,
    source_bucket TEXT NOT NULL,
    bucket        TEXT NOT NULL,
    amount        NUMERIC(14,2) NOT NULL
);
CREATE INDEX lines_customer ON lines(customer_id, import_id);

CREATE TABLE attr_values (
    customer_id  BIGINT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    attribute_id BIGINT NOT NULL REFERENCES attributes(id) ON DELETE CASCADE,
    value        TEXT NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by   TEXT,
    PRIMARY KEY (customer_id, attribute_id)
);

-- Activity log. Entries are never physically deleted: "delete" sets deleted_at.
CREATE TABLE activities (
    id          BIGSERIAL PRIMARY KEY,
    customer_id BIGINT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    entry_date  DATE NOT NULL,
    kind        TEXT NOT NULL DEFAULT 'Note',
    body        TEXT NOT NULL,
    author      TEXT,
    source      TEXT NOT NULL DEFAULT 'app',       -- app | import
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    edited_at   TIMESTAMPTZ,
    deleted_at  TIMESTAMPTZ,
    deleted_by  TEXT
);
CREATE INDEX activities_customer ON activities(customer_id, entry_date);

-- ---------------------------------------------------------------- history (append-only)

-- Every version of every activity entry: created, edited, deleted, restored.
CREATE TABLE activity_versions (
    id          BIGSERIAL PRIMARY KEY,
    activity_id BIGINT NOT NULL REFERENCES activities(id) ON DELETE CASCADE,
    change      TEXT NOT NULL CHECK (change IN ('created','edited','deleted','restored')),
    entry_date  DATE,
    kind        TEXT,
    body        TEXT,
    changed_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    changed_by  TEXT NOT NULL
);
CREATE INDEX activity_versions_activity ON activity_versions(activity_id, id);

-- Every change to a customer's attribute values, follow-up date and open/archived status.
CREATE TABLE change_log (
    id           BIGSERIAL PRIMARY KEY,
    customer_id  BIGINT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    field        TEXT NOT NULL,                    -- attribute | follow_up | status
    attribute_id BIGINT REFERENCES attributes(id) ON DELETE SET NULL,
    old_value    TEXT,
    new_value    TEXT,
    changed_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    changed_by   TEXT NOT NULL
);
CREATE INDEX change_log_customer ON change_log(customer_id, changed_at);

CREATE FUNCTION activities_history() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
DECLARE ch text;
BEGIN
    IF TG_OP = 'INSERT' THEN ch := 'created';
    ELSIF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN ch := 'deleted';
    ELSIF OLD.deleted_at IS NOT NULL AND NEW.deleted_at IS NULL THEN ch := 'restored';
    ELSIF (OLD.body, OLD.kind, OLD.entry_date) IS DISTINCT FROM (NEW.body, NEW.kind, NEW.entry_date) THEN ch := 'edited';
    ELSE RETURN NULL;
    END IF;
    INSERT INTO activity_versions (activity_id, change, entry_date, kind, body, changed_by)
    VALUES (NEW.id, ch, NEW.entry_date, NEW.kind, NEW.body, ar_actor());
    RETURN NULL;
END $$;
CREATE TRIGGER activities_history AFTER INSERT OR UPDATE ON activities
    FOR EACH ROW EXECUTE FUNCTION activities_history();

CREATE FUNCTION attr_values_history() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND OLD.value IS NOT DISTINCT FROM NEW.value THEN RETURN NULL; END IF;
    INSERT INTO change_log (customer_id, field, attribute_id, old_value, new_value, changed_by)
    VALUES (COALESCE(NEW.customer_id, OLD.customer_id), 'attribute', COALESCE(NEW.attribute_id, OLD.attribute_id),
            CASE WHEN TG_OP = 'INSERT' THEN NULL ELSE OLD.value END,
            CASE WHEN TG_OP = 'DELETE' THEN NULL ELSE NEW.value END, ar_actor());
    RETURN NULL;
END $$;
CREATE TRIGGER attr_values_history AFTER INSERT OR UPDATE OR DELETE ON attr_values
    FOR EACH ROW EXECUTE FUNCTION attr_values_history();

CREATE FUNCTION customers_history() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
    IF OLD.follow_up IS DISTINCT FROM NEW.follow_up THEN
        INSERT INTO change_log (customer_id, field, old_value, new_value, changed_by)
        VALUES (NEW.id, 'follow_up', OLD.follow_up::text, NEW.follow_up::text, ar_actor());
    END IF;
    IF OLD.status IS DISTINCT FROM NEW.status THEN
        INSERT INTO change_log (customer_id, field, old_value, new_value, changed_by)
        VALUES (NEW.id, 'status', OLD.status, NEW.status, ar_actor());
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER customers_history AFTER UPDATE ON customers
    FOR EACH ROW EXECUTE FUNCTION customers_history();

-- Activity entries can only be marked deleted, and history rows can never be changed.
CREATE FUNCTION refuse_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% rows cannot be %d (history is permanent)', TG_TABLE_NAME, lower(TG_OP);
END $$;
CREATE TRIGGER activities_no_delete BEFORE DELETE ON activities
    FOR EACH ROW EXECUTE FUNCTION refuse_change();
CREATE TRIGGER activity_versions_append_only BEFORE UPDATE OR DELETE ON activity_versions
    FOR EACH ROW EXECUTE FUNCTION refuse_change();
CREATE TRIGGER change_log_append_only BEFORE UPDATE OR DELETE ON change_log
    FOR EACH ROW EXECUTE FUNCTION refuse_change();
-- TRUNCATE skips row triggers, so block it separately.
CREATE TRIGGER activities_no_truncate BEFORE TRUNCATE ON activities
    FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER activity_versions_no_truncate BEFORE TRUNCATE ON activity_versions
    FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER change_log_no_truncate BEFORE TRUNCATE ON change_log
    FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();

-- ---------------------------------------------------------------- users & company access

CREATE TABLE users (
    username     TEXT PRIMARY KEY,                 -- sign-in user name (identity provider username)
    email        TEXT,
    roles        TEXT NOT NULL DEFAULT '',         -- roles seen at last sign-in, comma separated (display only)
    first_seen   TIMESTAMPTZ,                      -- NULL until they first sign in (added ahead by an admin)
    last_seen    TIMESTAMPTZ,
    added_by     TEXT
);
CREATE TABLE user_companies (
    username    TEXT NOT NULL REFERENCES users(username) ON DELETE CASCADE,
    company_id  BIGINT NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    granted_by  TEXT,
    granted_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (username, company_id)
);
CREATE TABLE access_log (
    id          BIGSERIAL PRIMARY KEY,
    at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    actor       TEXT NOT NULL,
    username    TEXT NOT NULL,
    change      TEXT NOT NULL
);
CREATE TRIGGER access_log_append_only BEFORE UPDATE OR DELETE ON access_log
    FOR EACH ROW EXECUTE FUNCTION refuse_change();
CREATE TRIGGER access_log_no_truncate BEFORE TRUNCATE ON access_log
    FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();

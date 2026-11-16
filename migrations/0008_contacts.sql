-- Contacts for each customer: one AP Contact (is_ap) plus any number of additional contacts with
-- a free-text title. Every contact can have a preferred contact method.
CREATE TABLE customer_contacts (
    id               BIGSERIAL PRIMARY KEY,
    customer_id      BIGINT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    is_ap            BOOLEAN NOT NULL DEFAULT false,   -- the AP Contact
    title            TEXT NOT NULL DEFAULT '',         -- "AP Contact" for the AP contact; free text otherwise
    name             TEXT NOT NULL DEFAULT '',
    phone            TEXT NOT NULL DEFAULT '',
    email            TEXT NOT NULL DEFAULT '',
    preferred_method TEXT NOT NULL DEFAULT '',         -- Phone | Email | Text | Mail | Customer portal (validated by the app)
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by       TEXT
);
CREATE INDEX customer_contacts_customer ON customer_contacts(customer_id);
CREATE UNIQUE INDEX customer_contacts_one_ap ON customer_contacts(customer_id) WHERE is_ap;

-- Every contact change (add, edit, remove) goes in the customer's permanent change log.
CREATE FUNCTION contacts_history() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
DECLARE
    old_s text;
    new_s text;
BEGIN
    IF TG_OP <> 'INSERT' THEN
        old_s := concat_ws(' · ', NULLIF(OLD.title, ''), NULLIF(OLD.name, ''), NULLIF(OLD.phone, ''), NULLIF(OLD.email, ''),
                           'prefers ' || NULLIF(OLD.preferred_method, ''));
    END IF;
    IF TG_OP <> 'DELETE' THEN
        new_s := concat_ws(' · ', NULLIF(NEW.title, ''), NULLIF(NEW.name, ''), NULLIF(NEW.phone, ''), NULLIF(NEW.email, ''),
                           'prefers ' || NULLIF(NEW.preferred_method, ''));
    END IF;
    IF TG_OP = 'UPDATE' AND old_s IS NOT DISTINCT FROM new_s THEN RETURN NULL; END IF;
    INSERT INTO change_log (customer_id, field, old_value, new_value, changed_by)
    VALUES (COALESCE(NEW.customer_id, OLD.customer_id), 'contact', old_s, new_s, ar_actor());
    RETURN NULL;
END $$;
CREATE TRIGGER contacts_history AFTER INSERT OR UPDATE OR DELETE ON customer_contacts
    FOR EACH ROW EXECUTE FUNCTION contacts_history();

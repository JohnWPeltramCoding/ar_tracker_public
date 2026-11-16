-- Promises to pay. Setting an account's Status to "Promise to Pay" requires one (amount, form of
-- payment, expected date). One open promise per account; closing it restores the earlier Status.
CREATE TABLE promises (
    id              BIGSERIAL PRIMARY KEY,
    customer_id     BIGINT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    amount          NUMERIC(14,2) NOT NULL CHECK (amount > 0),
    payment_method  TEXT NOT NULL,                     -- validated by the app (PAYMENT_METHODS)
    expected_date   DATE NOT NULL,
    notes           TEXT NOT NULL DEFAULT '',
    status          TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'kept', 'broken', 'cancelled')),
    status_before   TEXT,                              -- account Status before the promise, restored on close
    created_on      DATE NOT NULL,
    created_by      TEXT,
    closed_on       DATE,
    closed_by       TEXT,
    close_note      TEXT NOT NULL DEFAULT '',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX promises_customer ON promises(customer_id, status);
CREATE UNIQUE INDEX promises_one_open ON promises(customer_id) WHERE status = 'open';

CREATE FUNCTION promises_history() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
DECLARE
    old_s text;
    new_s text;
BEGIN
    new_s := concat_ws(' · ', '$' || to_char(NEW.amount, 'FM999,999,990.00'), NEW.payment_method,
                       'by ' || to_char(NEW.expected_date, 'MM/DD/YYYY'), NEW.status);
    IF TG_OP = 'UPDATE' THEN
        old_s := concat_ws(' · ', '$' || to_char(OLD.amount, 'FM999,999,990.00'), OLD.payment_method,
                           'by ' || to_char(OLD.expected_date, 'MM/DD/YYYY'), OLD.status);
        IF old_s IS NOT DISTINCT FROM new_s AND OLD.notes IS NOT DISTINCT FROM NEW.notes
           AND OLD.close_note IS NOT DISTINCT FROM NEW.close_note THEN
            RETURN NULL;
        END IF;
    END IF;
    INSERT INTO change_log (customer_id, field, old_value, new_value, changed_by)
    VALUES (NEW.customer_id, 'promise', old_s, new_s, ar_actor());
    RETURN NULL;
END $$;
CREATE TRIGGER promises_history AFTER INSERT OR UPDATE ON promises
    FOR EACH ROW EXECUTE FUNCTION promises_history();
CREATE TRIGGER promises_no_delete BEFORE DELETE ON promises
    FOR EACH ROW EXECUTE FUNCTION refuse_change();

-- "Promise to Pay" becomes a Status choice (Payment Plan already exists).
UPDATE attributes
SET options = (options::jsonb || '[{"label": "Promise to Pay", "color": "#d9f2e3"}]'::jsonb)::text
WHERE name = 'Status' AND kind = 'select' AND NOT (options::jsonb @> '[{"label": "Promise to Pay"}]'::jsonb);

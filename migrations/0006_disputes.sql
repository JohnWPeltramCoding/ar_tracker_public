-- Account disputes: an account can have several (e.g. different invoices); while any is open the
-- account shows as Disputed. Categories and outcomes are validated by the app (see DISPUTE_* in main.rs).
CREATE TABLE disputes (
    id           BIGSERIAL PRIMARY KEY,
    customer_id  BIGINT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    category     TEXT NOT NULL,                     -- Work not completed | Property or other damage | Change in scope | Pricing dispute
    amount       NUMERIC(14,2),                     -- disputed amount (may be part of the balance)
    reference    TEXT NOT NULL DEFAULT '',          -- invoice number or other reference
    description  TEXT NOT NULL,                     -- what the customer says is wrong
    status       TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'resolved')),
    opened_on    DATE NOT NULL DEFAULT CURRENT_DATE,
    opened_by    TEXT,
    resolved_on  DATE,
    resolved_by  TEXT,
    outcome      TEXT,
    resolution   TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX disputes_customer ON disputes(customer_id, status);

-- Every dispute change goes in the customer's permanent change log.
CREATE FUNCTION disputes_history() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
DECLARE
    old_s text;
    new_s text;
BEGIN
    new_s := concat_ws(' · ', NEW.category, CASE WHEN NEW.amount IS NULL THEN NULL ELSE '$' || to_char(NEW.amount, 'FM999,999,990.00') END,
                       NULLIF(NEW.reference, ''), NEW.status, NEW.outcome);
    IF TG_OP = 'UPDATE' THEN
        old_s := concat_ws(' · ', OLD.category, CASE WHEN OLD.amount IS NULL THEN NULL ELSE '$' || to_char(OLD.amount, 'FM999,999,990.00') END,
                           NULLIF(OLD.reference, ''), OLD.status, OLD.outcome);
        IF old_s IS NOT DISTINCT FROM new_s AND OLD.description IS NOT DISTINCT FROM NEW.description
           AND OLD.resolution IS NOT DISTINCT FROM NEW.resolution THEN
            RETURN NULL;
        END IF;
    END IF;
    INSERT INTO change_log (customer_id, field, old_value, new_value, changed_by)
    VALUES (NEW.customer_id, 'dispute', old_s, new_s, ar_actor());
    RETURN NULL;
END $$;
CREATE TRIGGER disputes_history AFTER INSERT OR UPDATE ON disputes
    FOR EACH ROW EXECUTE FUNCTION disputes_history();

-- Disputes are never deleted (resolve them instead).
CREATE TRIGGER disputes_no_delete BEFORE DELETE ON disputes
    FOR EACH ROW EXECUTE FUNCTION refuse_change();

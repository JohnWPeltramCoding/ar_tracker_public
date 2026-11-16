-- Name of the employee following up on a dispute.
ALTER TABLE disputes ADD COLUMN resolution_owner TEXT NOT NULL DEFAULT '';

-- Include the resolution owner in the change-log summary.
CREATE OR REPLACE FUNCTION disputes_history() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
DECLARE
    old_s text;
    new_s text;
BEGIN
    new_s := concat_ws(' · ', NEW.category, CASE WHEN NEW.amount IS NULL THEN NULL ELSE '$' || to_char(NEW.amount, 'FM999,999,990.00') END,
                       NULLIF(NEW.reference, ''), 'owner: ' || NULLIF(NEW.resolution_owner, ''), NEW.status, NEW.outcome);
    IF TG_OP = 'UPDATE' THEN
        old_s := concat_ws(' · ', OLD.category, CASE WHEN OLD.amount IS NULL THEN NULL ELSE '$' || to_char(OLD.amount, 'FM999,999,990.00') END,
                           NULLIF(OLD.reference, ''), 'owner: ' || NULLIF(OLD.resolution_owner, ''), OLD.status, OLD.outcome);
        IF old_s IS NOT DISTINCT FROM new_s AND OLD.description IS NOT DISTINCT FROM NEW.description
           AND OLD.resolution IS NOT DISTINCT FROM NEW.resolution THEN
            RETURN NULL;
        END IF;
    END IF;
    INSERT INTO change_log (customer_id, field, old_value, new_value, changed_by)
    VALUES (NEW.customer_id, 'dispute', old_s, new_s, ar_actor());
    RETURN NULL;
END $$;

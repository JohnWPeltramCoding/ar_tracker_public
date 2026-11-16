-- Payment plans. Setting an account's Status to "Payment Plan" requires one: the amount agreed (may be less than
-- the balance), the amount of each payment, how often (weekly, bi-weekly, monthly), the form of payment and the
-- first payment date. One open plan per account; closing it restores the earlier Status.
CREATE TABLE payment_plans (
    id                  BIGSERIAL PRIMARY KEY,
    customer_id         BIGINT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    total_amount        NUMERIC(14,2) NOT NULL CHECK (total_amount > 0),
    installment_amount  NUMERIC(14,2) NOT NULL CHECK (installment_amount > 0 AND installment_amount <= total_amount),
    frequency           TEXT NOT NULL CHECK (frequency IN ('Weekly', 'Bi-weekly', 'Monthly')),
    payment_method      TEXT NOT NULL,                     -- validated by the app (PAYMENT_METHODS)
    first_due_date      DATE NOT NULL,
    notes               TEXT NOT NULL DEFAULT '',
    status              TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'completed', 'defaulted', 'cancelled')),
    status_before       TEXT,                              -- account Status before the plan, restored on close
    created_on          DATE NOT NULL,
    created_by          TEXT,
    closed_on           DATE,
    closed_by           TEXT,
    close_note          TEXT NOT NULL DEFAULT '',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX payment_plans_customer ON payment_plans(customer_id, status);
CREATE UNIQUE INDEX payment_plans_one_open ON payment_plans(customer_id) WHERE status = 'open';

-- Due date of payment number k (0 = first payment).
CREATE FUNCTION plan_due(first date, freq text, k int) RETURNS date LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE freq WHEN 'Weekly' THEN first + 7 * k
                     WHEN 'Bi-weekly' THEN first + 14 * k
                     ELSE (first + make_interval(months => k))::date END
$$;
-- Number of payments in the plan (the last one may be smaller).
CREATE FUNCTION plan_payments(total numeric, installment numeric) RETURNS int LANGUAGE sql IMMUTABLE AS $$
    SELECT ceil(total / installment)::int
$$;
-- Next scheduled payment on or after `today` (NULL once the schedule has ended).
CREATE FUNCTION plan_next_due(first date, freq text, total numeric, installment numeric, today date)
RETURNS date LANGUAGE sql IMMUTABLE AS $$
    SELECT min(d) FROM (SELECT plan_due(first, freq, k) AS d
                        FROM generate_series(0, plan_payments(total, installment) - 1) k) s
    WHERE d >= today
$$;

CREATE FUNCTION payment_plans_history() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
DECLARE
    old_s text;
    new_s text;
BEGIN
    new_s := concat_ws(' · ', '$' || to_char(NEW.total_amount, 'FM999,999,990.00') || ' agreed',
                       '$' || to_char(NEW.installment_amount, 'FM999,999,990.00') || ' ' || NEW.frequency,
                       NEW.payment_method, 'from ' || to_char(NEW.first_due_date, 'MM/DD/YYYY'), NEW.status);
    IF TG_OP = 'UPDATE' THEN
        old_s := concat_ws(' · ', '$' || to_char(OLD.total_amount, 'FM999,999,990.00') || ' agreed',
                           '$' || to_char(OLD.installment_amount, 'FM999,999,990.00') || ' ' || OLD.frequency,
                           OLD.payment_method, 'from ' || to_char(OLD.first_due_date, 'MM/DD/YYYY'), OLD.status);
        IF old_s IS NOT DISTINCT FROM new_s AND OLD.notes IS NOT DISTINCT FROM NEW.notes
           AND OLD.close_note IS NOT DISTINCT FROM NEW.close_note THEN
            RETURN NULL;
        END IF;
    END IF;
    INSERT INTO change_log (customer_id, field, old_value, new_value, changed_by)
    VALUES (NEW.customer_id, 'plan', old_s, new_s, ar_actor());
    RETURN NULL;
END $$;
CREATE TRIGGER payment_plans_history AFTER INSERT OR UPDATE ON payment_plans
    FOR EACH ROW EXECUTE FUNCTION payment_plans_history();
CREATE TRIGGER payment_plans_no_delete BEFORE DELETE ON payment_plans
    FOR EACH ROW EXECUTE FUNCTION refuse_change();

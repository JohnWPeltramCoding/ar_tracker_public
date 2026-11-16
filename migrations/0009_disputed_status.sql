-- "Disputed" becomes a Status choice. Opening a dispute sets the account's Status to Disputed;
-- resolving the last open dispute restores the Status it had before (kept in status_before).
ALTER TABLE disputes ADD COLUMN status_before TEXT;

UPDATE attributes
SET options = (options::jsonb || '[{"label": "Disputed", "color": "#f9cbb7"}]'::jsonb)::text
WHERE name = 'Status' AND kind = 'select' AND NOT (options::jsonb @> '[{"label": "Disputed"}]'::jsonb);

-- Accounts that already have an open dispute: remember their current Status, then mark them Disputed.
SELECT set_config('ar.user', 'upgrade: disputed status', true);

WITH st AS (SELECT id FROM attributes WHERE name = 'Status'),
first_open AS (
    SELECT DISTINCT ON (d.customer_id) d.id, d.customer_id
    FROM disputes d WHERE d.status = 'open' ORDER BY d.customer_id, d.opened_on, d.id
)
UPDATE disputes d SET status_before = COALESCE(v.value, '')
FROM first_open f
CROSS JOIN st
LEFT JOIN attr_values v ON v.customer_id = f.customer_id AND v.attribute_id = st.id
WHERE d.id = f.id;

INSERT INTO attr_values (customer_id, attribute_id, value, updated_by)
SELECT DISTINCT d.customer_id, a.id, 'Disputed', 'upgrade: disputed status'
FROM disputes d CROSS JOIN attributes a
WHERE d.status = 'open' AND a.name = 'Status'
ON CONFLICT (customer_id, attribute_id) DO UPDATE SET value = excluded.value, updated_by = excluded.updated_by, updated_at = now();

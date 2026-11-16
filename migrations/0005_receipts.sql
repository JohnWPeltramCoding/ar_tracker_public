-- Individual receipts from the receipts register, for customers tracked in AR Tracker.
-- Used for "collected per week" on the dashboard. Re-importing the same file adds nothing
-- (unique on customer, date, amount and the register's transaction number).
CREATE TABLE receipts (
    id            BIGSERIAL PRIMARY KEY,
    customer_id   BIGINT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    receipt_date  DATE NOT NULL,
    amount        NUMERIC(14,2) NOT NULL,
    method        TEXT NOT NULL DEFAULT '',
    reference     TEXT NOT NULL DEFAULT '',          -- "Transaction number" column
    source_file   TEXT,
    imported_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (customer_id, receipt_date, amount, reference)
);
CREATE INDEX receipts_customer_date ON receipts(customer_id, receipt_date);
CREATE INDEX receipts_date ON receipts(receipt_date);

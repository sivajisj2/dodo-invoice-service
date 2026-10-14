CREATE TABLE payment_attempts (
    id UUID PRIMARY KEY,
    invoice_id UUID NOT NULL REFERENCES invoices(id) ON DELETE RESTRICT,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (
            status IN (
                'pending',
                'succeeded',
                'failed',
                'unknown'
            )
        ),
    amount_cents BIGINT NOT NULL CHECK (amount_cents > 0),
    idempotency_key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    provider_idempotency_key TEXT NOT NULL UNIQUE,
    psp_reference TEXT,
    failure_code TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ
);

CREATE INDEX idx_payment_attempts_invoice_created
    ON payment_attempts (invoice_id, created_at DESC);

CREATE UNIQUE INDEX idx_payment_attempts_psp_reference
    ON payment_attempts (psp_reference)
    WHERE psp_reference IS NOT NULL;

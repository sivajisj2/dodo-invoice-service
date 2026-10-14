CREATE TABLE webhook_events (
    id UUID PRIMARY KEY,
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE RESTRICT,
    event_type TEXT NOT NULL
        CHECK (
            event_type IN (
                'invoice.created',
                'invoice.paid',
                'invoice.payment_failed'
            )
        ),
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_webhook_events_business_created
    ON webhook_events (business_id, created_at DESC);

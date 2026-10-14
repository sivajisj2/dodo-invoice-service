CREATE TABLE invoices (
    id UUID PRIMARY KEY,
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE RESTRICT,
    customer_id UUID NOT NULL,
    total_cents BIGINT NOT NULL CHECK (total_cents > 0),
    currency CHAR(3) NOT NULL DEFAULT 'USD'
        CHECK (currency = 'USD'),
    state TEXT NOT NULL DEFAULT 'open'
        CHECK (
            state IN (
                'draft',
                'open',
                'paid',
                'void',
                'uncollectible'
            )
        ),
    due_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT invoices_customer_business_fk
        FOREIGN KEY (business_id, customer_id)
        REFERENCES customers (business_id, id)
        ON DELETE RESTRICT
);

CREATE INDEX idx_invoices_business_state_created
    ON invoices (business_id, state, created_at DESC);

CREATE INDEX idx_invoices_customer_id
    ON invoices (customer_id);

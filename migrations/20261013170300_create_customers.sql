CREATE TABLE customers (
    id UUID PRIMARY KEY,
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    email TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_customers_business_created
    ON customers (business_id, created_at DESC);

ALTER TABLE customers
    ADD CONSTRAINT customers_business_id_id_unique
    UNIQUE (business_id, id);

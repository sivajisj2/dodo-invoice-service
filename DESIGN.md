# Design Notes

Two binaries share one crate: the invoice API, and a mock PSP running as a separate process
so the payment code has to cross a real network boundary. Postgres is the source of truth.

Money never touches a float. Line totals are `quantity * unit_amount_cents` via
`checked_mul`/`checked_add`, stored as `BIGINT` cents, currency pinned to USD. The client
sends line items; the server computes the total.

## 1. Data Model

UUID primary keys, generated application-side. That costs some index locality versus
bigserial, but it lets the API mint an invoice ID before the insert and hand the same ID
to the PSP, which matters for the crash-recovery path in section 3.

| Table | Shape and indexes | Why this shape |
|---|---|---|
| `businesses` | UUID PK, name, created_at | The tenant root. Everything else hangs off it. |
| `api_keys` | business_id, prefix, SHA-256 hash, revoked_at; unique on hash, index on business | Only the hash is stored. The prefix lets a human identify a key in a list without it being a secret. |
| `customers` | business_id, name, optional email; index `(business_id, created_at)`, unique `(business_id, id)` | That odd-looking unique key lets invoices carry a composite FK, so the database itself guarantees an invoice's customer is in the same business. |
| `invoices` | business_id, customer_id, total_cents, currency, state, due_at; index `(business_id, state, created_at)` | State and amount live on the invoice row, so the payment path locks exactly one row and reads everything it needs from it. |
| `invoice_line_items` | invoice_id, description, quantity, unit_amount_cents; index on invoice_id | The inputs behind the server-computed total, kept for audit. |
| `payment_attempts` | invoice_id, status, amount_cents, request + provider idempotency keys, psp_ref, failure_code; unique provider key, partial unique psp_ref | Separate from the invoice so failed and uncertain tries stay on the record instead of being overwritten. |
| `idempotency_records` | business_id, key, request hash, attempt link, saved status + body; unique `(business_id, idempotency_key)` | Enough to replay a response byte-for-byte. A check constraint keeps status and body either both set or both null. |
| `webhook_endpoints` | business_id, url, signing secret, enabled | Per-business endpoint config. |
| `webhook_events` | business_id, event_type, JSON payload | The event is durable whether or not any HTTP request ever succeeds. |
| `webhook_deliveries` | event_id, endpoint_id, status, attempt count, next_attempt_at, last error; unique `(event_id, endpoint_id)`, partial index on pending | One row per destination, so a slow endpoint can't hold up a healthy one. |

At 100x I'd swap `LIMIT 100` for keyed pagination, roll off `webhook_deliveries` and
`webhook_events` by age, and move delivery to a real queue. The tables stay the
durable outbox, the queue just becomes the scheduler.

## 2. Invoice State Machine

```text
                      issue
      draft ───────────────────────────> open
        │                                 │
        │ void                            ├──── payment succeeds ────> paid
        v                                 │
       void <──────── void ───────────────┤
                                          │
                                          └── mark uncollectible ──> uncollectible
```

`paid`, `void`, and `uncollectible` are terminal; nothing leaves them. A failed payment is deliberately
not a transition. The invoice stays `open` so the customer can try another card. An unknown outcome is also not a transition, for the reason in 3(b).

Invalid pairs are rejected at the API: `POST /pay` only accepts `open`, and anything else
returns `409` with a code like `invoice_not_open`. The only reversible move is
`open -> void`, and only in the sense that voiding is a decision rather than an accident.

One honest gap: there is no `issue` endpoint. Invoices insert directly as `open`, so
`draft` exists in the model but nothing creates it today. I kept it in the diagram because
draft is where line-item editing would belong.

## 3. Payment Correctness and Failure Modes

The endpoint does two things before it ever calls the PSP. First it reserves
`(business_id, Idempotency-Key)` with a unique constraint, storing a hash of the invoice ID
and token. Then it opens a transaction, takes `SELECT ... FOR UPDATE` on the invoice,
checks the state, and commits a `pending` attempt plus its provider idempotency key. All
of that happens before any network call.

**Mechanism:** pessimistic row-level locking, not advisory locks or serializable isolation.
The contended resource *is* a row, so the lock belongs on it, next to the state check it
protects. Advisory locks would put the invariant in a separate namespace from the data;
`SERIALIZABLE` would push retry handling into every caller for a conflict this narrow.

**(a) Two simultaneous `POST /pay`.** Same key: the unique constraint means one request
wins the insert and the other replays its result. Different keys: the row lock serializes
them. The first commits a pending attempt; the second wakes up, sees a non-terminal
attempt already on the invoice, and is rejected. One charge either way.

**(b) PSP times out (30 s).** The HTTP client gives up at 5 s. The attempt is marked
`unknown`, not `failed`. The invoice stays `open` and the endpoint returns `202` with the
attempt ID. Writing `failed` here would be a lie. The charge may well have landed. The
caller resolves it via `POST /v1/payment-attempts/{id}/reconcile`, which looks the payment
up by the provider idempotency key and finalizes the original attempt.

**(c) PSP succeeds, service crashes before persisting.** The attempt and provider key were
written before the call, so the record survives. A retry with the same key finds that
attempt rather than starting a new one, and reconciliation asks the PSP about that exact
key. No double charge. This leans on the PSP honouring idempotency and supporting lookup
by key. A provider offering neither would need a different strategy, and I wouldn't assume
safety without checking.

**(d) Key reused with a different body.** The stored request hash won't match, so the
request is rejected with `409` and no second attempt is created.

**(e) Paying an already-`paid` invoice.** Only `open` is accepted, so it's a `409`. The
terminal state is never rewritten.

Every error, here and everywhere else, is `{"error": {"code": "...", "message": "..."}}`.
Clients match on `code`; `message` is for humans and may change.

## 4. Webhook Design

Finalizing a payment writes the confirmed result, the idempotency response, the
`webhook_event`, and one `webhook_delivery` per enabled endpoint, all in the same
transaction. Invoice creation does the same for `invoice.created`. The request path never
opens a socket to a customer's server, so a slow endpoint can't slow down an API response.
That's the whole reason the outbox exists.

A background worker claims due rows with `FOR UPDATE SKIP LOCKED`, which lets several
workers share the table without blocking each other.

Signing is HMAC-SHA256 over `timestamp + "." + body`, sent as `X-Dodo-Timestamp` and
`X-Dodo-Signature: t=...,v1=...`. Receivers should recompute the HMAC, reject timestamps
more than ~5 minutes off, and remember event IDs. A timestamp isn't replay protection
unless the receiver actually checks it, which is easy to skip.

Retries back off 1, 2, 4, 8, 16, 32, 64 seconds. Eight attempts, about a two-minute budget.
After the last failure the delivery goes to `exhausted` with the final status and error
kept. That's all it does, and it's the weakest part of this design. Reconciling a missed
event currently means someone reading a table. It needs a list endpoint and a replay
button.

## 5. API Key Model

32 random bytes, hex-encoded behind a `dodo_live_` prefix. Stored as a SHA-256 hash plus a
short display prefix, never the key itself, and shown exactly once at creation. Clients
send `x-api-key`; auth hashes the input, looks up a non-revoked row, and scopes every
downstream query by the resulting `business_id`.

No salt, deliberately. A 256-bit random key isn't guessable, so a per-key salt buys nothing
against the attack that actually matters, and an unsalted hash stays a plain indexed
lookup.
Blast radius if leaked: full read/write on one business until someone sets `revoked_at`.
The column exists; the rotation endpoint doesn't. Webhook secrets are separate, so a leaked
API key can't forge signatures.

## 6. What I Cut and Why

- **Refunds and partial payments.** The interesting half is the reversal and its own state
  machine. Faking it would have been worse than omitting it.
- **An `issue` endpoint for `draft -> open`.** Named above rather than pretending the state
  machine is fully exercised.
- **Webhook replay / delivery-list API.** The data is there; I ran out of budget before the
  endpoints, and section 4 says so instead of implying operators are covered.
- **Rate limiting.** Out of scope, and a credible version is per-business token buckets in
  shared state, not middleware written in an hour.
- **Multi-currency.** Column exists, pinned to USD. Doing it properly means minor-unit
  tables and FX at capture time.

## 7. Production Readiness Gap

1. **Observability.** Structured logs exist; there are no metrics and no traces. The things
   I'd want paging me are `unknown` attempts that stay unknown, and deliveries hitting
   `exhausted`. Both are completely silent today.
2. **Operator tooling for stuck money.** An `unknown` attempt currently needs someone to
   know the reconcile endpoint exists. That should be a sweeper job plus a queue someone
   actually looks at.
3. **Audit log.** Who created the key, who voided the invoice, when it was revoked. For
   anything touching money this gets asked for early, and retrofitting it is miserable.

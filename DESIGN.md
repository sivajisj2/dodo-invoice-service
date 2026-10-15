# Design Notes

Two binaries in one crate: the invoice API, and a mock PSP running as its own process so payment
calls cross a real network. Postgres is the source of truth.

No floats touch money. Line totals use `checked_mul` and `checked_add` on `i64`, stored as
`BIGINT` cents, currency pinned to USD. The client sends line items. The server computes the
total.

## 1. Data Model

UUID primary keys, generated in the app. That costs some index locality against bigserial, but it
lets the API mint an invoice ID before the insert and hand that same ID to the PSP. Section 3(c)
depends on it.

| Table | Key fields and indexes | Why |
|---|---|---|
| `businesses` | UUID PK, name | Tenant root. |
| `api_keys` | business_id, prefix, SHA-256 hash, revoked_at; unique on hash | Only the hash is stored. The prefix lets you spot a key in a list without it being a secret. |
| `customers` | business_id, name, email; index `(business_id, created_at)`, unique `(business_id, id)` | That second unique key lets invoices carry a composite FK, so the database itself guarantees an invoice's customer belongs to the same business. |
| `invoices` | business_id, customer_id, total_cents, state, due_at; index `(business_id, state, created_at)` | State and amount sit on the invoice row, so the payment path locks one row and reads everything it needs. |
| `invoice_line_items` | invoice_id, description, quantity, unit_amount_cents | The inputs behind the server-computed total. |
| `payment_attempts` | invoice_id, status, amount_cents, request + provider keys, psp_ref; unique provider key | Separate from the invoice so failed and uncertain tries stay on the record instead of being overwritten. |
| `idempotency_records` | business_id, key, request hash, saved status + body; unique `(business_id, key)` | Enough to replay a response exactly. |
| `webhook_endpoints` | business_id, url, signing secret | Per-business config. |
| `webhook_events` | business_id, event_type, payload | The event is durable whether or not any HTTP call succeeds. |
| `webhook_deliveries` | event_id, endpoint_id, status, attempt_count, next_attempt_at | One row per destination, so a dead endpoint can't block a healthy one. |

At 100x: keyed pagination instead of `LIMIT 100`, roll off old webhook rows by age, and move
delivery to a real queue with these tables as the durable outbox.

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

`paid`, `void` and `uncollectible` are terminal. Nothing leaves them.

A failed payment is deliberately not a transition. The invoice stays `open` so the customer can
try another card. An unknown outcome isn't one either, for the reason in 3(b).

Invalid moves are rejected at the API: `POST /pay` only accepts `open`, anything else returns
`409`. The only reversible-ish move is `open → void`, and only in the sense that voiding is a
decision rather than an accident.

One honest gap: there's no `issue` endpoint. Invoices insert straight as `open`, so `draft` exists
in the model but nothing creates it. I kept it because draft is where line-item editing belongs.

## 3. Payment Correctness and Failure Modes

Two things happen before the PSP is ever called. First the API reserves
`(business_id, Idempotency-Key)` behind a unique constraint, storing a hash of the invoice ID and
token. Then it opens a transaction, takes `SELECT ... FOR UPDATE` on the invoice, checks the
state, and commits a `pending` attempt with its provider key.

**Mechanism: pessimistic row locking.** The thing two requests contend over is one invoice row, so
the lock belongs on that row, right above the state check it protects. Advisory locks would put
the invariant in a namespace unrelated to the data. `SERIALIZABLE` would push retry logic onto
every caller for a conflict this narrow.

**(a) Two simultaneous calls.** Same key: the unique constraint means one wins and the other
replays the stored result. Different keys: the row lock serializes them. The first commits a
pending attempt, the second sees it and is rejected. One charge either way.

**(b) PSP times out at 30 s.** The client gives up at 5 s. The attempt is marked `unknown`, the
invoice stays `open`, and the endpoint returns `202` with the attempt ID. Writing `failed` here
would be a lie, because the charge may well have landed. The caller resolves it with
`POST /v1/payment-attempts/{id}/reconcile`, which looks the payment up by provider key.

**(c) PSP succeeds, service crashes before saving.** The attempt and provider key were written
first, so the record survives. A retry with the same key finds that attempt instead of starting a
new one, and reconciliation asks the PSP about that exact key. No double charge. This relies on
the PSP honouring idempotency and supporting lookup by key.

**(d) Same key, different body.** The stored request hash won't match. `409`, no new attempt.

**(e) Paying an already-paid invoice.** Only `open` is accepted, so `409`. Terminal states are
never rewritten.

Reconciliation is idempotent too. Finalizing moves the attempt out of `pending`/`unknown` with a
conditional update and checks the affected row count, so two concurrent reconciles can't both emit
an `invoice.payment_failed` event. There's a test for it.

Every error, everywhere, is `{"error": {"code": "...", "message": "..."}}`. Match on `code`.

## 4. Webhook Design

Finalizing a payment writes the result, the idempotency response, the event, and one delivery row
per enabled endpoint, all in one transaction. Invoice creation does the same for
`invoice.created`. The request path never opens a socket to a customer's server, which is the
whole point of the outbox. A worker claims due rows with `FOR UPDATE SKIP LOCKED`.

Signing is HMAC-SHA256 over `timestamp + "." + body`, sent as `X-Dodo-Timestamp` and
`X-Dodo-Signature: t=...,v1=...`. Receivers should recompute the HMAC, reject timestamps more than
about five minutes off, and remember event IDs. A timestamp isn't replay protection unless the
receiver actually checks it.

Retries back off 1, 2, 4, 8, 16, 32, 64 seconds. Eight attempts, roughly a two-minute budget.
After the last failure the delivery goes to `exhausted` with the final status and error kept.
That's all it does, and it's the weakest part of this design — reconciling a missed event means
someone reading a table. It needs a list endpoint and a replay button.

## 5. API Key Model

32 random bytes, hex-encoded behind a `dodo_live_` prefix. Stored as a SHA-256 hash plus a short
display prefix, shown exactly once. Clients send `x-api-key`; auth hashes the input, finds a
non-revoked row, and scopes every query by the resulting `business_id`.

No salt, deliberately. A 256-bit random key isn't guessable, so a per-key salt buys nothing
against the attack that matters, and an unsalted hash stays a plain indexed lookup.

Leaked key: full read/write on one business until someone sets `revoked_at`. The column exists,
the rotation endpoint doesn't. Webhook secrets are separate, so a leaked API key can't forge
signatures.

## 6. What I Cut and Why

- **Refunds and partial payments.** The interesting half is the reversal and its own state
  machine. Faking it would be worse than leaving it out.
- **An `issue` endpoint.** Named in section 2 rather than pretending the state machine is fully
  exercised.
- **Webhook replay and a delivery-list API.** The data is there; I ran out of budget before the
  endpoints.
- **Webhook URL hardening.** Only scheme and host are validated. Doing SSRF properly means
  resolved-IP checks, DNS rebinding defence and a redirect policy.
- **Rate limiting and multi-currency.** Out of scope. The currency column exists, pinned to USD.

## 7. Production Readiness Gap

1. **Observability.** Logs exist; metrics and traces don't. The two things I'd want paging someone
   are `unknown` attempts that stay unknown, and deliveries hitting `exhausted`. Both are silent
   today.
2. **Operator tooling for stuck money.** An `unknown` attempt needs a human who knows the reconcile
   endpoint exists. That should be a sweeper job plus a queue someone actually watches.
3. **Audit log.** Who created the key, who voided the invoice, when it was revoked. Anything
   touching money gets asked this early, and retrofitting it is miserable.

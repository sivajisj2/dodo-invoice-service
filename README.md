# Invoice & Payment Service

A small Rust/Axum service for customers, invoices, payments and webhooks, backed by
PostgreSQL. There's a separate mock PSP binary standing in for a card processor, so you can
trigger declines, timeouts and network errors on demand instead of waiting for them.

## Run it

You need Docker and Docker Compose. From the project root:

```bash
docker compose up --build
```

The API comes up on `http://localhost:8080`, the mock PSP on `9000`. Migrations run on
boot, so there's no separate migrate step.

The easiest way in is [Swagger UI at `/docs`](http://localhost:8080/docs). Create a business
with `POST /v1/businesses`, copy the API key out of the response, hit **Authorize**, paste
it in, and every other endpoint is live. The raw spec is at
[`/openapi.json`](http://localhost:8080/openapi.json).

`Ctrl+C` then `docker compose down` to stop. Add `-v` if you also want the database volume
gone.

> If port 5432 or 9000 is already taken on your machine, the container will fail to bind.
> Either stop the local service or remap the host side in `docker-compose.yml`. The API
> reaches the database over the compose network, so the published port is only for you.

## Try a payment

Use the API key from `POST /v1/businesses` for all of these.

**1. Create a customer**

```bash
curl -X POST http://localhost:8080/v1/customers \
  -H 'x-api-key: YOUR_API_KEY' \
  -H 'Content-Type: application/json' \
  -d '{"name":"Test Customer","email":"customer@example.com"}'
```

**2. Create an invoice** with the customer ID you just got back. Amounts are in cents, so
`2500` is $25.00. There's no total field to send. The server works it out from the line
items.

```bash
curl -X POST http://localhost:8080/v1/invoices \
  -H 'x-api-key: YOUR_API_KEY' \
  -H 'Content-Type: application/json' \
  -d '{"customer_id":"CUSTOMER_UUID","line_items":[{"description":"Support","quantity":1,"unit_amount_cents":2500}]}'
```

**3. Pay it.** The `Idempotency-Key` is required. Reuse the same one to retry safely; use a
fresh one for a genuinely new payment.

```bash
curl -X POST http://localhost:8080/v1/invoices/INVOICE_UUID/pay \
  -H 'x-api-key: YOUR_API_KEY' \
  -H 'Idempotency-Key: demo-payment-001' \
  -H 'Content-Type: application/json' \
  -d '{"token":"tok_success"}'
```

**4. Watch one fail.** Use a different open invoice and a new key:

```bash
curl -X POST http://localhost:8080/v1/invoices/ANOTHER_INVOICE_UUID/pay \
  -H 'x-api-key: YOUR_API_KEY' \
  -H 'Idempotency-Key: demo-payment-002' \
  -H 'Content-Type: application/json' \
  -d '{"token":"tok_card_declined"}'
```

The invoice stays `open` after a decline, so the customer can try another card.

## Webhooks

Register somewhere to receive events. [webhook.site](https://webhook.site) is easy for a
demo:

```bash
curl -X POST http://localhost:8080/v1/webhook-endpoints \
  -H 'x-api-key: YOUR_API_KEY' \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://webhook.site/your-unique-id"}'
```

The response includes a signing secret, shown once. Events emitted: `invoice.created`,
`invoice.paid`, `invoice.payment_failed`. Each delivery carries `X-Dodo-Timestamp` and
`X-Dodo-Signature: t=...,v1=...`, where `v1` is HMAC-SHA256 over `timestamp + "." + body`.
Delivery runs on a background worker, never on the request path. Failures retry at 1, 2, 4,
8, 16, 32 and 64 seconds, and then the delivery is marked `exhausted`.

## Mock PSP tokens

| Token | Result |
|---|---|
| `tok_success` | Succeeds after ~100 ms |
| `tok_insufficient_funds` | Fails with `insufficient_funds` |
| `tok_card_declined` | Fails with `card_declined` |
| `tok_timeout` | Takes 30 s; the API gives up at 5 s and records an `unknown` result |
| `tok_network_error` | Returns a server error |

If a payment comes back `202` with an unknown result, hold onto the attempt ID and call
`POST /v1/payment-attempts/{attempt_id}/reconcile`. Don't start a fresh payment with a new
key while an outcome is still unknown. That's how you double-charge someone.

## Errors

Every endpoint returns the same shape:

```json
{"error": {"code": "invoice_not_found", "message": "Invoice not found"}}
```

Match on `code`, which is stable. `message` is for humans and may change.

## Tests

The integration tests need the mock PSP running and a PostgreSQL database they're allowed
to wipe. Point `TEST_DATABASE_URL` at a scratch database, not one with data you care about.

```bash
TEST_DATABASE_URL=postgres://postgres:password@localhost:5432/dodo_payments_test \
MOCK_PSP_URL=http://127.0.0.1:9000 \
cargo test
```

What they cover:

- `concurrent_payments_create_at_most_one_attempt`: five simultaneous `POST /pay` against
  one invoice, asserting a single attempt and a consistent final state.
- `concurrent_payments_with_different_keys_do_not_duplicate_attempts`: the same race with
  no idempotency key to lean on, so it's the row lock being tested.
- `successful_payment_retry_reuses_same_attempt`: retrying one key returns the stored
  response and creates no second attempt.
- `reusing_idempotency_key_with_different_request_returns_conflict`: returns `409`.
- `timeout_recovery_resolves_to_succeeded_without_a_second_charge`: `tok_timeout`, then
  reconcile. The invoice never gets stuck and nobody pays twice.

## Demo video

**https://www.loom.com/share/e196a57e38ed459183870b66f9cefebf**

Covers the architecture and request flow, a live `docker compose up` with a successful
payment, a decline, and the resulting webhook deliveries, then the invoice state machine
and the timeout failure mode walked through in the code.

# AI Usage

I used ChatGPT to look things up, not to write the service. Mostly checking Postgres locking
behaviour before I picked an approach, and sorting out a trait-version problem in the RustCrypto
crates. The design and the Rust are mine. I can walk through any line of it.

## Three decisions I made myself

**Row lock, not advisory locks.** I asked what the options were and got a sensible list back:
advisory locks, `SERIALIZABLE`, conditional updates. I went with `SELECT ... FOR UPDATE`. Two
requests fighting over a payment are fighting over one invoice row, so that's where the lock
belongs — right above the state check it protects. An advisory lock hides that invariant
somewhere you can't see it from the query.

**A timed-out payment is `unknown`, not `failed`.** The one I'd defend hardest. A timeout means my
client gave up. It says nothing about whether the charge went through. Mark it `failed` and the
obvious next move is a retry, and now someone's paid twice for money that already moved. So it
stays `unknown`, the invoice stays `open`, and you reconcile against the provider key when you
want the real answer.

**Totals computed server-side, in cents.** The request body has line items and no total field, so
there's nothing to tamper with. `checked_mul` and `checked_add` on `i64` mean a huge quantity
returns a `400` instead of wrapping into a negative invoice.

## What I had to correct

**Dependencies that don't build together.** `hmac` 0.12 paired with `sha2` 0.11 pull different
major versions of the `digest` traits, so `Hmac<Sha256>` doesn't satisfy the bounds. The error
points at the trait bound, not the version mismatch, which is how this always shows up in
RustCrypto. Pinned `sha2` to 0.10.9.

**A raw string that closed early.** The Swagger handler wraps its HTML in `r#"..."#`, and the page
contains `dom_id: "#swagger-ui"`. That `"#` ends the literal. Three compile errors from one
character — `r##"..."##` fixes it.

**Middleware in the wrong position.** `.route_layer()` sat partway down the router chain, so it
only covered the routes above it. Everything below ran unauthenticated and then panicked looking
for a `business_id` that was never inserted. That surfaces as a 500, not a 401, which is worse —
it looks like a database bug.

**An event that existed everywhere except the code.** `invoice.created` was in the design doc and
allowed by the events table, but nothing in `create_invoice` wrote it. Only `invoice.paid` and
`invoice.payment_failed` were real.

**A duplicate webhook event under concurrency.** The succeeded path was protected by its
`WHERE state='open'` guard; the failed path had nothing equivalent, so two concurrent
reconciliations could both emit `invoice.payment_failed`. Fixed with a conditional status
transition and a row-count check.

The last two are the ones worth noting: neither showed up by running the service. Both came from
re-reading the requirements against the code. Compiling clean is not the same as being correct,
and docs drift ahead of the implementation quickly.

## How I checked

21 tests: 9 unit tests on the state machine, 4 on payments, 8 on reconciliation. The three the
brief asks for are `concurrent_payments_create_at_most_one_attempt` (five simultaneous requests,
one attempt), `successful_payment_retry_reuses_same_attempt` (same key, same response, no second
attempt), and `timeout_recovery_resolves_to_succeeded_without_a_second_charge` (`tok_timeout`,
then reconcile).

For the concurrency fix I didn't just check the new test passed. I removed the guard and re-ran
it to confirm it failed — it reported 2 events where 1 was expected — then put the guard back. A
test that passes with and without the fix isn't testing anything.

Not covered: webhook signature verification, retry scheduling and exhaustion. That's the gap I'd
close first.

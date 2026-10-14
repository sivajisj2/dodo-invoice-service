# AI Usage

I used ChatGPT as a research tool, not as the author of this service. The useful
cases were narrow: checking Postgres locking semantics before I committed to an approach,
reading back the `digest` trait version rules in the RustCrypto crates, and talking through
the idempotency flow out loud before I wrote it. The design, the schema, and the Rust are
mine, and I can walk through any line of it.

Everything was verified by compiling it and running it against a local Postgres with the
mock PSP up. No suggestion went in unread.

## Three decisions I made myself

**1. Row lock on the invoice instead of advisory locks.** The options I weighed were
advisory locks, `SERIALIZABLE`, and a status-conditional update. I chose
`SELECT ... FOR UPDATE`. The thing two requests contend over is one invoice row, so the
lock belongs on that row, sitting directly above the state check it protects. An advisory
lock puts the invariant in a namespace unrelated to the data, where you can't see it from
the query. `SERIALIZABLE` would push retry handling onto every caller to solve a conflict
this narrow.

**2. A timed-out payment is `unknown`, not `failed`.** A timeout tells you your HTTP client
gave up. It says nothing about whether the charge went through. Recording `failed` invites
the caller to retry, and now someone has paid twice for money that already moved. So the
attempt sits at `unknown`, the invoice stays `open`, the endpoint returns `202` with the
attempt ID, and the real answer comes from reconciling against the provider idempotency key.

**3. Totals computed server-side, in integer cents.** The request body carries line items
and nothing else, so there is no `total` field to tamper with. The arithmetic uses
`checked_mul` and `checked_add` on `i64`, so an overflowing quantity returns a `400` rather
than wrapping into a negative invoice. No float exists anywhere in the money path.

## One thing I had to correct

I was given `hmac` 0.12 paired with `sha2` 0.11 for the webhook signing code. Those resolve
to different major versions of the `digest` traits, so `Hmac<Sha256>` won't satisfy the
bounds and the crate doesn't build. The error points at the trait bound rather than the
version mismatch, which is the usual way this shows up in the RustCrypto ecosystem. I
pinned `sha2` to 0.10.9 to match `hmac` 0.12 and rebuilt.

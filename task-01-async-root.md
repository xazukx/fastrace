Status: DONE
# Async `#[trace(async_root)]` support for `async fn`

## Goal

Add an opt-in async tracing mode so this works:

```rust
#[trace(async_root)]
async fn job() {
    // ...
}
```

The intended behavior is:

- **`#[trace(async_root)]` on `async fn`** creates a real root span for the lifetime of that future.
- **No outer `.in_span(...)` is required** for that function to be collected.
- **The span is intentionally detached from any ambient local parent** and starts its own trace with a fresh `SpanContext::random()`.
- **Plain `#[trace]` behavior stays unchanged**.

This is a targeted feature addition, not a full async parent-propagation redesign.

## What the code does today

### `fastrace-macro/src/lib.rs`

The async path in `gen_block()` currently expands to the equivalent of:

```rust
let __span__ = Span::enter_with_local_parent(name);
FutureExt::in_span(
    async move { ... },
    __span__,
)
```

with `.await` appended when the input item is a native `async fn`.

Important detail: because this code sits inside the body of an `async fn`, it executes when the future is **polled**, not when the future object is first created.

### `fastrace/src/future.rs`

`FutureExt::in_span()` does the right thing once it is given a real `Span`:

- it restores that span as the local parent on every poll
- it keeps the span alive until completion
- it drops the span on `Ready`, which records the end timestamp

So the failure mode is not in `InSpan` itself. The failure mode is that top-level traced async functions often create a **noop span** because they call `Span::enter_with_local_parent(...)` while being polled with no ambient local parent.

### `fastrace/src/span.rs`

The root span constructor already exists:

```rust
Span::root(name, SpanContext::random())
```

That means this feature does **not** need a new runtime span type. It only needs the macro to choose a different constructor for the async span.

### `fastrace/src/local/local_span_stack.rs`

This file explains why the current behavior drops spans:

- `Span::enter_with_local_parent(...)`
- `SpanContext::current_local_parent()`
- `LocalSpan::enter_with_local_parent(...)`

all depend on `LocalSpanStack::current_collect_token()`.

If the future is polled without a local parent already installed, the current token lookup returns `None`, so the async macro path builds a noop span.

For **this specific task**, no `LocalSpanStack` change is necessary. The `async_root` option avoids that lookup entirely.

## Recommended design

## 1. Add a bare `async_root` flag to macro arguments

Extend `Args` in `fastrace-macro/src/lib.rs`:

- add `async_root: bool`
- default it to `false`

Support the exact syntax the user asked for:

```rust
#[trace(async_root)]
```

### Parsing rules

- **Allow bare `async_root` only**
  - `#[trace(async_root)]` should work
- **Keep all existing assignment-style arguments**
  - `name = "..."`
  - `short_name = true`
  - `enter_on_poll = true`
  - `properties = { ... }`
  - `crate = ...`
- **Do not allow arbitrary bare identifiers**
  - preserve the current rejection behavior for `#[trace(a, b)]`
- **Reject duplicates**
  - `#[trace(async_root, root)]`
  - `#[trace(async_root, short_name = true, root)]`

### Recommended parser approach

The current parser assumes every argument is `key = value`.

Refactor it so that after reading the identifier:

- if the identifier is `async_root` and the next token is **not** `=`, set `async_root = true`
- otherwise parse the existing assignment form

That keeps the parser change small and preserves all current argument handling.

## 2. Treat `async_root` as async-only

Reject `#[trace(async_root)]` on non-async functions.

Why:

- the request is specifically about async functions
- sync functions already work correctly with `LocalSpan::enter_with_local_parent(...)`
- keeping the feature async-only avoids inventing new semantics for sync code

Recommended compile error:

```text
`async_root` can only be applied on async function
```

This validation belongs in `gen_block()` because that function already knows whether it is handling an async context.

## 3. Make `async_root` incompatible with `enter_on_poll = true`

Reject this combination:

```rust
#[trace(async_root, enter_on_poll = true)]
```

Reason:

- `async_root` means one long-lived span for the whole future
- `enter_on_poll = true` means short per-poll spans
- combining them creates ambiguous behavior

Do not try to guess which one should win.

Recommended compile error:

```text
`async_root` can not be used with `enter_on_poll`
```

## 4. Keep properties, `name`, `short_name`, and `crate` working with `async_root`

These should still be supported:

```rust
#[trace(async_root, short_name = true)]
#[trace(async_root, name = "worker")]
#[trace(async_root, properties = { "k": "v" })]
#[trace(async_root, crate = ::fastrace)]
```

Implementation detail:

- properties already attach to a `Span` via `.with_properties(...)`
- `Span::root(...)` returns a `Span`
- so the current properties code path can be reused unchanged

## 5. Change only the async span constructor

In `gen_block()` inside the async non-`enter_on_poll` branch, switch the constructor based on `args.async_root`.

### Current behavior

```rust
let __span__ = #crate_path::Span::enter_with_local_parent(#name) #properties;
```

### New behavior

- **If `args.async_root == false`**
  - keep using `Span::enter_with_local_parent(#name)`
- **If `args.async_root == true`**
  - use `Span::root(#name, #crate_path::prelude::SpanContext::random())`

Recommended generated shape:

```rust
{
    let __span__ = #span_ctor #properties;
    #crate_path::future::FutureExt::in_span(
        async move {
            let __ret__: #output_ty_hint = #block;
            #[allow(unreachable_code)]
            __ret__
        },
        __span__,
    )
}
```

where `#span_ctor` expands to one of:

```rust
#crate_path::Span::enter_with_local_parent(#name)
```

or

```rust
#crate_path::Span::root(#name, #crate_path::prelude::SpanContext::random())
```

## 6. Do not change `fastrace/src/future.rs` runtime behavior

No functional `future.rs` change is required for this feature.

Why:

- `InSpan<T>` already handles lifetime and per-poll local-parent restoration correctly
- once `#[trace(async_root)]` gives it a real root span instead of a noop span, the function is collected

Only documentation in `future.rs` may need updating so it no longer implies that **every** traced async function must be wrapped externally with `.in_span(...)`.

## 7. Do not change `LocalSpanStack` or `Span::set_local_parent()`

This task does **not** require:

- `fastrace/src/local/local_span_stack.rs`
- `fastrace/src/span.rs`
- `fastrace/src/collector/id.rs`

runtime refactors.

Reason:

- the new mode is intentionally a new root trace
- it does not need to discover or preserve an ambient parent
- existing `Span::root(...)` behavior already guarantees collection

The user asked about those files because they explain the current failure mode, but the minimal fix does not have to modify them.

## Implementation steps

## Step 1: Extend `Args`

In `fastrace-macro/src/lib.rs`:

- add `async_root: bool` to `Args`
- update `Default` accordingly

## Step 2: Refactor argument parsing

Update `impl Parse for Args` so it can parse:

- bare `async_root`
- existing assignment arguments

Keep duplicate detection using the current `HashSet`.

Suggested rules:

- `async_root` without `=` sets the flag
- any other bare identifier remains an error
- if you want to keep the API strict, reject `async_root = true` and `async_root = false`

That last choice is recommended because it keeps the new syntax precise and matches the requested form.

## Step 3: Add validation

In `gen_block()`:

- if `args.async_root && !async_context`, abort
- if `args.async_root && args.enter_on_poll`, abort

This keeps validation close to the codegen branch that actually cares.

## Step 4: Swap the async constructor

Still in `gen_block()`:

- keep the non-async branch unchanged
- keep the `enter_on_poll` branch unchanged except for the new validation
- in the async non-`enter_on_poll` branch, select:
  - `Span::enter_with_local_parent(...)` for default mode
  - `Span::root(..., SpanContext::random())` for `async_root` mode

## Step 5: Ensure async-trait paths inherit the same behavior

The `async-trait` handling path already goes through `gen_block(..., true, false, &args, None)`.

That means once `gen_block()` understands `async_root`, the following should also work automatically:

- `#[async_trait]` trait methods annotated with `#[trace(async_root)]`
- async-trait-generated boxed futures instrumented by the macro

Do not add a second custom code path unless you find a real edge case.

## Tests to add or update

## 1. Macro UI tests

Under `tests/macros/tests/ui` add:

- **`ok/has-root.rs`**
  - `#[trace(async_root)] async fn f() {}`
- **`err/has-root-and-sync.rs`**
  - `#[trace(async_root)] fn f() {}`
- **`err/has-root-and-enter-on-poll.rs`**
  - `#[trace(async_root, enter_on_poll = true)] async fn f() {}`

If you choose to reject assignment syntax for `async_root`, also add:

- **`err/has-root-assignment.rs`**
  - `#[trace(async_root = true)] async fn f() {}`

Make sure `has-ident-arguments.rs` still fails for arbitrary bare identifiers so the parser did not become too permissive.

## 2. Integration tests in `fastrace/tests/lib.rs`

Add a regression test that proves the new mode works without external `.in_span(...)`.

Recommended scenario:

- set up `TestReporter`
- define:

```rust
#[trace(async_root, short_name = true)]
async fn work() {
    tokio::task::yield_now().await;
}
```

- call `pollster::block_on(work())`
- flush
- assert that one span named `work` was collected

### Also add a stronger nested test

Use:

```rust
#[trace(async_root, short_name = true)]
async fn outer() {
    inner().await;
}

#[trace(async_root, short_name = true)]
async fn inner() {
    tokio::task::yield_now().await;
}
```

Expected result:

- `outer` and `inner` are both collected
- they are **not** parent/child unless you explicitly connect them some other way
- they should usually appear as separate root traces

That verifies the semantics are truly “start a new trace”, not “find ambient local parent later”.

### Property coverage

Add one test with:

```rust
#[trace(async_root, short_name = true, properties = { "k": "v" })]
async fn work() {}
```

and assert the property is preserved.

## 3. No-feature compile coverage

Because `#[cfg(not(feature = "enable"))]` still parses `Args`, the parser must accept `async_root` in the disabled build too.

Recommended coverage:

- add at least one `#[trace(async_root)] async fn ...` use in `tests/statically-disable/src/main.rs`
- or otherwise ensure an existing compile-only path exercises it

## Docs to update

## `fastrace-macro/src/lib.rs`

Update the macro docs:

- add `async_root` to the argument list
- explain that it is async-only
- explain that it creates a fresh root trace
- add an example expansion for `#[trace(async_root)] async fn`

## `fastrace/src/future.rs`

Update wording like:

- current wording says the outermost future must use `.in_span()` or traces are lost

Revise it to something closer to:

- raw futures still need explicit `.in_span(...)`
- `#[trace(async_root)] async fn` is an exception because it creates its own root span

## `examples/asynchronous.rs`

Optional but recommended:

- add a small example showing a top-level traced async function using `#[trace(async_root)]`
- keep the existing explicit `.in_span(...)` example too, because raw futures still need it

## Non-goals

Do **not** do any of the following for this task:

- redesign async parent propagation
- change `LocalSpan` semantics
- add new runtime types in `fastrace/src/future.rs`
- modify `Span::set_local_parent()`
- modify `LocalSpanStack`
- make `#[trace(async_root)]` inherit an ambient parent

That last point is especially important:

- `#[trace(async_root)]` should be explicit opt-in for “collect this future as its own trace”
- it is not a substitute for end-to-end parent propagation

## Acceptance criteria

The task is complete when all of the following are true:

- **`#[trace(async_root)] async fn` compiles**
- **the function is collected even when awaited without an outer `.in_span(...)`**
- **plain `#[trace]` behavior is unchanged**
- **`async_root` works with `name`, `short_name`, `properties`, and `crate`**
- **`async_root` is rejected on sync functions**
- **`async_root` is rejected with `enter_on_poll = true`**
- **macro UI tests cover the new syntax and failure cases**
- **disabled-feature builds still compile**

# Async `#[trace]` parent propagation redesign

## Goal

Make `#[trace]` on `async fn` self-sufficient so callers do not need to manually wrap every returned future with `.in_span(...)` just to keep spans connected.

The desired end state is:

- `#[trace] async fn` attaches to the correct parent when that parent is available either at future construction time or at first poll.
- A traced async function restores its own span as the current parent on every poll.
- Any traced async child function beneath it inherits the correct parent automatically.
- `enter_on_poll = true` keeps its short-per-poll semantics.
- `LocalSpan` remains a single-thread / no-cross-`.await` optimization and should not be changed into an await-safe type.
- Public `SpanContext` remains a lightweight remote-propagation type. Do not stuff collector internals into it.

## Root cause

Today the async macro path in `fastrace-macro/src/lib.rs` expands roughly like this:

- build `let __span__ = Span::enter_with_local_parent(name)`
- wrap the async block in `FutureExt::in_span(..., __span__)`

That has two structural problems:

- Parent binding happens too early.
  - The child `Span` is created when the future object is constructed, not when it is first polled.
  - If the correct parent does not exist yet at construction time, the span becomes a noop and is lost forever.

- Parent lookup is too narrow.
  - `Span::enter_with_local_parent()` ultimately reads `LocalSpanStack::current_collect_token()`.
  - That token only exists while a current local parent scope is already installed on the thread.
  - This works for some direct call stacks, but it does not make async `#[trace]` self-contained.

The result is that async spans depend on outside callers using `.in_span(...)` correctly, which is exactly the requirement to remove.

## Design decisions

### 1. Separate in-process parent capture from public `SpanContext`

Do not extend public `SpanContext` with `CollectToken`, `GlobalCollect`, or any other runtime collector internals.

Instead, add an internal captured-parent abstraction, for example:

- `CapturedParent`
- or `LocalParentContext`

This internal type should contain only what is needed to create child spans later in-process:

- `CollectToken`

Keep it internal to the crate. `SpanContext` should remain the lightweight serializable context used for remote propagation and links.

### 2. Use two-phase parent resolution for async `#[trace]`

For macro-generated async instrumentation, resolve the parent in this order:

- first choice: the parent captured when the future object is created
- fallback: if no parent was captured, try again on first poll

This covers both important cases:

- future created under the correct parent, then moved elsewhere before polling
- future created before the correct parent exists, but first polled under the correct parent

### 3. Do not promise impossible automatic propagation

No change inside `fastrace` can magically infer the right parent across fully detached boundaries such as:

- `tokio::spawn` inside a third-party library
- thread spawns
- callbacks stored and invoked later
- libraries that call an async function long after the original parent scope is gone

Those cases need an explicit capture/restore boundary API. The redesign should make that API easy, but it should not pretend those cases can be solved implicitly.

## Implementation plan

### Step 1: Add an internal captured-parent API

In `fastrace/src/span.rs` or a small new internal module, add an internal opaque type for current parent capture.

Required operations:

- `CapturedParent::current() -> Option<Self>`
- `CapturedParent::to_span_context() -> SpanContext`
- `Span::enter_with_captured_parent(name, &CapturedParent) -> Span`

`CapturedParent::current()` should use the same authoritative parent lookup that `Span::enter_with_local_parent()` uses after this refactor.

Do not expose this type publicly unless you later decide to use it for an explicit boundary API.

### Step 2: Make `LocalSpanStack` support ambient parent fallback

Refactor `fastrace/src/local/local_span_stack.rs`.

Today `current_collect_token()` only looks at the active span line. That makes current-parent resolution depend on already having an active local collector line.

Change the stack so it can represent both:

- the current local-span line parent
- an ambient parent token when no local-span line is active

The cleanest approach is to add an additional stack/field, for example:

- `parent_tokens: Vec<CollectToken>`

Add helpers such as:

- `push_parent(CollectToken)`
- `pop_parent()`
- update `current_collect_token()` to return:
  - the active span-line token if a span line exists
  - otherwise `parent_tokens.last().copied()`

This keeps current `LocalSpan` nesting behavior intact while giving async wrappers a place to restore parentage before any local spans are entered.

### Step 3: Update `Span::set_local_parent()` and `LocalParentGuard`

Refactor the sync parent installation path in `fastrace/src/span.rs`.

`Span::set_local_parent()` should:

- push the span's `issue_collect_token()` into the ambient parent stack
- create the `LocalCollector` for local spans as it does today

`LocalParentGuard` should, on drop:

- collect and submit local spans exactly as today
- pop the ambient parent token

Nesting must restore the previous parent correctly.

This is important because async wrappers will need to use the same mechanism. Parent restoration should become an explicit runtime concept, not an accidental side-effect of opening a local collector line.

### Step 4: Stop creating async spans eagerly in the macro

Refactor the async branch of `gen_block()` in `fastrace-macro/src/lib.rs`.

Current behavior:

- create child `Span` immediately with `Span::enter_with_local_parent(name)`
- wrap the inner future with `.in_span(__span__)`

Replace that with:

- capture `let __parent__ = <internal current-parent helper>`
- materialize properties once into owned values
- construct a dedicated async tracing wrapper that owns:
  - the inner future
  - the span name
  - the captured parent
  - the materialized properties
  - `Option<Span>` for lazy creation

Important:

- Do not keep formatting properties lazily per poll.
- Keep property evaluation equivalent to today's semantics: once, before the wrapper is returned.

### Step 5: Add a dedicated internal future wrapper in `fastrace/src/future.rs`

Do not make the macro depend exclusively on public `FutureExt::in_span(Span)`.

Add a macro-facing internal wrapper, for example `TraceFuture<T>`, with behavior:

- On first poll:
  - if the child span does not exist yet, create it from:
    - captured parent, or
    - poll-time fallback parent, or
    - `Span::noop()` if neither exists
- On every poll:
  - restore the span as the current local parent
  - poll the inner future
- On `Ready`:
  - take and drop the span once so end time is recorded correctly

Keep public `FutureExt::in_span()` as the explicit adapter for raw futures, but stop requiring `#[trace] async fn` to go through that public API path.

### Step 6: Fix the `enter_on_poll = true` async path

Do not leave `enter_on_poll = true` as a pure wrapper around `LocalSpan::enter_with_local_parent(...)` with no parent restoration.

Its wrapper must:

- restore the captured/current ambient parent before polling
- then create the short `LocalSpan` for that poll
- then poll the inner future

Otherwise nested traced async functions or child `Span::enter_with_local_parent()` calls inside an `enter_on_poll` function can still lose their parent.

Keep the existing semantics that `enter_on_poll = true` creates short spans per poll rather than one long span spanning the full future.

### Step 7: Keep `SpanContext` lightweight, but make lookup follow the new runtime parent

Update `fastrace/src/collector/id.rs`.

`SpanContext::current_local_parent()` should read the new authoritative current-parent resolution path instead of assuming that an active local span line is the only source of truth.

`SpanContext::from_span()` can stay as-is.

The goal is:

- public remote propagation semantics stay stable
- `current_local_parent()` works correctly inside traced async execution while polling

### Step 8: Add an explicit boundary API for detached work

Because detached work cannot be solved implicitly, add an explicit capture/restore API built on the same internal mechanism.

Examples of acceptable shapes:

- `Span::capture_local_parent() -> Option<CapturedParent>`
- `CapturedParent::in_scope(|| ...)`
- `CapturedParent::wrap_future(fut)`

This is the correct tool for integrations with executors, callback registries, channel workers, or third-party libraries that detach the work from the original poll chain.

That API is the honest solution for the exact cases where `.in_span(...)` is currently required but inaccessible.

## File-by-file change list

### `fastrace-macro/src/lib.rs`

- refactor async `gen_block()`
- stop eagerly constructing the child `Span`
- capture parent handle instead
- route async instrumentation through the new internal future wrapper
- ensure `enter_on_poll = true` also restores parentage

### `fastrace/src/future.rs`

- keep public `FutureExt::in_span()` for raw futures
- add internal wrapper/helper for macro-generated async tracing
- implement lazy span creation on first poll
- implement poll-time fallback parent resolution
- ensure span is installed as local parent on every poll

### `fastrace/src/span.rs`

- add internal captured-parent representation
- add helper to create `Span` from captured parent
- update `Span::set_local_parent()`
- update `LocalParentGuard`

### `fastrace/src/local/local_span_stack.rs`

- add ambient parent token stack
- refactor `current_collect_token()`
- keep span-line behavior unchanged for `LocalSpan` nesting

### `fastrace/src/collector/id.rs`

- update `SpanContext::current_local_parent()` to use the new current-parent path

### `fastrace-futures/src/lib.rs`

- optional, but recommended:
  - reuse the same captured-parent helper for stream/sink wrappers
  - keep behavior consistent with the future wrapper model

## Tests to add or update

Add regression coverage in `fastrace/tests/lib.rs` for the following:

- future created before parent exists, then first polled under parent
  - expected: traced async span attaches correctly

- future created under parent, then moved and polled later without manual `.in_span(...)`
  - expected: traced async span still attaches to the captured parent

- nested `#[trace] async fn` beneath another traced async fn after at least one `.await`
  - expected: child attaches to the parent's traced span automatically

- `enter_on_poll = true` function that:
  - creates a `LocalSpan`
  - calls another traced async function
  - crosses at least one `.await`
  - expected: everything remains connected to the correct parent

- `SpanContext::current_local_parent()` from inside a traced async function
  - expected: it returns the current traced span while that future is being polled

- explicit detached-boundary case
  - expected: implicit propagation does not work across a fully detached spawn
  - expected: the new explicit capture/restore API does work

Also update any relevant snapshot expectations.

## Docs to update

Update docs that currently imply the outermost future must always use `.in_span(...)`:

- `fastrace/src/future.rs`
- `fastrace/src/lib.rs`
- `fastrace-macro/src/lib.rs`
- `examples/asynchronous.rs`

New documentation should say:

- `#[trace] async fn` propagates its own parent automatically across poll boundaries
- raw futures / streams / sinks still need explicit instrumentation unless wrapped by traced async code or an explicit captured-parent boundary helper

## Non-goals

Do not do any of the following:

- do not make `LocalSpan` await-safe
- do not store runtime collector state inside public `SpanContext`
- do not claim detached tasks/callbacks can inherit parents automatically without boundary instrumentation

## Acceptance criteria

The work is done when all of the following are true:

- `#[trace] async fn` no longer depends on the caller manually wrapping the returned future with `.in_span(...)`
- traced async children inherit the correct parent from traced async ancestors
- `enter_on_poll = true` still works with correct parentage
- detached work has an explicit capture/restore solution instead of implicit magic
- docs and tests match the new model
- the no-feature build still compiles

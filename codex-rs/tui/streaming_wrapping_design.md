# Streaming Markdown Wrapping & Animation – Design Notes

This document describes a rendering quirk in the Codex TUI when displaying
**streaming agent responses**, and outlines options for improving it.

The goal is to give enough context that someone who doesn’t live in this code
can understand:

- Where streaming rendering happens
- Why streamed content doesn’t reflow when the terminal width changes
- What behaviors we must preserve (newlines, lists, blockquotes, headings,
  animation)
- Possible directions for a more robust design

---

## 1. Problem Overview

When the model streams an answer, the TUI shows it incrementally, animating out
new lines as they arrive. The streaming pipeline currently **wraps lines at a
fixed width at commit time**, and those wraps are baked into the stored
`Line<'static>` values.

Later, when the terminal is resized or the viewport changes, the transcript
rendering code wraps again for the new width, but it can only insert *additional*
breaks. It cannot “un‑wrap” the splits that were baked in when the stream
started. The result:

- Streamed messages **do not reflow** correctly when the terminal width grows.
- Long paragraphs are permanently split according to the width at the time they
  were streamed.

Non‑streaming messages don’t have this issue; they are rendered without a
width and wrapped only at display time, so they reflow correctly.

---

## 2. Current Streaming Rendering Flow

This section walks through the main pieces involved in rendering a streaming
agent result.

### 2.1 ChatWidget: handling streaming deltas

Streaming agent content arrives as deltas and is handled in
`ChatWidget::handle_streaming_delta`:

```rust
// codex-rs/tui/src/chatwidget.rs:940+
#[inline]
fn handle_streaming_delta(&mut self, delta: String) {
    // Before streaming agent content, flush any active exec cell group.
    self.flush_active_cell();

    if self.stream_controller.is_none() {
        if self.needs_final_message_separator {
            let elapsed_seconds = self
                .bottom_pane
                .status_widget()
                .map(super::status_indicator_widget::StatusIndicatorWidget::elapsed_seconds);
            self.add_to_history(history_cell::FinalMessageSeparator::new(elapsed_seconds));
            self.needs_final_message_separator = false;
        }
        self.stream_controller = Some(StreamController::new(
            self.last_rendered_width.get().map(|w| w.saturating_sub(2)),
        ));
    }
    if let Some(controller) = self.stream_controller.as_mut()
        && controller.push(&delta)
    {
        self.app_event_tx.send(AppEvent::StartCommitAnimation);
    }
    self.request_redraw();
}
```

Key points:

- On the **first** delta of a stream, we create a `StreamController`, passing a
  width based on the **last rendered width of the main view** (minus some
  padding).
- Every delta is pushed into the controller; if `push` returns `true`, we start
  the commit animation.

### 2.2 StreamController and StreamState

The controller ties together streaming state and the commit‑animation loop:

```rust
// codex-rs/tui/src/streaming/controller.rs
pub(crate) struct StreamController {
    state: StreamState,
    finishing_after_drain: bool,
    header_emitted: bool,
}

impl StreamController {
    pub(crate) fn new(width: Option<usize>) -> Self {
        Self {
            state: StreamState::new(width),
            finishing_after_drain: false,
            header_emitted: false,
        }
    }

    pub(crate) fn push(&mut self, delta: &str) -> bool {
        let state = &mut self.state;
        if !delta.is_empty() {
            state.has_seen_delta = true;
        }
        state.collector.push_delta(delta);
        if delta.contains('\n') {
            let newly_completed = state.collector.commit_complete_lines();
            if !newly_completed.is_empty() {
                state.enqueue(newly_completed);
                return true;
            }
        }
        false
    }

    pub(crate) fn on_commit_tick(&mut self) -> (Option<Box<dyn HistoryCell>>, bool) {
        let step = self.state.step();
        (self.emit(step), self.state.is_idle())
    }
}
```

The associated `StreamState` holds a `MarkdownStreamCollector` and a queue of
rendered lines:

```rust
// codex-rs/tui/src/streaming/mod.rs
pub(crate) struct StreamState {
    pub(crate) collector: MarkdownStreamCollector,
    queued_lines: VecDeque<Line<'static>>,
    pub(crate) has_seen_delta: bool,
}

impl StreamState {
    pub(crate) fn new(width: Option<usize>) -> Self {
        Self {
            collector: MarkdownStreamCollector::new(width),
            queued_lines: VecDeque::new(),
            has_seen_delta: false,
        }
    }
    // step / drain_all / enqueue just move Line<'static> values around.
}
```

### 2.3 MarkdownStreamCollector: width‑aware streaming and commits

The collector is where markdown is rendered and **width‑dependent wrapping**
happens during streaming:

```rust
// codex-rs/tui/src/markdown_stream.rs
pub(crate) struct MarkdownStreamCollector {
    buffer: String,
    committed_line_count: usize,
    width: Option<usize>,
}

impl MarkdownStreamCollector {
    pub fn new(width: Option<usize>) -> Self { ... }

    pub fn push_delta(&mut self, delta: &str) {
        self.buffer.push_str(delta);
    }

    pub fn commit_complete_lines(&mut self) -> Vec<Line<'static>> {
        let source = self.buffer.clone();
        let last_newline_idx = source.rfind('\n');
        let source = if let Some(last_newline_idx) = last_newline_idx {
            source[..=last_newline_idx].to_string()
        } else {
            return Vec::new();
        };
        let mut rendered: Vec<Line<'static>> = Vec::new();
        markdown::append_markdown(&source, self.width, &mut rendered);
        let mut complete_line_count = rendered.len();
        // trim trailing blank line
        if complete_line_count > 0
            && crate::render::line_utils::is_blank_line_spaces_only(
                &rendered[complete_line_count - 1],
            )
        {
            complete_line_count -= 1;
        }

        if self.committed_line_count >= complete_line_count {
            return Vec::new();
        }

        let out_slice = &rendered[self.committed_line_count..complete_line_count];

        let out = out_slice.to_vec();
        self.committed_line_count = complete_line_count;
        out
    }
}
```

Important details:

- The collector owns the **full raw markdown buffer** for the active stream.
- It calls `append_markdown(&source, self.width, &mut rendered)`, passing the
  width that was captured when the stream started.
- It tracks `committed_line_count` in terms of the **number of rendered lines**
  (after wrapping), and returns only the *new* rendered lines as
  `Vec<Line<'static>>`.

### 2.4 markdown::append_markdown and markdown_render

`append_markdown` is a thin wrapper around the markdown renderer:

```rust
// codex-rs/tui/src/markdown.rs
pub(crate) fn append_markdown(
    markdown_source: &str,
    width: Option<usize>,
    lines: &mut Vec<Line<'static>>,
) {
    let rendered = crate::markdown_render::render_markdown_text_with_width(markdown_source, width);
    crate::render::line_utils::push_owned_lines(&rendered.lines, lines);
}
```

Inside `markdown_render`, the width is used to decide whether to wrap each line:

```rust
// codex-rs/tui/src/markdown_render.rs:444+
fn flush_current_line(&mut self) {
    if let Some(line) = self.current_line_content.take() {
        let style = self.current_line_style;
        // NB we don't wrap code in code blocks, in order to preserve whitespace for copy/paste.
        if !self.current_line_in_code_block
            && let Some(width) = self.wrap_width
        {
            let opts = RtOptions::new(width)
                .initial_indent(self.current_initial_indent.clone().into())
                .subsequent_indent(self.current_subsequent_indent.clone().into());
            for wrapped in word_wrap_line(&line, opts) {
                let owned = line_to_static(&wrapped).style(style);
                self.text.lines.push(owned);
            }
        } else {
            let mut spans = self.current_initial_indent.clone();
            let mut line = line;
            spans.append(&mut line.spans);
            self.text.lines.push(Line::from_iter(spans).style(style));
        }
        // ...
    }
}
```

So the collector is not just parsing markdown; it’s producing **fully wrapped**
lines, complete with list/blockquote prefixes and styles, at a fixed width for
this stream.

### 2.5 Emitting HistoryCells and final display

`StreamController::emit` turns those wrapped lines into an `AgentMessageCell`:

```rust
// codex-rs/tui/src/streaming/controller.rs:75+
fn emit(&mut self, lines: Vec<Line<'static>>) -> Option<Box<dyn HistoryCell>> {
    if lines.is_empty() {
        return None;
    }
    Some(Box::new(history_cell::AgentMessageCell::new(lines, {
        let header_emitted = self.header_emitted;
        self.header_emitted = true;
        !header_emitted
    })))
}
```

`AgentMessageCell`’s `HistoryCell` implementation wraps **again** at display time:

```rust
// codex-rs/tui/src/history_cell.rs:236+
impl HistoryCell for AgentMessageCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        word_wrap_lines(
            &self.lines,
            RtOptions::new(width as usize)
                .initial_indent(if self.is_first_line {
                    "• ".dim().into()
                } else {
                    "  ".into()
                })
                .subsequent_indent("  ".into()),
        )
    }
}
```

This is the same `display_lines(width)` used by both:

- The **main transcript area** in `App::render_transcript_cells`
  (`codex-rs/tui/src/app.rs`), and
- The transcript overlay (pager) in `pager_overlay.rs`.

---

## 3. Why Streamed Content Doesn’t Reflow

The key problem is that we are **wrapping twice**, at two different layers:

1. `MarkdownStreamCollector` wraps logical markdown lines into `Line<'static>`s
   at a fixed, per‑stream width, and uses the count of those wrapped lines as
   its commit unit.
2. `AgentMessageCell::display_lines(width)` wraps those `Line<'static>`s again
   for the current viewport width.

Once the collector has split a paragraph into multiple lines at the streaming
width, those splits are **baked into `self.lines`** inside `AgentMessageCell`.
Later, when the terminal width increases:

- `display_lines(new_width)` can break those lines further if needed,
- but it can’t “rejoin” lines that were already split in the collector,
  because it has no access to the original unbroken text.

So streaming content behaves like this:

- Width at **stream start** defines “hard” line boundaries.
- Width at **render time** can only refine, not undo, those boundaries.

By contrast, non‑streaming messages are rendered with `width = None` in
`append_markdown`, so their `AgentMessageCell` is seeded with logical lines that
do *not* depend on the viewport. All wrapping is deferred to `display_lines`,
and they reflow correctly when the terminal is resized.

---

## 4. Behaviors We Must Preserve

Any redesign needs to keep the following stable behaviors:

1. **Newline‑gated commits.**
   - `MarkdownStreamCollector::commit_complete_lines` currently only emits new
     content when a newline is seen in the buffer. This keeps streaming output
     “coherent” at newline boundaries.
2. **Markdown semantics.**
   - Lists, nested lists, blockquotes, headings, and code blocks must continue
     to render with the same structure and styling as they do today. That logic
     lives in `markdown_render.rs` and is tested via
     `markdown_stream.rs` and `markdown_render.rs` tests.
3. **Streaming animation feel.**
   - `StreamController::on_commit_tick` currently emits at most one new
     `HistoryCell` per tick, revealing the response incrementally.
   - The step size is “one wrapped line” at the stream’s width. Changing the
     granularity is acceptable, but we should avoid regressing to a single huge
     jump per tick for long paragraphs.
4. **History & backtrack integration.**
   - The transcript is stored as a sequence of `HistoryCell`s in
     `App::transcript_cells` (`app.rs`).
   - Backtracking, overlays, and session replay assume that once a cell is
     emitted, its content is stable.

---

## 5. Design Direction 1: `width = None` for Streaming

The simplest change conceptually is to make streaming behave like non‑streaming
markdown:

- Construct the streaming collector with `MarkdownStreamCollector::new(None)`:
  - i.e. **no width** when we call `append_markdown`.
- `MarkdownStreamCollector` would still:
  - Buffer the raw markdown,
  - Commit only at newline boundaries,
  - Return newly completed logical lines, but they would not be wrapped.
- `AgentMessageCell::display_lines(width)` remains the sole place where
  wrapping happens, based on the *current* viewport width.

**Impact:**

- ✅ Streamed messages would reflow correctly when the terminal size changes
  (like non‑streaming messages).
- ✅ Newline‑gated behavior is unchanged; we still commit at newline boundaries.
- ✅ Lists / blockquotes / headings remain correct; tests already exercise the
  `width = None` path by streaming into `MarkdownStreamCollector::new(None)`
  and wrapping later for assertions.
- ⚠️ Animation granularity changes:
  - Today: animation steps per **wrapped line** at the stream’s width.
  - With `None`: animation steps per **logical line** (paragraph / list item /
    blockquote line), which could be visually larger steps on narrow terminals.

This is likely an acceptable behavioral change, but it is a tradeoff:
better reflow vs. slightly coarser animation steps.

---

## 6. Design Direction 2: Streaming Cell with “Committed Prefix”

If we want to keep a **width‑aware sense of progress** (i.e., roughly “one
visual line” per tick) *and* support reflow on resize, we probably need a more
stateful streaming cell type instead of the current `Vec<Line<'static>>`
pipeline.

### 6.1 High‑level idea

Introduce a `StreamingAgentMessageCell` that owns:

- The full raw markdown buffer for the message (or a pre‑parsed representation).
- A notion of “how much has been revealed so far”:
  - e.g. a byte index `commit_upto` in the source, or a logical line index.
- Optional caches keyed by `(width, commit_upto)` to avoid re‑parsing
  everything on every frame.

`display_lines(width)` for this cell would:

1. Compute the visible prefix of the message based on `commit_upto`.
2. Render that prefix with `append_markdown(prefix, None, &mut rendered)`.
3. Apply presentation wrapping for `width` (e.g. via `AgentMessageCell`‑like
   logic).

`StreamController` would no longer enqueue pre‑wrapped `Line<'static>` values.
Instead:

- It would own a single `StreamingAgentMessageCell`.
- `push(delta)` would append to the cell’s buffer, and determine whether a
  newline has been added (unchanged gating behavior).
- `on_commit_tick` would advance `commit_upto` by some unit:
  - simplest: one logical newline‑terminated line per tick,
  - more advanced: enough characters to approximate one visual line at the last
    known width.

Once streaming completes, the cell could be finalized into a plain
`AgentMessageCell` for long‑term storage and simpler replay.

### 6.2 Challenges

This direction touches more pieces and has some non‑trivial challenges:

- **Commit units vs. width.**  
  Today, `committed_line_count` is “number of rendered lines” at a fixed width.
  If we switch to a “committed prefix” model:
  - We must decide whether `commit_upto` is tracked in source bytes, logical
    lines, or derived from visual wrapping at a given width.
  - If visual wrapping is used, changing widths mid‑stream changes the mapping
    from “N visual lines” to a source position; we’d need to tolerate that
    non‑linearity.

- **Performance / recomputation.**  
  On each render we potentially need to:
  - Re‑parse markdown for the prefix up to `commit_upto`.
  - Re‑wrap for the current width.
  - This can be mitigated with simple caching keyed by `(width, commit_upto)`,
    but it’s inherently heavier than the current “pre‑wrap once” approach.

- **History expectations.**  
  The transcript currently stores a sequence of *static* `HistoryCell`s.
  A streaming cell whose content changes (grows) over time is already implied
  by how we append streamed agent content, but we need to ensure:
  - Backtracking, overlay rendering, and session replay can handle a partially
    revealed cell evolving over time.

Given the complexity, this direction is best suited for a deeper refactor where
we’re comfortable changing the shape of the streaming API, not just its width
parameter.

---

## 7. Design Direction 3: Hybrid “Visual Line Count” Model

A more incremental (but complex) approach would try to preserve the current
`StreamController` API while making the collector width‑agnostic:

- Track “number of visual lines committed” as a scalar `N`.
- On each render, re‑render the full source with `append_markdown(..., None)`
  and wrap to the *current* width.
- Display only the first `N` visual lines; the remainder stays hidden until the
  next tick.

This keeps the idea of “we reveal one line per tick” and allows reflow, but:

- Width changes mid‑stream will cause `N` visual lines to correspond to a
  different amount of text than before (more text if the width grows, less if
  it shrinks).
- You still need a place to store the raw buffer and the committed `N`, which
  pushes towards a streaming cell type similar to Direction 2.

This model is conceptually appealing (because `StreamController` still owns
“line count”), but it’s arguably more complex than Direction 1 and doesn’t buy
much over a clean streaming cell refactor.

---

## 8. Recommended Path Forward

Given the tradeoffs, a pragmatic staged approach could be:

1. **Stage 1 – behavior fix with minimal code churn.**
   - Change streaming to use `MarkdownStreamCollector::new(None)` instead of
     passing a width, so streamed messages reflow based on the current
     viewport width via `AgentMessageCell::display_lines(width)`.
   - Accept that animation steps are per logical line instead of per
     pre‑wrapped visual line.

2. **Stage 2 – if necessary, richer streaming cell.**
   - If we want finer control of animation granularity or more sophisticated
     streaming behavior (e.g. per visual line with correct reflow), introduce a
     dedicated `StreamingAgentMessageCell` as sketched above and move commit /
     animation semantics into that cell.

3. **Throughout: keep tests green.**
   - Use existing tests in:
     - `tui/src/markdown_stream.rs`
     - `tui/src/markdown_render.rs`
     - `tui/src/history_cell.rs`
   - Add new tests that:
     - Stream a long paragraph at one width, then simulate a resize and verify
       the transcript reflows as expected.
     - Ensure lists / blockquotes / headings maintain their semantics under
       streaming with the new model.

This doc is intentionally speculative; it describes the problem and outlines
options without committing to a concrete implementation. Any actual change
should be scoped and rolled out carefully, with a focus on keeping the
streaming UX smooth while making the layout more robust to viewport changes.


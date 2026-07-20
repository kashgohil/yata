# yata

**Yet Another Terminal browser** — a fast, keyboard-first web browser for the terminal.

yata renders real web pages in your terminal with its own HTML parser, CSS cascade, layout engine, and paint pipeline. Networking and media decoding use proven libraries; everything you see on screen is laid out and drawn by yata.

Designed for reading and navigating the web without leaving the shell: articles, documentation, Hacker News, Wikipedia, and the everyday sites you open between builds.

## Features

### Rendering

- **Full pipeline in process** — fetch → HTML parse → CSS cascade → cell-based layout → display list → terminal paint
- **CSS styling** — selectors (tag, class, id, compound, descendant, child), specificity, inheritance, and cascade across user-agent styles, linked stylesheets, `<style>` blocks, and inline `style` attributes
- **Layout** — block and inline flow, box model (margin, padding, border in cell units), `width` / `max-width`, text alignment, and flexbox for modern page structure
- **Readable by default** — content measure capped around ~90 columns and centered in wide terminals, with sensible defaults for headings, paragraphs, lists, and links
- **Colors** — truecolor when the terminal supports it (`COLORTERM=truecolor`), otherwise nearest ANSI-256
- **Images** — asynchronous fetch and decode; Unicode half-block fallback everywhere, [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/) when available
- **JavaScript** — embedded engine with DOM bindings, event dispatch, and timers integrated into the browser event loop

### Navigation & interaction

- **Link hints** — press `f` to label every visible link with short keys (vimium-style); type a label to follow, or `F` to copy the URL
- **Mouse support** — click links via hit-testing against the layout tree; hover styles without full relayout
- **Keyboard focus** — Tab / Shift-Tab cycle links; Enter follows
- **History** — back and forward with scroll position restored
- **In-page search** — `/` to find, `n` / `N` for next and previous match, match count in the status line
- **URL bar** — open a URL (`o`) or edit the current one (`O`); reload, yank URL to clipboard

### Terminal UX

- **Non-blocking UI** — network and decoding run on worker threads; typing, scrolling, and quit always stay responsive
- **Instant feedback** — keypress to screen under a frame; loads longer than ~100 ms show progress on the status line
- **Diffed renderer** — double-buffered cell grid, per-frame diff, single batched write, synchronized output so frames do not tear
- **Scroll without relayout** — scrolling re-emits a cached display list at a new offset
- **Resize-safe** — re-layout keeps the top visible content anchored
- **Errors as pages** — DNS, TLS, HTTP, and content-type failures render a clear page with a retry hint, not a crash
- **Clean exit** — `q` / Ctrl-c always restore the terminal, including after panics

### Developer tools

Built-in inspectors (toggle with function keys):

| Key | Inspector |
|-----|-----------|
| `F1` | DOM tree |
| `F2` | Computed styles for the node under the cursor |
| `F3` | Layout boxes (x, y, width, height) |
| `F4` | Timing overlay (per-stage and last-frame cost) |

## Installation

### From source

Requires a recent [Rust](https://rustup.rs/) toolchain.

```bash
git clone https://github.com/kashgohil/yata.git
cd yata
cargo install --path .
```

Or build a release binary without installing:

```bash
cargo build --release
./target/release/yata https://example.com
```

### From crates.io

```bash
cargo install yata
```

*(Available when published.)*

## Usage

```bash
yata [OPTIONS] [URL]
```

Open a URL on launch, or start and press `o` to focus the URL bar.

```bash
yata https://en.wikipedia.org/wiki/Terminal_emulator
yata                          # empty session; open a URL with o
```

### Command-line options

| Option | Description |
|--------|-------------|
| `URL` | Document to load on startup |
| `--dump` | Fetch and print the response body, then exit (no TUI) |
| `--timing` | Print per-stage pipeline timings to stderr and exit |

### Status line

One persistent row at the bottom of the screen:

```
 left: mode + URL          middle: progress / message           right: scroll% · frame time
 ┌─────────────────────────────────────────────────────────────────────────────────────────┐
 │ https://example.com/              ⣤ loading… 12 KB                         34% · 2.1 ms │
 └─────────────────────────────────────────────────────────────────────────────────────────┘
```

## Keyboard shortcuts

Press `?` at any time for the in-app help overlay.

### Scrolling

| Key | Action |
|-----|--------|
| `j` / `k` or `↓` / `↑` | Line down / up |
| `Ctrl-d` / `Ctrl-u` or `PgDn` / `PgUp` | Half-page down / up |
| `gg` / `G` or `Home` / `End` | Top / bottom |
| Mouse wheel | Scroll |

### Navigation

| Key | Action |
|-----|--------|
| `o` | Open URL (focus URL bar) |
| `O` | Edit current URL |
| `f` | Link hints — follow |
| `F` | Link hints — yank link URL |
| `Tab` / `Shift-Tab` | Next / previous link |
| `Enter` | Follow focused link |
| `H` or `Backspace` | History back |
| `L` or `Shift-Backspace` | History forward |
| `r` | Reload |
| `yy` | Yank current page URL |
| Mouse click | Follow link under cursor |

### Search & help

| Key | Action |
|-----|--------|
| `/` | Find in page |
| `n` / `N` | Next / previous match |
| `?` | Help overlay |
| `q` / `Ctrl-c` | Quit |

## Architecture

yata is a message-driven browser with a strict, staged pipeline. Each stage is a pure transform: it consumes the previous stage’s output and produces a new structure. No stage reaches backward into earlier trees.

```
            ┌────────┐   ┌───────────┐   ┌─────────────┐   ┌──────────────┐
 URL ──────▶│ Fetcher │──▶│ HTML      │──▶│ DOM tree    │   │ CSS parser   │
            │  HTTP   │   │ tokenizer │   │  (arena)    │   │ + stylesheets│
            └────────┘   │ + tree    │   └──────┬──────┘   └──────┬───────┘
                          │ builder   │          │                 │
                          └───────────┘          ▼                 ▼
                                          ┌─────────────────────────────┐
                                          │ Style resolution            │
                                          │ cascade · specificity       │
                                          │ inheritance → styled tree   │
                                          └──────────────┬──────────────┘
                                                         ▼
                                          ┌─────────────────────────────┐
                                          │ Layout (cells)              │
                                          │ box tree · block · inline   │
                                          │ flex → layout tree x,y,w,h  │
                                          └──────────────┬──────────────┘
                                                         ▼
                                          ┌─────────────────────────────┐
                                          │ Paint → display list        │
                                          └──────────────┬──────────────┘
                                                         ▼
                                          ┌─────────────────────────────┐
                                          │ Renderer                    │
                                          │ cell buffer · diff · ANSI   │
                                          │ (+ Kitty graphics)          │
                                          └─────────────────────────────┘

 Mouse / hints ──▶ hit-test layout tree ──▶ DOM node ──▶ navigate or event
```

### Event loop

A single UI thread owns the terminal and engine state. Everything else is a producer on a channel:

```
 input thread (keyboard, mouse, resize) ──┐
 fetch workers (HTTP)                   ──┼──▶ mpsc ──▶ event loop ──▶ update ──▶ render
 timers (scripts)                       ──┘
```

The loop blocks on the channel (no idle spin). Input is coalesced so a held key drains and paints once. Dirty flags (`restyle`, `relayout`, `repaint`) keep invalidation incremental—for example, `:hover` restyles and repaints without reparsing or relaying out.

### DOM

The document is an arena: `Vec<Node>` indexed by `NodeId`, with parent / child / sibling links as IDs. That keeps traversal cache-friendly for style and layout, makes node handles cheap to copy for hit-testing and link hints, and avoids reference-counted trees.

### Display list & renderer

Paint produces a display list (draw text, fill rect, …) independent of the terminal backend. Scrolling shifts the viewport over that list. The renderer maintains previous and next cell grids, diffs them, coalesces runs of identical style, and writes a single batched ANSI update per frame, optionally wrapped in synchronized-output sequences (CSI `?2026`).

### Layout units

Layout works in **terminal cells**, not CSS pixels: roughly `8px ≈ 1` cell width, `16px ≈ 1` line, with text measured via Unicode display width (CJK and emoji occupy two cells).

### Source layout

```
src/
  main.rs       # process setup, panic hook, event loop
  msg.rs        # message types for the event loop
  term/         # cells, frames, diff, ANSI emission, capability detection
  net/          # URL handling, HTTP client, charset
  html/         # tokenizer, tree builder
  dom/          # arena nodes and tree utilities
  css/          # CSS tokenizer, parser, selectors, values
  style/        # cascade, specificity, inheritance
  layout/       # box tree, block, inline, flex
  paint/        # layout tree → display list
  browser/      # chrome: history, URL bar, bindings, hit-test, search
```

## Performance

yata is built around low latency and idle efficiency:

| Metric | Target |
|--------|--------|
| Cold start → first frame | &lt; 50 ms |
| Keypress → updated screen (scroll, hints, focus) | &lt; 10 ms |
| Scroll step (cached display list) | &lt; 5 ms |
| Full pipeline (parse → style → layout → paint), typical article | &lt; 50 ms |
| Full pipeline, large Wikipedia article | &lt; 250 ms |
| Idle CPU (no input, no load) | 0% |
| Memory with a large article open | &lt; 100 MB |

Techniques that make those numbers realistic:

- Double-buffered cell grid with minimal diffed output
- Scroll and pan on the display list only—no restyle or relayout on the scroll path
- Selector matching via a rule index (bucket by rightmost simple selector)
- Arena DOM for fast style and layout walks
- Parallel stylesheet and image fetches without blocking the UI thread
- Width measurement cached per text run during layout

Use `F4` for a live timing overlay, or `--timing` for a scriptable summary on stderr.

## Requirements

- A modern terminal with reasonable Unicode and color support
- For full-color pages: truecolor (`COLORTERM=truecolor`) recommended
- For pixel images: a terminal that implements the Kitty graphics protocol (otherwise half-block rendering is used)
- macOS, Linux, or other Unix-like environments with a working terminal

## License

MIT — see [LICENSE](./LICENSE).

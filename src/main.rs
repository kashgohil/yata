use std::io::{self, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{env, iter, panic, process, thread};

use crossterm::event::{self, Event};
use crossterm::terminal;

use yata::browser::app::{self, App, Browser, DocumentWork, Effect};
use yata::browser::{error_page, yank};
use yata::js::console::Console;
use yata::msg::Msg;
use yata::term::{self, Renderer};
use yata::timers::{TimerRequest, Timers};
use yata::{headless, html, js, layout, net, style};

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let panic_requested = args.iter().any(|a| a == "--panic");
    let dump = args.iter().any(|a| a == "--dump");
    let dump_dom = args.iter().any(|a| a == "--dump-dom");
    let dump_text = args.iter().any(|a| a == "--dump-text");
    let dump_boxes = args.iter().any(|a| a == "--dump-boxes");
    let dump_js = args.iter().any(|a| a == "--dump-js");
    let timing = args.iter().any(|a| a == "--timing");
    // `yata <url>`: the first non-flag argument (`--panic` etc. are flags,
    // not URLs). In the TUI, no argument → no fetch, blank page.
    let url = args.into_iter().find(|a| !a.starts_with("--"));

    // Headless modes are decided and finished here — before the panic hook,
    // `Screen::new`, raw mode, or the input thread exist. `--dump`'s stdout
    // carries body bytes and nothing else, so piping to a file is byte-exact.
    // Exit codes are part of the spec: 0 success · 1 fetch failure · 2 usage.
    if dump || dump_dom || dump_text || dump_boxes || dump_js || timing {
        if [dump, dump_dom, dump_text, dump_boxes, dump_js, timing]
            .into_iter()
            .filter(|&f| f)
            .count()
            > 1
        {
            process::exit(usage());
        }
        let Some(url) = url else {
            process::exit(usage());
        };
        process::exit(if dump {
            run_dump(&url)
        } else if dump_dom {
            run_dump_dom(&url)
        } else if dump_text {
            run_dump_text(&url)
        } else if dump_boxes {
            run_dump_boxes(&url)
        } else if dump_js {
            run_dump_js(&url)
        } else {
            run_timing(&url)
        });
    }

    // Installed before the Screen exists so no panic window is uncovered.
    // Restore first, then report: the default hook's output must land on the
    // normal screen, not vanish with the alternate one.
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = term::restore();
        default_hook(info);
    }));

    let _screen = term::Screen::new()?;

    if panic_requested {
        panic!("deliberate panic via --panic; the terminal should be restored");
    }

    let (w, h) = terminal::size()?;
    let caps = term::detect_caps_from_env();
    let mut renderer = Renderer::new(w, h, caps);
    let mut app = Browser::with_caps(w, h, caps.kitty);

    let (tx, rx) = mpsc::channel();
    if let Some(url) = url {
        // Scheme defaulting for the CLI argument goes through the same helper
        // the URL bar uses. The id makes any previous generation stale; each
        // worker owns its own Sender clone.
        let url = net::normalize_url(&url);
        let effect = app.start_navigation(url);
        if let Some((id, request)) = effect.fetch {
            net::spawn_fetch(id, request, tx.clone());
        }
        if let Some((id, url, response, elapsed)) = effect.cached {
            net::spawn_cached(id, url, response, elapsed, tx.clone());
        }
    }
    // The loop keeps `tx` alive so a URL-bar commit can spawn a fetch (below);
    // the input thread gets its own clone. Because the loop holds a sender,
    // `recv` never ends on its own — input-thread death instead sends
    // `Msg::InputClosed`, which resolves to quit through the normal
    // `update` → `Effect` path (still just `effect.quit`, no extra loop branch).
    spawn_input_thread(tx.clone());
    // One timer thread for the app (M10.9), one more producer on the same
    // channel. It parks on a condvar until the earliest deadline, so a page
    // with nothing scheduled costs no wakeups at all.
    let timers = Timers::spawn(tx.clone());

    let mut out = io::stdout();
    render(&mut app, &mut renderer, &mut out)?;

    // Blocking recv is the only wait in the process: idle CPU must be 0%.
    while let Ok(first) = rx.recv() {
        let batch = iter::once(first).chain(iter::from_fn(|| rx.try_recv().ok()));
        let effect = apply_batch(&mut app, batch);
        if effect.quit {
            break;
        }
        // A committed navigation: `App` already started the generation, the
        // loop's only job is to spawn the worker with its own Sender clone.
        if let Some((id, request)) = effect.fetch {
            // A new generation: everything the old page scheduled is dead. The
            // `PageId` guard would drop those messages anyway; cancelling
            // means the thread does not wake for them at all.
            timers.apply(TimerRequest::CancelOthers { keep: id });
            net::spawn_fetch(id, request, tx.clone());
        }
        if let Some((id, url, response, elapsed)) = effect.cached {
            timers.apply(TimerRequest::CancelOthers { keep: id });
            net::spawn_cached(id, url, response, elapsed, tx.clone());
        }
        for document in effect.documents {
            match document {
                DocumentWork::Fetch(id, request) => {
                    timers.apply(TimerRequest::CancelOthers { keep: id });
                    net::spawn_fetch(id, request, tx.clone());
                }
                DocumentWork::Cached(id, url, response, elapsed) => {
                    timers.apply(TimerRequest::CancelOthers { keep: id });
                    net::spawn_cached(id, url, response, elapsed, tx.clone());
                }
            }
        }
        for request in effect.timers {
            timers.apply(request);
        }
        // One worker per `fetch()` (M10.12), like every other producer. The UI
        // thread never waits: the promise settles in a later turn.
        for (page, ask) in effect.fetches {
            net::spawn_js_fetch(
                page,
                ask.request,
                // The `Cookie:` header the binding's `credentials` reading
                // already settled (M11.7).
                ask.ask,
                ask.method,
                ask.headers,
                ask.body,
                tx.clone(),
            );
        }
        // One worker per linked stylesheet, all spawned before this turn's
        // render: they run in parallel with each other and with the page the
        // user is already reading (PLAN.md M4, UX §3.2).
        for (id, slot, request) in effect.sheets {
            net::spawn_stylesheet(id, slot, request, tx.clone());
        }
        // One worker per external script (M10.10), parallel with everything
        // else — the *fetches* race, the executions do not.
        for (id, slot, request) in effect.scripts {
            net::spawn_script(id, slot, request, tx.clone());
        }
        // One worker per image URL (M8), parallel with the page and sheets.
        for (id, request) in effect.images {
            net::spawn_image(id, request, tx.clone());
        }
        // The script pass (M10.2) goes back through the channel rather than
        // being called here, so it arrives as its own turn — after the render
        // at the bottom of this one. That ordering is the guarantee: the page
        // is on screen before any of its script runs.
        if let Some(id) = effect.run_scripts {
            let _ = tx.send(Msg::RunScripts { id });
        }
        // Clipboard is a side channel: OSC 52, not the cell buffer (CLAUDE.md).
        if let Some(text) = effect.yank {
            write!(out, "{}", yank::osc52_set_clipboard(&text))?;
            out.flush()?;
        }
        if effect.dirty {
            render(&mut app, &mut renderer, &mut out)?;
        }
    }
    Ok(())
}

/// The one usage line. Returns the usage exit code for `main` to exit with.
fn usage() -> i32 {
    eprintln!(
        "usage: yata [--dump | --dump-dom | --dump-text | --dump-boxes | --dump-js | --timing] <url>"
    );
    2
}

/// Column width for `--dump-text`. Fixed, not the terminal's: a greppable hook
/// whose output moved with the window would be useless in a test.
const DUMP_TEXT_WIDTH: u16 = 80;

/// One hop of a headless navigation (M11.7a) — a `Msg::Redirect` the dump
/// followed, kept so that `--timing` can replay the chain into `App` exactly as
/// the event loop saw it.
struct Hop {
    url: String,
    to: String,
    status: u16,
    elapsed: Duration,
    set_cookie: Vec<String>,
}

/// What one headless run has instead of an `App`: the jar the chain fills and
/// the console its warnings land in.
///
/// It exists because a redirect chain *is* a session (M11.7a): the 302 hands
/// out a cookie and the request that follows has to carry it, which needs
/// somewhere to keep it between two workers. Created per run and dropped with
/// it — nothing headless outlives one page, which is the rule
/// `headless::run_scripts_from` already followed with a jar of its own.
struct Session {
    cookies: js::cookies::Jar,
    console: Console,
}

impl Session {
    fn new() -> Session {
        Session {
            cookies: js::cookies::Jar::new(),
            console: Console::new(),
        }
    }

    /// The request for `url`, with whatever the jar says it may carry — the
    /// same one function `App::request` and `headless::script_request` ask.
    fn request(&self, url: String) -> net::Request {
        let cookie = js::cookies::header_for(&self.cookies, &url, &url, js::cookies::now());
        net::Request {
            url,
            cookie,
            method: net::Method::Get,
        }
    }

    /// A response's `Set-Cookie` lines, scoped to the URL that sent them.
    fn apply(&self, url: &str, lines: &[String]) {
        js::cookies::apply_set_cookie(&self.cookies, url, lines, js::cookies::now(), &self.console);
    }
}

/// What a headless navigation ended at, named rather than a tuple because two
/// of its fields (M11.7's `Set-Cookie` lines, M11.7a's hops) are the point of
/// two of the five modes and noise in the other three.
struct Loaded {
    url: String,
    status: u16,
    body: Vec<u8>,
    elapsed: Duration,
    set_cookie: Vec<String>,
    hops: Vec<Hop>,
}

/// The headless fetch: the *production* path — `net::normalize_url`, then
/// `net::spawn_fetch` — following redirects the same way the event loop does
/// (M11.7a), and handing back both what landed and the channel the final
/// worker is still sending into. Each mode drains that channel exactly as far
/// as it needs: `--dump` stops here (raw bytes need no parse, and must not wait
/// on one), `--dump-dom`/`--timing` go on to `Parsed`.
///
/// The hop loop is this file's, not `App`'s, and it is the same shape: one
/// worker per request, a message back, the next request decided here. The bound
/// is `app::MAX_REDIRECTS` — literally the constant the TUI stops at, because a
/// dump that gave up sooner or later than the browser would be a second
/// browser.
///
/// The `session` is what makes a login flow work headlessly: each response's
/// `Set-Cookie` lines go into its jar scoped to the URL that sent them, and the
/// request for the next hop asks that jar — the same order, through the same
/// two functions, as `App`'s handler.
fn headless_fetch(url: &str, session: &Session) -> Result<(Loaded, mpsc::Receiver<Msg>), String> {
    let (tx, rx) = mpsc::channel();
    net::spawn_fetch(
        net::PageId::headless(1),
        // The first request of the run, and the jar is empty: it carries no
        // cookies because there are none, not because this path skips asking.
        session.request(net::normalize_url(url)),
        tx.clone(),
    );
    let mut hops: Vec<Hop> = Vec::new();
    loop {
        match rx.recv() {
            Ok(Msg::Loaded {
                url,
                status,
                body,
                elapsed,
                set_cookie,
                ..
            }) => {
                // The last worker keeps its own sender, so a `Parsed` still
                // arrives; ours goes, so a mode that waits for one that will
                // never come gets a closed channel rather than a hang.
                drop(tx);
                session.apply(&url, &set_cookie);
                return Ok((
                    Loaded {
                        url,
                        status,
                        body,
                        elapsed,
                        set_cookie,
                        hops,
                    },
                    rx,
                ));
            }
            Ok(Msg::Redirect {
                url,
                to,
                status,
                elapsed,
                set_cookie,
                ..
            }) => {
                if hops.len() as u32 >= app::MAX_REDIRECTS {
                    return Err(format!(
                        "{to}: {}",
                        error_page::redirect_loop_reason(app::MAX_REDIRECTS)
                    ));
                }
                // The session first, then the request that carries it: the
                // ordering is the task, and it is the same one `App` follows.
                session.apply(&url, &set_cookie);
                net::spawn_fetch(
                    net::PageId::headless(1),
                    session.request(to.clone()),
                    tx.clone(),
                );
                hops.push(Hop {
                    url,
                    to,
                    status,
                    elapsed,
                    set_cookie,
                });
            }
            Ok(Msg::NetError { url, reason, .. }) => return Err(format!("{url}: {reason}")),
            Ok(_) => {}
            Err(_) => return Err("fetch worker exited without a result".into()),
        }
    }
}

/// Block until the `Parsed` that follows a `Loaded` on the same channel.
fn recv_parsed(rx: &mpsc::Receiver<Msg>) -> Result<(yata::dom::Dom, Duration), String> {
    loop {
        match rx.recv() {
            Ok(Msg::Parsed { dom, elapsed, .. }) => return Ok((dom, elapsed)),
            Ok(_) => {}
            Err(_) => return Err("fetch worker exited before parsing".into()),
        }
    }
}

/// `--dump`: raw body bytes to stdout, verbatim — no lossy decode, no added
/// newline. Any HTTP status dumps its body (curl semantics: a 404 page is
/// still a page). Exit 0, or 1 with the reason on stderr.
fn run_dump(url: &str) -> i32 {
    match headless_fetch(url, &Session::new()) {
        Ok((loaded, _rx)) => {
            let body = loaded.body;
            let mut out = io::stdout();
            if out.write_all(&body).and_then(|()| out.flush()).is_err() {
                return 1;
            }
            0
        }
        Err(reason) => {
            eprintln!("{reason}");
            1
        }
    }
}

/// `--dump-dom`: the parsed tree as indented text on stdout — the parser's
/// headless, greppable test hook, mirroring what `--dump` is for raw bytes.
/// The tree printed is the worker's own parse, not a second one.
fn run_dump_dom(url: &str) -> i32 {
    match headless_fetch(url, &Session::new()).and_then(|(_, rx)| recv_parsed(&rx)) {
        Ok((dom, _)) => {
            let mut out = io::stdout();
            if out
                .write_all(html::debug_tree(&dom).as_bytes())
                .and_then(|()| out.flush())
                .is_err()
            {
                return 1;
            }
            0
        }
        Err(reason) => {
            eprintln!("{reason}");
            1
        }
    }
}

/// `--dump-text`: the laid-out page as plain text on stdout — M3's headless
/// hook, mirroring `--dump-dom` for M2. Styles are dropped (a pipe has no
/// attributes) and the column is fixed at `DUMP_TEXT_WIDTH`, so the output is
/// the same everywhere. Parse and layout, but no TUI.
fn run_dump_text(url: &str) -> i32 {
    match headless_fetch(url, &Session::new())
        .and_then(|(l, rx)| recv_parsed(&rx).map(|p| (l.url, p)))
    {
        Ok((final_url, (mut dom, _))) => {
            // Scripts run before the dump, under the headless rule (one pass,
            // no timers) that `headless::run_scripts` documents. The URL is
            // handed over so `location` is real; external scripts are still
            // not fetched on this path.
            let _ = yata::headless::run_scripts(&mut dom, Some(&final_url));
            // No worker to fetch <link> sheets in a headless run, so the page
            // is styled by the UA sheet plus its own inline blocks.
            let sheets = style::sources::inline_sheets(&dom);
            let styles = style::style_tree(&dom, &sheets.iter().collect::<Vec<_>>());
            // Same never-blank rule as the TUI: a dump of a page that hides
            // itself pending JavaScript is useless, and this is the harness
            // the ladder tests read.
            let (lines, _revealed) = layout::layout_readable(&dom, &styles, DUMP_TEXT_WIDTH);
            let mut text = String::new();
            for line in lines {
                for span in &line.spans {
                    text.push_str(&span.text);
                }
                text.push('\n');
            }
            let mut out = io::stdout();
            if out
                .write_all(text.as_bytes())
                .and_then(|()| out.flush())
                .is_err()
            {
                return 1;
            }
            0
        }
        Err(reason) => {
            eprintln!("{reason}");
            1
        }
    }
}

/// `--dump-js`: run the page's `<script>` elements in document order and print
/// one line each — the headless hook for M10, mirroring `--dump-dom` for M2.
/// A script that throws is reported and the ones after it still run: a page
/// with a broken script is a degraded page, not an error page, so the exit
/// code is still 0. External `<script src>` does not appear until M10.10
/// fetches it.
fn run_dump_js(url: &str) -> i32 {
    // The one mode that keeps its session past the fetch: the jar the chain
    // filled is the jar the page's scripts read (M11.7, M11.7a).
    let session = Session::new();
    match headless_fetch(url, &session).and_then(|(l, rx)| recv_parsed(&rx).map(|p| (l, p))) {
        Ok((loaded, (mut dom, _))) => {
            // Pointed at a real URL, so external scripts are fetched here —
            // the one headless path that does; see `headless::run_scripts_from`.
            // The response's own cookies go in before the scripts run, the
            // same way `App` does it — `--dump-js` is what M11.25's ladder
            // sweep reads, so a script that reads a cookie the *server* set
            // has to see it here too (M11.7).
            let (runs, pending) = headless::run_scripts_fetching(
                &mut dom,
                &loaded.url,
                &session.cookies,
                &session.console,
            );
            let console = &session.console;
            let mut text = String::new();
            for run in runs {
                text.push_str(&run.dump_line());
                text.push('\n');
            }
            // Then the console pane's contents, in order (M10.7). This is the
            // assertion surface the rest of M10 tests against, so the two
            // sections are always both present and always in this order.
            for entry in console.entries() {
                text.push_str(&entry.to_string());
                text.push('\n');
            }
            // Headless never *runs* timers (M10.2's determinism rule), but it
            // says how many are outstanding, so a test can tell "the page
            // scheduled work" from "the page did nothing".
            if pending > 0 {
                text.push_str(&format!("timers pending {pending}\n"));
            }
            let mut out = io::stdout();
            if out
                .write_all(text.as_bytes())
                .and_then(|()| out.flush())
                .is_err()
            {
                return 1;
            }
            0
        }
        Err(reason) => {
            eprintln!("{reason}");
            1
        }
    }
}

/// `--dump-boxes`: the layout stage's box tree on stdout — exactly the lines
/// `F3` shows, at the fixed `DUMP_TEXT_WIDTH` column so the output is the same
/// everywhere. This is the hook `tests/layout.rs` goldens are read against, so
/// the flag, the inspector and the harness all print through one function
/// (`inspector::box_lines`): a divergence between them would make the goldens
/// pin something no one can see on screen.
fn run_dump_boxes(url: &str) -> i32 {
    let loaded =
        headless_fetch(url, &Session::new()).and_then(|(l, rx)| recv_parsed(&rx).map(|p| (l, p)));
    match loaded {
        Ok((loaded, (mut dom, _))) => {
            let final_url = loaded.url;
            // Relative `src` resolves against the URL the body actually came
            // from (redirects included), like the TUI's discovery does.
            let text = yata::headless::box_dump(&mut dom, Some(&final_url), DUMP_TEXT_WIDTH);
            let mut out = io::stdout();
            if out
                .write_all(text.as_bytes())
                .and_then(|()| out.flush())
                .is_err()
            {
                return 1;
            }
            0
        }
        Err(reason) => {
            eprintln!("{reason}");
            1
        }
    }
}

/// `--timing`: the same headless fetch, then one full first-frame render —
/// the same `draw` + `present` pair the event loop times, into a sink — and
/// the `Timings` table (exactly the `F4` overlay's rows) on stderr. Stdout
/// stays empty.
fn run_timing(url: &str) -> i32 {
    let loaded =
        headless_fetch(url, &Session::new()).and_then(|(l, rx)| recv_parsed(&rx).map(|p| (l, p)));
    let (loaded, (dom, parse_elapsed)) = match loaded {
        Ok(ok) => ok,
        Err(reason) => {
            eprintln!("{reason}");
            return 1;
        }
    };
    // The normal App + Renderer at the real terminal size when there is one
    // (best-effort — a pipe has none), else 80×24.
    let (w, h) = terminal::size().unwrap_or((80, 24));
    let caps = term::detect_caps_from_env();
    let mut renderer = Renderer::new(w, h, caps);
    let mut app = App::with_caps(w, h, caps.kitty);
    // The chain that got here, replayed as the messages the event loop saw
    // (M11.7a). Not decoration: `timings.fetch` is the *whole* chain, and an
    // App handed only the landing page would print a fast fetch with two slow
    // hops hidden inside it — which is exactly the number this deliverable
    // exists to keep honest. Each hop's `Set-Cookie` goes in scoped to the hop,
    // through the same handler the TUI uses. The `Effect` asking for the next
    // request is ignored here: this fetch has already happened.
    let id = app.start_fetch(loaded.hops.first().map_or(&loaded.url, |h| &h.url).clone());
    for hop in loaded.hops {
        app.update(Msg::Redirect {
            id,
            url: hop.url,
            to: hop.to,
            status: hop.status,
            elapsed: hop.elapsed,
            set_cookie: hop.set_cookie,
        });
    }
    app.update(Msg::Loaded {
        id,
        url: loaded.url,
        status: loaded.status,
        body: loaded.body,
        elapsed: loaded.elapsed,
        content_type: None,
        // The response's own, through the same path the TUI uses — the jar is
        // on the load path now, and `--timing` is what measures the load path.
        set_cookie: loaded.set_cookie,
        metadata: Default::default(),
    });
    app.update(Msg::Parsed {
        id,
        dom,
        elapsed: parse_elapsed,
    });
    // The pass the event loop would send itself after painting (M10.2), so the
    // `script` row is real work and not a stub. Timers never run here — see
    // `headless::run_scripts` for why that rule exists.
    app.update(Msg::RunScripts { id });

    let started = Instant::now();
    app.draw(renderer.frame());
    // A sink write cannot fail; the Result exists for real terminals.
    let _ = renderer.present(&mut io::sink());
    app.record_frame(started.elapsed());

    for row in app.timings().rows() {
        eprintln!("{row}");
    }
    0
}

/// Input coalescing: apply every already-queued message, then decide **once**
/// whether to redraw, so a flood of events costs one render, not one each.
/// Quit short-circuits — nothing rendered or applied after it matters.
trait UiApp {
    fn update(&mut self, msg: Msg) -> Effect;
    fn size(&self) -> (u16, u16);
    fn draw(&self, frame: &mut yata::term::Frame);
    fn kitty_frame(&mut self) -> Option<Vec<u8>>;
    fn record_frame(&mut self, duration: Duration);
}

impl UiApp for App {
    fn update(&mut self, msg: Msg) -> Effect {
        App::update(self, msg)
    }
    fn size(&self) -> (u16, u16) {
        App::size(self)
    }
    fn draw(&self, frame: &mut yata::term::Frame) {
        App::draw(self, frame)
    }
    fn kitty_frame(&mut self) -> Option<Vec<u8>> {
        App::kitty_frame(self)
    }
    fn record_frame(&mut self, duration: Duration) {
        App::record_frame(self, duration)
    }
}

impl UiApp for Browser {
    fn update(&mut self, msg: Msg) -> Effect {
        Browser::update(self, msg)
    }
    fn size(&self) -> (u16, u16) {
        Browser::size(self)
    }
    fn draw(&self, frame: &mut yata::term::Frame) {
        Browser::draw(self, frame)
    }
    fn kitty_frame(&mut self) -> Option<Vec<u8>> {
        Browser::kitty_frame(self)
    }
    fn record_frame(&mut self, duration: Duration) {
        Browser::record_frame(self, duration)
    }
}

fn apply_batch(app: &mut impl UiApp, msgs: impl Iterator<Item = Msg>) -> Effect {
    let mut effect = Effect::default();
    for msg in msgs {
        let e = app.update(msg);
        effect.dirty |= e.dirty;
        // Keep only the last fetch of the batch: an earlier commit is already a
        // stale generation, so spawning its worker would be pure waste.
        if let Some(tab) = e.fetch.as_ref().map(|(id, _)| id.tab) {
            preserve_other_tab_document(&mut effect, tab);
            effect.fetch = e.fetch;
            effect.cached = None;
        }
        if let Some(tab) = e.cached.as_ref().map(|(id, ..)| id.tab) {
            preserve_other_tab_document(&mut effect, tab);
            effect.cached = e.cached;
            effect.fetch = None;
        }
        effect.documents.extend(e.documents);
        // Sheets accumulate rather than replace: two parses in one batch each
        // want their own sheets fetched, and a stale generation's are dropped
        // by the id guard in `App::update`, not here.
        effect.sheets.extend(e.sheets);
        // Images accumulate like sheets: each parse in a batch may request its
        // own URLs; stale generations are dropped by the PageId guard in App.
        effect.images.extend(e.images);
        // Scripts accumulate like sheets and images: each parse in a batch may
        // request its own, and stale generations are dropped by the `PageId`
        // guard in `App`, not here.
        effect.scripts.extend(e.scripts);
        effect.fetches.extend(e.fetches);
        effect.timers.extend(e.timers);
        if e.yank.is_some() {
            effect.yank = e.yank;
        }
        // Keep only the last pass request, for the same reason as `fetch`: if
        // two parses landed in one batch, the earlier page is already a stale
        // generation and its pass would be dropped by the id guard anyway.
        // Losing this field entirely is a silent bug — the loop would render
        // pages that never run their script — so it is merged explicitly
        // rather than by a `..` that would not exist to be forgotten.
        if e.run_scripts.is_some() {
            effect.run_scripts = e.run_scripts;
        }
        if e.tab.is_some() {
            effect.tab = e.tab;
        }
        if e.quit {
            effect.quit = true;
            break;
        }
    }
    effect
}

fn preserve_other_tab_document(effect: &mut Effect, incoming: net::TabId) {
    effect.documents.retain(|work| match work {
        DocumentWork::Fetch(id, _) | DocumentWork::Cached(id, ..) => id.tab != incoming,
    });
    if let Some((id, request)) = effect.fetch.take()
        && id.tab != incoming
    {
        effect.documents.push(DocumentWork::Fetch(id, request));
    }
    if let Some((id, url, response, elapsed)) = effect.cached.take()
        && id.tab != incoming
    {
        effect
            .documents
            .push(DocumentWork::Cached(id, url, response, elapsed));
    }
}

fn render(app: &mut impl UiApp, renderer: &mut Renderer, out: &mut impl Write) -> io::Result<()> {
    let started = Instant::now();
    // A coalesced batch of resizes syncs the renderer once, at the final size.
    let (w, h) = app.size();
    if (renderer.frame().width(), renderer.frame().height()) != (w, h) {
        renderer.resize(w, h);
    }
    app.draw(renderer.frame());
    renderer.present(out)?;
    // Kitty graphics are a side channel like OSC 52 (CLAUDE.md: only the cell
    // renderer owns per-cell writes; images ride after the synchronized frame).
    if let Some(kitty) = app.kitty_frame() {
        out.write_all(&kitty)?;
        out.flush()?;
    }
    // A plain setter, deliberately not a Msg: a message would dirty the app
    // and every frame would schedule the next. The statusline shows this on
    // whatever paint comes next.
    app.record_frame(started.elapsed());
    Ok(())
}

/// Detached producer: blocks in `event::read()`, forwards key and resize
/// events into the channel. Never joined — it sits in `read` at shutdown and
/// process exit reaps it.
fn spawn_input_thread(tx: mpsc::Sender<Msg>) {
    thread::spawn(move || {
        loop {
            let msg = match event::read() {
                Ok(Event::Key(key)) => Msg::Key(key),
                Ok(Event::Mouse(mouse)) => Msg::Mouse(mouse),
                Ok(Event::Resize(w, h)) => Msg::Resize(w, h),
                Ok(_) => continue,
                // Input is gone for good. Signal the loop to quit (if it is
                // already gone the channel is closed, and the failed send is
                // fine), then stop.
                Err(_) => {
                    let _ = tx.send(Msg::InputClosed);
                    return;
                }
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: char) -> Msg {
        Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    #[test]
    fn batch_of_dead_keys_is_one_decision_with_no_redraw() {
        let mut app = App::new(80, 24);
        // 'z' is bound to nothing; a key that does nothing must not redraw, no
        // matter how many arrive in one batch.
        let effect = apply_batch(&mut app, (0..200).map(|_| key('z')));
        assert_eq!(effect, Effect::default());
    }

    #[test]
    fn batch_of_scroll_keys_coalesces_to_one_redraw() {
        let mut app = App::new(80, 6); // 5-row page area
        let id = app.start_fetch("http://x/".into());
        let body = (0..100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes();
        app.update(Msg::Loaded {
            id,
            url: "http://x/".into(),
            status: 200,
            body,
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        // 200 'j' now scroll for real: still one coalesced redraw decision, not
        // 200 renders. (Clamping to the last page is covered by viewport tests.)
        let effect = apply_batch(&mut app, (0..200).map(|_| key('j')));
        assert!(effect.dirty);
        assert!(!effect.quit);
        assert!(effect.fetch.is_none());
    }

    #[test]
    fn a_batched_parse_still_reaches_the_loop_with_its_script_request() {
        // Regression: `apply_batch` merges `Effect` field by field, so a field
        // added to `Effect` and forgotten here is dropped silently. When that
        // field is `run_scripts`, the symptom is that no page in the real TUI
        // ever runs its script, while every test that calls `App::update`
        // directly still passes.
        let mut app = App::new(80, 24);
        let id = app.start_fetch("http://x/".into());
        let html = b"<p>hi</p><script>1</script>".to_vec();
        let effect = apply_batch(
            &mut app,
            [
                Msg::Loaded {
                    id,
                    url: "http://x/".into(),
                    status: 200,
                    body: html.clone(),
                    elapsed: Duration::ZERO,
                    content_type: None,
                    set_cookie: Vec::new(),
                    metadata: Default::default(),
                },
                Msg::Parsed {
                    id,
                    dom: html::parse(&String::from_utf8(html).unwrap()),
                    elapsed: Duration::ZERO,
                },
                key('z'),
            ]
            .into_iter(),
        );
        assert_eq!(effect.run_scripts, Some(id));
    }

    #[test]
    fn batch_of_dirtying_messages_coalesces_to_one_redraw() {
        let mut app = App::new(80, 24);
        // 200 resize wiggles: one redraw decision at the final state, not 200
        // renders.
        let msgs = (0..200).map(|i| Msg::Resize(80, 24 + (i % 2)));
        let effect = apply_batch(&mut app, msgs);
        assert!(effect.dirty && !effect.quit);
        assert_eq!(app.size(), (80, 25), "state reflects the last message");
    }

    #[test]
    fn quit_in_a_batch_reports_quit_and_stops_applying() {
        let mut app = App::new(80, 24);
        // The resize after 'q' must never be applied: quit short-circuits the
        // batch, so state still shows the pre-quit size.
        let msgs = vec![key('j'), key('q'), Msg::Resize(10, 10)];
        assert!(apply_batch(&mut app, msgs.into_iter()).quit);
        assert_eq!(app.size(), (80, 24), "message after quit was applied");
    }

    #[test]
    fn batch_ending_in_input_closed_reports_quit() {
        let mut app = App::new(80, 24);
        // Input-thread death rides the same coalescing path as any quit: no
        // special loop branch, just `effect.quit`.
        let msgs = vec![key('j'), Msg::InputClosed];
        assert!(apply_batch(&mut app, msgs.into_iter()).quit);
    }

    #[test]
    fn empty_batch_does_nothing() {
        let mut app = App::new(80, 24);
        assert_eq!(apply_batch(&mut app, iter::empty()), Effect::default());
    }

    #[test]
    fn batch_forwards_image_urls_from_parsed() {
        // Regression: apply_batch used to drop effect.images, so the loop never
        // spawned image workers even though App::update listed them.
        let mut app = App::new(80, 20);
        let id = app.start_fetch("http://site.test/page".into());
        app.update(Msg::Loaded {
            id,
            url: "http://site.test/page".into(),
            status: 200,
            body: b"<img src=pic.png width=8 height=16>".to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        let effect = apply_batch(
            &mut app,
            std::iter::once(Msg::Parsed {
                id,
                dom: html::parse(r#"<img src="pic.png" width="8" height="16">"#),
                elapsed: Duration::ZERO,
            }),
        );
        assert!(
            effect
                .images
                .iter()
                .any(|(i, r)| *i == id && r.url == "http://site.test/pic.png"),
            "coalesced effect must carry image URLs: {:?}",
            effect.images
        );
    }

    #[test]
    fn batch_keeps_only_the_last_fetch_commit() {
        // Two URL-bar commits in one coalesced batch: the first is already a
        // stale generation by the time the loop sees the effect, so only the
        // second may be spawned (M1.5: `apply_batch` keeps the last fetch).
        let commit = |url: &str| {
            let mut msgs = vec![key('o')];
            msgs.extend(url.chars().map(key));
            msgs.push(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
            msgs
        };
        let mut msgs = commit("a.com");
        msgs.extend(commit("b.com"));

        let mut app = App::new(80, 24);
        let effect = apply_batch(&mut app, msgs.into_iter());
        let (id, url) = effect.fetch.expect("a commit must surface a fetch");
        assert_eq!(url.url, "https://b.com", "an earlier commit leaked through");
        assert_eq!(
            id,
            net::PageId::headless(2),
            "the id must be the second generation"
        );
    }

    #[test]
    fn a_batch_keeps_document_work_for_two_different_tabs() {
        let mut browser = Browser::new(80, 24);
        let a = browser
            .start_navigation("https://a.test/start".into())
            .fetch
            .unwrap()
            .0;
        browser.update(key('t'));
        let b = browser
            .start_navigation("https://b.test/start".into())
            .fetch
            .unwrap()
            .0;
        let effect = apply_batch(
            &mut browser,
            [
                Msg::Redirect {
                    id: a,
                    url: "https://a.test/start".into(),
                    to: "https://a.test/end".into(),
                    status: 302,
                    elapsed: Duration::ZERO,
                    set_cookie: vec![],
                },
                Msg::Redirect {
                    id: b,
                    url: "https://b.test/start".into(),
                    to: "https://b.test/end".into(),
                    status: 302,
                    elapsed: Duration::ZERO,
                    set_cookie: vec![],
                },
            ]
            .into_iter(),
        );
        let mut pages = effect
            .documents
            .iter()
            .map(|work| match work {
                DocumentWork::Fetch(id, _) | DocumentWork::Cached(id, ..) => id.tab,
            })
            .collect::<Vec<_>>();
        pages.push(effect.fetch.expect("newest tab lost its redirect").0.tab);
        pages.sort_by_key(|tab| tab.0);
        assert_eq!(pages, [a.tab, b.tab]);
    }
}

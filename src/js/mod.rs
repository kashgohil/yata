//! The JavaScript host: one engine, one context, three hard limits.
//!
//! M10 embeds QuickJS through `rquickjs` (PLAN.md §6 M10; the human sign-off
//! CLAUDE.md rule 1 requires covers `rquickjs` and nothing else). This module
//! owns the engine and the rules that keep a page's script from taking the
//! browser down with it: the execution budget, the memory and stack caps, and
//! the boundary that keeps engine types out of the rest of the tree. What a
//! page can *reach* is `bindings`; a name that module does not define — and
//! `console.log` is one until M10.7 — is undefined by design, not by accident.
//!
//! ## Where scripts sit in the architecture (M10.2)
//!
//! **Script execution is an event source, not a pipeline stage.** It sits
//! beside the fetcher: a pass is asked for by a message, it owns the DOM for
//! the duration of one tick, and what comes out is a new version of the DOM
//! that the pipeline re-runs forward from style. No stage reaches backward and
//! none mutates its input, so CLAUDE.md's purity invariant is intact — what
//! changed is that the DOM now has *two* producers, the parser and this, where
//! it used to have one.
//!
//! The browser-classic model is not available here and is not wanted: our
//! parse finishes on a worker and arrives whole in `Msg::Parsed`, so there is
//! no token stream left to stop at a `<script>` and nothing for
//! `document.write` to write into. The pass runs after the page is on screen,
//! as its own turn of the event loop, so a script that spends its whole budget
//! cannot delay first paint (UX §3.2).
//!
//! ## One host per page generation
//!
//! A `Host` is created when a page starts running script and dropped when the
//! page goes away. Navigation drops it, and with it every global, closure and
//! (later) listener and timer the page created. Page JS state cannot outlive
//! its page because there is nowhere for it to live: everything hangs off the
//! `Runtime` inside this struct. Nothing here is shared, cloned or cached
//! across pages — if you find yourself wanting to keep a `Host` alive across a
//! navigation, that is the bug.
//!
//! ## The boundary
//!
//! No `rquickjs` type crosses out of `src/js/`. The public surface is
//! [`Host::eval`], [`JsValue`] and [`JsError`] — plain owned Rust data. That is
//! what keeps engine lifetimes (`'js`) out of `App`, `browser/` and the tests,
//! and it is what would make replacing QuickJS a change to one directory. A
//! `pub fn` outside this directory that mentions an `rquickjs` type is a bug.
//!
//! ## Threading
//!
//! The host lives on the UI thread and no worker ever touches it. The type
//! system agrees: `rquickjs::Runtime` is `!Send` without the `parallel`
//! feature, which we do not enable, so `Host` cannot be moved to another
//! thread at all. It also *must* be created on the thread that will run it —
//! QuickJS records the native stack top at `Runtime::new` and measures
//! `STACK_LIMIT` against it.
//!
//! ## What a page can reach
//!
//! Exactly what `bindings` hands it, and nothing by accident: QuickJS's core
//! intrinsics are the language only — no file, process or network access
//! exists for a page to find. Since M10.4 that means `window`, `document` and
//! the read half of the DOM; everything else a real browser has is absent
//! rather than stubbed, so a page's feature detection gets a true answer.
//!
//! ## `q` still quits (PLAN.md §1.5)
//!
//! JS runs on the UI thread, so the honest statement is: **keys queue in the
//! channel while a script runs and are served when the tick ends; the tick ends
//! within one [`SCRIPT_BUDGET`], so worst-case quit latency is one budget.**
//! Measured on this machine, `while (true) {}` returns in **100.03 ms** and the
//! worst of several runaway shapes in **100.25 ms** — the interrupt costs
//! ~0.25 ms over the budget, because QuickJS polls the handler every few
//! thousand bytecode ops rather than continuously. This holds per script; a
//! page with many runaway scripts pays it once each, which is what M10.13
//! re-measures under adversarial pages before deciding whether the budget
//! alone is enough.

// Private: the object model is reached by running script, never by calling
// into it from Rust. Keeping the module closed is what stops `DomSlot::lend`
// from becoming an API somebody outside the engine can hand a tree to.
mod bindings;
pub mod console;
pub mod queue;
pub mod sources;

use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rquickjs::context::EvalOptions;
use rquickjs::{
    CatchResultExt, CaughtError, Context, Function, Object, Persistent, Runtime, Type, Value,
};

use crate::dom::Dom;
use crate::js::bindings::{TimerAsk, TimerQueue};
use crate::js::console::{Console, Level};
use crate::timers::TimerId;

/// What one script did: the name it was known by, and its completion value or
/// its error. Data, not output — `--dump-js` prints it today and M10.7's
/// console pane will show it.
#[derive(Clone, PartialEq, Debug)]
pub struct ScriptRun {
    pub name: String,
    pub outcome: Result<JsValue, JsError>,
}

/// Run a ready prefix of the page's script queue — one tick (M10.10).
///
/// The DOM is **lent**, not shared. `App` owns the `Dom` before and after,
/// there is no copy, and nothing holds the tree between ticks: this moves it
/// into the host's slot (`bindings::DomSlot`, which every binding reads
/// through) and moves it back out before returning. Outside a tick the slot is
/// empty and every binding throws, which is the honest answer for a callback
/// that runs when no tick owns a tree.
///
/// `page` is the page generation the tree belongs to. Handles minted during
/// the tick carry it, so one held past a navigation refuses to resolve rather
/// than reading whatever node now sits at its index.
///
/// `finished` says this was the last prefix — the queue has nothing pending —
/// which is when `DOMContentLoaded` and `load` fire.
///
/// A script that throws does not stop the ones after it. Browsers behave the
/// same way, and the discipline matches a failed stylesheet: a page with a
/// broken script is a *degraded page*, never an error page.
///
/// `host` is the page generation's host, created here on first use: a page
/// with no script never starts an engine at all, which is why this takes the
/// slot rather than a live `Host`.
pub fn run_prefix(
    host: &mut Option<Host>,
    dom: &mut Dom,
    page: u64,
    console: &Console,
    scripts: Vec<(String, String)>,
    finished: bool,
) -> Vec<ScriptRun> {
    if scripts.is_empty() && !finished {
        return Vec::new();
    }

    let host = match host {
        Some(host) => host,
        None => {
            // A page whose only scripts all failed to fetch never starts an
            // engine: there is nothing to run and nothing to fire events at.
            if scripts.is_empty() {
                return Vec::new();
            }
            match Host::new(console) {
                Ok(new) => host.insert(new),
                // The engine itself would not start. The page is degraded, not
                // broken — and the failure is reported rather than swallowed.
                Err(error) => {
                    console.push(
                        Level::Error,
                        Some(error.source.clone()),
                        error.line,
                        &error.message,
                    );
                    return vec![ScriptRun {
                        name: error.source.clone(),
                        outcome: Err(error),
                    }];
                }
            }
        }
    };

    // The lend, and the whole of it: the tree goes into the slot the bindings
    // read through, and comes back out below. `Dom::new_document` is a
    // one-node placeholder standing in the caller's variable meanwhile — the
    // caller cannot observe it, because it holds a `&mut` for the whole call.
    host.dom
        .lend(std::mem::replace(dom, Dom::new_document()), page);

    let runs: Vec<ScriptRun> = scripts
        .into_iter()
        .map(|(name, source)| {
            let outcome = host.eval(&name, &source);
            // Uncaught exceptions join the console in the order they happened,
            // interleaved with whatever the script logged before throwing —
            // that interleaving is most of the story.
            if let Err(error) = &outcome {
                console.push(
                    Level::Error,
                    Some(error.source.clone()),
                    error.line,
                    &error.message,
                );
            }
            ScriptRun { name, outcome }
        })
        .collect();

    // The two events a page hangs almost all of its behaviour on, in the order
    // a browser fires them — but only once the queue has nothing left to run,
    // so a listener registered by the *last* external script still sees them.
    //
    // They fire even if every script threw or failed to arrive: a page whose
    // first script broke may still have registered a handler in its second.
    if finished {
        for (target, kind) in [
            (Target::Document, "DOMContentLoaded"),
            (Target::Window, "load"),
        ] {
            if let Err(error) = host.dispatch(target, kind, kind == "DOMContentLoaded") {
                console.push(
                    Level::Error,
                    Some(error.source.clone()),
                    error.line,
                    &error.message,
                );
            }
        }
    }

    // Promise jobs the page queued run before the tick ends — a `.then` that
    // never fires is indistinguishable to a page from a broken engine.
    host.pump_microtasks(console);

    // Back to the caller. A handle a script stored in a global outlives this,
    // and that is correct: it stays valid for as long as the page does, and
    // stops resolving the moment a different page is lent in.
    if let Some(returned) = host.dom.take() {
        *dom = returned;
    }
    runs
}

/// The whole document-order pass in one call, with every external script
/// treated as unfetchable — what `run_pass` was before M10.10 split execution
/// across arrivals.
///
/// Test-only: the engine's real callers drive the queue a prefix at a time,
/// and a test that is about `classList` should not have to say so.
#[cfg(test)]
pub fn run_pass(
    host: &mut Option<Host>,
    dom: &mut Dom,
    page: u64,
    console: &Console,
) -> Vec<ScriptRun> {
    let (mut queue, externals) = queue::ScriptQueue::new(sources::sources(dom), console);
    for external in externals {
        queue.fill(external.slot, None);
    }
    let ready = queue.take_ready_prefix();
    let finished = queue.is_finished();
    run_prefix(host, dom, page, console, ready, finished)
}

/// Wall clock a single script gets before the interrupt handler stops it.
///
/// The cost is symmetric and this is the trade: an honest page whose script
/// genuinely needs more than 100 ms of straight-line CPU is killed, and a
/// hostile page freezes the UI for 100 ms per script instead of forever.
///
/// 100 ms is 10× the keypress→screen budget (PLAN.md §4) and under the ~200 ms
/// at which a stall stops reading as "slow" and starts reading as "hung" — one
/// overrun costs the user a visibly late keystroke, not a dead browser. Against
/// that, the ladder's real inline scripts are single-digit milliseconds of
/// work, so the headroom for honest pages is one to two orders of magnitude,
/// even in a `dev` build (where QuickJS itself is compiled unoptimized — `cc`
/// inherits cargo's `OPT_LEVEL`, so tests run a far slower engine than the
/// release binary a user gets).
pub const SCRIPT_BUDGET: Duration = Duration::from_millis(100);

/// JS heap ceiling. PLAN.md §4 budgets a whole Wikipedia page at under 100 MB;
/// the DOM, style and layout data for one already account for the bulk of
/// that, so JS gets 32 MB — far above what any ladder page's script uses, far
/// below what would let an allocation loop reach the process limit. Overrun
/// surfaces as a `JsError`, not an OOM kill.
const MEMORY_LIMIT: usize = 32 * 1024 * 1024;

/// Native stack ceiling for the interpreter. QuickJS recurses on the C stack
/// for JS calls, so unbounded recursion is a native stack overflow — which is
/// an abort, not an error. 512 KB is a few thousand frames (deeper than any
/// honest page recurses) and sits far below both the main thread's 8 MB and a
/// Rust test thread's 2 MB, so the engine's check always trips first.
const STACK_LIMIT: usize = 512 * 1024;

/// Deadline sentinel meaning "no script is running, never interrupt". The
/// handler stays installed for the host's whole life, so between ticks it has
/// to be told to do nothing.
const NO_DEADLINE: u64 = u64::MAX;

/// Shared with the interrupt handler, which QuickJS calls from inside the
/// interpreter loop every few thousand bytecode ops.
#[derive(Debug)]
struct Budget {
    /// Nanoseconds since the host's `origin` at which the running script must
    /// stop, or [`NO_DEADLINE`].
    deadline: AtomicU64,
    /// Set by the handler when it actually stopped a script. This — not
    /// string-matching QuickJS's "interrupted" message — is how `eval` knows
    /// an error was an overrun.
    tripped: AtomicBool,
}

/// The engine, its context, and its limits. See the module docs for the
/// lifetime rule (one per page generation) and the threading rule (UI thread,
/// created where it runs).
pub struct Host {
    /// The prelude's entry points — `dispatch` (M10.8) and `fireTimer`
    /// (M10.9) — held across ticks.
    ///
    /// A `Persistent` rather than a global name: the page never sees it, so it
    /// cannot be called, overwritten or deleted, and the engine keeps the only
    /// way to start a dispatch. That is what makes "no synthetic dispatch API"
    /// a property of the build rather than a promise.
    ///
    /// **Declared first on purpose.** Rust drops fields in declaration order,
    /// and a `Persistent` roots a JS object: releasing it *after* the runtime
    /// is freed trips QuickJS's own assertion that nothing is still alive
    /// (`list_empty(&rt->gc_obj_list)`), which aborts the process. It has to
    /// go before `context` and `runtime`.
    entries: Persistent<Object<'static>>,
    /// Timer work the page asked for during a tick, drained by `App` and
    /// handed to the timer thread by the event loop.
    timers: TimerQueue,
    /// The engine handle. `Context` keeps the runtime alive on its own, so
    /// this is not what makes the host valid — it is how the limits get set
    /// and how the tests read the heap back. Holding it is deliberate: it is
    /// the only handle to the engine, and dropping it here would mean
    /// re-deriving one to do anything runtime-wide later.
    #[allow(dead_code)]
    runtime: Runtime,
    context: Context,
    budget: Arc<Budget>,
    /// Fixed reference point for the deadline arithmetic: `Instant` has no
    /// integer form, and the interrupt handler needs a lock-free one.
    origin: Instant,
    /// The tree the current tick is working on. Shared with every binding
    /// closure, which is why it is an `Rc` and why it is empty between ticks —
    /// see `bindings`.
    dom: Rc<bindings::DomSlot>,
}

impl Host {
    /// Build a host with the budget, memory and stack limits armed. Fails only
    /// if the engine cannot allocate its runtime or context.
    pub fn new(console: &Console) -> Result<Self, JsError> {
        let runtime = Runtime::new().map_err(|e| JsError::internal(&e.to_string()))?;
        runtime.set_memory_limit(MEMORY_LIMIT);
        runtime.set_max_stack_size(STACK_LIMIT);

        let context = Context::full(&runtime).map_err(|e| JsError::internal(&e.to_string()))?;

        let budget = Arc::new(Budget {
            deadline: AtomicU64::new(NO_DEADLINE),
            tripped: AtomicBool::new(false),
        });
        let origin = Instant::now();

        let handler_budget = Arc::clone(&budget);
        runtime.set_interrupt_handler(Some(Box::new(move || {
            let now = origin.elapsed().as_nanos() as u64;
            if now < handler_budget.deadline.load(Ordering::Relaxed) {
                return false;
            }
            handler_budget.tripped.store(true, Ordering::Relaxed);
            // Returning true raises an *uncatchable* exception, so a page
            // cannot swallow its own overrun with try/catch.
            true
        })));

        let dom = Rc::new(bindings::DomSlot::default());
        let timers = TimerQueue::default();
        let entries = context.with(|ctx| {
            bindings::install(&ctx, &dom, console, &timers)
                .map(|entry_points| Persistent::save(&ctx, entry_points))
                .catch(&ctx)
                .map_err(|caught| JsError::from_caught("<bindings>", &caught))
            // A prelude that will not install is a broken engine, not a broken
            // page: fail here rather than hand every script a DOM-less window.
        })?;

        Ok(Host {
            entries,
            timers,
            runtime,
            context,
            budget,
            origin,
            dom,
        })
    }

    /// Run `source` as a classic script under the budget. `name` is what the
    /// page called this script (a URL, or something like `inline#2`); it names
    /// the source in errors and in QuickJS's own backtraces.
    ///
    /// `&mut self` is deliberate: it makes a script that re-enters the host
    /// while another is running a compile error rather than a runtime
    /// surprise, which is the property M10.8's dispatch and M10.9's timers
    /// will need.
    ///
    /// Errors never poison the host — the next `eval` on the same host runs
    /// normally, and globals set before the error survive, because a page's
    /// broken script must not disable the rest of its page.
    pub fn eval(&mut self, name: &str, source: &str) -> Result<JsValue, JsError> {
        // `EvalOptions` is `#[non_exhaustive]`, so it can only be built by
        // mutating the default.
        let mut options = EvalOptions::default();
        // A classic `<script>` is global, sloppy-mode code. rquickjs defaults
        // `strict` to true, which is not what a browser does: under it `x = 1`
        // throws instead of creating a global, and enough of the ladder's
        // script would break on that alone.
        options.global = true;
        options.strict = false;
        options.filename = Some(name.to_string());

        self.under_budget(name, |ctx| {
            ctx.eval_with_options::<Value, _>(source, options)
                .map(|value| JsValue::from_value(&value))
        })
    }

    /// Dispatch one event through the prelude's dispatcher (M10.8), and report
    /// whether a listener called `preventDefault()`.
    ///
    /// `target` says where the event starts: a node, `document`, or `window`.
    /// A page with no listeners at all still pays only a map lookup per node
    /// on the path.
    pub fn dispatch(&mut self, target: Target, kind: &str, bubbles: bool) -> Result<bool, JsError> {
        let (tag, id) = match target {
            Target::Node(id) => ("node", id),
            Target::Document => ("document", 0),
            Target::Window => ("window", 0),
        };
        let entries = self.entries.clone();
        let kind = kind.to_string();
        // Under the same budget as a script: a listener that loops forever is
        // a runaway script that happens to have been reached by a click.
        self.under_budget(&format!("{kind} listener"), move |ctx| {
            entries
                .restore(ctx)?
                .get::<_, Function>("dispatch")?
                .call::<_, bool>((tag, id, kind.as_str(), bubbles))
        })
    }

    /// Fire one timer's callback (M10.9). Unknown ids — a timer cancelled
    /// after its message was already in the channel — do nothing.
    pub fn fire_timer(&mut self, id: TimerId) -> Result<(), JsError> {
        let entries = self.entries.clone();
        self.under_budget("timer callback", move |ctx| {
            entries
                .restore(ctx)?
                .get::<_, Function>("fireTimer")?
                .call::<_, ()>((id.0 as f64,))
        })
    }

    /// How many timers the page is still holding callbacks for. What
    /// `--dump-js` reports so a test can tell "the page scheduled work" from
    /// "the page did nothing".
    pub fn pending_timers(&mut self) -> usize {
        let entries = self.entries.clone();
        self.under_budget("timer count", move |ctx| {
            entries
                .restore(ctx)?
                .get::<_, Function>("pending")?
                .call::<_, f64>(())
        })
        .map_or(0, |count| count as usize)
    }

    /// Run every queued promise job to quiescence (M10.9).
    ///
    /// QuickJS queues them; nothing runs them unless we do, so without this a
    /// `.then` never fires. Bounded, because `Promise.resolve().then(f)` that
    /// re-queues itself is a loop the queue can never drain — and unlike a
    /// runaway script it never returns to the interrupt handler, so the
    /// execution budget cannot see it. The bound turns that into an error in
    /// the console instead of a hung UI.
    fn pump_microtasks(&mut self, console: &Console) {
        for _ in 0..MAX_MICROTASKS {
            match self.runtime.execute_pending_job() {
                Ok(true) => {}
                Ok(false) => return,
                Err(exception) => {
                    let error = exception.0.with(|ctx| {
                        let caught = CaughtError::from_error(&ctx, rquickjs::Error::Exception);
                        JsError::from_caught("microtask", &caught)
                    });
                    console.push(
                        Level::Error,
                        Some(error.source.clone()),
                        error.line,
                        &error.message,
                    );
                }
            }
        }
        console.push(
            Level::Error,
            None,
            None,
            "a promise kept queueing more work: stopped after the microtask limit \
             so the page could be drawn",
        );
    }

    /// Timer work the page asked for during the tick that just ended.
    pub fn take_timer_requests(&self) -> Vec<TimerAsk> {
        self.timers.drain()
    }

    /// Run `body` inside the context with the execution budget armed, and turn
    /// whatever it produces into plain data. The one place the deadline is set
    /// and cleared, so every way into JS — a script, a listener, and M10.9's
    /// timers — is interruptible on the same terms.
    fn under_budget<T>(
        &mut self,
        name: &str,
        body: impl for<'js> FnOnce(&rquickjs::Ctx<'js>) -> rquickjs::Result<T>,
    ) -> Result<T, JsError> {
        let deadline = (self.origin.elapsed() + SCRIPT_BUDGET).as_nanos() as u64;
        self.budget.tripped.store(false, Ordering::Relaxed);
        self.budget.deadline.store(deadline, Ordering::Relaxed);

        let result = self.context.with(|ctx| {
            body(&ctx)
                .catch(&ctx)
                .map_err(|caught| JsError::from_caught(name, &caught))
        });

        // Disarm before returning: the handler outlives the tick, and a stale
        // deadline would kill the *next* script the instant it started.
        self.budget.deadline.store(NO_DEADLINE, Ordering::Relaxed);
        let timed_out = self.budget.tripped.swap(false, Ordering::Relaxed);

        result.map_err(|error| JsError { timed_out, ..error })
    }

    /// Bytes the JS heap currently holds. Not used by the engine itself — it
    /// is how the tests show the memory cap is the thing that fired.
    #[cfg(test)]
    fn heap_bytes(&self) -> usize {
        self.runtime.memory_usage().malloc_size as usize
    }
}

/// Dispatch one event at `target`, as its own tick (M10.8).
///
/// Returns whether a listener called `preventDefault()`, which is the caller's
/// cue to skip the default action — following a link, in every case this
/// milestone has. A page with no script has no host and therefore no
/// listeners, so this costs nothing at all rather than starting an engine to
/// find that out.
///
/// The DOM is lent for the dispatch exactly as it is for the document-order
/// pass: listeners read and mutate through the same bindings, and the caller
/// runs one invalidation cycle after.
pub fn dispatch(
    host: &mut Option<Host>,
    dom: &mut Dom,
    page: u64,
    console: &Console,
    target: Target,
    kind: &str,
) -> bool {
    let Some(host) = host.as_mut() else {
        return false;
    };

    host.dom
        .lend(std::mem::replace(dom, Dom::new_document()), page);
    // Which events bubble, as the DOM says. It matters more than it looks:
    // `DOMContentLoaded` bubbles, which is the only reason a listener put on
    // `window` for it ever runs, and `load` does not.
    let bubbles = matches!(kind, "click" | "DOMContentLoaded");
    let prevented = match host.dispatch(target, kind, bubbles) {
        Ok(prevented) => prevented,
        Err(error) => {
            console.push(
                Level::Error,
                Some(error.source.clone()),
                error.line,
                &error.message,
            );
            false
        }
    };
    host.pump_microtasks(console);
    if let Some(returned) = host.dom.take() {
        *dom = returned;
    }
    prevented
}

/// How many promise jobs one tick may run before the engine calls it a loop.
///
/// A microtask that queues another microtask is legal and common — a chain of
/// `.then`s is exactly that — so the number has to be far above any honest
/// page's chain. 10,000 is: a page resolving a hundred promises with a
/// hundred links each fits, and a self-requeueing loop hits it in a few
/// milliseconds rather than never.
const MAX_MICROTASKS: usize = 10_000;

/// Fire one timer's callback, as its own tick (M10.9).
///
/// The same shape as [`dispatch`]: the DOM is lent for the callback, promise
/// jobs the callback queued are pumped to quiescence before it returns, and
/// the caller runs one invalidation cycle after.
pub fn fire_timer(
    host: &mut Host,
    dom: &mut Dom,
    page: u64,
    console: &Console,
    id: TimerId,
) -> Result<(), JsError> {
    host.dom
        .lend(std::mem::replace(dom, Dom::new_document()), page);
    let outcome = host.fire_timer(id);
    if let Err(error) = &outcome {
        console.push(
            Level::Error,
            Some(error.source.clone()),
            error.line,
            &error.message,
        );
    }
    host.pump_microtasks(console);
    if let Some(returned) = host.dom.take() {
        *dom = returned;
    }
    outcome
}

/// Where an event starts. `document` and `window` are event targets without
/// being nodes, which is why this is not just a `NodeId`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Node(u32),
    Document,
    Window,
}

/// The script name and line from a listener's stack, skipping frames inside
/// the prelude — a page author cannot act on a line number in our glue.
fn script_frame(stack: &str) -> (Option<String>, Option<u32>) {
    for frame in stack.lines() {
        if let Some((file, line)) = frame_location(frame)
            && file != "<bindings>"
        {
            return (Some(file.to_string()), Some(line));
        }
    }
    (None, None)
}

/// A JavaScript value, owned and engine-free, in the shapes the rest of the
/// engine needs. `Other` carries the value's `typeof` string (`"object"`,
/// `"function"`, …) rather than a stringification: coercing an object to a
/// string runs the page's own `toString`, and value conversion is not a place
/// where user code should be able to run, throw, or spend the budget.
#[derive(Debug, Clone, PartialEq)]
pub enum JsValue {
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Other(String),
}

impl JsValue {
    fn from_value(value: &Value<'_>) -> JsValue {
        if value.is_undefined() {
            return JsValue::Undefined;
        }
        if value.is_null() {
            return JsValue::Null;
        }
        if let Some(b) = value.as_bool() {
            return JsValue::Bool(b);
        }
        if let Some(i) = value.as_int() {
            return JsValue::Num(f64::from(i));
        }
        if let Some(f) = value.as_float() {
            return JsValue::Num(f);
        }
        if let Some(s) = value.as_string() {
            // JS strings are UTF-16 and may hold unpaired surrogates, which
            // have no UTF-8 form. Those become `Other` rather than a silently
            // mangled `Str`.
            return s
                .to_string()
                .map_or_else(|_| JsValue::Other("string".to_string()), JsValue::Str);
        }
        JsValue::Other(typeof_name(value.type_of()).to_string())
    }

    /// How the value reads when a page *throws* it instead of an `Error`.
    /// Unquoted, because that is what a browser shows for `throw 'nope'`; the
    /// `Display` impl quotes instead, because a dump has to tell `42` from
    /// `"42"`.
    fn describe(&self) -> String {
        match self {
            JsValue::Undefined => "undefined".to_string(),
            JsValue::Null => "null".to_string(),
            JsValue::Bool(b) => b.to_string(),
            JsValue::Num(n) => n.to_string(),
            JsValue::Str(s) => s.clone(),
            JsValue::Other(kind) => kind.clone(),
        }
    }
}

impl fmt::Display for JsValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Quoted so a dump distinguishes the number 42 from the string
            // "42"; `{:?}` also escapes the newlines a page can put in one.
            JsValue::Str(s) => write!(f, "{s:?}"),
            other => write!(f, "{}", other.describe()),
        }
    }
}

impl ScriptRun {
    /// One line of `--dump-js`. The grammar is fixed, because this is the
    /// harness the rest of M10 tests against:
    ///
    /// ```text
    /// NAME ok VALUE
    /// NAME error LINE: MESSAGE     (when the engine reported a line)
    /// NAME error: MESSAGE          (when it did not)
    /// ```
    pub fn dump_line(&self) -> String {
        match &self.outcome {
            Ok(value) => format!("{} ok {value}", self.name),
            Err(error) => match error.line {
                Some(line) => format!("{} error {line}: {}", self.name, error.message),
                None => format!("{} error: {}", self.name, error.message),
            },
        }
    }
}

/// A script that did not finish: a parse error, an uncaught throw, or a limit
/// this module imposed. All `String`s and plain data — no engine lifetimes, so
/// this survives the tick that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsError {
    pub message: String,
    /// The `name` passed to [`Host::eval`], not whatever the engine inferred.
    pub source: String,
    /// Line within `source`, when the engine reported one. Parse errors and
    /// uncaught `Error`s have it; a thrown non-`Error` value does not, because
    /// there is no stack on it to read.
    pub line: Option<u32>,
    pub stack: String,
    /// The script overran [`SCRIPT_BUDGET`] and this module stopped it. Not a
    /// page bug — a page decision.
    pub timed_out: bool,
}

impl JsError {
    /// A failure of the host itself, with no page script to blame.
    fn internal(message: &str) -> JsError {
        JsError {
            message: message.to_string(),
            source: "<host>".to_string(),
            line: None,
            stack: String::new(),
            timed_out: false,
        }
    }

    fn from_caught(name: &str, caught: &CaughtError<'_>) -> JsError {
        let (message, stack) = match caught {
            CaughtError::Exception(exception) => (
                exception.message().unwrap_or_default(),
                exception.stack().unwrap_or_default(),
            ),
            // A page can throw anything: `throw 'nope'`, `throw {code: 5}`.
            // QuickJS also reports a memory-limit hit this way, as a thrown
            // `null`, because it has no memory left to build an Error object
            // in — so an allocation bomb's message reads `null`.
            CaughtError::Value(value) => (JsValue::from_value(value).describe(), String::new()),
            // Not a JS exception: the engine failed on our side of the
            // boundary (a conversion, an allocation).
            CaughtError::Error(error) => (error.to_string(), String::new()),
        };

        JsError {
            message,
            source: name.to_string(),
            line: line_from_stack(&stack, name),
            stack,
            timed_out: false,
        }
    }
}

impl fmt::Display for JsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{}: {}", self.source, line, self.message),
            None => write!(f, "{}: {}", self.source, self.message),
        }
    }
}

/// What JavaScript's `typeof` calls a value of this engine type.
///
/// rquickjs's `Type` is finer-grained than the language is — it distinguishes
/// `Constructor` from `Function`, and `Array`/`Promise`/`Proxy` from `Object`.
/// Those are engine internals, and the whole point of this boundary is that
/// they do not cross it: what leaves is the name the language itself uses.
fn typeof_name(ty: Type) -> &'static str {
    match ty {
        Type::Undefined | Type::Uninitialized => "undefined",
        Type::Null => "object", // `typeof null === "object"`, famously.
        Type::Bool => "boolean",
        Type::Int | Type::Float => "number",
        Type::String => "string",
        Type::Symbol => "symbol",
        Type::BigInt => "bigint",
        Type::Function | Type::Constructor => "function",
        _ => "object",
    }
}

/// Read the line number back out of QuickJS's stack string.
///
/// `Exception` exposes only `message` and `stack`, so the stack is the only
/// place the location exists.
///
/// The frame to report is the first one **in the script being run**, not the
/// first one on the stack. When a binding throws — an invalid selector, a
/// stale handle — the innermost frame is inside `<bindings>`, and reporting
/// its line would point a page author at a line number in our prelude that
/// has nothing to do with their bug. The first frame is the fallback for a
/// stack that never names the script, such as a parse error.
fn line_from_stack(stack: &str, source: &str) -> Option<u32> {
    let mut innermost = None;
    for frame in stack.lines() {
        let Some((file, line)) = frame_location(frame) else {
            continue;
        };
        if file == source {
            return Some(line);
        }
        innermost.get_or_insert(line);
    }
    innermost
}

/// One stack frame's file and line. QuickJS writes them in two shapes:
///
/// ```text
///     at page.js:1:1             (parse errors)
///     at <eval> (page.js:3:1)    (uncaught throws)
/// ```
///
/// Splitting from the right is what makes a URL source name (`https://…`,
/// full of colons) parse correctly.
fn frame_location(frame: &str) -> Option<(&str, u32)> {
    let frame = frame.trim();
    let location = match frame.rsplit_once('(') {
        Some((_, inside)) => inside.strip_suffix(')')?,
        None => frame.strip_prefix("at ")?,
    };
    let (file_and_line, _column) = location.rsplit_once(':')?;
    let (file, line) = file_and_line.rsplit_once(':')?;
    Some((file, line.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::{Console, Host, JsValue, SCRIPT_BUDGET, line_from_stack, run_pass};
    use std::time::Instant;

    /// The document-order pass over a parsed page, as `App` and the headless
    /// hooks run it.
    fn pass(html: &str) -> Vec<String> {
        let mut dom = crate::html::parse(html);
        let mut host = None;
        run_pass(&mut host, &mut dom, 1, &Console::new())
            .iter()
            .map(super::ScriptRun::dump_line)
            .collect()
    }

    #[test]
    fn scripts_run_in_document_order_sharing_one_global_scope() {
        // Order is observable through the globals: each script appends, so the
        // final value is the order they ran in.
        assert_eq!(
            pass(
                "<script>var order = 'a';</script>\
                 <p>text between them</p>\
                 <script>order += 'b';</script>\
                 <script>order;</script>"
            ),
            [
                "inline#1 ok undefined",
                "inline#2 ok \"ab\"",
                "inline#3 ok \"ab\""
            ]
        );
    }

    #[test]
    fn a_script_that_throws_does_not_stop_the_ones_after_it() {
        // Browsers keep going, and the discipline matches a failed stylesheet:
        // a page with a broken script is a degraded page, not an error page.
        assert_eq!(
            pass(
                "<script>var reached = 1;</script>\
                 <script>null.x;</script>\
                 <script>reached + 1;</script>"
            ),
            [
                "inline#1 ok undefined",
                "inline#2 error 1: cannot read property 'x' of null",
                "inline#3 ok 2"
            ]
        );
    }

    #[test]
    fn a_page_with_no_script_starts_no_engine() {
        let mut dom = crate::html::parse("<p>just prose</p>");
        let mut host = None;
        assert!(run_pass(&mut host, &mut dom, 1, &Console::new()).is_empty());
        assert!(
            host.is_none(),
            "an engine was started for a page with no script"
        );
    }

    #[test]
    fn an_external_script_runs_nothing_yet_but_holds_its_slot() {
        // M10.10 fills it. Until then it contributes no line, and — the part
        // that matters — the inline script after it keeps the name its slot
        // gives it.
        assert_eq!(
            pass("<script src=lib.js></script><script>1 + 1;</script>"),
            ["inline#2 ok 2"]
        );
    }

    #[test]
    fn the_dump_line_grammar_is_fixed() {
        // This is the harness the rest of M10 tests against, so its output is
        // pinned rather than left to drift.
        assert_eq!(
            pass("<script>undefined</script>"),
            ["inline#1 ok undefined"]
        );
        assert_eq!(pass("<script>null</script>"), ["inline#1 ok null"]);
        assert_eq!(pass("<script>42</script>"), ["inline#1 ok 42"]);
        assert_eq!(pass("<script>'hi'</script>"), ["inline#1 ok \"hi\""]);
        assert_eq!(pass("<script>({})</script>"), ["inline#1 ok object"]);
        assert_eq!(
            pass("<script>throw 'nope'</script>"),
            ["inline#1 error: nope"]
        );
        assert_eq!(
            pass("<script>\nthrow new Error('boom')</script>"),
            ["inline#1 error 2: boom"]
        );
    }

    #[test]
    fn the_pass_holds_no_dom_after_it_returns() {
        // "Lent, not shared" is a borrow, not a field: the caller can mutate
        // the tree the instant the pass returns, which would not compile if
        // anything in `src/js` had kept a reference to it.
        let mut dom = crate::html::parse("<script>1</script>");
        let mut host = None;
        let runs = run_pass(&mut host, &mut dom, 1, &Console::new());
        assert_eq!(runs.len(), 1);
        let fresh = dom.create_element("p", vec![]);
        dom.append(dom.root, fresh).unwrap();
        assert_eq!(dom.node(fresh).parent, Some(dom.root));
    }

    fn host() -> Host {
        Host::new(&Console::new()).expect("engine starts")
    }

    #[test]
    fn a_runaway_script_is_stopped_within_the_budget() {
        let mut host = host();
        let started = Instant::now();
        let error = host.eval("hang.js", "while (true) {}").unwrap_err();
        let elapsed = started.elapsed();

        assert!(error.timed_out, "overrun must be reported as one: {error}");
        // Wall clock, not a flag: a regression that disables the interrupt
        // handler hangs here instead of quietly passing.
        assert!(
            elapsed < 2 * SCRIPT_BUDGET,
            "runaway script ran for {elapsed:?}, budget is {SCRIPT_BUDGET:?}"
        );
        assert!(
            elapsed >= SCRIPT_BUDGET,
            "stopped early ({elapsed:?}) — an honest slow script would be killed too"
        );
    }

    #[test]
    fn a_runaway_script_cannot_catch_its_own_interruption() {
        let mut host = host();
        // The budget is not a page-visible exception: if try/catch could
        // swallow it, `while(true)` inside a catch-all would be unkillable.
        let error = host
            .eval("hang.js", "try { while (true) {} } catch (e) { 'caught' }")
            .unwrap_err();
        assert!(error.timed_out, "{error}");
    }

    #[test]
    fn the_host_survives_an_overrun_and_runs_the_next_script() {
        let mut host = host();
        host.eval("hang.js", "globalThis.before = 1; while (true) {}")
            .unwrap_err();

        // Each script gets its own budget: if the overrun's deadline carried
        // over, this would be interrupted the instant it started.
        assert_eq!(host.eval("next.js", "2 + 3"), Ok(JsValue::Num(5.0)));
        // And what the killed script had already done still stands — a page is
        // not rolled back because one of its scripts ran long.
        assert_eq!(
            host.eval("next.js", "globalThis.before"),
            Ok(JsValue::Num(1.0))
        );
    }

    #[test]
    fn unbounded_recursion_errors_instead_of_overflowing_the_native_stack() {
        let mut host = host();
        let error = host
            .eval("deep.js", "function f() { return f(); } f();")
            .unwrap_err();

        assert!(
            error.message.contains("stack size exceeded"),
            "expected a stack error, got {error}"
        );
        assert!(!error.timed_out);
        assert_eq!(host.eval("after.js", "1"), Ok(JsValue::Num(1.0)));
    }

    #[test]
    fn an_allocation_loop_errors_instead_of_being_oom_killed() {
        let mut host = host();
        // Big blocks on purpose: the cap has to be reached in a handful of
        // iterations, not a hundred thousand, or the *budget* fires first and
        // this test silently stops testing the memory cap on a slow machine.
        let error = host
            .eval(
                "bomb.js",
                "let a = []; while (true) { a.push(new Array(500000).fill(0)); }",
            )
            .unwrap_err();

        // The memory cap must be what stopped it, not the clock: if the
        // deadline fires first the cap is untested and an allocation bomb
        // inside a *fast* script would still reach the process limit.
        assert!(!error.timed_out, "the budget fired before the memory cap");
        assert!(
            host.heap_bytes() <= super::MEMORY_LIMIT,
            "heap grew past the cap: {} bytes",
            host.heap_bytes()
        );
    }

    #[test]
    fn a_syntax_error_carries_the_source_name_and_a_line_number() {
        let mut host = host();
        let error = host
            .eval("page.js", "let ok = 1;\nfunction ( {\n")
            .unwrap_err();

        assert_eq!(error.source, "page.js");
        assert_eq!(error.line, Some(2), "{error}");
        assert!(!error.message.is_empty());
    }

    #[test]
    fn an_uncaught_throw_carries_the_line_it_was_thrown_on() {
        let mut host = host();
        let error = host
            .eval("page.js", "let a = 1;\nlet b = 2;\nnull.x;\n")
            .unwrap_err();

        assert_eq!(error.source, "page.js");
        assert_eq!(error.line, Some(3), "{error}");
    }

    #[test]
    fn a_thrown_value_is_an_error_and_the_host_stays_usable() {
        let mut host = host();
        let error = host.eval("page.js", "throw 'plain string'").unwrap_err();

        assert_eq!(error.message, "plain string");
        assert_eq!(error.source, "page.js");
        assert_eq!(error.line, None, "a thrown string has no stack to read");

        // A poisoned context is a bug: one broken script must not disable the
        // rest of the page.
        assert_eq!(host.eval("next.js", "'ok'"), Ok(JsValue::Str("ok".into())));
    }

    #[test]
    fn page_controlled_bytes_cannot_panic_the_host() {
        let mut host = host();
        // Both of these reach a `CString` inside the engine bindings, so an
        // interior NUL is the shortest path from page bytes to a panic. It
        // must be an error instead — for the script text and for the name the
        // page's URL supplies.
        assert!(host.eval("page.js", "var a = 1;\0var a = 2;").is_err());
        assert!(host.eval("na\0me.js", "1").is_err());
        assert_eq!(host.eval("after.js", "'ok'"), Ok(JsValue::Str("ok".into())));
    }

    #[test]
    fn a_global_from_one_script_is_visible_to_the_next() {
        let mut host = host();
        // This is what makes document-order execution meaningful in M10.2.
        host.eval("first.js", "var shared = 'from first';").unwrap();
        assert_eq!(
            host.eval("second.js", "shared"),
            Ok(JsValue::Str("from first".into()))
        );
    }

    #[test]
    fn scripts_run_sloppy_like_a_classic_script_element() {
        let mut host = host();
        // Strict mode would throw here; a browser creates a global.
        host.eval("first.js", "implicitGlobal = 7;").unwrap();
        assert_eq!(
            host.eval("second.js", "implicitGlobal"),
            Ok(JsValue::Num(7.0))
        );
    }

    #[test]
    fn a_new_host_shares_nothing_with_the_last_one() {
        // The lifetime rule: navigation drops the host, so page state cannot
        // outlive its page.
        let mut first = host();
        first
            .eval("page.js", "var leaked = 'from the old page';")
            .unwrap();
        drop(first);

        let mut second = host();
        assert_eq!(
            second.eval("page.js", "typeof leaked"),
            Ok(JsValue::Str("undefined".into()))
        );
    }

    #[test]
    fn values_cross_the_boundary_as_owned_data() {
        let mut host = host();
        assert_eq!(host.eval("v.js", "undefined"), Ok(JsValue::Undefined));
        assert_eq!(host.eval("v.js", "null"), Ok(JsValue::Null));
        assert_eq!(host.eval("v.js", "true"), Ok(JsValue::Bool(true)));
        assert_eq!(host.eval("v.js", "42"), Ok(JsValue::Num(42.0)));
        assert_eq!(host.eval("v.js", "0.5"), Ok(JsValue::Num(0.5)));
        assert_eq!(host.eval("v.js", "'hi'"), Ok(JsValue::Str("hi".into())));
        // Objects and functions do not stringify — that would run page code.
        assert_eq!(
            host.eval("v.js", "({a: 1})"),
            Ok(JsValue::Other("object".into()))
        );
        assert_eq!(
            host.eval("v.js", "(function f() {})"),
            Ok(JsValue::Other("function".into()))
        );
        // A statement, not an expression: the completion value is undefined.
        assert_eq!(host.eval("v.js", "var x = 1;"), Ok(JsValue::Undefined));
    }

    #[test]
    fn stack_frames_parse_in_both_of_quickjs_shapes() {
        assert_eq!(
            line_from_stack("    at page.js:12:3\n", "page.js"),
            Some(12)
        );
        assert_eq!(
            line_from_stack("    at <eval> (page.js:7:1)\n", "page.js"),
            Some(7)
        );
        // A source name that is a URL: colons in the name must not confuse it.
        assert_eq!(
            line_from_stack(
                "    at f (https://example.com/a.js:31:4)\n    at g (x:1:1)\n",
                "https://example.com/a.js"
            ),
            Some(31)
        );
        assert_eq!(line_from_stack("", "page.js"), None);
        assert_eq!(line_from_stack("garbage", "page.js"), None);
    }

    #[test]
    fn an_error_thrown_inside_a_binding_reports_the_page_line() {
        // The innermost frame is in our prelude. Reporting *its* line would
        // point a page author at a line number in code they cannot see.
        let stack = "    at querySelector (<bindings>:73:74)\n    at <eval> (inline#1:2:10)\n";
        assert_eq!(line_from_stack(stack, "inline#1"), Some(2));
        // A stack that never names the script still reports something.
        assert_eq!(line_from_stack(stack, "other.js"), Some(73));
    }
}

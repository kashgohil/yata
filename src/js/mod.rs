//! The JavaScript host: one engine, one context, three hard limits.
//!
//! M10 embeds QuickJS through `rquickjs` (PLAN.md §6 M10; the human sign-off
//! CLAUDE.md rule 1 requires covers `rquickjs` and nothing else). This module
//! owns the engine and the rules that keep a page's script from taking the
//! browser down with it. It executes nothing on its own — `<script>` ordering
//! is M10.2, and there are no bindings here at all, so a fixture calling
//! `console.log` fails in this module by design.
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
//! Nothing, yet, and nothing by accident later: QuickJS's core intrinsics are
//! the language only — no file, process or network access exists to bind. A
//! page reaches exactly what M10.4 onward hands it and no more.
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
//! page with
//! many runaway scripts pays it once each, which is what M10.13 re-measures
//! under adversarial pages before deciding whether the budget alone is enough.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rquickjs::context::EvalOptions;
use rquickjs::{CatchResultExt, CaughtError, Context, Runtime, Type, Value};

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
}

impl Host {
    /// Build a host with the budget, memory and stack limits armed. Fails only
    /// if the engine cannot allocate its runtime or context.
    pub fn new() -> Result<Self, JsError> {
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

        Ok(Host {
            runtime,
            context,
            budget,
            origin,
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
        let deadline = (self.origin.elapsed() + SCRIPT_BUDGET).as_nanos() as u64;
        self.budget.tripped.store(false, Ordering::Relaxed);
        self.budget.deadline.store(deadline, Ordering::Relaxed);

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

        let result = self.context.with(|ctx| {
            match ctx
                .eval_with_options::<Value, _>(source, options)
                .catch(&ctx)
            {
                Ok(value) => Ok(JsValue::from_value(&value)),
                Err(caught) => Err(JsError::from_caught(name, &caught)),
            }
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

    /// How the value reads when a page throws it instead of an `Error`.
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
            line: line_from_stack(&stack),
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
/// `Exception` exposes only `message` and `stack`, so the stack's first frame
/// is the only place the location exists. It comes in two shapes:
///
/// ```text
///     at page.js:1:1             (parse errors)
///     at <eval> (page.js:3:1)    (uncaught throws)
/// ```
///
/// Splitting from the right is what makes a URL source name (`https://…`,
/// full of colons) parse correctly.
fn line_from_stack(stack: &str) -> Option<u32> {
    let frame = stack.lines().next()?.trim();
    let location = match frame.rsplit_once('(') {
        Some((_, inside)) => inside.strip_suffix(')')?,
        None => frame.strip_prefix("at ")?,
    };
    let (file_and_line, _column) = location.rsplit_once(':')?;
    let (_file, line) = file_and_line.rsplit_once(':')?;
    line.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{Host, JsValue, SCRIPT_BUDGET, line_from_stack};
    use std::time::Instant;

    fn host() -> Host {
        Host::new().expect("engine starts")
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
        assert_eq!(line_from_stack("    at page.js:12:3\n"), Some(12));
        assert_eq!(line_from_stack("    at <eval> (page.js:7:1)\n"), Some(7));
        // A source name that is a URL: colons in the name must not confuse it.
        assert_eq!(
            line_from_stack("    at f (https://example.com/a.js:31:4)\n    at g (x:1:1)\n"),
            Some(31)
        );
        assert_eq!(line_from_stack(""), None);
        assert_eq!(line_from_stack("garbage"), None);
    }
}

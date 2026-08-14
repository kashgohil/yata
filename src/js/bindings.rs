//! The DOM as JavaScript sees it (M10.4): the global object, `document`, node
//! handles and queries.
//!
//! ## Shape: numeric primitives in Rust, the object model in JS
//!
//! Everything Rust exposes takes and returns plain numbers, strings and arrays
//! — a node is a `NodeId`'s `u32`. The object model on top (the `Node` class,
//! `document`, interning, the stale-handle guard) is a JavaScript prelude
//! evaluated once when the host is built. Two reasons:
//!
//! - It keeps M10.1's boundary trivially true. No `rquickjs` type reaches a
//!   signature, nothing has to hold a JS object across a tick, and prototypes,
//!   getters and identity are written in the language that already has them.
//! - It is *less* code, and the code it is is the readable half. Building the
//!   same prototype chain through `Object::set_prototype` in Rust would be
//!   three times the lines and none of them would say what they mean.
//!
//! The prelude receives the primitive object as an argument and the global is
//! deleted immediately after, so a page cannot reach past the object model to
//! the raw ids underneath.
//!
//! ## The DOM is in a slot, not in the closures
//!
//! Binding closures live as long as the context — across ticks — but the DOM
//! is only lent for the duration of one. So the closures share an
//! [`DomSlot`]: the tick puts the tree in, takes it back out at the end, and
//! between ticks the slot is empty and every binding throws. That is the
//! honest answer for a callback that runs when no tick owns a tree (M10.9's
//! timers will be the first), and it costs one `RefCell` borrow per call.
//!
//! ## Handles carry their page
//!
//! A handle is a node id **plus the page generation it was minted in**. There
//! is only ever one page's DOM in memory, so without that a handle held by a
//! stale closure would read whatever node now sits at that index — the wrong
//! element, silently, with no error anywhere. The guard lives in one place in
//! the prelude (`idOf`) and every read funnels through it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use rquickjs::context::EvalOptions;
use rquickjs::{Ctx, Exception, Function, Object, Result as JsResult};

use crate::css;
use crate::dom::{Dom, DomError, NodeData, NodeId};
use crate::html;
use crate::js::console::{self, Console};
use crate::js::storage::{Area, Storage, origin_of};
use crate::style::{StyleContext, matching};

/// The tree the current tick is working on, shared between the host and every
/// binding closure. Empty between ticks.
#[derive(Default)]
pub struct DomSlot {
    dom: RefCell<Option<Dom>>,
    /// The page's post-redirect URL, which `location` is parsed from. Lent
    /// with the tree, because it belongs to the same page and changes only
    /// when the tree does.
    url: RefCell<String>,
    /// The page generation the slot currently holds. Kept when the tree is
    /// taken back out, because handles minted this page stay valid between
    /// ticks — it is the *next page* that must invalidate them.
    page: Cell<u64>,
}

impl DomSlot {
    /// Lend the tree to the bindings for one tick.
    pub fn lend(&self, dom: Dom, page: u64, url: &str) {
        self.page.set(page);
        *self.url.borrow_mut() = url.to_string();
        *self.dom.borrow_mut() = Some(dom);
    }

    /// Take it back. Panics only if a binding kept a borrow across a JS call,
    /// which no binding does: each one borrows, reads, and returns.
    pub fn take(&self) -> Option<Dom> {
        self.dom.borrow_mut().take()
    }

    /// Read the lent tree, or raise a JS exception if no tick owns one.
    fn with<T>(&self, ctx: &Ctx<'_>, f: impl FnOnce(&Dom) -> T) -> JsResult<T> {
        match self.dom.borrow().as_ref() {
            Some(dom) => Ok(f(dom)),
            None => Err(Exception::throw_message(
                ctx,
                "the DOM is not available outside a script tick",
            )),
        }
    }

    /// The same, for a binding that writes. Every mutation goes through
    /// `Dom`'s API (M10.3), so the invariants that API enforces — no cycles,
    /// no reused ids, only elements holding children — hold for anything a
    /// page can do, and `Dom::version` counts the edit for the dirty signal.
    fn with_mut<T>(&self, ctx: &Ctx<'_>, f: impl FnOnce(&mut Dom) -> T) -> JsResult<T> {
        match self.dom.borrow_mut().as_mut() {
            Some(dom) => Ok(f(dom)),
            None => Err(Exception::throw_message(
                ctx,
                "the DOM is not available outside a script tick",
            )),
        }
    }
}

/// Timer work a tick asked for, collected for the event loop to hand to the
/// timer thread. `App` drains it after every tick, exactly as it drains the
/// console: a binding decides, the loop dispatches, and nothing inside
/// `src/js/` touches a thread.
#[derive(Clone, Default)]
pub struct TimerQueue {
    requests: Rc<RefCell<Vec<TimerAsk>>>,
}

/// A timer id and what to do with it: `Some(delay)` schedules, `None` cancels.
pub type TimerAsk = (u64, Option<Duration>);

impl TimerQueue {
    /// `Some(delay)` schedules, `None` cancels.
    fn push(&self, id: u64, delay: Option<Duration>) {
        self.requests.borrow_mut().push((id, delay));
    }

    /// Take everything asked for since the last drain.
    pub fn drain(&self) -> Vec<TimerAsk> {
        std::mem::take(&mut *self.requests.borrow_mut())
    }
}

/// A navigation a script asked for (M10.11), collected for the event loop.
///
/// JS never touches the network: `location.href = …` records this, `App`
/// turns it into the same `Effect::fetch` the URL bar and a link click
/// produce, and the loop performs it. One per tick, last assignment wins —
/// a script that assigns in a loop navigates once.
#[derive(Clone, Default)]
pub struct NavQueue {
    request: Rc<RefCell<Option<NavRequest>>>,
}

/// Where a script asked to go, and whether it should replace the current
/// history entry rather than push one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NavRequest {
    pub url: String,
    pub replace: bool,
}

impl NavQueue {
    fn set(&self, url: String, replace: bool) {
        *self.request.borrow_mut() = Some(NavRequest { url, replace });
    }

    /// Take the navigation this tick asked for, if any.
    pub fn take(&self) -> Option<NavRequest> {
        self.request.borrow_mut().take()
    }
}

/// How many `fetch()` calls one page may have outstanding. A page that loops
/// issuing requests must find a wall rather than a queue that grows until the
/// process does — M10.13 will try exactly that.
pub const MAX_IN_FLIGHT: usize = 32;

/// A `fetch()` the page asked for (M10.12), collected for the event loop.
///
/// Same discipline as navigation and timers: JS never touches the network, the
/// binding records what was asked, and the loop spawns the worker.
#[derive(Clone, Default)]
pub struct FetchQueue {
    requests: Rc<RefCell<Vec<FetchAsk>>>,
    /// How many are outstanding for this page, for the concurrency cap.
    in_flight: Rc<Cell<usize>>,
}

/// One request: the id its promise is waiting on, and what to send.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FetchAsk {
    pub request: u64,
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl FetchQueue {
    /// Take everything asked for since the last drain.
    pub fn drain(&self) -> Vec<FetchAsk> {
        std::mem::take(&mut *self.requests.borrow_mut())
    }

    /// One outstanding request has settled.
    pub fn settled(&self) {
        self.in_flight.set(self.in_flight.get().saturating_sub(1));
    }
}

/// `<script>` elements a tick put into the document (M11.5), collected for
/// `App` to hand to the execution queue.
///
/// Same discipline as the timer, navigation and fetch queues: the binding
/// records *that an element became part of the document*, and `App` decides
/// what that means — whether the element is connected, whether it sits inside
/// something inert, whether it has already run. Nothing here reads the queue
/// or starts a fetch.
///
/// **What this costs a tick that inserts no script: nothing.** The list is
/// only ever pushed to from inside `appendChild`, `insertBefore` and a `src`
/// write, so a tick that calls none of them does no work at all, and one that
/// calls them pays a tag comparison per call. That is why the signal is here
/// and not a change list in the arena like M11.3's: an arena list would have
/// to be *read* after every tick whether or not anything inserted anything,
/// and `appendChild` is the only place that knows an insert happened at all —
/// `Dom::append` is also how `innerHTML` builds a subtree, and a script
/// written by `innerHTML` must never run.
#[derive(Clone, Default)]
pub struct InsertQueue {
    /// Node ids, in insertion order.
    nodes: Rc<RefCell<Vec<u32>>>,
    /// A/B switch for M11.5's measurement, and nothing else: disarmed, the
    /// whole check — including the tag test at each call site — is skipped,
    /// which is the code as it was before this task. The field does not exist
    /// in a release build, so neither does the branch.
    #[cfg(test)]
    disarmed: Rc<Cell<bool>>,
}

impl InsertQueue {
    /// Take everything recorded since the last drain.
    pub fn drain(&self) -> Vec<u32> {
        std::mem::take(&mut *self.nodes.borrow_mut())
    }

    /// Whether the call sites should look at what they are inserting.
    #[cfg(test)]
    fn armed(&self) -> bool {
        !self.disarmed.get()
    }

    #[cfg(not(test))]
    fn armed(&self) -> bool {
        true
    }

    /// Turn the check off for the interleaved measurement (see `disarmed`).
    #[cfg(test)]
    pub fn disarm(&self) {
        self.disarmed.set(true);
    }

    /// Record `node` if it is a `<script>` element and the page still has
    /// budget. Connectivity is *not* decided here — `App` decides it, against
    /// the tree, through `js::sources::connected_script`.
    fn record(&self, dom: &Dom, node: NodeId) {
        if !self.armed()
            || !matches!(&dom.node(node).data, NodeData::Element { tag, .. } if tag == "script")
        {
            return;
        }
        // The page's real bound is `ScriptQueue`'s, which counts insertions
        // over the page's whole life. This is the matching bound on the
        // *list*: one tick, so a page appending script elements in a loop
        // cannot buy memory with attempts the queue is going to refuse anyway.
        let mut nodes = self.nodes.borrow_mut();
        if nodes.len() < crate::js::queue::MAX_INSERTED_SCRIPTS {
            nodes.push(node.0);
        }
    }
}

/// Whether `name` is one the serializer could write and the tokenizer read
/// back as the same attribute.
///
/// HTML has no escape for attribute *names*, so a name holding a space or a
/// quote is not merely ugly — `setAttribute('a b="c', 'x')` serializes to
/// `a b="c="x"`, which the next parse reads as three different attributes, and
/// a read-modify-write through `innerHTML` silently corrupts the tree. A
/// browser refuses these with `InvalidCharacterError` for the same reason, so
/// the check belongs at the door rather than in the serializer.
fn is_valid_attribute_name(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().any(|ch| {
            ch.is_whitespace()
                || ch.is_control()
                || matches!(ch, '"' | '\'' | '<' | '>' | '/' | '=')
        })
}

/// Turn one of `Dom`'s refusals into the exception a browser throws, so a page
/// that appends a node into its own subtree finds out instead of watching the
/// call quietly do nothing.
fn throw_dom_error(ctx: &Ctx<'_>, error: DomError) -> rquickjs::Error {
    match error {
        DomError::HierarchyRequest => Exception::throw_message(
            ctx,
            "HierarchyRequestError: the node cannot be placed there",
        ),
        DomError::NotFound => {
            Exception::throw_message(ctx, "NotFoundError: the node is not a child of this one")
        }
        DomError::TooDeep => Exception::throw_message(
            ctx,
            "HierarchyRequestError: the tree is already as deeply nested as this browser allows",
        ),
    }
}

/// How deep the console formatter descends. One level, as M10.7 specifies:
/// `{a: 1, b: "x"}` is useful, a whole object graph is a wall of text.
const CONSOLE_MAX_DEPTH: u32 = 2;

/// How many entries of an array or object the formatter shows before saying
/// how many it left out.
const CONSOLE_MAX_ITEMS: u32 = 20;

/// Install the primitives and evaluate the prelude that builds the object
/// model on top of them. Called once per host.
// Eight arguments, and each is a distinct thing the page can reach: the tree,
// the console, and the four queues a tick fills for the loop to drain. Bundling
// them would name a group that has no other use and no other caller.
#[allow(clippy::too_many_arguments)]
pub fn install<'js>(
    ctx: &Ctx<'js>,
    slot: &Rc<DomSlot>,
    console: &Console,
    timers: &TimerQueue,
    navigation: &NavQueue,
    storage: &Storage,
    fetches: &FetchQueue,
    inserts: &InsertQueue,
) -> JsResult<Object<'js>> {
    let api = Object::new(ctx.clone())?;

    // The console's caps, read by the formatter in the prelude, so the numbers
    // live in Rust where a reviewer greps for them.
    api.set("consoleMaxDepth", CONSOLE_MAX_DEPTH)?;
    api.set("consoleMaxItems", CONSOLE_MAX_ITEMS)?;
    api.set("consoleMaxText", console::MAX_TEXT as u32)?;

    // One primitive for all five levels: the prelude has already formatted the
    // arguments into a line, so nothing here has to know about JS values.
    let log = console.clone();
    api.set(
        "consoleWrite",
        Function::new(ctx.clone(), move |level: String, text: String| {
            let level = match level.as_str() {
                "debug" => console::Level::Debug,
                "info" => console::Level::Info,
                "warn" => console::Level::Warn,
                "error" => console::Level::Error,
                _ => console::Level::Log,
            };
            // No source or line: a browser gets those from a stack walk we do
            // not do. An uncaught exception carries them; a `console.log` does
            // not, and inventing one would be worse than admitting it.
            log.push(level, None, None, &text);
        })?,
    )?;

    // Which page the slot holds. The prelude compares a handle's page against
    // this on every read.
    let s = Rc::clone(slot);
    api.set(
        "page",
        Function::new(ctx.clone(), move || s.page.get() as f64)?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "documentRoot",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>| {
            s.with(&ctx, |dom| id_of(dom.root))
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "documentElement",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>| {
            s.with(&ctx, |dom| {
                element_children(dom, dom.root).first().copied().map(id_of)
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "body",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>| {
            s.with(&ctx, |dom| find_tag(dom, dom.root, "body").map(id_of))
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "title",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>| {
            s.with(&ctx, |dom| {
                find_tag(dom, dom.root, "title").map_or_else(String::new, |node| text_of(dom, node))
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "getElementById",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, wanted: String| {
            s.with(&ctx, |dom| {
                // Document order, first match wins — the DOM's rule for a
                // document with duplicate ids, which real pages do have.
                find_descendant(dom, dom.root, &mut |dom, node| {
                    dom.attr(node, "id") == Some(wanted.as_str())
                })
                .map(id_of)
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "getElementsByTagName",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, scope: u32, name: String| {
                s.with(&ctx, |dom| {
                    let wanted = name.to_ascii_lowercase();
                    collect_descendants(dom, scope, |dom, node| match &dom.node(node).data {
                        // `*` matches every element, which is what the DOM says
                        // and what a page uses to walk a subtree.
                        NodeData::Element { tag, .. } => {
                            wanted == "*" || tag.eq_ignore_ascii_case(&wanted)
                        }
                        _ => false,
                    })
                })
            },
        )?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "getElementsByClassName",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, scope: u32, names: String| {
                s.with(&ctx, |dom| {
                    let wanted: Vec<&str> = names.split_whitespace().collect();
                    collect_descendants(dom, scope, |dom, node| {
                        if wanted.is_empty() {
                            return false;
                        }
                        let Some(classes) = dom.attr(node, "class") else {
                            return false;
                        };
                        // **All** of them, not any: `getElementsByClassName('a b')`
                        // is an intersection, which pages rely on for compound
                        // state classes.
                        wanted
                            .iter()
                            .all(|want| classes.split_whitespace().any(|have| have == *want))
                    })
                })
            },
        )?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "querySelector",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, selector: String| {
            let selectors = parse_selector_list(&ctx, &selector)?;
            s.with(&ctx, |dom| {
                query(dom, &selectors).into_iter().next().map(id_of)
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "querySelectorAll",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, selector: String| {
            let selectors = parse_selector_list(&ctx, &selector)?;
            s.with(&ctx, |dom| {
                query(dom, &selectors)
                    .into_iter()
                    .map(id_of)
                    .collect::<Vec<u32>>()
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "tagName",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, id: u32| {
            s.with(&ctx, |dom| {
                node(dom, id).and_then(|node| match &dom.node(node).data {
                    // Uppercase, as the DOM specifies for HTML elements.
                    NodeData::Element { tag, .. } => Some(tag.to_ascii_uppercase()),
                    _ => None,
                })
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "getAttribute",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, id: u32, name: String| {
            s.with(&ctx, |dom| {
                node(dom, id).and_then(|node| dom.attr(node, &name).map(str::to_string))
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "textContent",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, id: u32| {
            s.with(&ctx, |dom| {
                node(dom, id).map_or_else(String::new, |node| text_of(dom, node))
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "parentElement",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, id: u32| {
            s.with(&ctx, |dom| {
                let node = node(dom, id)?;
                let parent = dom.node(node).parent?;
                // `parentElement`, not `parentNode`: the document is not an
                // element, so the root's children report null.
                matches!(dom.node(parent).data, NodeData::Element { .. }).then(|| id_of(parent))
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "parentNode",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, id: u32| {
            s.with(&ctx, |dom| {
                // A *node*, not an element: the document is a node too, so
                // unlike `parentElement` this does not stop at it. The prelude
                // turns the root into the `document` object; a node with no
                // parent at all is null there.
                dom.node(node(dom, id)?).parent.map(id_of)
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "children",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, id: u32| {
            s.with(&ctx, |dom| {
                node(dom, id).map_or_else(Vec::new, |node| {
                    element_children(dom, node)
                        .into_iter()
                        .map(id_of)
                        .collect::<Vec<u32>>()
                })
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "nextElementSibling",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, id: u32| {
            s.with(&ctx, |dom| {
                let mut walk = dom.node(node(dom, id)?).next_sibling;
                while let Some(sibling) = walk {
                    if matches!(dom.node(sibling).data, NodeData::Element { .. }) {
                        return Some(id_of(sibling));
                    }
                    walk = dom.node(sibling).next_sibling;
                }
                None
            })
        })?,
    )?;

    // ---- writes (M10.5) ----

    let s = Rc::clone(slot);
    api.set(
        "createElement",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, tag: String| {
            s.with_mut(&ctx, |dom| {
                // Lowercased like the parser's tags, so `createElement('DIV')`
                // and a parsed `<div>` are the same tag to selector matching.
                id_of(dom.create_element(&tag.to_ascii_lowercase(), Vec::new()))
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "createTextNode",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, text: String| {
            s.with_mut(&ctx, |dom| id_of(dom.create_text(&text)))
        })?,
    )?;

    // The two insertion routes (M11.5). Each records the node it just put in
    // the tree *after* the arena accepted the edit — a refused insert has not
    // inserted anything — and `App` decides what the record means. Note what
    // is not here: `setInnerHTML` builds its subtree through the same
    // `Dom::append`, and deliberately records nothing, which is how a
    // `<script>` written by `innerHTML` stays unrun.
    let (s, queue) = (Rc::clone(slot), inserts.clone());
    api.set(
        "appendChild",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, parent: u32, child: u32| {
            let outcome = s.with_mut(&ctx, |dom| {
                let (Some(parent), Some(child)) = (node(dom, parent), node(dom, child)) else {
                    return Err(DomError::NotFound);
                };
                dom.append(parent, child)?;
                queue.record(dom, child);
                Ok(())
            })?;
            outcome.map_err(|error| throw_dom_error(&ctx, error))
        })?,
    )?;

    let (s, queue) = (Rc::clone(slot), inserts.clone());
    api.set(
        "insertBefore",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, parent: u32, child: u32, reference: u32| {
                let outcome = s.with_mut(&ctx, |dom| {
                    let (Some(parent), Some(child), Some(reference)) =
                        (node(dom, parent), node(dom, child), node(dom, reference))
                    else {
                        return Err(DomError::NotFound);
                    };
                    dom.insert_before(parent, child, reference)?;
                    queue.record(dom, child);
                    Ok(())
                })?;
                outcome.map_err(|error| throw_dom_error(&ctx, error))
            },
        )?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "remove",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, id: u32| {
            s.with_mut(&ctx, |dom| {
                if let Some(node) = node(dom, id) {
                    dom.remove(node);
                }
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "removeChild",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, parent: u32, child: u32| {
            let outcome = s.with_mut(&ctx, |dom| {
                let (Some(parent), Some(child)) = (node(dom, parent), node(dom, child)) else {
                    return Err(DomError::NotFound);
                };
                // `removeChild` is `remove` plus a check the caller named the
                // right parent — the one thing it has that `remove` does not.
                if dom.node(child).parent != Some(parent) {
                    return Err(DomError::NotFound);
                }
                dom.remove(child);
                Ok(())
            })?;
            outcome.map_err(|error| throw_dom_error(&ctx, error))
        })?,
    )?;

    let (s, queue) = (Rc::clone(slot), inserts.clone());
    api.set(
        "setAttribute",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, id: u32, name: String, value: String| {
                if !is_valid_attribute_name(&name) {
                    return Err(Exception::throw_message(
                        &ctx,
                        &format!("InvalidCharacterError: '{name}' is not a valid attribute name"),
                    ));
                }
                s.with_mut(&ctx, |dom| {
                    if let Some(node) = node(dom, id) {
                        dom.set_attr(node, &name, &value);
                        // The third insertion route (M11.5): a `src` written
                        // on a script element that is *already* in the tree,
                        // which is what a page does when it reuses one. It
                        // arrives as an attribute write rather than an
                        // insertion, and the two must not disagree — the
                        // element the GA snippet builds takes **both** routes
                        // (`a.src = g` while detached, then `insertBefore`),
                        // so what keeps it from running twice is the queue's
                        // "already started" list and not a rule here about
                        // which signal wins.
                        if name.eq_ignore_ascii_case("src") {
                            queue.record(dom, node);
                        }
                    }
                })
            },
        )?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "removeAttribute",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, id: u32, name: String| {
            s.with_mut(&ctx, |dom| {
                if let Some(node) = node(dom, id) {
                    dom.remove_attr(node, &name);
                }
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "setTextContent",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, id: u32, text: String| {
            s.with_mut(&ctx, |dom| {
                let Some(node) = node(dom, id) else {
                    return;
                };
                // On a text node it is the text; on an element it replaces
                // every child with one text node — including the empty case,
                // which is how a page clears a container.
                if dom.set_text(node, &text) {
                    return;
                }
                clear_children(dom, node);
                if !text.is_empty() {
                    let child = dom.create_text(&text);
                    let _ = dom.append(node, child);
                }
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "innerHTML",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, id: u32| {
            s.with(&ctx, |dom| {
                node(dom, id).map_or_else(String::new, |node| html::serialize_children(dom, node))
            })
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "setInnerHTML",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, id: u32, source: String| {
            s.with_mut(&ctx, |dom| {
                let Some(target) = node(dom, id) else {
                    return;
                };
                clear_children(dom, target);
                let (fragment, roots) = html::parse_fragment(&source);
                for root in roots {
                    adopt(dom, target, &fragment, root);
                }
            })
        })?,
    )?;

    // A listener's exception, reported with whatever location its stack
    // carries. Parsed here rather than in JS so the console shows the same
    // shape a script's uncaught throw does (M10.7).
    let errors = console.clone();
    api.set(
        "reportError",
        Function::new(ctx.clone(), move |message: String, stack: String| {
            let (source, line) = super::script_frame(&stack);
            errors.push(console::Level::Error, source, line, &message);
        })?,
    )?;

    // Timers (M10.9). The prelude owns the callbacks and the ids; all that
    // crosses here is "schedule id N in D milliseconds" and "cancel id N".
    let queue = timers.clone();
    api.set(
        "scheduleTimer",
        Function::new(ctx.clone(), move |id: f64, delay: f64| {
            // A page can pass anything: `NaN`, `-1`, `Infinity`. The floor is
            // applied by the timer thread; this only has to produce a finite
            // non-negative number for it.
            let delay = if delay.is_finite() && delay > 0.0 {
                Duration::from_millis(delay as u64)
            } else {
                Duration::ZERO
            };
            queue.push(id as u64, Some(delay));
        })?,
    )?;

    let queue = timers.clone();
    api.set(
        "cancelTimer",
        Function::new(ctx.clone(), move |id: f64| {
            queue.push(id as u64, None);
        })?,
    )?;

    // ---- location and storage (M10.11) ----

    // The page's URL, for `location` to take apart. Parsed in JS from this one
    // string, using the same value `net::` produced — there is no second URL
    // parser here, and there must not be.
    let s = Rc::clone(slot);
    api.set(
        "pageUrl",
        Function::new(ctx.clone(), move || s.url.borrow().clone())?,
    )?;

    // A navigation the script asked for. JS never touches the network: this
    // records what was asked, `App` turns it into the same `Effect::fetch` a
    // link click produces, and the loop performs it.
    let nav = navigation.clone();
    api.set(
        "navigate",
        Function::new(ctx.clone(), move |url: String, replace: bool| {
            nav.set(url, replace);
        })?,
    )?;

    // Storage, per origin. The origin is derived here rather than in JS so a
    // page cannot spoof it by rewriting `location`.
    let (store, s) = (storage.clone(), Rc::clone(slot));
    api.set(
        "storageGet",
        Function::new(
            ctx.clone(),
            move |session: bool, key: String| -> Option<String> {
                let origin = origin_of(&s.url.borrow())?;
                store.get(&origin, area(session), &key)
            },
        )?,
    )?;

    let (store, s) = (storage.clone(), Rc::clone(slot));
    api.set(
        "storageSet",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, session: bool, key: String, value: String| {
                let Some(origin) = origin_of(&s.url.borrow()) else {
                    return Ok(());
                };
                if store.set(&origin, area(session), &key, &value) {
                    Ok(())
                } else {
                    Err(Exception::throw_message(
                        &ctx,
                        "QuotaExceededError: this origin's storage is full",
                    ))
                }
            },
        )?,
    )?;

    let (store, s) = (storage.clone(), Rc::clone(slot));
    api.set(
        "storageRemove",
        Function::new(ctx.clone(), move |session: bool, key: String| {
            if let Some(origin) = origin_of(&s.url.borrow()) {
                store.remove(&origin, area(session), &key);
            }
        })?,
    )?;

    let (store, s) = (storage.clone(), Rc::clone(slot));
    api.set(
        "storageClear",
        Function::new(ctx.clone(), move |session: bool| {
            if let Some(origin) = origin_of(&s.url.borrow()) {
                store.clear(&origin, area(session));
            }
        })?,
    )?;

    let (store, s) = (storage.clone(), Rc::clone(slot));
    api.set(
        "storageLength",
        Function::new(ctx.clone(), move |session: bool| {
            origin_of(&s.url.borrow()).map_or(0, |origin| store.len(&origin, area(session)) as u32)
        })?,
    )?;

    let (store, s) = (storage.clone(), Rc::clone(slot));
    api.set(
        "storageKey",
        Function::new(
            ctx.clone(),
            move |session: bool, index: u32| -> Option<String> {
                let origin = origin_of(&s.url.borrow())?;
                store.key_at(&origin, area(session), index as usize)
            },
        )?,
    )?;

    // ---- fetch (M10.12) ----

    let (queue, s, log) = (fetches.clone(), Rc::clone(slot), console.clone());
    api.set(
        "startFetch",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>,
                  request: f64,
                  url: String,
                  method: String,
                  headers: String,
                  body: Option<String>| {
                let page_url = s.url.borrow().clone();
                let Some(resolved) = crate::net::resolve_url(&page_url, &url) else {
                    return Err(Exception::throw_message(
                        &ctx,
                        &format!("TypeError: '{url}' is not a URL this page can fetch"),
                    ));
                };

                // **Same origin only.** A browser would send the request and
                // let CORS decide who may *read* the answer; we have no CORS
                // implementation, and `fetch` reads bodies — so allowing this
                // would let any page pull whatever the reader's network
                // position can reach and post it back out. The cost is pages
                // that call an API on another host; see `DEVIATIONS.md`.
                if origin_of(&resolved) != origin_of(&page_url) {
                    let message = format!(
                        "refused to fetch {resolved}: only same-origin requests are allowed"
                    );
                    log.push(console::Level::Error, None, None, &message);
                    return Err(Exception::throw_message(
                        &ctx,
                        &format!("TypeError: {message}"),
                    ));
                }

                if queue.in_flight.get() >= MAX_IN_FLIGHT {
                    let message = format!(
                        "refused to fetch {resolved}: this page already has {MAX_IN_FLIGHT} \
                         requests in flight"
                    );
                    log.push(console::Level::Error, None, None, &message);
                    return Err(Exception::throw_message(
                        &ctx,
                        &format!("TypeError: {message}"),
                    ));
                }

                // Headers cross as JSON: one string is a simpler boundary than
                // a shape, and this is the only place that needs it.
                let headers: Vec<(String, String)> = serde_pairs(&headers);
                queue.in_flight.set(queue.in_flight.get() + 1);
                queue.requests.borrow_mut().push(FetchAsk {
                    request: request as u64,
                    url: resolved,
                    method,
                    headers,
                    body,
                });
                Ok(())
            },
        )?,
    )?;

    ctx.globals().set("__dom", api)?;
    // Named, so a stack frame from inside the object model says where it came
    // from instead of the engine's anonymous `eval_script`.
    let mut options = EvalOptions::default();
    options.global = true;
    options.strict = false;
    options.filename = Some("<bindings>".to_string());
    // The prelude's value is its entry-point object (see the end of
    // `PRELUDE`): `dispatch`, `fireTimer`, `pending`.
    ctx.eval_with_options::<Object<'js>, _>(PRELUDE, options)
}

/// Parse the prelude's header JSON — a flat object of string values — without
/// a JSON crate. The only producer is our own prelude, which builds it with
/// `JSON.stringify` from strings it has already coerced, so this handles the
/// shape it emits and nothing more.
fn serde_pairs(json: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut current = String::new();
    let mut strings = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for ch in json.chars() {
        match (in_string, escaped, ch) {
            (true, true, _) => {
                current.push(ch);
                escaped = false;
            }
            (true, false, '\\') => escaped = true,
            (true, false, '"') => {
                strings.push(std::mem::take(&mut current));
                in_string = false;
            }
            (true, false, _) => current.push(ch),
            (false, _, '"') => in_string = true,
            _ => {}
        }
    }
    // Alternating key, value — the shape `JSON.stringify` gives a flat object.
    for pair in strings.chunks(2) {
        if let [name, value] = pair {
            pairs.push((name.clone(), value.clone()));
        }
    }
    pairs
}

/// Which store a `session` flag from the prelude names.
fn area(session: bool) -> Area {
    if session { Area::Session } else { Area::Local }
}

fn id_of(node: NodeId) -> u32 {
    node.0
}

/// Detach every child of `node`. They stay in the arena — ids are never
/// reused (M10.3), so a handle a script still holds keeps meaning the node it
/// always meant, detached rather than dangling.
fn clear_children(dom: &mut Dom, node: NodeId) {
    for child in dom.children(node).collect::<Vec<_>>() {
        dom.remove(child);
    }
}

/// Copy `source`'s subtree out of a fragment arena and into `dom` under
/// `parent`. Nodes are *rebuilt* through the write API rather than moved:
/// there is no way to transplant a node between arenas, and there should not
/// be — an id only means anything in the arena that issued it.
///
/// Comments and the doctype are dropped: `Dom`'s write API creates elements
/// and text, which is what M10.3 defined, and nothing renders the rest.
fn adopt(dom: &mut Dom, parent: NodeId, fragment: &Dom, source: NodeId) {
    let copy = match &fragment.node(source).data {
        NodeData::Element { tag, attrs } => dom.create_element(tag, attrs.clone()),
        NodeData::Text(text) => dom.create_text(text),
        NodeData::Comment(_) | NodeData::Doctype(_) | NodeData::Document => return,
    };
    // The parent is always an element or the document, and `copy` is fresh, so
    // this cannot be refused; ignoring the result keeps the recursion honest
    // about there being no error path to report.
    let _ = dom.append(parent, copy);
    for child in fragment.children(source) {
        adopt(dom, copy, fragment, child);
    }
}

/// A JS-supplied id, bounds-checked. A page cannot reach these numbers (the
/// prelude hides them), but nothing may panic on one either.
fn node(dom: &Dom, id: u32) -> Option<NodeId> {
    ((id as usize) < dom.node_count()).then_some(NodeId(id))
}

/// Every descendant of `scope` matching `keep`, in document order — the shape
/// both `getElementsBy*` collections need.
///
/// `scope` is a node id from the prelude; an id outside the arena yields
/// nothing rather than panicking, like every other primitive here. The walk is
/// `for_each_descendant`, the same one `querySelectorAll` uses: a second
/// traversal in this module would be a second thing to keep correct.
fn collect_descendants(dom: &Dom, scope: u32, keep: impl Fn(&Dom, NodeId) -> bool) -> Vec<u32> {
    let Some(scope) = node(dom, scope) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for_each_descendant(dom, scope, &mut |node| {
        if keep(dom, node) {
            found.push(id_of(node));
        }
    });
    found
}

/// Visit every descendant of `node` in document order, excluding `node`.
///
/// A callback rather than a returned `Vec`: `textContent` is a property, and a
/// page reading it in a loop would otherwise allocate a list of the whole
/// subtree per read. The walk recurses like every other walk in the engine
/// (style's resolve, layout's), so it inherits the same depth assumption.
fn for_each_descendant(dom: &Dom, node: NodeId, visit: &mut impl FnMut(NodeId)) {
    for child in dom.children(node) {
        visit(child);
        for_each_descendant(dom, child, visit);
    }
}

/// The first descendant satisfying `pred`, in document order. Separate from
/// [`for_each_descendant`] because the queries that use it — `getElementById`,
/// `body`, `title` — stop at the first hit and should not walk a Wikipedia
/// article to find a `<title>` in its head.
fn find_descendant(
    dom: &Dom,
    node: NodeId,
    pred: &mut impl FnMut(&Dom, NodeId) -> bool,
) -> Option<NodeId> {
    for child in dom.children(node) {
        if pred(dom, child) {
            return Some(child);
        }
        if let Some(found) = find_descendant(dom, child, pred) {
            return Some(found);
        }
    }
    None
}

fn element_children(dom: &Dom, node: NodeId) -> Vec<NodeId> {
    dom.children(node)
        .filter(|&child| matches!(dom.node(child).data, NodeData::Element { .. }))
        .collect()
}

fn find_tag(dom: &Dom, from: NodeId, tag: &str) -> Option<NodeId> {
    find_descendant(
        dom,
        from,
        &mut |dom, node| matches!(&dom.node(node).data, NodeData::Element { tag: t, .. } if t == tag),
    )
}

/// Concatenated descendant text in document order. Includes the node's own
/// text when it is a text node, so this is `textContent` for any node kind.
fn text_of(dom: &Dom, node: NodeId) -> String {
    let mut out = String::new();
    if let NodeData::Text(text) = &dom.node(node).data {
        out.push_str(text);
    }
    for_each_descendant(dom, node, &mut |child| {
        if let NodeData::Text(text) = &dom.node(child).data {
            out.push_str(text);
        }
    });
    out
}

/// Parse a selector string **through the CSS parser** — there is one selector
/// syntax in this engine and one matcher, and a second one here would be free
/// to disagree with the cascade about what `div > p.note` means.
///
/// `css::parse` drops a rule whose prelude it cannot evaluate, so "no rule
/// came back" is exactly "invalid selector". Requiring precisely one rule with
/// no declarations is what stops `p{} body` from being read as a selector.
fn parse_selector_list(ctx: &Ctx<'_>, selector: &str) -> JsResult<Vec<css::Selector>> {
    let sheet = css::parse(&format!("{selector}{{}}"));
    match sheet.rules.as_slice() {
        [rule] if rule.declarations.is_empty() && !rule.selectors.is_empty() => {
            Ok(rule.selectors.clone())
        }
        // A browser throws SyntaxError for a selector it cannot parse.
        _ => Err(Exception::throw_syntax(
            ctx,
            &format!("'{selector}' is not a valid selector"),
        )),
    }
}

/// Every element matching any selector in the list, in document order.
fn query(dom: &Dom, selectors: &[css::Selector]) -> Vec<NodeId> {
    let ctx = StyleContext::default();
    let mut found = Vec::new();
    for_each_descendant(dom, dom.root, &mut |node| {
        if matches!(dom.node(node).data, NodeData::Element { .. })
            && selectors
                .iter()
                .any(|selector| matching::matches(dom, node, selector, &ctx))
        {
            found.push(node);
        }
    });
    found
}

/// The object model, in the language that has objects.
///
/// Runs once per host, before any page script. It takes the primitive object
/// as an argument and the global holding it is deleted immediately, so a page
/// gets the model and never the raw ids under it.
const PRELUDE: &str = r#"
(function (raw) {
  // node -> {id, page}. A WeakMap rather than properties on the handle: a
  // page can enumerate its own objects, and `__id` showing up in a for-in
  // over an element would be a lie about what the DOM has.
  const handles = new WeakMap();

  // One wrapper per node id, so `a === b` is true for two handles to the same
  // node — pages write `if (e.target === el)` constantly. The cache is
  // per page: it is cleared the moment the slot reports a different one.
  let cache = new Map();
  let cachedPage = raw.page();

  function idOf(node) {
    const handle = handles.get(node);
    if (handle === undefined) {
      throw new TypeError("not a DOM node");
    }
    if (handle.page !== raw.page()) {
      throw new Error("stale node handle: it belongs to a page that is no longer loaded");
    }
    return handle.id;
  }

  function wrap(id) {
    if (id === null || id === undefined) return null;
    const page = raw.page();
    if (page !== cachedPage) { cache = new Map(); cachedPage = page; }
    let node = cache.get(id);
    if (node === undefined) {
      node = Object.create(Element.prototype);
      handles.set(node, { id: id, page: page });
      cache.set(id, node);
    }
    return node;
  }

  function orNull(value) { return value === undefined ? null : value; }

  // A class attribute as an ordered set of tokens, the DOM's model of it:
  // split on ASCII whitespace, duplicates collapsed, order of first
  // appearance kept.
  function tokens(node) {
    const value = orNull(raw.getAttribute(idOf(node), "class")) || "";
    const seen = [];
    for (const token of value.split(/\s+/)) {
      if (token !== "" && seen.indexOf(token) === -1) seen.push(token);
    }
    return seen;
  }

  // Writing the set back serializes it single-spaced, so whitespace a page
  // wrote by hand is normalized the first time it touches classList.
  function setTokens(node, list) {
    raw.setAttribute(idOf(node), "class", list.join(" "));
  }

  // A *live view*, not a snapshot: every call re-reads the attribute, so
  // `el.classList.add('x')` is visible to a `getAttribute('class')` right
  // after it, and two `classList` reads see each other's writes.
  function classListFor(node) {
    return {
      contains: function (name) { return tokens(node).indexOf(String(name)) !== -1; },
      add: function () {
        const list = tokens(node);
        for (const name of arguments) {
          if (list.indexOf(String(name)) === -1) list.push(String(name));
        }
        setTokens(node, list);
      },
      remove: function () {
        let list = tokens(node);
        for (const name of arguments) {
          list = list.filter(function (t) { return t !== String(name); });
        }
        setTokens(node, list);
      },
      toggle: function (name, force) {
        const list = tokens(node);
        const at = list.indexOf(String(name));
        const present = at !== -1;
        const want = force === undefined ? !present : !!force;
        if (want && !present) list.push(String(name));
        if (!want && present) list.splice(at, 1);
        setTokens(node, list);
        return want;
      },
      get length() { return tokens(node).length; },
      item: function (i) { const list = tokens(node); return i < list.length ? list[i] : null; },
      toString: function () { return tokens(node).join(" "); },
    };
  }

  // A boolean content attribute: present or absent, its value meaningless.
  function booleanAttribute(name) {
    return {
      get: function () { return raw.getAttribute(idOf(this), name) !== undefined; },
      set: function (value) {
        if (value) raw.setAttribute(idOf(this), name, "");
        else raw.removeAttribute(idOf(this), name);
      },
    };
  }

  function Element() {
    throw new TypeError("Illegal constructor");
  }

  Object.defineProperties(Element.prototype, {
    tagName: { get: function () { return orNull(raw.tagName(idOf(this))); } },
    id: {
      get: function () { return orNull(raw.getAttribute(idOf(this), "id")) || ""; },
      set: function (value) { raw.setAttribute(idOf(this), "id", String(value)); },
    },
    className: {
      get: function () { return orNull(raw.getAttribute(idOf(this), "class")) || ""; },
      set: function (value) { raw.setAttribute(idOf(this), "class", String(value)); },
    },
    classList: { get: function () { return classListFor(this); } },
    innerHTML: {
      get: function () { return raw.innerHTML(idOf(this)); },
      set: function (value) { raw.setInnerHTML(idOf(this), String(value)); },
    },
    textContent: {
      get: function () { return raw.textContent(idOf(this)); },
      set: function (value) { raw.setTextContent(idOf(this), String(value)); },
    },
    setAttribute: {
      value: function (name, value) {
        raw.setAttribute(idOf(this), String(name), String(value));
      }
    },
    removeAttribute: {
      value: function (name) { raw.removeAttribute(idOf(this), String(name)); }
    },
    appendChild: {
      value: function (child) { raw.appendChild(idOf(this), idOf(child)); return child; }
    },
    insertBefore: {
      value: function (child, reference) {
        // `insertBefore(node, null)` is an append; pages rely on it.
        if (reference === null || reference === undefined) {
          raw.appendChild(idOf(this), idOf(child));
        } else {
          raw.insertBefore(idOf(this), idOf(child), idOf(reference));
        }
        return child;
      }
    },
    removeChild: {
      value: function (child) { raw.removeChild(idOf(this), idOf(child)); return child; }
    },
    remove: { value: function () { raw.remove(idOf(this)); } },
    parentElement: { get: function () { return wrap(raw.parentElement(idOf(this))); } },
    // A node's parent, which is *not* always an element: `<html>`'s is the
    // document. A detached node has none. This is the line
    // motherfuckingwebsite.com's analytics loader threw on (M11.5).
    parentNode: {
      get: function () {
        const parent = raw.parentNode(idOf(this));
        if (parent === undefined || parent === null) return null;
        return parent === raw.documentRoot() ? document : wrap(parent);
      }
    },
    // `src` reflects, spelled the way `id` and `className` are: the attribute
    // *as the page wrote it*, not resolved against the page URL the way a
    // browser's `.src` getter reports it. The engine resolves it once, where
    // every other subresource URL is resolved (`App::resolve_script_urls`),
    // rather than in two places that could disagree.
    src: {
      get: function () { return orNull(raw.getAttribute(idOf(this), "src")) || ""; },
      set: function (value) { raw.setAttribute(idOf(this), "src", String(value)); },
    },
    // `async` and `defer` reflect, and reflecting is **all** they do: every
    // script still runs in the order this engine decided (see `js::queue`),
    // which is the standing `defer`/`async` deviation and stays one. They are
    // here because a bootstrap writes `a.async = 1` before it writes `a.src`,
    // and a property assignment that silently lands on the wrapper object
    // instead of the element is the kind of thing that makes a page's later
    // feature detection lie.
    async: booleanAttribute("async"),
    defer: booleanAttribute("defer"),
    children: { get: function () { return raw.children(idOf(this)).map(wrap); } },
    firstElementChild: {
      get: function () {
        const kids = raw.children(idOf(this));
        return kids.length ? wrap(kids[0]) : null;
      }
    },
    nextElementSibling: {
      get: function () { return wrap(raw.nextElementSibling(idOf(this))); }
    },
    getAttribute: {
      value: function (name) { return orNull(raw.getAttribute(idOf(this), String(name))); }
    },
    getElementsByTagName: {
      value: function (name) {
        return raw.getElementsByTagName(idOf(this), String(name)).map(wrap);
      }
    },
    getElementsByClassName: {
      value: function (names) {
        return raw.getElementsByClassName(idOf(this), String(names)).map(wrap);
      }
    },
    hasAttribute: {
      value: function (name) { return raw.getAttribute(idOf(this), String(name)) !== undefined; }
    },
  });

  const document = {
    get documentElement() { return wrap(raw.documentElement()); },
    get body() { return wrap(raw.body()); },
    get title() { return raw.title(); },
    createElement: function (tag) { return wrap(raw.createElement(String(tag))); },
    createTextNode: function (text) { return wrap(raw.createTextNode(String(text))); },
    getElementById: function (id) { return wrap(raw.getElementById(String(id))); },
    getElementsByTagName: function (name) {
      return raw.getElementsByTagName(raw.documentRoot(), String(name)).map(wrap);
    },
    getElementsByClassName: function (names) {
      return raw.getElementsByClassName(raw.documentRoot(), String(names)).map(wrap);
    },
    querySelector: function (sel) { return wrap(raw.querySelector(String(sel))); },
    querySelectorAll: function (sel) { return raw.querySelectorAll(String(sel)).map(wrap); },
  };

  // ---- console (M10.7) ----

  function clip(text) {
    return text.length > raw.consoleMaxText
      ? text.slice(0, raw.consoleMaxText) + "…"
      : text;
  }

  // Format one value for the pane. `seen` is the cycle guard: without it
  // `console.log(window)` walks the global object into itself and the tick
  // spends its whole budget building a string nobody can read.
  function show(value, depth, seen) {
    if (value === null) return "null";
    if (value === undefined) return "undefined";

    const kind = typeof value;
    if (kind === "string") {
      // Bare at the top level, quoted inside a structure — the only way to
      // tell the number 42 from the string "42" once it is nested.
      return depth === 0 ? clip(value) : JSON.stringify(clip(value));
    }
    if (kind === "number" || kind === "boolean" || kind === "bigint") return String(value);
    if (kind === "symbol") return value.toString();
    if (kind === "function") return "[function]";

    // A DOM handle prints as the element it stands for. `{}` would be true —
    // the handle has no own properties — and useless.
    if (handles.has(value)) {
      const id = raw.getAttribute(idOf(value), "id");
      const tag = (raw.tagName(idOf(value)) || "node").toLowerCase();
      return id === undefined ? "<" + tag + ">" : "<" + tag + " id=\"" + id + "\">";
    }

    if (seen.indexOf(value) !== -1) return "[circular]";
    if (depth >= raw.consoleMaxDepth) return Array.isArray(value) ? "[…]" : "{…}";

    seen.push(value);
    try {
      if (Array.isArray(value)) {
        const shown = value.slice(0, raw.consoleMaxItems)
          .map(function (item) { return show(item, depth + 1, seen); });
        if (value.length > raw.consoleMaxItems) {
          shown.push("… " + (value.length - raw.consoleMaxItems) + " more");
        }
        return "[" + shown.join(", ") + "]";
      }
      if (value instanceof Error) {
        return value.name + ": " + value.message;
      }
      const keys = Object.keys(value);
      const shown = keys.slice(0, raw.consoleMaxItems).map(function (key) {
        return key + ": " + show(value[key], depth + 1, seen);
      });
      if (keys.length > raw.consoleMaxItems) {
        shown.push("… " + (keys.length - raw.consoleMaxItems) + " more");
      }
      return "{" + shown.join(", ") + "}";
    } finally {
      seen.pop();
    }
  }

  function writer(level) {
    return function () {
      const parts = [];
      for (const value of arguments) parts.push(show(value, 0, []));
      raw.consoleWrite(level, clip(parts.join(" ")));
    };
  }

  globalThis.console = {
    log: writer("log"),
    info: writer("info"),
    warn: writer("warn"),
    error: writer("error"),
    debug: writer("debug"),
  };

  // ---- events (M10.8) ----

  // The registry lives here, not in the arena: the DOM stays plain data that
  // `Msg::Parsed` can carry and tests can compare. Keyed by node id, or by the
  // strings "document"/"window" for the two targets that are not nodes.
  const listeners = new Map();

  function keyOf(target) {
    if (target === globalThis) return "window";
    if (target === document) return "document";
    return idOf(target);
  }

  // The legacy third argument is a boolean `capture`; the modern one is an
  // options object. Anything else in that object — `passive`, `signal` — is
  // ignored, deliberately and on the record.
  function captureOf(options) {
    return options === true || (!!options && options.capture === true);
  }

  function addListener(target, type, fn, options) {
    if (typeof fn !== "function") return;
    const key = keyOf(target);
    const capture = captureOf(options);
    const list = listeners.get(key) || [];
    // The DOM deduplicates on (type, callback, capture): registering the same
    // handler twice runs it once.
    for (const entry of list) {
      if (entry.type === type && entry.fn === fn && entry.capture === capture) return;
    }
    list.push({
      type: String(type),
      fn: fn,
      capture: capture,
      once: !!options && options.once === true,
      target: target,
    });
    listeners.set(key, list);
  }

  function removeListener(target, type, fn, options) {
    const key = keyOf(target);
    const list = listeners.get(key);
    if (!list) return;
    const capture = captureOf(options);
    for (let i = 0; i < list.length; i++) {
      if (list[i].type === type && list[i].fn === fn && list[i].capture === capture) {
        list.splice(i, 1);
        return;
      }
    }
  }

  function makeEvent(type, target, bubbles) {
    let stopped = false;
    let stoppedImmediately = false;
    let prevented = false;
    const event = {
      type: type,
      target: target,
      currentTarget: null,
      eventPhase: 0,
      bubbles: bubbles,
      cancelable: true,
      get defaultPrevented() { return prevented; },
      preventDefault: function () { prevented = true; },
      stopPropagation: function () { stopped = true; },
      stopImmediatePropagation: function () { stopped = true; stoppedImmediately = true; },
    };
    // Read by the dispatcher, not by the page: kept off the object so a page
    // cannot forge them.
    return {
      event: event,
      stopped: function () { return stopped; },
      stoppedImmediately: function () { return stoppedImmediately; },
      prevented: function () { return prevented; },
      clearImmediate: function () { stoppedImmediately = false; },
    };
  }

  // Run one node's listeners. `phase` is 1 capture, 2 target, 3 bubble; at the
  // target both capture and non-capture listeners run, in registration order,
  // which is what the DOM specifies.
  function fire(key, state, phase) {
    const registered = listeners.get(key);
    if (!registered) return;
    // **Snapshotted before the phase runs.** A listener that adds or removes
    // listeners during dispatch cannot change what *this* dispatch does — the
    // alternative is a page that can make dispatch iterate a list it is
    // mutating, and the result depends on the iteration order of the engine.
    const snapshot = registered.slice();
    const wantCapture = phase === 1;
    state.clearImmediate();
    for (const entry of snapshot) {
      if (state.stoppedImmediately()) return;
      if (entry.type !== state.event.type) continue;
      if (phase !== 2 && entry.capture !== wantCapture) continue;
      if (entry.once) removeListener(entry.target, entry.type, entry.fn, entry.capture);
      state.event.currentTarget = entry.target;
      state.event.eventPhase = phase;
      try {
        entry.fn.call(entry.target, state.event);
      } catch (error) {
        // A listener that throws does not stop the others and does not break
        // the page — the same discipline as a script that throws (M10.2).
        raw.reportError(String(error && error.message ? error.message : error),
                        (error && error.stack) || "");
      }
    }
  }

  // Root → target, with the two non-node targets at the front. Built from the
  // arena at dispatch time, so a path is always the tree as it is now.
  function pathTo(target) {
    // The path *ends* at the target, so an event aimed at `window` has a path
    // of one. Appending both non-node targets unconditionally would make
    // `document` the target of a `load` event and quietly skip every
    // non-capture listener on `window` — which is where pages put `load`.
    if (target === globalThis) return ["window"];
    if (target === document) return ["window", "document"];

    const chain = [];
    let id = idOf(target);
    while (id !== null && id !== undefined) {
      chain.push(id);
      id = raw.parentElement(id);
    }
    chain.reverse();
    return ["window", "document"].concat(chain);
  }

  function dispatch(kind, id, type, bubbles) {
    const target = kind === "window" ? globalThis : kind === "document" ? document : wrap(id);
    if (target === null) return false;

    const state = makeEvent(String(type), target, !!bubbles);
    const path = pathTo(target);
    const last = path.length - 1;

    for (let i = 0; i < last && !state.stopped(); i++) fire(path[i], state, 1);
    if (!state.stopped()) fire(path[last], state, 2);
    if (state.event.bubbles) {
      for (let i = last - 1; i >= 0 && !state.stopped(); i--) fire(path[i], state, 3);
    }
    state.event.currentTarget = null;
    state.event.eventPhase = 0;
    return state.prevented();
  }

  // `el.onload = fn` — an event-handler *property*, which is what a script
  // bootstrap chains on (`s.onload = next`) and which `addEventListener`
  // cannot express: assigning **replaces** whatever was there, and assigning
  // null removes it. So the entry it makes is marked, and the setter takes the
  // marked one out before putting the new one in. Everything after that is an
  // ordinary listener — it runs through the same dispatcher, in registration
  // order among the others (M10.8).
  function handlerProperty(type) {
    return {
      get: function () {
        for (const entry of listeners.get(keyOf(this)) || []) {
          if (entry.type === type && entry.handler) return entry.fn;
        }
        return null;
      },
      set: function (fn) {
        const key = keyOf(this);
        const kept = (listeners.get(key) || []).filter(function (entry) {
          return !(entry.type === type && entry.handler);
        });
        if (typeof fn === "function") {
          kept.push({
            type: type, fn: fn, capture: false, once: false,
            target: this, handler: true,
          });
        }
        listeners.set(key, kept);
      },
    };
  }

  Object.defineProperties(Element.prototype, {
    addEventListener: {
      value: function (type, fn, options) { addListener(this, String(type), fn, options); }
    },
    removeEventListener: {
      value: function (type, fn, options) { removeListener(this, String(type), fn, options); }
    },
    // The two an inserted `<script>` fires (M11.5). `error` is half the point:
    // a page that chains on `onload` and never hears anything back because a
    // fetch failed is a page that hangs, and a silent failure is the one
    // outcome the task refused.
    onload: handlerProperty("load"),
    onerror: handlerProperty("error"),
  });
  document.addEventListener = function (type, fn, options) {
    addListener(document, String(type), fn, options);
  };
  document.removeEventListener = function (type, fn, options) {
    removeListener(document, String(type), fn, options);
  };
  globalThis.addEventListener = function (type, fn, options) {
    addListener(globalThis, String(type), fn, options);
  };
  globalThis.removeEventListener = function (type, fn, options) {
    removeListener(globalThis, String(type), fn, options);
  };

  // ---- timers (M10.9) ----

  // Callbacks live here; Rust only ever sees an id and a delay. Ids are
  // browser-compatible: positive integers, never reused within a page, so
  // `clearTimeout` on an id that already fired is harmless rather than a
  // cancellation of whoever got that number next.
  const timers = new Map();
  let nextTimerId = 1;

  function schedule(fn, delay, args, repeating) {
    if (typeof fn === "string") {
      // `setTimeout("code")` is an implicit `eval`, and the surface is not
      // worth it. A browser accepts it; we say why instead.
      throw new TypeError(
        "setTimeout with a string is not supported: pass a function"
      );
    }
    if (typeof fn !== "function") return 0;
    const id = nextTimerId++;
    const ms = Number(delay);
    timers.set(id, {
      fn: fn,
      args: args,
      repeating: repeating,
      delay: isFinite(ms) && ms > 0 ? ms : 0,
    });
    raw.scheduleTimer(id, isFinite(ms) && ms > 0 ? ms : 0);
    return id;
  }

  function cancel(id) {
    const key = Number(id);
    if (timers.delete(key)) raw.cancelTimer(key);
  }

  // Called by the engine when a deadline comes up. Returns nothing a page can
  // see; its only job is to run the callback and re-arm an interval.
  function fireTimer(id) {
    const timer = timers.get(id);
    if (timer === undefined) return;
    // A one-shot is gone before its callback runs, so a `clearTimeout` inside
    // itself is a no-op rather than a cancel of the next id.
    if (!timer.repeating) timers.delete(id);
    try {
      timer.fn.apply(globalThis, timer.args);
    } catch (error) {
      raw.reportError(String(error && error.message ? error.message : error),
                      (error && error.stack) || "");
    }
    // An interval re-arms **after** its callback returns, timed from this
    // moment: a callback that runs longer than the interval falls behind
    // rather than building a queue of catch-up ticks it can never drain.
    if (timer.repeating && timers.has(id)) raw.scheduleTimer(id, timer.delay);
  }

  globalThis.setTimeout = function (fn, delay) {
    return schedule(fn, delay, Array.prototype.slice.call(arguments, 2), false);
  };
  globalThis.setInterval = function (fn, delay) {
    return schedule(fn, delay, Array.prototype.slice.call(arguments, 2), true);
  };
  globalThis.clearTimeout = cancel;
  globalThis.clearInterval = cancel;

  // ---- location and storage (M10.11) ----

  // `location` is parsed from the one string Rust hands over, which is the
  // page's post-redirect URL as `net::` produced it. The parsing is small and
  // lives here so there is no second URL parser in the engine.
  function parts() {
    const href = raw.pageUrl();
    const scheme = href.indexOf("://");
    // A page with no URL — a dump with nothing to resolve against — still has
    // to answer every property. Returning a partial object would make
    // `location.pathname` `undefined`, which a page cannot tell from a bug.
    if (scheme === -1) {
      return {
        href: href, protocol: "", host: "", hostname: "", port: "",
        pathname: href, search: "", hash: "", origin: "",
      };
    }
    const protocol = href.slice(0, scheme) + ":";
    let rest = href.slice(scheme + 3);
    let hash = "";
    const hashAt = rest.indexOf('#');
    if (hashAt !== -1) { hash = rest.slice(hashAt); rest = rest.slice(0, hashAt); }
    let search = "";
    const queryAt = rest.indexOf("?");
    if (queryAt !== -1) { search = rest.slice(queryAt); rest = rest.slice(0, queryAt); }
    const slash = rest.indexOf("/");
    const host = slash === -1 ? rest : rest.slice(0, slash);
    const path = slash === -1 ? "/" : rest.slice(slash);
    const colon = host.lastIndexOf(":");
    return {
      href: href,
      protocol: protocol,
      host: host,
      hostname: colon === -1 ? host : host.slice(0, colon),
      port: colon === -1 ? "" : host.slice(colon + 1),
      pathname: path,
      search: search,
      hash: hash,
      origin: protocol + "//" + host,
    };
  }

  const location = {
    get href() { return parts().href; },
    set href(value) { raw.navigate(String(value), false); },
    get protocol() { return parts().protocol; },
    get host() { return parts().host; },
    get hostname() { return parts().hostname; },
    get port() { return parts().port; },
    get pathname() { return parts().pathname; },
    get search() { return parts().search; },
    get hash() { return parts().hash; },
    set hash(value) {
      // An assignment, not a replacement: a browser pushes a history entry
      // for a fragment change, so `H` goes back to where the reader was.
      const to = String(value);
      raw.navigate(to.charAt(0) === '#' ? to : '#' + to, false);
    },
    get origin() { return parts().origin; },
    assign: function (url) { raw.navigate(String(url), false); },
    // `replace` does not push history — the distinction M6's `History`
    // already models, so it is carried through rather than invented here.
    replace: function (url) { raw.navigate(String(url), true); },
    reload: function () { raw.navigate(parts().href, true); },
    toString: function () { return parts().href; },
  };

  function storageArea(session) {
    return {
      getItem: function (key) {
        const value = raw.storageGet(session, String(key));
        return value === undefined ? null : value;
      },
      setItem: function (key, value) { raw.storageSet(session, String(key), String(value)); },
      removeItem: function (key) { raw.storageRemove(session, String(key)); },
      clear: function () { raw.storageClear(session); },
      key: function (i) {
        const key = raw.storageKey(session, Number(i) >>> 0);
        return key === undefined ? null : key;
      },
      get length() { return raw.storageLength(session); },
    };
  }

  // Defined so it can explain itself. A page calling it gets `not a function`
  // otherwise, which tells its author nothing about *why* — and the why is
  // architectural rather than a gap: we parse a page before running any of its
  // scripts, so there is no open token stream to write into.
  document.write = function () {
    console.warn('document.write is not supported: this browser finishes parsing ' +
                 'a page before running its scripts, so there is no token stream ' +
                 'left to write into');
  };
  document.writeln = document.write;

  globalThis.location = location;
  document.location = location;
  globalThis.localStorage = storageArea(false);
  globalThis.sessionStorage = storageArea(true);

  // Enough that feature detection does not crash, and no more: every field is
  // a promise about behaviour, and a browser string we do not honour is worse
  // than an honest one nobody recognises.
  globalThis.navigator = {
    userAgent: "yata (terminal browser; +https://github.com/yata)",
  };

  // ---- fetch (M10.12) ----

  // Requests waiting on the network, by id. The promise's resolvers live here
  // until the loop brings an answer back; nothing else can settle them.
  const inFlight = new Map();
  let nextRequestId = 1;

  // Options a browser honours and we do not. Logged once each rather than
  // ignored silently: quietly dropping a caller's option is how a page ends up
  // mystifyingly wrong.
  const IGNORED = ['mode', 'credentials', 'cache', 'redirect', 'referrer',
                   'integrity', 'signal', 'keepalive'];
  const warned = {};

  globalThis.fetch = function (input, init) {
    const url = String(input);
    const options = init || {};
    for (const name of IGNORED) {
      if (options[name] !== undefined && !warned[name]) {
        warned[name] = true;
        console.warn('fetch: the `' + name + '` option is ignored by this browser');
      }
    }

    const method = String(options.method || 'GET').toUpperCase();
    const headers = {};
    if (options.headers) {
      for (const key of Object.keys(options.headers)) {
        headers[String(key)] = String(options.headers[key]);
      }
    }
    const body = options.body === undefined || options.body === null
      ? undefined
      : String(options.body);

    const id = nextRequestId++;
    return new Promise(function (resolve, reject) {
      inFlight.set(id, { resolve: resolve, reject: reject });
      try {
        raw.startFetch(id, url, method, JSON.stringify(headers), body);
      } catch (error) {
        // Refused before it left: same origin, a bad URL, or too many in
        // flight. The promise rejects like a browser's would.
        inFlight.delete(id);
        reject(error);
      }
    });
  };

  function makeResponse(status, statusText, url, headerJson, body) {
    const headers = JSON.parse(headerJson);
    let consumed = false;
    function take() {
      if (consumed) {
        // A browser rejects a second read of the same body, and a page that
        // does it by accident should find out.
        return Promise.reject(new TypeError('body has already been consumed'));
      }
      consumed = true;
      return Promise.resolve(body);
    }
    return {
      // `ok` is false for a 404 — the response arrived, so the promise
      // *resolves*. Pages get this wrong constantly; we must not.
      ok: status >= 200 && status < 300,
      status: status,
      statusText: statusText,
      url: url,
      headers: {
        get: function (name) {
          const wanted = String(name).toLowerCase();
          for (const key of Object.keys(headers)) {
            if (key.toLowerCase() === wanted) return headers[key];
          }
          return null;
        },
      },
      text: take,
      json: function () { return take().then(function (t) { return JSON.parse(t); }); },
    };
  }

  // Called by the engine when a response comes back. `error` non-null means
  // the request never completed.
  function settleFetch(id, error, status, statusText, url, headerJson, body) {
    const pending = inFlight.get(id);
    if (pending === undefined) return;
    inFlight.delete(id);
    if (error !== null && error !== undefined) {
      pending.reject(new TypeError('fetch failed: ' + error));
    } else {
      pending.resolve(makeResponse(status, statusText, url, headerJson, body));
    }
  }

  // `window`, `globalThis` and the global scope are one object: pages branch
  // on `typeof window` and read `window.x` for a top-level `var x`.
  globalThis.window = globalThis;
  globalThis.document = document;
  globalThis.Element = Element;

  // The primitive object goes away here rather than in a statement after the
  // call, so that this function's value — the dispatcher — is what the eval
  // returns. Rust holds it as a `Persistent`, which is why dispatch needs no
  // global name a page could find or overwrite.
  delete globalThis.__dom;
  // Two entry points for the engine: one to dispatch an event, one to fire a
  // timer. Rust holds them as `Persistent`s, so a page can reach neither.
  return {
    dispatch: dispatch,
    fireTimer: fireTimer,
    settleFetch: settleFetch,
    pending: function () { return timers.size; },
  };
})(__dom)
"#;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::html;
    use crate::js::console::Console;
    use crate::js::{self, Host, JsValue};

    /// Run `script` against `page` and return the script's completion value,
    /// the way `--dump-js` would show it.
    fn eval_on(page: &str, script: &str) -> String {
        let mut dom = html::parse(&format!("{page}<script>{script}</script>"));
        let mut host = None;
        let runs = js::run_pass(&mut host, &mut dom, 7, &Console::new());
        assert_eq!(runs.len(), 1, "expected exactly one script");
        runs[0].dump_line()
    }

    /// The fixture the read bindings are exercised against.
    const PAGE: &str = r#"<title>Fixture</title>
        <div id="wrap" class="outer">
          <p class="note">hello <b>world</b></p>
          <p class="other">second</p>
        </div>"#;

    fn value(script: &str) -> String {
        eval_on(PAGE, script)
            .strip_prefix("inline#1 ok ")
            .expect("script threw")
            .to_string()
    }

    #[test]
    fn window_globalthis_and_the_global_scope_are_one_object() {
        // Pages branch on `typeof window`; getting this wrong makes half the
        // web's feature detection lie.
        assert_eq!(value("window === globalThis"), "true");
        assert_eq!(value("typeof window"), "\"object\"");
        assert_eq!(value("var top_level = 5; window.top_level"), "5");
        assert_eq!(value("typeof window.document"), "\"object\"");
    }

    #[test]
    fn document_reads_its_own_landmarks() {
        assert_eq!(value("document.title"), "\"Fixture\"");
        assert_eq!(value("document.body.tagName"), "\"BODY\"");
        assert_eq!(value("document.documentElement.tagName"), "\"HTML\"");
        // Uppercase, as the DOM specifies, even though the parser lowercases.
        assert_eq!(value("document.getElementById('wrap').tagName"), "\"DIV\"");
    }

    #[test]
    fn a_miss_is_null_and_not_an_exception() {
        assert_eq!(value("document.getElementById('absent')"), "null");
        assert_eq!(value("document.querySelector('table')"), "null");
        assert_eq!(value("document.querySelectorAll('table').length"), "0");
        assert_eq!(
            value("document.querySelector('p').getAttribute('zzz')"),
            "null"
        );
        assert_eq!(
            value("document.querySelector('p').hasAttribute('zzz')"),
            "false"
        );
        assert_eq!(
            value("document.querySelector('.other').nextElementSibling"),
            "null"
        );
    }

    #[test]
    fn two_handles_to_one_node_are_identical() {
        // `if (e.target === el)` is everywhere. Without interning, every read
        // would mint a fresh object and every such comparison would be false.
        assert_eq!(
            value("document.getElementById('wrap') === document.querySelector('#wrap')"),
            "true"
        );
        assert_eq!(
            value("document.querySelectorAll('p')[0] === document.querySelector('p')"),
            "true"
        );
        assert_eq!(
            value("document.querySelector('p') === document.querySelector('.other')"),
            "false"
        );
    }

    #[test]
    fn queries_go_through_the_css_matcher() {
        // A combinator and a compound: if `src/js` had grown its own selector
        // parser, this is where it would start disagreeing with the cascade.
        assert_eq!(
            value("document.querySelector('div > p.note').textContent"),
            "\"hello world\""
        );
        assert_eq!(value("document.querySelectorAll('#wrap p').length"), "2");
        assert_eq!(value("document.querySelectorAll('p.note, b').length"), "2");
        assert_eq!(value("document.querySelector('.outer').id"), "\"wrap\"");
    }

    #[test]
    fn an_invalid_selector_throws_a_syntax_error() {
        assert_eq!(
            value("try { document.querySelector('###') } catch (e) { e.name }"),
            "\"SyntaxError\""
        );
        // A selector string that closes its own block cannot smuggle a second
        // rule past the parser.
        assert_eq!(
            value("try { document.querySelectorAll('p{} body') } catch (e) { e.name }"),
            "\"SyntaxError\""
        );
    }

    #[test]
    fn element_reads_are_the_minimal_set() {
        assert_eq!(value("document.querySelector('p').className"), "\"note\"");
        assert_eq!(value("document.querySelector('p').id"), "\"\"");
        assert_eq!(
            value("document.querySelector('p').getAttribute('class')"),
            "\"note\""
        );
        assert_eq!(
            value("document.querySelector('p').hasAttribute('class')"),
            "true"
        );
        // textContent concatenates descendant text in document order.
        assert_eq!(
            value("document.getElementById('wrap').textContent.trim().split(/\\s+/).join('|')"),
            "\"hello|world|second\""
        );
        assert_eq!(
            value("document.querySelector('b').parentElement.tagName"),
            "\"P\""
        );
        assert_eq!(
            value("document.getElementById('wrap').children.length"),
            "2"
        );
        assert_eq!(
            value("document.getElementById('wrap').firstElementChild.className"),
            "\"note\""
        );
        assert_eq!(
            value("document.querySelector('.note').nextElementSibling.className"),
            "\"other\""
        );
    }

    #[test]
    fn a_handle_exposes_no_internals_to_the_page() {
        // The node id lives in a WeakMap, not on the object: `__id` turning up
        // in a for-in over an element would be a lie about what the DOM has.
        assert_eq!(value("Object.keys(document.body).length"), "0");
        assert_eq!(value("JSON.stringify(document.body)"), "\"{}\"");
        assert_eq!(value("typeof globalThis.__dom"), "\"undefined\"");
        assert_eq!(value("document.body instanceof Element"), "true");
    }

    #[test]
    fn a_removed_node_is_not_found_by_a_query() {
        let mut dom = html::parse(
            "<div id=keep>kept</div><div id=gone>removed</div><script>\
             [document.getElementById('keep') ? 'keep' : 'no-keep',\
              document.getElementById('gone') ? 'gone' : 'no-gone'].join(',')\
             </script>",
        );
        // Removed through the arena API rather than through `remove()`, so
        // this test is about the *query* and not about the binding.
        let gone = find_descendant(&dom, dom.root, &mut |dom, node| {
            dom.attr(node, "id") == Some("gone")
        })
        .expect("fixture has the node");
        dom.remove(gone);

        let mut host = None;
        let runs = js::run_pass(&mut host, &mut dom, 7, &Console::new());
        assert_eq!(runs[0].dump_line(), "inline#1 ok \"keep,no-gone\"");
    }

    #[test]
    fn a_handle_from_a_superseded_page_refuses_to_resolve() {
        // The bug this prevents: there is only ever one page's DOM in memory,
        // so a handle held by a stale closure would otherwise read whichever
        // node now sits at that index — silently the wrong element.
        let mut host: Option<Host> = None;

        let mut first = html::parse(
            "<p id=one>first page</p><script>globalThis.kept = document.getElementById('one');</script>",
        );
        let runs = js::run_pass(&mut host, &mut first, 1, &Console::new());
        assert_eq!(runs[0].dump_line(), "inline#1 ok object");
        assert!(host.is_some());

        // The same host, a different page. (The real app drops the host on
        // navigation — this is the guard for the day something keeps one.)
        let mut second = html::parse(
            "<p id=two>second page</p><script>\
             try { kept.tagName } catch (e) { 'refused: ' + e.message }</script>",
        );
        let runs = js::run_pass(&mut host, &mut second, 2, &Console::new());
        assert_eq!(
            runs[0].dump_line(),
            "inline#1 ok \"refused: stale node handle: it belongs to a page that is no longer loaded\""
        );
    }

    #[test]
    fn a_handle_stays_valid_across_ticks_of_the_same_page() {
        // The other half of the guard: within one page a handle must survive
        // between passes, or M10.8's listeners could never hold one.
        let mut host: Option<Host> = None;

        let mut dom = html::parse(
            "<p id=one>text</p><script>globalThis.kept = document.getElementById('one');</script>",
        );
        js::run_pass(&mut host, &mut dom, 3, &Console::new());

        let mut again = html::parse("<p id=one>text</p><script>kept.tagName</script>");
        let runs = js::run_pass(&mut host, &mut again, 3, &Console::new());
        assert_eq!(runs[0].dump_line(), "inline#1 ok \"P\"");
    }

    #[test]
    fn the_dom_is_unreachable_between_ticks() {
        // A callback that runs when no tick owns a tree — M10.9's timers will
        // be the first — gets an exception, not a stale read.
        let slot = DomSlot::default();
        assert!(slot.take().is_none());
        let mut host = Host::new(&Console::new(), &Storage::new()).expect("host starts");
        // Nothing has been lent, so the bindings have no tree to read.
        let error = host.eval("probe.js", "document.body").unwrap_err();
        assert!(
            error
                .message
                .contains("not available outside a script tick"),
            "{error}"
        );
        // And the host is still usable once one is lent.
        let mut dom = html::parse("<p>after</p><script>document.body.tagName</script>");
        let mut host = Some(host);
        let runs = js::run_pass(&mut host, &mut dom, 9, &Console::new());
        assert_eq!(runs[0].dump_line(), "inline#1 ok \"BODY\"");
    }

    #[test]
    fn reads_work_on_a_subtree_that_is_not_in_the_document() {
        // The primitives underneath must be total over the arena, because
        // `createElement` hands a page a node that is in it and in no tree —
        // this pins that reading one is well-defined before any binding does.
        let mut dom = html::parse("<p>in the tree</p>");
        let orphan = dom.create_element("section", vec![("id".into(), "loose".into())]);
        let text = dom.create_text("detached text");
        dom.append(orphan, text).unwrap();

        assert_eq!(text_of(&dom, orphan), "detached text");
        assert_eq!(dom.attr(orphan, "id"), Some("loose"));
        assert_eq!(element_children(&dom, orphan), vec![]);
        // And it is not in the document, so a query cannot see it.
        assert_eq!(
            find_descendant(&dom, dom.root, &mut |dom, node| dom.attr(node, "id")
                == Some("loose")),
            None
        );
    }

    #[test]
    fn a_js_supplied_id_outside_the_arena_cannot_panic() {
        // The prelude hides these numbers, but nothing may panic on one.
        let dom = html::parse("<p>x</p>");
        assert_eq!(node(&dom, u32::MAX), None);
        assert_eq!(node(&dom, dom.node_count() as u32), None);
        assert_eq!(node(&dom, 0), Some(dom.root));
    }

    #[test]
    fn text_content_of_a_text_node_is_its_own_text() {
        let dom = html::parse("<p>just text</p>");
        let text = find_descendant(&dom, dom.root, &mut |dom, node| {
            matches!(dom.node(node).data, NodeData::Text(_))
        })
        .expect("the fixture has a text node");
        assert_eq!(text_of(&dom, text), "just text");
    }

    // ---- writes (M10.5) ----

    /// Run `script` against `page` and hand back both the completion value and
    /// the tree it left behind — the DOM effect and the script's own view of
    /// it are the two halves a write binding has to get right.
    ///
    /// `box` is bound for the script: a browser would expose an element's `id`
    /// as a global by itself, and we deliberately do not (see the M10.4
    /// deviations), so the tests say so out loud instead of relying on it.
    fn mutate(page: &str, script: &str) -> (String, Dom) {
        let script = format!("var box = document.getElementById('box');\n{script}");
        let mut dom = html::parse(&format!("{page}<script>{script}</script>"));
        let mut host = None;
        let runs = js::run_pass(&mut host, &mut dom, 7, &Console::new());
        crate::dom::check_links(&dom);
        let line = runs.last().expect("the script ran").dump_line();
        (line, dom)
    }

    fn wrote(page: &str, script: &str) -> String {
        let (line, _) = mutate(page, script);
        line.strip_prefix("inline#1 ok ")
            .unwrap_or(&line)
            .to_string()
    }

    const BOX: &str = "<div id=box class='a  b'><p>one</p><p>two</p></div>";

    #[test]
    fn text_content_replaces_every_child_with_one_text_node() {
        assert_eq!(
            wrote(BOX, "box.textContent = 'replaced'; box.innerHTML"),
            "\"replaced\""
        );
        // The empty case is how a page clears a container, and it must leave
        // no text node behind rather than an empty one.
        let (_, dom) = mutate(BOX, "box.textContent = '';");
        let target = find_descendant(&dom, dom.root, &mut |dom, node| {
            dom.attr(node, "id") == Some("box")
        })
        .unwrap();
        assert_eq!(dom.children(target).count(), 0);
    }

    #[test]
    fn id_and_class_writes_are_visible_to_the_next_query() {
        assert_eq!(
            wrote(
                BOX,
                "box.id = 'renamed'; document.querySelector('#renamed') !== null"
            ),
            "true"
        );
        assert_eq!(
            wrote(BOX, "box.className = 'x y'; box.getAttribute('class')"),
            "\"x y\""
        );
        assert_eq!(
            wrote(
                BOX,
                "box.setAttribute('data-k', 'v'); box.getAttribute('data-k')"
            ),
            "\"v\""
        );
        assert_eq!(
            wrote(BOX, "box.removeAttribute('class'); box.className"),
            "\"\""
        );
    }

    #[test]
    fn class_list_is_a_live_view_over_the_attribute() {
        // Live, not a snapshot: the write is visible to a read right after it,
        // through either spelling.
        assert_eq!(
            wrote(BOX, "box.classList.add('c'); box.getAttribute('class')"),
            "\"a b c\""
        );
        assert_eq!(
            wrote(BOX, "box.classList.add('c'); box.classList.contains('c')"),
            "true"
        );
        assert_eq!(
            wrote(BOX, "box.classList.remove('a'); box.className"),
            "\"b\""
        );
        assert_eq!(
            wrote(
                BOX,
                "[box.classList.toggle('a'), box.classList.toggle('z'), box.className].join('|')"
            ),
            "\"false|true|b z\""
        );
        // `toggle(name, force)` sets rather than flips.
        assert_eq!(
            wrote(BOX, "box.classList.toggle('a', true); box.className"),
            "\"a b\""
        );
        assert_eq!(
            wrote(
                BOX,
                "[box.classList.contains('a'), box.classList.contains('nope')].join('|')"
            ),
            "\"true|false\""
        );
    }

    #[test]
    fn class_list_follows_the_dom_on_whitespace_and_duplicates() {
        // The attribute is an ordered set of tokens: `'a  b'` is two tokens,
        // and writing the set back serializes it single-spaced.
        assert_eq!(wrote(BOX, "box.classList.length"), "2");
        assert_eq!(
            wrote(BOX, "box.classList.add('a'); box.className"),
            "\"a b\""
        );
        assert_eq!(
            wrote(
                "<div id=box class='dup dup  x'></div>",
                "box.classList.length"
            ),
            "2"
        );
        assert_eq!(
            wrote(
                "<div id=box class='dup dup  x'></div>",
                "box.classList.add('y'); box.className"
            ),
            "\"dup x y\""
        );
        assert_eq!(
            wrote(
                "<div id=box class='  spaced  '></div>",
                "box.classList.toString()"
            ),
            "\"spaced\""
        );
    }

    #[test]
    fn created_nodes_join_the_tree_and_are_found_by_queries() {
        assert_eq!(
            wrote(
                BOX,
                "var e = document.createElement('SPAN'); e.textContent = 'new';\
                 document.body.appendChild(e);\
                 document.querySelector('span').textContent"
            ),
            "\"new\""
        );
        // `createElement` lowercases, so a created tag and a parsed one are
        // the same tag to the selector matcher.
        assert_eq!(
            wrote(
                BOX,
                "document.body.appendChild(document.createElement('SPAN')).tagName"
            ),
            "\"SPAN\""
        );
        assert_eq!(
            wrote(
                BOX,
                "box.appendChild(document.createTextNode('!')); box.textContent"
            ),
            "\"onetwo!\""
        );
    }

    #[test]
    fn tree_edits_move_and_remove() {
        assert_eq!(
            wrote(
                BOX,
                "box.insertBefore(document.createElement('i'), box.firstElementChild);\
                 box.firstElementChild.tagName"
            ),
            "\"I\""
        );
        // `insertBefore(node, null)` is an append — pages rely on it.
        assert_eq!(
            wrote(
                BOX,
                "box.insertBefore(document.createElement('i'), null);\
                 box.children[box.children.length - 1].tagName"
            ),
            "\"I\""
        );
        assert_eq!(
            wrote(
                BOX,
                "box.removeChild(box.firstElementChild); box.children.length"
            ),
            "1"
        );
        assert_eq!(
            wrote(
                BOX,
                "document.querySelector('p').remove(); document.querySelectorAll('p').length"
            ),
            "1"
        );
        // Appending a node that already has a parent moves it (M10.3), it does
        // not alias it into two places.
        assert_eq!(
            wrote(
                BOX,
                "document.body.appendChild(box.firstElementChild);\
                 [box.children.length, document.body.children.length].join('|')"
            ),
            "\"1|3\""
        );
    }

    #[test]
    fn a_refused_edit_throws_instead_of_doing_nothing() {
        // The page has to find out. A silent no-op here is the failure mode
        // M10.3's refusals exist to make visible.
        assert_eq!(
            wrote(BOX, "try { box.appendChild(box) } catch (e) { e.message }"),
            "\"HierarchyRequestError: the node cannot be placed there\""
        );
        assert_eq!(
            wrote(
                BOX,
                "try { box.appendChild(document.documentElement) } catch (e) { e.message }"
            ),
            "\"HierarchyRequestError: the node cannot be placed there\""
        );
        assert_eq!(
            wrote(
                BOX,
                "try { document.body.removeChild(box.firstElementChild) } catch (e) { e.message }"
            ),
            "\"NotFoundError: the node is not a child of this one\""
        );
        // A text node cannot hold children (M10.3's rule, surfaced here).
        assert_eq!(
            wrote(
                BOX,
                "try { document.createTextNode('t').appendChild(document.createElement('b')) }\
                 catch (e) { e.message }"
            ),
            "\"HierarchyRequestError: the node cannot be placed there\""
        );
    }

    #[test]
    fn an_attribute_name_that_could_not_be_read_back_is_refused() {
        // HTML has no escape for attribute names, so accepting these would
        // mean `innerHTML` produces markup that reparses as *different*
        // attributes — a read-modify-write would corrupt the tree.
        for bad in [
            "a b=\"c", // the shape that turns one attribute into three
            "<script>",
            "has space",
            "quote\"",
            "eq=als",
            "",
        ] {
            assert_eq!(
                wrote(
                    BOX,
                    &format!(
                        "try {{ box.setAttribute({bad:?}, 'x') }} catch (e) {{ e.message.split(':')[0] }}"
                    )
                ),
                "\"InvalidCharacterError\"",
                "setAttribute accepted {bad:?}"
            );
        }
        // The names pages actually use keep working.
        for good in ["data-x", "aria-label", "xml:lang", "_x", "x1"] {
            assert_eq!(
                wrote(
                    BOX,
                    &format!("box.setAttribute({good:?}, 'v'); box.getAttribute({good:?})")
                ),
                "\"v\"",
                "setAttribute refused {good:?}"
            );
        }
    }

    #[test]
    fn inner_html_reads_back_what_the_parser_built() {
        assert_eq!(wrote(BOX, "box.innerHTML"), "\"<p>one</p><p>two</p>\"");
        // Escaping: what comes out must parse back as text, not as markup.
        assert_eq!(
            wrote("<div id=box>a &lt; b &amp; c</div>", "box.innerHTML"),
            "\"a &lt; b &amp; c\""
        );
        assert_eq!(
            wrote(
                r#"<div id=box><a href='?x=1&amp;y=2' title='he said "hi"'>l</a></div>"#,
                "box.innerHTML"
            ),
            "\"<a href=\\\"?x=1&amp;y=2\\\" title=\\\"he said &quot;hi&quot;\\\">l</a>\""
        );
        // A void element gets no closing tag: `</br>` would parse back as a
        // second element.
        assert_eq!(
            wrote("<div id=box>a<br>b</div>", "box.innerHTML"),
            "\"a<br>b\""
        );
    }

    #[test]
    fn inner_html_writes_a_parsed_fragment() {
        assert_eq!(
            wrote(
                BOX,
                "box.innerHTML = '<b>bold</b> text'; box.children.length"
            ),
            "1"
        );
        assert_eq!(
            wrote(
                BOX,
                "box.innerHTML = '<b>bold</b> &amp; text'; box.textContent"
            ),
            "\"bold & text\""
        );
        // Setting it empty clears the container.
        assert_eq!(wrote(BOX, "box.innerHTML = ''; box.innerHTML"), "\"\"");
        // And the new subtree is a real part of the document.
        assert_eq!(
            wrote(
                BOX,
                "box.innerHTML = '<ul><li class=item>a</li><li class=item>b</li></ul>';\
                 document.querySelectorAll('#box li.item').length"
            ),
            "2"
        );
    }

    #[test]
    fn inner_html_round_trips_the_ladder_fixtures() {
        // parse → serialize → parse gives the same tree. If it does not, one
        // of the two is lying about what the document says.
        for fixture in [
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/example.com.html"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/motherfuckingwebsite.com.html"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/danluu.com.html"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/news.ycombinator.com.html"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/en.wikipedia.org.html"
            )),
        ] {
            let dom = html::parse(fixture);
            let body = find_tag(&dom, dom.root, "body").expect("every parse synthesizes a body");
            let serialized = html::serialize_children(&dom, body);

            let reparsed = html::parse(&format!("<body>{serialized}"));
            let reparsed_body = find_tag(&reparsed, reparsed.root, "body").unwrap();
            assert_eq!(
                html::serialize_children(&reparsed, reparsed_body),
                serialized,
                "a second round trip changed the document"
            );
        }
    }

    #[test]
    fn a_fragment_is_parsed_without_its_context() {
        // The deviation, deliberately: a browser parses `innerHTML` *with the
        // target element as context*, so table parts written into a `<div>`
        // lose their tags and keep their text. Ours parses as a document and
        // adopts the body's children, so the cells survive.
        let (_, dom) = mutate("<div id=box></div>", "box.innerHTML = '<td>cell</td>';");
        let serialized = {
            let target = find_descendant(&dom, dom.root, &mut |dom, node| {
                dom.attr(node, "id") == Some("box")
            })
            .unwrap();
            html::serialize_children(&dom, target)
        };
        assert_eq!(
            serialized, "<td>cell</td>",
            "a browser would give `cell` here — see the M10.5 deviations"
        );
    }

    #[test]
    fn a_script_set_style_attribute_reaches_computed_values() {
        // The write goes through `setAttribute`, and the cascade's existing
        // `style=""` parsing does the rest — no new path.
        let (_, dom) = mutate(
            "<p id=box>text</p>",
            "box.setAttribute('style', 'display: none');",
        );
        let styles = crate::style::style_tree(&dom, &[]);
        let target = find_descendant(&dom, dom.root, &mut |dom, node| {
            dom.attr(node, "id") == Some("box")
        })
        .unwrap();
        assert_eq!(
            styles.get(target).display,
            crate::style::values::Display::None
        );
    }

    #[test]
    fn a_class_set_by_script_is_matched_by_the_page_stylesheet() {
        // A binding that changes the tree but not the screen is the bug this
        // milestone exists to prevent, so the assertion goes all the way to a
        // computed value: the page's own rule has to find the element the
        // script just tagged.
        let (_, dom) = mutate(
            "<style>.hidden { display: none }</style><p id=box>text</p>",
            "box.classList.add('hidden');",
        );
        let sheets = crate::style::sources::inline_sheets(&dom);
        let styles = crate::style::style_tree(&dom, &sheets.iter().collect::<Vec<_>>());
        let target = find_descendant(&dom, dom.root, &mut |dom, node| {
            dom.attr(node, "id") == Some("box")
        })
        .unwrap();
        assert_eq!(
            styles.get(target).display,
            crate::style::values::Display::None
        );
    }

    #[test]
    fn mutating_a_collection_while_iterating_it_is_a_snapshot() {
        // The semantics chosen, and the reason: `children` returns a plain
        // array taken at the moment of the call, so removing during iteration
        // visits every element exactly once. A live collection would skip
        // every other one, which is the classic bug this avoids.
        assert_eq!(
            wrote(
                "<div id=box><p>1</p><p>2</p><p>3</p><p>4</p></div>",
                "var seen = 0; for (const c of box.children) { seen++; c.remove(); }\
                 [seen, box.children.length].join('|')"
            ),
            "\"4|0\""
        );
        // Removing the same node twice is a no-op, not a corruption: the id
        // still means that node, it simply has no parent any more.
        assert_eq!(
            wrote(
                BOX,
                "var p = box.firstElementChild; p.remove(); p.remove(); p.tagName"
            ),
            "\"P\""
        );
        // And a handle to a removed node still reads.
        assert_eq!(
            wrote(
                BOX,
                "var p = box.firstElementChild; p.remove(); p.textContent"
            ),
            "\"one\""
        );
    }

    // ---- collections (M11.1) ----------------------------------------------

    const NESTED: &str = "<div id=box class='a b'><p class='a'>one</p>\
         <section><p class='b other'>two</p><span class='a b'>three</span></section></div>";

    #[test]
    fn get_elements_by_tag_name_walks_in_document_order() {
        let page = NESTED;
        assert_eq!(
            eval_on(
                page,
                "document.getElementsByTagName('p').map(function (e) { return e.textContent; }).join(',')"
            ),
            "inline#1 ok \"one,two\""
        );
        // Case-insensitive, because HTML tag names are.
        assert_eq!(
            eval_on(page, "document.getElementsByTagName('P').length"),
            "inline#1 ok 2"
        );
        // A miss is an empty array, not null — pages index straight into it.
        assert_eq!(
            eval_on(page, "document.getElementsByTagName('table').length"),
            "inline#1 ok 0"
        );
        // `*` is every element.
        assert_eq!(
            eval_on(page, "document.getElementsByTagName('*').length > 5"),
            "inline#1 ok true"
        );
    }

    #[test]
    fn an_element_scoped_call_excludes_itself_and_everything_outside() {
        assert_eq!(
            eval_on(
                NESTED,
                "var s = document.querySelector('section');\
                 [s.getElementsByTagName('p').length, s.getElementsByTagName('section').length,\
                  document.getElementById('box').getElementsByTagName('p').length].join(',')"
            ),
            "inline#1 ok \"1,0,2\""
        );
    }

    #[test]
    fn get_elements_by_class_name_needs_every_class() {
        assert_eq!(
            eval_on(NESTED, "document.getElementsByClassName('a').length"),
            "inline#1 ok 3"
        );
        // Both, not either: an intersection is what a page means by two names.
        assert_eq!(
            eval_on(
                NESTED,
                "document.getElementsByClassName('a b').map(function (e) { return e.tagName; }).join(',')"
            ),
            "inline#1 ok \"DIV,SPAN\""
        );
        assert_eq!(
            eval_on(
                NESTED,
                "document.getElementsByClassName('a missing').length"
            ),
            "inline#1 ok 0"
        );
        assert_eq!(
            eval_on(NESTED, "document.getElementsByClassName('').length"),
            "inline#1 ok 0"
        );
    }

    #[test]
    fn the_analytics_shape_that_started_this_task_now_runs() {
        // motherfuckingwebsite.com's Google Analytics loader, every line of
        // it. M11.1 made `getElementsByTagName` work and this test recorded
        // how much further the snippet got: `"reached, no parentNode"`. M11.5
        // is the task that deletes that string — `parentNode`, `.async` and
        // `.src` all exist now, so the loader runs to its end and leaves a
        // `<script src>` in the tree where the engine can see it.
        //
        // What happens to that element afterwards is `App`'s, and is pinned
        // there (`the_analytics_loader_fetches_google_analytics_exactly_once`).
        assert_eq!(
            eval_on(
                "<p>page</p>",
                "var o = 'script', s = document, g = '//www.google-analytics.com/analytics.js';\
                 var a = s.createElement(o);\
                 var m = s.getElementsByTagName(o)[0];\
                 a.async = 1; a.src = g;\
                 m.parentNode.insertBefore(a, m);\
                 [a.parentNode === m.parentNode, a.async, a.src].join(' ')"
            ),
            "inline#1 ok \"true true //www.google-analytics.com/analytics.js\""
        );
    }

    #[test]
    fn parent_node_reaches_the_document_where_parent_element_stops() {
        // The difference between the two, which is the whole reason the
        // snippet above needed this one: `<html>`'s parent is a node but not
        // an element.
        assert_eq!(value("document.body.parentNode.tagName"), "\"HTML\"");
        assert_eq!(value("document.documentElement.parentElement"), "null");
        assert_eq!(
            value("document.documentElement.parentNode === document"),
            "true"
        );
        // A node nothing has inserted has no parent of either kind.
        assert_eq!(value("document.createElement('p').parentNode"), "null");
    }

    #[test]
    fn src_async_and_defer_reflect_in_both_directions() {
        // Reflection, and nothing more: `async`/`defer` change no order (the
        // standing deviation), they just stop being lost on the wrapper.
        assert_eq!(
            wrote(
                BOX,
                "box.src = 'a.js'; [box.getAttribute('src'), box.src].join(',')"
            ),
            "\"a.js,a.js\""
        );
        assert_eq!(
            wrote(BOX, "[box.async, box.defer].join(',')"),
            "\"false,false\""
        );
        assert_eq!(
            wrote(
                BOX,
                "box.async = 1; box.defer = true;\
                 [box.async, box.defer, box.getAttribute('async'), box.hasAttribute('defer')]\
                   .join(',')"
            ),
            "\"true,true,,true\""
        );
        assert_eq!(
            wrote(
                BOX,
                "box.async = 1; box.async = false; box.hasAttribute('async')"
            ),
            "false"
        );
        // Set from the markup side, read from the property side.
        assert_eq!(
            wrote(
                "<div id=box async defer></div>",
                "[box.async, box.defer].join(',')"
            ),
            "\"true,true\""
        );
    }

    #[test]
    fn an_event_handler_property_replaces_rather_than_accumulates() {
        // `s.onload = next` is what a bootstrap chains on, and it is not
        // `addEventListener`: assigning twice leaves one handler, not two, and
        // assigning null leaves none.
        assert_eq!(
            wrote(
                BOX,
                "var log = [];\
                 box.onload = function () { log.push('first'); };\
                 box.onload = function () { log.push('second'); };\
                 box.addEventListener('load', function () { log.push('listener'); });\
                 [typeof box.onload, log.length].join(',')"
            ),
            "\"function,0\""
        );
        assert_eq!(
            wrote(
                BOX,
                "box.onload = function () {}; box.onload = null; box.onload"
            ),
            "null"
        );
    }

    // ---- location and storage (M10.11) ------------------------------------

    /// Run `script` on a page served from `url` and return what it logged.
    fn at(url: &str, script: &str) -> Vec<String> {
        let mut dom = html::parse(&format!("<p>x</p><script>{script}</script>"));
        let mut host = None;
        let console = Console::new();
        js::run_pass_at(&mut host, &mut dom, 1, url, &console);
        console.entries().iter().map(ToString::to_string).collect()
    }

    #[test]
    fn location_reports_the_pages_own_url_in_pieces() {
        assert_eq!(
            at(
                "https://example.com:8443/docs/a?q=1&r=2#frag",
                "console.log([location.protocol, location.host, location.hostname,\
                              location.port, location.pathname, location.search,\
                              location.hash, location.origin].join(' | '));"
            ),
            [
                "log   https: | example.com:8443 | example.com | 8443 | /docs/a | ?q=1&r=2 | #frag | https://example.com:8443"
            ]
        );
        assert_eq!(
            at(
                "https://example.com/",
                "console.log(location.href + ' ' + location);"
            ),
            ["log   https://example.com/ https://example.com/"]
        );
        // No port, no query, no fragment: the empty cases a page tests for.
        assert_eq!(
            at(
                "http://example.com/",
                "console.log([location.port, location.search, location.hash, location.pathname].join('|'));"
            ),
            ["log   |||/"]
        );
        // `document.location` is the same object a page can read either way.
        assert_eq!(
            at(
                "https://example.com/",
                "console.log(document.location === location);"
            ),
            ["log   true"]
        );
    }

    #[test]
    fn local_storage_round_trips_and_coerces_to_strings() {
        assert_eq!(
            at(
                "https://a.test/",
                "localStorage.setItem('k', 42);\
                 console.log(typeof localStorage.getItem('k'), localStorage.getItem('k'));"
            ),
            ["log   string 42"]
        );
        assert_eq!(
            at(
                "https://a.test/",
                "console.log(localStorage.getItem('absent'));"
            ),
            ["log   null"]
        );
        assert_eq!(
            at(
                "https://a.test/",
                "localStorage.setItem('a', '1'); localStorage.setItem('b', '2');\
                 var keys = []; for (var i = 0; i < localStorage.length; i++) keys.push(localStorage.key(i));\
                 localStorage.removeItem('a');\
                 console.log(keys.join(',') + ' then ' + localStorage.length);\
                 localStorage.clear();\
                 console.log('after clear ' + localStorage.length);"
            ),
            ["log   a,b then 1", "log   after clear 0"]
        );
    }

    #[test]
    fn local_and_session_storage_are_separate() {
        assert_eq!(
            at(
                "https://a.test/",
                "localStorage.setItem('k', 'local'); sessionStorage.setItem('k', 'session');\
                 console.log(localStorage.getItem('k') + ' / ' + sessionStorage.getItem('k'));"
            ),
            ["log   local / session"]
        );
    }

    #[test]
    fn two_origins_never_see_each_others_storage() {
        // One session, two origins: the isolation test the task asks for. The
        // store outlives each page, so this is the shape a real session has.
        let storage = Storage::new();
        let console = Console::new();

        let run = |url: &str, script: &str| {
            let mut dom = html::parse(&format!("<p>x</p><script>{script}</script>"));
            let mut host = None;
            let (mut queue, _) =
                crate::js::queue::ScriptQueue::new(crate::js::sources::sources(&dom), &console);
            let ready = queue.take_ready_prefix();
            js::run_prefix(
                &mut host,
                &mut dom,
                &js::PageContext {
                    page: 1,
                    url,
                    console: &console,
                    storage: &storage,
                },
                ready,
                true,
            );
        };

        run(
            "https://a.test/one",
            "localStorage.setItem('who', 'from a');",
        );
        // A second page on the *same* origin sees it.
        run(
            "https://a.test/two",
            "console.log('same origin reads: ' + localStorage.getItem('who'));",
        );
        // A different origin does not.
        run(
            "https://b.test/one",
            "console.log('other origin reads: ' + localStorage.getItem('who'));",
        );
        // Nor does the same host on a different scheme.
        run(
            "http://a.test/one",
            "console.log('other scheme reads: ' + localStorage.getItem('who'));",
        );

        assert_eq!(
            console
                .entries()
                .iter()
                .map(|e| e.text.clone())
                .collect::<Vec<_>>(),
            [
                "same origin reads: from a",
                "other origin reads: null",
                "other scheme reads: null",
            ]
        );
    }

    #[test]
    fn exceeding_the_quota_throws_the_way_a_browser_does() {
        let entries = at(
            "https://a.test/",
            "try { localStorage.setItem('k', 'x'.repeat(2 * 1024 * 1024)); }\
             catch (e) { console.log(e.message.split(':')[0]); }",
        );
        assert_eq!(entries, ["log   QuotaExceededError"]);
    }

    #[test]
    fn navigator_says_what_we_are_and_nothing_else() {
        // Every field is a promise about behaviour; the only one worth making
        // is what this is.
        assert_eq!(
            at(
                "https://a.test/",
                "console.log(navigator.userAgent.indexOf('yata') === 0, Object.keys(navigator).length);"
            ),
            ["log   true 1"]
        );
    }

    #[test]
    fn cookies_and_the_history_api_are_absent_rather_than_stubbed() {
        // A stub that lies is worse than a name that is not there: a page can
        // feature-detect an absence, but it cannot detect a `pushState` that
        // silently does nothing.
        assert_eq!(
            at(
                "https://a.test/",
                "console.log(document.cookie === undefined, typeof history);"
            ),
            ["log   true undefined"]
        );
    }

    // ---- timers and microtasks (M10.9) ------------------------------------

    /// Run `script`, then fire whatever timers it scheduled, in the order the
    /// timer thread would: earliest deadline first, ties by insertion. Returns
    /// the console.
    ///
    /// The clock is *not* involved — the scheduling order is computed here so
    /// the test asserts the engine's behaviour rather than the machine's load.
    fn with_timers(script: &str, rounds: usize) -> Vec<String> {
        let mut dom = html::parse(&format!("<p id=out>x</p><script>{script}</script>"));
        let mut host = None;
        let console = Console::new();
        js::run_pass(&mut host, &mut dom, 1, &console);

        let mut queue: Vec<(Duration, u64, u64)> = Vec::new();
        let mut seq = 0u64;
        let mut now = Duration::ZERO;
        let drain = |host: &Option<Host>, queue: &mut Vec<_>, seq: &mut u64, now: Duration| {
            for (id, delay) in host.as_ref().unwrap().take_timer_requests() {
                match delay {
                    Some(delay) => {
                        *seq += 1;
                        queue.push((now + delay.max(crate::timers::MIN_DELAY), *seq, id));
                    }
                    None => queue.retain(|&(_, _, queued)| queued != id),
                }
            }
        };
        drain(&host, &mut queue, &mut seq, now);

        for _ in 0..rounds {
            queue.sort();
            if queue.is_empty() {
                break;
            }
            let (due, _, id) = queue.remove(0);
            now = due;
            let engine = host.as_mut().expect("the page ran script");
            let _ = js::fire_timer(
                engine,
                &mut dom,
                &js::PageContext {
                    page: 1,
                    url: "https://fixture.test/page",
                    console: &console,
                    storage: &Storage::new(),
                },
                crate::timers::TimerId(id),
            );
            drain(&host, &mut queue, &mut seq, now);
        }
        console.entries().iter().map(ToString::to_string).collect()
    }

    #[test]
    fn set_timeout_runs_its_callback_with_its_extra_arguments() {
        assert_eq!(
            with_timers(
                "setTimeout(function (a, b) { console.log('fired', a, b); }, 5, 'one', 2);",
                4
            ),
            ["log   fired one 2"]
        );
    }

    #[test]
    fn timers_fire_in_deadline_order_not_registration_order() {
        // Deliverable 8's sequence: deadline first, ties by insertion, and a
        // timer scheduled from inside a callback runs after the current one.
        assert_eq!(
            with_timers(
                "setTimeout(function () { console.log('c'); }, 30);\
                 setTimeout(function () { console.log('a'); }, 0);\
                 setTimeout(function () { console.log('b'); }, 0);\
                 setTimeout(function () {\
                   console.log('nested-parent');\
                   setTimeout(function () { console.log('nested-child'); }, 0);\
                 }, 10);",
                8
            ),
            [
                "log   a",
                "log   b",
                "log   nested-parent",
                "log   nested-child",
                "log   c",
            ]
        );
    }

    #[test]
    fn an_interval_repeats_until_it_is_cleared() {
        assert_eq!(
            with_timers(
                "var n = 0;\
                 var h = setInterval(function () {\
                   n++;\
                   console.log('tick ' + n);\
                   if (n === 3) clearInterval(h);\
                 }, 10);",
                10
            ),
            ["log   tick 1", "log   tick 2", "log   tick 3"]
        );
    }

    #[test]
    fn a_cleared_timeout_never_runs() {
        assert_eq!(
            with_timers(
                "var h = setTimeout(function () { console.log('should not run'); }, 5);\
                 clearTimeout(h);\
                 setTimeout(function () { console.log('the other one'); }, 5);",
                4
            ),
            ["log   the other one"]
        );
    }

    #[test]
    fn timer_ids_are_positive_and_never_reused() {
        assert_eq!(
            with_timers(
                "var a = setTimeout(function () {}, 5);\
                 clearTimeout(a);\
                 var b = setTimeout(function () {}, 5);\
                 console.log(a > 0 && b > 0 && a !== b);",
                2
            ),
            ["log   true"]
        );
    }

    #[test]
    fn set_timeout_with_a_string_throws_instead_of_evaluating_it() {
        // An implicit `eval` is not worth the surface. A browser accepts it;
        // we say why, in the console, where the page's author can see it.
        let entries = with_timers("setTimeout('console.log(1)', 5);", 2);
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(
            entries[0].contains("setTimeout with a string is not supported"),
            "{:?}",
            entries[0]
        );
    }

    #[test]
    fn a_timer_callback_that_throws_is_reported_and_the_next_one_still_runs() {
        let entries = with_timers(
            "setTimeout(function () { null.x; }, 5);\
             setTimeout(function () { console.log('the next one'); }, 10);",
            4,
        );
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(entries[0].starts_with("error"), "{:?}", entries[0]);
        assert_eq!(entries[1], "log   the next one");
    }

    #[test]
    fn a_timer_callback_mutates_through_the_same_bindings() {
        assert_eq!(
            with_timers(
                "setTimeout(function () {\
                   document.getElementById('out').textContent = 'changed';\
                   console.log(document.getElementById('out').textContent);\
                 }, 5);",
                2
            ),
            ["log   changed"]
        );
    }

    #[test]
    fn promise_jobs_run_after_the_script_and_before_the_tick_ends() {
        // QuickJS queues them and nothing runs them unless we do, so without
        // the pump a `.then` is indistinguishable from a broken engine.
        let mut dom = html::parse(
            "<p>x</p><script>\
             console.log('sync');\
             Promise.resolve(1).then(function (v) { console.log('then ' + v); });\
             Promise.resolve().then(function () { return 2; })\
                              .then(function (v) { console.log('chained ' + v); });\
             console.log('sync end');</script>",
        );
        let mut host = None;
        let console = Console::new();
        js::run_pass(&mut host, &mut dom, 1, &console);
        assert_eq!(
            console
                .entries()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "log   sync",
                "log   sync end",
                "log   then 1",
                "log   chained 2",
                // The load events come after the microtasks the script queued,
                // because the pump runs at the end of the tick.
            ]
        );
    }

    #[test]
    fn a_promise_that_requeues_itself_ends_as_an_error_not_a_hang() {
        // The bound exists because this loop never returns to the interrupt
        // handler: the execution budget cannot see it, so only a count can
        // stop it.
        let started = std::time::Instant::now();
        let mut dom = html::parse(
            "<p>x</p><script>function again() { Promise.resolve().then(again); } again();</script>",
        );
        let mut host = None;
        let console = Console::new();
        js::run_pass(&mut host, &mut dom, 1, &console);
        let elapsed = started.elapsed();

        let entries: Vec<String> = console.entries().iter().map(ToString::to_string).collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(
            entries[0].contains("a promise kept queueing more work"),
            "{:?}",
            entries[0]
        );
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn pending_timers_are_counted_for_the_headless_dump() {
        let mut dom = html::parse(
            "<p>x</p><script>setTimeout(function () {}, 50); setInterval(function () {}, 50);\
             var gone = setTimeout(function () {}, 50); clearTimeout(gone);</script>",
        );
        let mut host = None;
        let console = Console::new();
        js::run_pass(&mut host, &mut dom, 1, &console);
        assert_eq!(host.as_mut().map(Host::pending_timers), Some(2));
    }

    // ---- events (M10.8) ---------------------------------------------------

    /// Run `page`'s scripts, then click the element with `id=t`, and return
    /// what the console saw plus whether the default action was cancelled.
    fn click(page: &str) -> (Vec<String>, bool) {
        let mut dom = html::parse(page);
        let mut host = None;
        let console = Console::new();
        js::run_pass(&mut host, &mut dom, 1, &console);

        let target = find_descendant(&dom, dom.root, &mut |dom, node| {
            dom.attr(node, "id") == Some("t")
        })
        .expect("the fixture has a target");
        let prevented = js::dispatch(
            &mut host,
            &mut dom,
            &js::PageContext {
                page: 1,
                url: "https://fixture.test/page",
                console: &console,
                storage: &Storage::new(),
            },
            js::Target::Node(target.0),
            "click",
        );
        crate::dom::check_links(&dom);
        (
            console.entries().iter().map(ToString::to_string).collect(),
            prevented,
        )
    }

    #[test]
    fn dispatch_runs_capture_then_target_then_bubble() {
        // The exact sequence, not "the handler ran": phase order is the whole
        // deliverable, and a dispatch that fires the right listeners in the
        // wrong order is a page that behaves differently from every browser.
        let (entries, _) = click(
            r#"<div id=outer><p id=mid><b id=t>x</b></p></div><script>
              var log = [];
              function note(where) {
                return function (e) { log.push(where + '@' + e.eventPhase); };
              }
              window.addEventListener('click', note('window'), true);
              document.addEventListener('click', note('document'), true);
              document.getElementById('outer').addEventListener('click', note('outer-capture'), true);
              document.getElementById('mid').addEventListener('click', note('mid-capture'), true);
              document.getElementById('t').addEventListener('click', note('target-first'));
              document.getElementById('t').addEventListener('click', note('target-second'), true);
              document.getElementById('mid').addEventListener('click', note('mid-bubble'));
              document.getElementById('outer').addEventListener('click', note('outer-bubble'));
              window.addEventListener('click', function () { console.log(log.join(' ')); });
            </script>"#,
        );
        assert_eq!(
            entries,
            [concat!(
                "log   ",
                "window@1 document@1 outer-capture@1 mid-capture@1 ",
                // At the target both flags run, in registration order — the
                // capture flag stops mattering once the event is there.
                "target-first@2 target-second@2 ",
                "mid-bubble@3 outer-bubble@3"
            )]
        );
    }

    #[test]
    fn the_event_object_carries_what_a_page_reads_from_it() {
        let (entries, _) = click(
            "<div id=outer><b id=t>x</b></div><script>\
             document.getElementById('outer').addEventListener('click', function (e) {\
               console.log(e.type, e.target.tagName, e.currentTarget.tagName,\
                           e.eventPhase, e.bubbles, e.defaultPrevented);\
             });</script>",
        );
        assert_eq!(entries, ["log   click B DIV 3 true false"]);
    }

    #[test]
    fn prevent_default_is_reported_to_the_caller() {
        // The whole point: `App` skips the navigation when this is true.
        let (_, prevented) = click(
            "<a id=t href='/next'>go</a><script>\
             document.getElementById('t').addEventListener('click', function (e) {\
               e.preventDefault();\
             });</script>",
        );
        assert!(prevented);

        let (_, prevented) = click("<a id=t href='/next'>go</a><script>1;</script>");
        assert!(!prevented, "a page with no listener cancels nothing");
    }

    #[test]
    fn stop_propagation_ends_the_walk_and_stop_immediate_ends_the_node() {
        // `stopPropagation` lets the rest of *this* node's listeners run.
        let (entries, _) = click(
            "<div id=outer><b id=t>x</b></div><script>\
             var t = document.getElementById('t');\
             t.addEventListener('click', function (e) { e.stopPropagation(); console.log('first'); });\
             t.addEventListener('click', function () { console.log('second'); });\
             document.getElementById('outer').addEventListener('click', function () { console.log('ancestor'); });\
             </script>",
        );
        assert_eq!(entries, ["log   first", "log   second"]);

        // `stopImmediatePropagation` does not.
        let (entries, _) = click(
            "<div id=outer><b id=t>x</b></div><script>\
             var t = document.getElementById('t');\
             t.addEventListener('click', function (e) { e.stopImmediatePropagation(); console.log('first'); });\
             t.addEventListener('click', function () { console.log('second'); });\
             document.getElementById('outer').addEventListener('click', function () { console.log('ancestor'); });\
             </script>",
        );
        assert_eq!(entries, ["log   first"]);
    }

    #[test]
    fn the_listener_list_is_snapshotted_before_a_phase_runs() {
        // The reentrancy rule. Without it a page can make dispatch iterate a
        // list it is mutating, and what happens then is a property of the
        // engine's iteration order rather than of the page.
        let (entries, _) = click(
            "<b id=t>x</b><script>\
             var t = document.getElementById('t');\
             t.addEventListener('click', function () {\
               t.addEventListener('click', function () { console.log('added during dispatch'); });\
               console.log('first');\
             });\
             t.addEventListener('click', function () { console.log('second'); });\
             </script>",
        );
        assert_eq!(
            entries,
            ["log   first", "log   second"],
            "a listener added during dispatch ran in the same dispatch"
        );

        // And one removed during dispatch still runs, for the same reason.
        let (entries, _) = click(
            "<b id=t>x</b><script>\
             var t = document.getElementById('t');\
             function second() { console.log('second'); }\
             t.addEventListener('click', function () { t.removeEventListener('click', second); console.log('first'); });\
             t.addEventListener('click', second);\
             </script>",
        );
        assert_eq!(entries, ["log   first", "log   second"]);
    }

    #[test]
    fn once_fires_exactly_once_and_remove_takes_effect_next_time() {
        let (entries, _) = click(
            "<b id=t>x</b><script>\
             var t = document.getElementById('t');\
             t.addEventListener('click', function () { console.log('once'); }, {once: true});\
             </script>",
        );
        assert_eq!(entries, ["log   once"]);

        // Registering the same function twice is one registration, as the DOM
        // specifies.
        let (entries, _) = click(
            "<b id=t>x</b><script>\
             var t = document.getElementById('t');\
             function handler() { console.log('handled'); }\
             t.addEventListener('click', handler);\
             t.addEventListener('click', handler);\
             </script>",
        );
        assert_eq!(entries, ["log   handled"]);

        // The legacy boolean-capture form is accepted on both sides, so a
        // remove that spells it that way actually removes.
        let (entries, _) = click(
            "<div id=outer><b id=t>x</b></div><script>\
             var o = document.getElementById('outer');\
             function handler() { console.log('should not run'); }\
             o.addEventListener('click', handler, true);\
             o.removeEventListener('click', handler, true);\
             </script>",
        );
        assert!(entries.is_empty(), "{entries:?}");
    }

    #[test]
    fn a_listener_that_throws_does_not_stop_the_others() {
        // The same discipline as a script that throws (M10.2), and it lands in
        // the console with the line it threw on (M10.7).
        let (entries, _) = click(
            "<b id=t>x</b><script>\
             var t = document.getElementById('t');\
             t.addEventListener('click', function () { null.x; });\
             t.addEventListener('click', function () { console.log('still ran'); });\
             </script>",
        );
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(
            entries[0].starts_with("error inline#1:1: cannot read property 'x' of null"),
            "{:?}",
            entries[0]
        );
        assert_eq!(entries[1], "log   still ran");
    }

    #[test]
    fn listeners_mutate_through_the_same_bindings() {
        let (entries, _) = click(
            "<button id=t>press</button><p id=out>before</p><script>\
             document.getElementById('t').addEventListener('click', function () {\
               document.getElementById('out').textContent = 'after';\
               console.log(document.getElementById('out').textContent);\
             });</script>",
        );
        assert_eq!(entries, ["log   after"]);
    }

    #[test]
    fn dom_content_loaded_then_load_fire_after_the_pass() {
        // Pages register almost all of their behaviour inside these two.
        let mut dom = html::parse(
            "<p>x</p><script>\
             document.addEventListener('DOMContentLoaded', function () { console.log('dcl'); });\
             window.addEventListener('DOMContentLoaded', function () { console.log('dcl on window'); });\
             window.addEventListener('load', function () { console.log('load'); });\
             console.log('script body');</script>",
        );
        let mut host = None;
        let console = Console::new();
        js::run_pass(&mut host, &mut dom, 1, &console);
        assert_eq!(
            console
                .entries()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "log   script body",
                "log   dcl",
                // `DOMContentLoaded` bubbles, which is the only reason a
                // listener for it on `window` ever runs.
                "log   dcl on window",
                "log   load",
            ]
        );
    }

    #[test]
    fn a_listener_on_a_removed_node_never_fires() {
        // The node is detached, not freed, so the registration survives — the
        // handle still reads, which is the proof — but dispatch walks the
        // tree, and the node is no longer in it.
        //
        // (The script is one line: `\` continuations in a Rust string strip
        // the newline, so a `//` comment inside one comments out the rest.)
        let (entries, _) = click(
            "<div id=t>still here<b id=gone>x</b></div><script>\
             var gone = document.getElementById('gone');\
             gone.addEventListener('click', function () { console.log('should not run'); });\
             gone.remove();\
             console.log('removed node still reads as ' + gone.tagName);\
             </script>",
        );
        assert_eq!(entries, ["log   removed node still reads as B"]);
    }

    #[test]
    fn a_runaway_listener_is_stopped_by_the_script_budget() {
        // A listener that loops forever is a runaway script that happened to
        // be reached by a click, and it is held to the same budget.
        let started = std::time::Instant::now();
        let (entries, _) = click(
            "<b id=t>x</b><script>\
             document.getElementById('t').addEventListener('click', function () { while (true) {} });\
             </script>",
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < 3 * super::super::SCRIPT_BUDGET,
            "a runaway listener ran for {elapsed:?}"
        );
        assert!(
            entries.iter().any(|e| e.starts_with("error")),
            "the overrun was not reported: {entries:?}"
        );
    }

    #[test]
    fn listeners_on_dropped_nodes_are_retained_and_cost_what_they_cost() {
        // Deliverable 7's number. A listener registration holds its callback
        // and its target handle, and `remove()` detaches the node without
        // freeing it (ids are never reused, M10.3), so nothing here is
        // reclaimed until the page goes.
        // Small enough that a `dev` build — where QuickJS itself is compiled
        // unoptimized — finishes inside the execution budget. The number is a
        // *delta* from a warm host, so the engine's fixed overhead is not
        // divided into it.
        const NODES: usize = 500;
        let console = Console::new();
        let mut host = Some(Host::new(&console, &Storage::new()).expect("the engine starts"));

        let mut warm = html::parse("<p>x</p><script>1</script>");
        js::run_pass(&mut host, &mut warm, 1, &console);
        let before = host.as_ref().unwrap().heap_bytes();

        let page = format!(
            "<div id=host></div><script>\
             var host = document.getElementById('host');\
             for (var i = 0; i < {NODES}; i++) {{\
               var el = document.createElement('span');\
               el.addEventListener('click', function () {{}});\
               host.appendChild(el);\
               el.remove();\
             }}\
             'done'</script>"
        );
        let mut dom = html::parse(&page);
        let runs = js::run_pass(&mut host, &mut dom, 1, &console);
        let after = host.as_ref().unwrap().heap_bytes();

        assert_eq!(
            runs.last().map(|r| r.outcome.clone()),
            Some(Ok(JsValue::Str("done".into()))),
            "the loop did not finish inside the budget — lower NODES"
        );
        eprintln!(
            "LISTENER-COST {NODES} listener-bearing nodes created and dropped: \
             heap {before} -> {after} bytes (~{} each), arena {} nodes",
            (after - before) / NODES,
            dom.node_count()
        );
        // Nothing is reclaimed, which is the point of the measurement — but it
        // must at least be *linear*, not quadratic in the number of nodes.
        assert!(
            (after - before) / NODES < 4096,
            "a listener-bearing node cost {} bytes",
            (after - before) / NODES
        );
    }

    // ---- console (M10.7) --------------------------------------------------

    /// What `page`'s script logged, as `--dump-js` and the `F5` pane show it.
    fn logged(page: &str, script: &str) -> Vec<String> {
        let mut dom = html::parse(&format!("{page}<script>{script}</script>"));
        let mut host = None;
        let console = Console::new();
        js::run_pass(&mut host, &mut dom, 7, &console);
        console.entries().iter().map(ToString::to_string).collect()
    }

    fn only(script: &str) -> String {
        let lines = logged("<p id=box>t</p>", script);
        assert_eq!(lines.len(), 1, "expected one entry, got {lines:?}");
        lines.into_iter().next().unwrap()
    }

    #[test]
    fn every_level_reaches_the_console_in_order() {
        assert_eq!(
            logged(
                "",
                "console.debug('d'); console.log('l'); console.info('i');\
                 console.warn('w'); console.error('e');"
            ),
            ["debug d", "log   l", "info  i", "warn  w", "error e",]
        );
    }

    #[test]
    fn values_format_so_a_reader_can_tell_them_apart() {
        // Strings bare at the top level, quoted once nested: the only way to
        // tell the number 42 from the string "42" inside a structure.
        assert_eq!(only("console.log('plain')"), "log   plain");
        assert_eq!(
            only("console.log(42, true, null, undefined)"),
            "log   42 true null undefined"
        );
        assert_eq!(only("console.log(['a', 1])"), "log   [\"a\", 1]");
        assert_eq!(
            only("console.log({a: 1, b: 'x'})"),
            "log   {a: 1, b: \"x\"}"
        );
        assert_eq!(only("console.log(function f() {})"), "log   [function]");
        assert_eq!(only("console.log(new Error('boom'))"), "log   Error: boom");
        // One level deep, as specified: deeper structures say so rather than
        // unrolling a whole object graph into the pane.
        assert_eq!(only("console.log({a: {b: {c: 1}}})"), "log   {a: {b: {…}}}");
        // A DOM handle prints as the element it stands for. `{}` would be
        // true — the handle has no own properties — and useless.
        assert_eq!(
            only("console.log(document.getElementById('box'))"),
            "log   <p id=\"box\">"
        );
        assert_eq!(only("console.log(document.body)"), "log   <body>");
    }

    #[test]
    fn a_cyclic_value_terminates_instead_of_hanging_the_tick() {
        // `console.log(window)` is the real-world shape of this: the global
        // object refers to itself, and without the guard the formatter walks
        // it until the budget runs out.
        assert_eq!(
            only("var a = {}; a.self = a; console.log(a)"),
            "log   {self: [circular]}"
        );
        assert_eq!(
            only("var a = []; a.push(a); console.log(a)"),
            "log   [[circular]]"
        );
        // The one that matters: this must return, not spend 100 ms.
        let entry = only("console.log(window)");
        assert!(entry.starts_with("log   {"), "{entry}");
    }

    #[test]
    fn a_long_message_is_clipped_before_it_reaches_the_pane() {
        let entry = only("console.log('x'.repeat(10 * 1024 * 1024))");
        assert!(
            entry.len() < console::MAX_TEXT + 64,
            "a 10 MB string put {} bytes in the pane",
            entry.len()
        );
    }

    #[test]
    fn a_long_collection_says_how_much_it_left_out() {
        let entry = only("console.log(Array.from({length: 100}, (_, i) => i))");
        assert!(entry.contains("… 80 more"), "{entry}");
    }

    #[test]
    fn an_uncaught_exception_lands_in_the_console_with_its_line() {
        assert_eq!(
            logged("", "\nnull.x;"),
            ["error inline#1:2: cannot read property 'x' of null"]
        );
    }

    #[test]
    fn logs_and_throws_interleave_in_the_order_they_happened() {
        // The interleaving is the information: "it logged twice and then
        // threw" is a different story from "it threw and then logged twice".
        let mut dom = html::parse(
            "<script>console.log('first'); null.x;</script>\
             <script>console.warn('second');</script>",
        );
        let mut host = None;
        let console = Console::new();
        js::run_pass(&mut host, &mut dom, 7, &console);
        assert_eq!(
            console
                .entries()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "log   first",
                "error inline#1:1: cannot read property 'x' of null",
                "warn  second",
            ]
        );
    }

    #[test]
    fn a_script_skipped_for_its_type_says_so() {
        // "Nothing happened" and "we ignored what the page asked for" look
        // identical to a reader otherwise.
        assert_eq!(
            logged("", "1;")
                .into_iter()
                .chain(logged("<script type=module>x()</script>", "1;"))
                .collect::<Vec<_>>(),
            ["warn  <script type=module>: not run: `module` is not a classic script"]
        );
    }

    #[test]
    fn query_selector_all_on_the_wikipedia_fixture() {
        // The cost number the PR reports. Selector matching is on CLAUDE.md's
        // hot-path list, and a page that queries in a loop pays this inside
        // the tick budget.
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/en.wikipedia.org.html"
        ));
        let dom = html::parse(fixture);

        // The matcher alone: one walk of the arena, `matches` per element.
        let selectors = parse_selector_list_for_test("a");
        let started = std::time::Instant::now();
        let found = query(&dom, &selectors);
        let matching = started.elapsed();

        // And through the binding, as a page calls it. The fixture runs its
        // own scripts either way, so the honest number is the difference
        // between a pass that queries and one that does not — measured
        // alternating, because this machine drifts several percent between
        // runs of the same thing.
        let pass_with = |probe: &str| {
            let mut page = html::parse(&format!("{fixture}<script>{probe}</script>"));
            let mut host = None;
            let started = std::time::Instant::now();
            let runs = js::run_pass(&mut host, &mut page, 1, &Console::new());
            (started.elapsed(), runs)
        };
        let (mut baseline, mut queried, mut twice) =
            (Duration::ZERO, Duration::ZERO, Duration::ZERO);
        let mut runs = Vec::new();
        const ROUNDS: u32 = 3;
        for _ in 0..ROUNDS {
            baseline += pass_with("1").0;
            let (elapsed, done) = pass_with("document.querySelectorAll('a').length");
            queried += elapsed;
            runs = done;
            // The same query twice: the second walk matches just as much, but
            // every handle it needs is already interned.
            twice +=
                pass_with("document.querySelectorAll('a'); document.querySelectorAll('a').length")
                    .0;
        }
        let first = queried.saturating_sub(baseline) / ROUNDS;
        let second = twice.saturating_sub(queried) / ROUNDS;

        eprintln!(
            "QSA-WIKIPEDIA {} anchors over {} nodes · matcher alone {matching:?} · \
             first call +{first:?} · second (handles already interned) +{second:?}",
            found.len(),
            dom.node_count()
        );
        assert!(
            found.len() > 100,
            "expected an article's worth of links, got {}",
            found.len()
        );
        // The *last* run: the fixture carries its own scripts, and the probe
        // was appended after them.
        assert_eq!(
            runs.last().expect("the probe ran").outcome,
            Ok(JsValue::Num(found.len() as f64)),
            "the binding and the matcher disagree about how many there are"
        );
    }

    #[test]
    fn interning_costs_one_object_per_node_a_page_touches() {
        // Deliverable 3's price: identity comes from caching one wrapper per
        // node id for the life of the page, so a script that walks thousands
        // of nodes keeps thousands of small objects alive. The number is what
        // makes that a decision rather than an accident.
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/en.wikipedia.org.html"
        ));
        let mut host = Some(Host::new(&Console::new(), &Storage::new()).expect("host starts"));

        let mut warm = html::parse("<p>x</p><script>1</script>");
        js::run_pass(&mut host, &mut warm, 1, &Console::new());
        let before = host.as_ref().unwrap().heap_bytes();

        // Same page generation, so the handle cache is not cleared.
        let mut page = html::parse(&format!(
            "{fixture}<script>globalThis.all = document.querySelectorAll('a'); all.length</script>"
        ));
        let runs = js::run_pass(&mut host, &mut page, 1, &Console::new());
        let after = host.as_ref().unwrap().heap_bytes();

        let Ok(JsValue::Num(count)) = runs.last().expect("the probe ran").outcome else {
            panic!("the probe did not return a count: {:?}", runs.last());
        };
        eprintln!(
            "HANDLE-COST {count} handles retained, heap {before} -> {after} bytes \
             (~{:.0} bytes each)",
            (after - before) as f64 / count
        );
        assert!(count > 100.0);
    }

    /// `parse_selector_list` needs a `Ctx` to throw through; the tests only
    /// ever pass it valid selectors, so this is the same parse without one.
    fn parse_selector_list_for_test(selector: &str) -> Vec<css::Selector> {
        css::parse(&format!("{selector}{{}}")).rules[0]
            .selectors
            .clone()
    }

    #[test]
    fn values_of_the_binding_layer_cross_as_plain_data() {
        // M10.1's boundary: nothing an rquickjs type ever reaches the caller.
        let mut dom =
            html::parse("<p id=x>t</p><script>document.getElementById('x').tagName</script>");
        let mut host = None;
        let runs = js::run_pass(&mut host, &mut dom, 1, &Console::new());
        assert_eq!(runs[0].outcome, Ok(JsValue::Str("P".into())));
    }
}

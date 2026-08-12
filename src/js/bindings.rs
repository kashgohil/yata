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

use rquickjs::context::EvalOptions;
use rquickjs::{Ctx, Exception, Function, Object, Result as JsResult};

use crate::css;
use crate::dom::{Dom, DomError, NodeData, NodeId};
use crate::html;
use crate::style::{StyleContext, matching};

/// The tree the current tick is working on, shared between the host and every
/// binding closure. Empty between ticks.
#[derive(Default)]
pub struct DomSlot {
    dom: RefCell<Option<Dom>>,
    /// The page generation the slot currently holds. Kept when the tree is
    /// taken back out, because handles minted this page stay valid between
    /// ticks — it is the *next page* that must invalidate them.
    page: Cell<u64>,
}

impl DomSlot {
    /// Lend the tree to the bindings for one tick.
    pub fn lend(&self, dom: Dom, page: u64) {
        self.page.set(page);
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
    }
}

/// Install the primitives and evaluate the prelude that builds the object
/// model on top of them. Called once per host.
pub fn install(ctx: &Ctx<'_>, slot: &Rc<DomSlot>) -> JsResult<()> {
    let api = Object::new(ctx.clone())?;

    // Which page the slot holds. The prelude compares a handle's page against
    // this on every read.
    let s = Rc::clone(slot);
    api.set(
        "page",
        Function::new(ctx.clone(), move || s.page.get() as f64)?,
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

    let s = Rc::clone(slot);
    api.set(
        "appendChild",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, parent: u32, child: u32| {
            let outcome = s.with_mut(&ctx, |dom| match (node(dom, parent), node(dom, child)) {
                (Some(parent), Some(child)) => dom.append(parent, child),
                _ => Err(DomError::NotFound),
            })?;
            outcome.map_err(|error| throw_dom_error(&ctx, error))
        })?,
    )?;

    let s = Rc::clone(slot);
    api.set(
        "insertBefore",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, parent: u32, child: u32, reference: u32| {
                let outcome = s.with_mut(&ctx, |dom| {
                    match (node(dom, parent), node(dom, child), node(dom, reference)) {
                        (Some(parent), Some(child), Some(reference)) => {
                            dom.insert_before(parent, child, reference)
                        }
                        _ => Err(DomError::NotFound),
                    }
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

    let s = Rc::clone(slot);
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

    ctx.globals().set("__dom", api)?;
    // Named, so a stack frame from inside the object model says where it came
    // from instead of the engine's anonymous `eval_script`.
    let mut options = EvalOptions::default();
    options.global = true;
    options.strict = false;
    options.filename = Some("<bindings>".to_string());
    ctx.eval_with_options::<(), _>(PRELUDE, options)?;
    Ok(())
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
    querySelector: function (sel) { return wrap(raw.querySelector(String(sel))); },
    querySelectorAll: function (sel) { return raw.querySelectorAll(String(sel)).map(wrap); },
  };

  // `window`, `globalThis` and the global scope are one object: pages branch
  // on `typeof window` and read `window.x` for a top-level `var x`.
  globalThis.window = globalThis;
  globalThis.document = document;
  globalThis.Element = Element;
})(__dom);
delete globalThis.__dom;
"#;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::html;
    use crate::js::{self, Host, JsValue};

    /// Run `script` against `page` and return the script's completion value,
    /// the way `--dump-js` would show it.
    fn eval_on(page: &str, script: &str) -> String {
        let mut dom = html::parse(&format!("{page}<script>{script}</script>"));
        let mut host = None;
        let runs = js::run_pass(&mut host, &mut dom, 7);
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
        // Removed in Rust (M10.3) — there are no write bindings until M10.5.
        let gone = find_descendant(&dom, dom.root, &mut |dom, node| {
            dom.attr(node, "id") == Some("gone")
        })
        .expect("fixture has the node");
        dom.remove(gone);

        let mut host = None;
        let runs = js::run_pass(&mut host, &mut dom, 7);
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
        let runs = js::run_pass(&mut host, &mut first, 1);
        assert_eq!(runs[0].dump_line(), "inline#1 ok object");
        assert!(host.is_some());

        // The same host, a different page. (The real app drops the host on
        // navigation — this is the guard for the day something keeps one.)
        let mut second = html::parse(
            "<p id=two>second page</p><script>\
             try { kept.tagName } catch (e) { 'refused: ' + e.message }</script>",
        );
        let runs = js::run_pass(&mut host, &mut second, 2);
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
        js::run_pass(&mut host, &mut dom, 3);

        let mut again = html::parse("<p id=one>text</p><script>kept.tagName</script>");
        let runs = js::run_pass(&mut host, &mut again, 3);
        assert_eq!(runs[0].dump_line(), "inline#1 ok \"P\"");
    }

    #[test]
    fn the_dom_is_unreachable_between_ticks() {
        // A callback that runs when no tick owns a tree — M10.9's timers will
        // be the first — gets an exception, not a stale read.
        let slot = DomSlot::default();
        assert!(slot.take().is_none());
        let mut host = Host::new().expect("host starts");
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
        let runs = js::run_pass(&mut host, &mut dom, 9);
        assert_eq!(runs[0].dump_line(), "inline#1 ok \"BODY\"");
    }

    #[test]
    fn reads_work_on_a_subtree_that_is_not_in_the_document() {
        // JS cannot reach a detached node until M10.5 creates one, but the
        // primitives underneath must already be total over the arena: they are
        // what M10.5's `createElement` will read through.
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
        let runs = js::run_pass(&mut host, &mut dom, 7);
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
            let runs = js::run_pass(&mut host, &mut page, 1);
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
        let mut host = Some(Host::new().expect("host starts"));

        let mut warm = html::parse("<p>x</p><script>1</script>");
        js::run_pass(&mut host, &mut warm, 1);
        let before = host.as_ref().unwrap().heap_bytes();

        // Same page generation, so the handle cache is not cleared.
        let mut page = html::parse(&format!(
            "{fixture}<script>globalThis.all = document.querySelectorAll('a'); all.length</script>"
        ));
        let runs = js::run_pass(&mut host, &mut page, 1);
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
        let runs = js::run_pass(&mut host, &mut dom, 1);
        assert_eq!(runs[0].outcome, Ok(JsValue::Str("P".into())));
    }
}

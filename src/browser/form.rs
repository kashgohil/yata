//! What a `<form>` submission *is* (M11.10, M11.11): a GET query or a POST body.
//!
//! **A submission is a navigation, and there is only one of those.** Everything
//! here is a pure function of the DOM plus the control that was activated; it
//! fetches nothing, mutates nothing and reads nothing downstream of the tree.
//! `App::submit_form` takes what this produces to `App::navigate` — the
//! same path a link click takes, so a submission gets history, the `FetchId`
//! guard, the jar's `Cookie:` header (M11.7) and redirects through the event
//! loop (M11.7a) without any of it being written twice. GET puts the data set
//! on the URL; POST puts it on the body and leaves the action URL alone.
//!
//! The data set is a **snapshot of state, in tree order** — never of the
//! screen. A control scrolled out of view, clipped by an `overflow`, or off the
//! bottom of the page is still submitted, because a form is not what a reader
//! can see. That is why this module knows about `Dom` and not about
//! `LayoutTree`.

use crate::dom::{Dom, NodeData, NodeId};
use crate::layout::field;
use crate::net;

/// Largest encoded POST body this engine will put on the wire (M11.11).
///
/// A form a reader typed is small; a hidden field a page wrote is not.
/// M10.13 forbids a page from buying unbounded work with an attribute, and a
/// 4 MB `<input type=hidden>` copied onto the channel would be that. 1 MiB is
/// larger than any form a person filled in and smaller than
/// [`net::MAX_FETCH_BYTES`].
pub const MAX_POST_BODY: usize = 1024 * 1024;

/// What activating a control comes to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Submit {
    /// Navigate here: the action resolved, with its query replaced by the form
    /// data set.
    Get(String),
    /// Navigate here with this `application/x-www-form-urlencoded` body. The
    /// action's own query is **kept** — GET replaces it, POST does not.
    Post { url: String, body: String },
    /// A method or encoding this engine does not implement, named so the
    /// reader can be told which one. **Not a silent GET**: a value meant for a
    /// request body has no business in a URL, a history entry or a `Referer`.
    Unsupported(String),
}

/// The form a control belongs to: the nearest `<form>` ancestor, up the arena's
/// parent links.
///
/// The `form=""` attribute — a control owned by a form it is not inside — is
/// out of scope (M11.10): no ladder page uses it, and it turns this parent walk
/// into an id lookup with its own invalidation story.
///
/// `None` means the control is in no form, and then `Enter` does nothing at
/// all. A search box outside a form is real — sites submit those with
/// JavaScript — and M11.13 is where they start working.
pub fn owner(dom: &Dom, node: NodeId) -> Option<NodeId> {
    let mut current = Some(node);
    while let Some(id) = current {
        if let NodeData::Element { tag, .. } = &dom.node(id).data
            && tag.eq_ignore_ascii_case("form")
        {
            return Some(id);
        }
        current = dom.node(id).parent;
    }
    None
}

/// Is this element a control that submits the form when it is activated?
///
/// HTML's rule, including the part pages actually rely on: **a `<button>` with
/// no `type` is a submit button**, which is exactly what Wikipedia's search
/// form has. `type=button` is not one. Neither is `type=reset`, which in this
/// engine also does not *reset* — restoring the dirty values M11.8 stores is
/// its own small task, no ladder page has a reset button, and a button that
/// quietly does nothing is better than one that clears a form nobody asked it
/// to (recorded as a deviation).
pub fn is_submit_button(dom: &Dom, node: NodeId) -> bool {
    let NodeData::Element { tag, .. } = &dom.node(node).data else {
        return false;
    };
    let ty = dom.attr(node, "type").map(str::trim);
    if tag.eq_ignore_ascii_case("button") {
        // No `type` on a `<button>` means submit — the default a page gets by
        // saying nothing, and the one every framework leans on.
        return ty.is_none_or(|t| t.eq_ignore_ascii_case("submit"));
    }
    tag.eq_ignore_ascii_case("input") && ty.is_some_and(|t| t.eq_ignore_ascii_case("submit"))
}

/// What submitting `activator`'s form asks for, or `None` when it is in no
/// form (nothing happens, and nothing runs).
///
/// `base` is the document's **post-redirect** URL — the one `Fetch::Loaded`
/// holds, not the one the reader typed (M11.7a) — because that is what
/// `action` resolves against.
pub fn submit(dom: &Dom, base: &str, activator: NodeId) -> Option<Submit> {
    let form = owner(dom, activator)?;
    // An absent `method` is GET, which is what Wikipedia's form says by saying
    // nothing. HTML treats an *unrecognized* method as GET too; this engine
    // refuses it instead, for the reason `Submit::Unsupported` gives — the
    // deviation is deliberate and recorded. Empty is GET as well: a page that
    // wrote `method=""` asked for nothing.
    let method = dom.attr(form, "method").unwrap_or("get");
    let method = method.trim();
    let is_get = method.is_empty() || method.eq_ignore_ascii_case("get");
    let is_post = method.eq_ignore_ascii_case("post");
    if !is_get && !is_post {
        // Bounded, because this is page-controlled text on its way to the
        // statusline and `<form method="…a megabyte…">` must cost this engine
        // nothing (M10.13). No real method is longer than `dialog`.
        return Some(Submit::Unsupported(bounded_token(method)));
    }
    if is_post && let Some(refused) = refuse_enctype(dom, form) {
        return Some(refused);
    }
    let data = data_set(dom, form, activator);
    let action = dom.attr(form, "action").unwrap_or("").trim();
    // An absent or empty `action` submits to the document's own URL, which is
    // what joining an empty href against it already means. `resolve_url` also
    // gets HN's protocol-relative `//hn.algolia.com/` right, by inheriting the
    // page's scheme.
    let resolved = net::resolve_url(base, action)?;
    if is_get {
        // **Replaced, not appended to**: `action="/w/index.php?oldid=5"` submitted
        // with `search=cat` is `/w/index.php?search=cat`, and the `oldid` is gone.
        // HTML says so, and the append version looks right on both ladder pages,
        // which have no query to lose.
        Some(Submit::Get(net::set_query(&resolved, &data)?))
    } else {
        // **Kept, not replaced**: `action="/login?next=/app"` plus `acct=pg` is
        // URL `/login?next=/app` and body `acct=pg`. GET's replace looks right
        // on a login whose action has no query to preserve, and putting the
        // fields on both sides is how a password ends up in `H`.
        if data.len() > MAX_POST_BODY {
            return Some(Submit::Unsupported("huge".into()));
        }
        Some(Submit::Post {
            url: resolved,
            body: data,
        })
    }
}

/// `enctype` is ignored on GET (HTML). On POST, only urlencoded is sent;
/// multipart and anything else refuse rather than silently drop files.
fn refuse_enctype(dom: &Dom, form: NodeId) -> Option<Submit> {
    let raw = dom.attr(form, "enctype").unwrap_or("").trim();
    if raw.is_empty() {
        return None;
    }
    // `;charset=…` is ignored: we always encode UTF-8.
    let ty = raw.split(';').next().unwrap_or(raw).trim();
    if ty.eq_ignore_ascii_case("application/x-www-form-urlencoded") {
        None
    } else if ty.eq_ignore_ascii_case("multipart/form-data") {
        Some(Submit::Unsupported("FILE".into()))
    } else if ty.eq_ignore_ascii_case("text/plain") {
        Some(Submit::Unsupported("PLAIN".into()))
    } else {
        Some(Submit::Unsupported(bounded_token(ty)))
    }
}

fn bounded_token(s: &str) -> String {
    s.chars().take(16).flat_map(char::to_uppercase).collect()
}

/// The form data set, encoded: the successful controls in `form`, in tree
/// order, as `application/x-www-form-urlencoded`.
///
/// Which controls are successful (HTML §4.10.22.4, minus what this engine has
/// no state for):
///
/// - **named** — a control with no `name` is not in the set, which is why
///   Wikipedia's `<button>` contributes nothing even when it is what was
///   pressed;
/// - **not `disabled`** — and `readonly` **is** successful. M11.9 made those
///   two different for typing and they are different here, in the same
///   direction: `readonly` shows you something and submits it, `disabled` does
///   neither;
/// - the text-ish `<input>` types, `<textarea>`, and `<input type=hidden>` —
///   Wikipedia's `title=Special:Search` is the case that proves hidden matters,
///   and it is the difference between a search that works and one that lands on
///   the wrong page;
/// - the **activating** submit button contributes its own `name=value` if it
///   has a name; no other button in the form does, ever. Implicit
///   submission from a field therefore sends no button — a deviation from
///   HTML, which still has a submitter then (the first submit button).
///
/// Choice controls contribute their live state. File inputs and specialized
/// controls still contribute nothing because this engine cannot operate them.
fn data_set(dom: &Dom, form: NodeId, activator: NodeId) -> String {
    let mut pairs: Vec<String> = Vec::new();
    walk(dom, form, &mut |node, tag| {
        let Some(name) = dom.attr(node, "name").filter(|n| !n.is_empty()) else {
            return;
        };
        if dom.attr(node, "disabled").is_some() {
            return;
        }
        let values = match entry(dom, node, tag) {
            Entry::Value => vec![field::value(dom, node, tag)],
            Entry::Checkbox | Entry::Radio if field::checked(dom, node, tag) => {
                vec![dom.attr(node, "value").unwrap_or("on").to_string()]
            }
            Entry::Checkbox | Entry::Radio => return,
            Entry::Select => {
                let options = field::options(dom, node);
                field::selected_options(dom, node, &options)
                    .into_iter()
                    .filter_map(|selected| {
                        options
                            .iter()
                            .find(|option| option.node == selected && !option.disabled)
                            .map(|option| option.value.clone())
                    })
                    .collect()
            }
            // A button is in the set only when it is the one that was pressed,
            // and then it contributes the `value` attribute it was written
            // with — not the label a reader sees, which HTML invents
            // ("Submit") for a button that has none.
            Entry::Submitter if node == activator && is_submit_button(dom, node) => {
                vec![dom.attr(node, "value").unwrap_or_default().to_string()]
            }
            Entry::Submitter | Entry::None => return,
        };
        for value in values {
            pairs.push(format!(
                "{}={}",
                net::form_urlencode(name),
                net::form_urlencode(&crlf(&value))
            ));
        }
    });
    pairs.join("&")
}

/// What a control contributes to the data set, before its name and value are
/// looked at.
enum Entry {
    /// Its value: the text-ish `<input>` types, `<textarea>`, and `hidden`.
    Value,
    /// Its value, but only if it is the control that was activated.
    Submitter,
    Checkbox,
    Radio,
    Select,
    /// Nothing, ever.
    None,
}

fn entry(dom: &Dom, node: NodeId, tag: &str) -> Entry {
    if tag.eq_ignore_ascii_case("textarea") {
        return Entry::Value;
    }
    if tag.eq_ignore_ascii_case("button") {
        return Entry::Submitter;
    }
    if tag.eq_ignore_ascii_case("select") {
        return Entry::Select;
    }
    if !tag.eq_ignore_ascii_case("input") {
        // `<output>`, `<object>` and the rest of HTML's list are not
        // controls this engine has at all.
        return Entry::None;
    }
    let ty = dom.attr(node, "type").unwrap_or("text").trim();
    let is = |names: &[&str]| names.iter().any(|n| ty.eq_ignore_ascii_case(n));
    if is(&["submit", "button", "reset"]) {
        return Entry::Submitter;
    }
    // The list is written out rather than borrowed from `field::kind`, which
    // groups by "draws no box" and would put `hidden` on the wrong side of it.
    // The two lists mean different things: hidden serializes without a box,
    // while choice controls have both boxes and live state.
    if is(&["checkbox"]) {
        return Entry::Checkbox;
    }
    if is(&["radio"]) {
        return Entry::Radio;
    }
    if is(&[
        "file",
        "image",
        "range",
        "color",
        "date",
        "datetime-local",
        "month",
        "week",
        "time",
    ]) {
        return Entry::None;
    }
    // `text`, `search`, `password`, `email`, `url`, `tel`, `number`, `hidden` —
    // and a type nobody has heard of, which HTML says is a text field.
    Entry::Value
}

/// Every element under `form`, in tree order.
fn walk(dom: &Dom, node: NodeId, f: &mut impl FnMut(NodeId, &str)) {
    for child in dom.children(node) {
        if let NodeData::Element { tag, .. } = &dom.node(child).data {
            f(child, tag);
        }
        walk(dom, child, f);
    }
}

/// Newlines normalized to **CRLF**, which HTML requires of every value in a
/// urlencoded data set — so a `<textarea>` the reader typed one line-break into
/// sends `%0D%0A` and not `%0A`.
///
/// This is the one place a value is not sent byte for byte as it was typed, and
/// it looks like a bug forever afterwards, so: it is deliberate, it is HTML's,
/// and it is why the encoder's table has a CRLF row. Nothing else can be
/// affected — an `<input>`'s value has had its newlines flattened to spaces
/// since M11.8 (`field::value`), because HTML strips them there.
fn crlf(value: &str) -> String {
    if !value.contains(['\r', '\n']) {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                out.push_str("\r\n");
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            '\n' => out.push_str("\r\n"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    /// The page's first element with this tag.
    fn first(dom: &Dom, tag: &str) -> NodeId {
        (0..dom.node_count() as u32)
            .map(NodeId)
            .find(|&id| {
                matches!(&dom.node(id).data,
                    NodeData::Element { tag: t, .. } if t.eq_ignore_ascii_case(tag))
            })
            .unwrap_or_else(|| panic!("the fixture has no <{tag}>"))
    }

    fn find_id(dom: &Dom, wanted: &str) -> NodeId {
        (0..dom.node_count() as u32)
            .map(NodeId)
            .find(|&id| dom.attr(id, "id") == Some(wanted))
            .unwrap_or_else(|| panic!("the fixture has no #{wanted}"))
    }

    fn url_of(submitted: Option<Submit>) -> String {
        match submitted {
            Some(Submit::Get(url)) => url,
            other => panic!("expected a GET submission, got {other:?}"),
        }
    }

    #[test]
    fn hns_search_form_is_a_url() {
        // The whole of HN's search form, and the whole point of the task: a
        // protocol-relative action inherits the page's scheme, and the field's
        // value is the query.
        let dom = html::parse(
            "<form method=\"get\" action=\"//hn.algolia.com/\">\
             Search: <input type=\"text\" name=\"q\" size=\"17\"></form>",
        );
        let field = first(&dom, "input");
        // What the reader typed, where M11.8 puts it.
        let mut dom = dom;
        dom.set_field_value(field, "redirect");
        assert_eq!(
            submit(&dom, "http://news.ycombinator.com/news", field),
            Some(Submit::Get("http://hn.algolia.com/?q=redirect".into()))
        );
    }

    #[test]
    fn wikipedias_search_form_sends_its_hidden_field_in_tree_order() {
        // Three controls, one of them hidden, and a `<button>` with no `type`
        // and no `name`: the hidden `title` is the difference between a search
        // that works and one that lands on the wrong page.
        let mut dom = html::parse(
            "<form action=\"/w/index.php\" id=searchform>\
             <input type=search name=search placeholder=\"Search Wikipedia\">\
             <input type=hidden name=title value=\"Special:Search\">\
             <button class=cdx-button>Search</button></form>",
        );
        let field = first(&dom, "input");
        dom.set_field_value(field, "cat");
        let expected = "https://en.wikipedia.org/w/index.php?search=cat&title=Special%3ASearch";
        assert_eq!(
            submit(&dom, "https://en.wikipedia.org/wiki/Cat", field),
            Some(Submit::Get(expected.into())),
            "the field's own submission"
        );
        // And pressing the button produces the identical request — it is one
        // function, so the two activations cannot disagree. The button
        // contributes nothing, because it has no name.
        assert_eq!(
            submit(
                &dom,
                "https://en.wikipedia.org/wiki/Cat",
                first(&dom, "button")
            ),
            Some(Submit::Get(expected.into()))
        );
    }

    #[test]
    fn a_get_submission_replaces_the_actions_query_rather_than_appending() {
        // The trap: both ladder pages have an action with no query, so the
        // append version looks right on every page this engine is tested
        // against.
        let mut dom =
            html::parse("<form action=\"/w/index.php?oldid=5&x=1\"><input name=search></form>");
        let field = first(&dom, "input");
        dom.set_field_value(field, "cat");
        assert_eq!(
            url_of(submit(&dom, "http://x/page", field)),
            "http://x/w/index.php?search=cat",
            "the action's own query survived"
        );
    }

    #[test]
    fn an_absent_or_empty_action_submits_to_the_documents_own_url() {
        let mut dom = html::parse("<form><input name=q></form>");
        let field = first(&dom, "input");
        dom.set_field_value(field, "here");
        assert_eq!(
            url_of(submit(&dom, "http://x/dir/page?old=1#frag", field)),
            "http://x/dir/page?q=here",
            "the document's own URL, with its query replaced"
        );
        let mut dom = html::parse("<form action=\"\"><input name=q></form>");
        let field = first(&dom, "input");
        dom.set_field_value(field, "here");
        assert_eq!(
            url_of(submit(&dom, "http://x/dir/page", field)),
            "http://x/dir/page?q=here"
        );
    }

    #[test]
    fn only_successful_controls_are_in_the_data_set() {
        // Named, not disabled, in tree order — and `readonly` is successful,
        // which is the one place M11.9's two "cannot type here" attributes
        // come apart.
        let dom = html::parse(
            "<form>\
             <input name=a value=1>\
             <input value=noname>\
             <input name=b value=2 disabled>\
             <input name=c value=3 readonly>\
             <input name=d value=4 type=hidden>\
             <input name=e type=checkbox checked>\
             <select name=f><option value=1>one</option></select>\
             <textarea name=g>text</textarea>\
             <input name=h type=button value=nope>\
             <input name=i type=reset value=nope>\
             </form>",
        );
        assert_eq!(
            url_of(submit(&dom, "http://x/page", first(&dom, "input"))),
            "http://x/page?a=1&c=3&d=4&e=on&f=1&g=text"
        );
    }

    #[test]
    fn choices_contribute_zero_one_or_many_values_in_tree_order() {
        let mut dom = html::parse(
            "<form method=post><input id=a type=checkbox name=c value=yes>\
             <input id=b type=checkbox name=c checked>\
             <input type=radio name=r value=old checked>\
             <input id=r type=radio name=r value=new checked>\
             <select name=s multiple><option value=1 selected>one</option>\
             <option disabled selected value=2>two</option>\
             <option selected> three </option></select></form>",
        );
        let checkbox = find_id(&dom, "a");
        dom.set_choice_state(checkbox, "checked", true);
        assert_eq!(
            submit(&dom, "http://x/page", checkbox),
            Some(Submit::Post {
                url: "http://x/page".into(),
                body: "c=yes&c=on&r=new&s=1&s=three".into(),
            })
        );
    }

    #[test]
    fn choice_data_is_identical_for_get_and_post_and_skips_disabled_placeholders() {
        let controls = "<input type=checkbox name=off>\
                        <input type=radio name=r value=no>\
                        <select name=placeholder><option disabled selected value=no>Choose</option>\
                        <option value=yes>Yes</option></select>\
                        <select name=empty><option selected value=''>Blank label</option></select>\
                        <select name=none multiple><option>No choice</option></select>";
        for (method, expected) in [
            ("", "http://x/page?empty="),
            (" method=post", "POST http://x/page empty="),
        ] {
            let dom = html::parse(&format!("<form{method}>{controls}</form>"));
            let submitted = submit(&dom, "http://x/page", first(&dom, "input"));
            let actual = match submitted {
                Some(Submit::Get(url)) => url,
                Some(Submit::Post { url, body }) => format!("POST {url} {body}"),
                other => panic!("unexpected submission: {other:?}"),
            };
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn the_activating_button_is_the_only_button_in_the_set() {
        let dom = html::parse(
            "<form><input name=q value=v>\
             <button name=one value=1>one</button>\
             <button name=two value=2>two</button></form>",
        );
        let buttons: Vec<NodeId> = (0..dom.node_count() as u32)
            .map(NodeId)
            .filter(|&id| {
                matches!(&dom.node(id).data,
                NodeData::Element { tag, .. } if tag == "button")
            })
            .collect();
        assert_eq!(
            url_of(submit(&dom, "http://x/page", buttons[0])),
            "http://x/page?q=v&one=1",
            "the pressed button, at its own place in tree order"
        );
        assert_eq!(
            url_of(submit(&dom, "http://x/page", buttons[1])),
            "http://x/page?q=v&two=2"
        );
        // **The deviation**: HTML still has a submitter here — the first
        // submit button in the form — so this would send `one=1` in a browser.
        // The caret's field is the activator, and a default-button rule would
        // send a name the reader never pressed. It will matter the moment a
        // server keys off the button (`search` vs `cancel`).
        assert_eq!(
            url_of(submit(&dom, "http://x/page", first(&dom, "input"))),
            "http://x/page?q=v"
        );
        // A named submitter with no `value` sends an empty one, as HTML says.
        let dom = html::parse("<form><button name=go>Go</button></form>");
        assert_eq!(
            url_of(submit(&dom, "http://x/page", first(&dom, "button"))),
            "http://x/page?go="
        );
    }

    #[test]
    fn a_type_that_does_not_submit_never_contributes_its_own_pair() {
        // Even when it is what was activated: `type=button` and `type=reset`
        // are not submit buttons, so `is_submit_button` refuses them and the
        // `Submitter` arm never fires.
        let dom = html::parse(
            "<form><input name=q value=v>\
             <input type=button name=b value=1>\
             <input type=reset name=r value=2></form>",
        );
        let controls: Vec<NodeId> = (0..dom.node_count() as u32)
            .map(NodeId)
            .filter(|&id| {
                matches!(&dom.node(id).data,
                NodeData::Element { tag, .. } if tag == "input")
            })
            .collect();
        for control in [controls[1], controls[2]] {
            assert!(!is_submit_button(&dom, control));
            assert_eq!(
                url_of(submit(&dom, "http://x/page", control)),
                "http://x/page?q=v"
            );
        }
    }

    #[test]
    fn a_button_with_no_type_is_a_submit_button() {
        // Wikipedia's, exactly. And `type=button` is not, and an `<input>`
        // with no type is a text field rather than a button.
        let dom = html::parse(
            "<form><button>bare</button><button type=submit>s</button>\
             <button type=button>b</button><button type=reset>r</button>\
             <input type=submit value=is><input value=not></form>",
        );
        let kinds: Vec<bool> = (0..dom.node_count() as u32)
            .map(NodeId)
            .filter(|&id| {
                matches!(&dom.node(id).data,
                NodeData::Element { tag, .. } if tag == "button" || tag == "input")
            })
            .map(|id| is_submit_button(&dom, id))
            .collect();
        assert_eq!(kinds, [true, true, false, false, true, false]);
    }

    #[test]
    fn a_control_in_no_form_submits_nothing() {
        let dom = html::parse("<p><input name=q value=v></p>");
        assert_eq!(submit(&dom, "http://x/page", first(&dom, "input")), None);
        assert_eq!(owner(&dom, first(&dom, "input")), None);
    }

    #[test]
    fn the_form_is_the_nearest_ancestor_however_deep_the_control_sits() {
        // Wikipedia's field is four `<div>`s inside its form.
        let dom =
            html::parse("<form id=outer><div><div><span><input name=q></span></div></div></form>");
        assert_eq!(owner(&dom, first(&dom, "input")), Some(first(&dom, "form")));
    }

    #[test]
    fn a_method_this_engine_cannot_send_is_refused_by_name() {
        // `dialog` and anything unrecognized stay refused. POST is a real
        // method now (M11.11); putting it back in this table would silently
        // undo the task.
        for (method, named) in [("dialog", "DIALOG"), ("wibble", "WIBBLE")] {
            let dom = html::parse(&format!(
                "<form method={method}><input name=pw value=hunter2></form>"
            ));
            assert_eq!(
                submit(&dom, "http://x/page", first(&dom, "input")),
                Some(Submit::Unsupported(named.into())),
                "method={method}"
            );
        }
        // A method a hostile page made a paragraph long is truncated before it
        // can reach the statusline (M10.13).
        let dom = html::parse(&format!(
            "<form method={}><input name=q></form>",
            "z".repeat(4096)
        ));
        assert_eq!(
            submit(&dom, "http://x/page", first(&dom, "input")),
            Some(Submit::Unsupported("Z".repeat(16)))
        );
        // GET, in any spelling and by omission.
        for form in ["<form method=GET>", "<form method=\" get \">", "<form>"] {
            let dom = html::parse(&format!("{form}<input name=q value=v></form>"));
            assert_eq!(
                url_of(submit(&dom, "http://x/page", first(&dom, "input"))),
                "http://x/page?q=v"
            );
        }
        // POST, in any spelling: body, not query. The action's query is kept.
        for method in ["post", "POST", " Post "] {
            let dom = html::parse(&format!(
                "<form method=\"{method}\" action=/login?next=/app>\
                 <input name=acct value=pg></form>"
            ));
            assert_eq!(
                submit(&dom, "http://x/page", first(&dom, "input")),
                Some(Submit::Post {
                    url: "http://x/login?next=/app".into(),
                    body: "acct=pg".into(),
                }),
                "method={method}"
            );
        }
    }

    #[test]
    fn a_post_keeps_the_actions_query_and_puts_fields_in_the_body() {
        // **The trap, inverted.** GET would replace `next=/app` with `acct=pg`.
        let dom = html::parse(
            "<form method=post action=/login?next=/app>\
             <input name=acct value=pg><input name=pw value=secret></form>",
        );
        assert_eq!(
            submit(&dom, "http://x/page", first(&dom, "input")),
            Some(Submit::Post {
                url: "http://x/login?next=/app".into(),
                body: "acct=pg&pw=secret".into(),
            })
        );
    }

    #[test]
    fn enctype_urlencoded_is_the_only_post_this_engine_sends() {
        let post = |enctype: &str| {
            let attr = if enctype.is_empty() {
                String::new()
            } else {
                format!(" enctype=\"{enctype}\"")
            };
            let dom = html::parse(&format!(
                "<form method=post action=/login{attr}><input name=q value=v></form>"
            ));
            submit(&dom, "http://x/page", first(&dom, "input"))
        };
        assert_eq!(
            post(""),
            Some(Submit::Post {
                url: "http://x/login".into(),
                body: "q=v".into(),
            })
        );
        assert_eq!(
            post("application/x-www-form-urlencoded;charset=UTF-8"),
            Some(Submit::Post {
                url: "http://x/login".into(),
                body: "q=v".into(),
            }),
            "charset is ignored; we always encode UTF-8"
        );
        assert_eq!(
            post("multipart/form-data"),
            Some(Submit::Unsupported("FILE".into()))
        );
        assert_eq!(
            post("text/plain"),
            Some(Submit::Unsupported("PLAIN".into()))
        );
        assert_eq!(post("wibble"), Some(Submit::Unsupported("WIBBLE".into())));
        // GET ignores enctype, as HTML says.
        let dom = html::parse(
            "<form enctype=multipart/form-data action=/search>\
             <input name=q value=v></form>",
        );
        assert_eq!(
            url_of(submit(&dom, "http://x/page", first(&dom, "input"))),
            "http://x/search?q=v"
        );
    }

    #[test]
    fn a_post_body_over_the_cap_is_refused() {
        // The encoded body is `h=` plus the value (ASCII, so no percent-growth).
        let mut over = html::parse("<form method=post action=/x><input type=hidden name=h></form>");
        let field = first(&over, "input");
        over.set_field_value(field, &"x".repeat(MAX_POST_BODY));
        assert_eq!(
            submit(&over, "http://x/page", field),
            Some(Submit::Unsupported("huge".into()))
        );
        let mut under =
            html::parse("<form method=post action=/x><input type=hidden name=h></form>");
        let field = first(&under, "input");
        under.set_field_value(field, &"x".repeat(MAX_POST_BODY - 2));
        match submit(&under, "http://x/page", field) {
            Some(Submit::Post { body, .. }) => {
                assert_eq!(body.len(), MAX_POST_BODY);
            }
            other => panic!("expected a POST under the cap, got {other:?}"),
        }
    }

    #[test]
    fn multiple_selected_options_share_the_post_body_cap() {
        let mut dom = html::parse(
            "<form method=post action=/x><select name=m multiple>\
             <option id=a selected>A</option><option id=b selected>B</option>\
             </select></form>",
        );
        let select = first(&dom, "select");
        let (first_option, second_option) = (find_id(&dom, "a"), find_id(&dom, "b"));
        let first_len = MAX_POST_BODY / 2;
        let second_len = MAX_POST_BODY - first_len - 5;
        dom.set_attr(first_option, "value", &"a".repeat(first_len));
        dom.set_attr(second_option, "value", &"b".repeat(second_len));
        match submit(&dom, "http://x/page", select) {
            Some(Submit::Post { body, .. }) => assert_eq!(body.len(), MAX_POST_BODY),
            other => panic!("expected a POST at the cap, got {other:?}"),
        }
        dom.set_attr(second_option, "value", &"b".repeat(second_len + 1));
        assert_eq!(
            submit(&dom, "http://x/page", select),
            Some(Submit::Unsupported("huge".into()))
        );
    }

    #[test]
    fn a_textareas_newlines_are_crlf_on_the_wire() {
        // HTML's normalization, and the one place a value is not sent exactly
        // as it was typed.
        let mut dom = html::parse("<form><textarea name=t></textarea></form>");
        let area = first(&dom, "textarea");
        dom.set_field_value(area, "one\ntwo");
        assert_eq!(
            url_of(submit(&dom, "http://x/page", area)),
            "http://x/page?t=one%0D%0Atwo"
        );
        // However the value spells its line break.
        for typed in ["a\nb", "a\r\nb", "a\rb"] {
            dom.set_field_value(area, typed);
            assert_eq!(
                url_of(submit(&dom, "http://x/page", area)),
                "http://x/page?t=a%0D%0Ab",
                "{typed:?}"
            );
        }
    }

    #[test]
    fn what_is_sent_is_what_the_reader_can_see() {
        // The store first, the markup second — `field::value`'s rule, reached
        // through it rather than re-derived, so the query and the box can
        // never disagree.
        let mut dom = html::parse(
            "<form><input name=q value=markup>\
             <textarea name=t>\nfrom markup</textarea></form>",
        );
        assert_eq!(
            url_of(submit(&dom, "http://x/page", first(&dom, "input"))),
            "http://x/page?q=markup&t=from+markup",
            "the markup defaults, including the textarea's stripped newline"
        );
        let field = first(&dom, "input");
        dom.set_field_value(field, "typed over");
        assert_eq!(
            url_of(submit(&dom, "http://x/page", field)),
            "http://x/page?q=typed+over&t=from+markup"
        );
        // And the attribute is untouched by the edit (M11.8), so a page that
        // reads it still sees what it wrote.
        assert_eq!(dom.attr(field, "value"), Some("markup"));
    }

    #[test]
    fn a_form_with_nothing_to_send_still_submits() {
        // Not a refusal: a browser sends `?` here, and refusing would invent a
        // rule HTML does not have — a search form whose only control lost its
        // name still goes somewhere.
        let dom = html::parse("<form action=/search><input value=nameless></form>");
        assert_eq!(
            url_of(submit(&dom, "http://x/page", first(&dom, "input"))),
            "http://x/search?"
        );
    }

    #[test]
    fn names_are_encoded_exactly_like_values() {
        let mut dom = html::parse("<form><input name=\"a b&c\"></form>");
        let field = first(&dom, "input");
        dom.set_field_value(field, "x y&z");
        assert_eq!(
            url_of(submit(&dom, "http://x/page", field)),
            "http://x/page?a+b%26c=x+y%26z"
        );
    }
}

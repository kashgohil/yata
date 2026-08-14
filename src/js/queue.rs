//! The document-order execution queue (M10.10).
//!
//! `sources::sources` gives the scripts a page asks for, in order. Inline ones
//! carry their text; external ones have to be fetched, and their bodies arrive
//! whenever the network feels like it. This is the piece that makes arrival
//! order irrelevant to execution order.
//!
//! ## Why this is not the stylesheet path
//!
//! M4.3 allocates a slot per stylesheet in document order and lets the sheets
//! land in any order, because the cascade sorts them out afterwards. Scripts
//! cannot do that: they must **execute** in order, since the first may define
//! what the second calls. So a slot that is still pending is a *hole*, and
//! nothing after a hole may run — the queue advances by taking the longest
//! complete prefix and stopping at the first gap.
//!
//! An inline script written after a pending external one therefore waits for
//! it, which is exactly what a browser does with a classic `<script>` — and
//! the one place where our "never block the parser" model and a browser's
//! agree on the observable result.
//!
//! ## A script a script inserted (M11.5)
//!
//! It gets a slot too, appended after the document's — but it is **not** part
//! of the document-order run, and the difference is the ordering question this
//! queue exists to answer, asked from the other side. See [`ScriptQueue::insert`].

use crate::dom::NodeId;
use crate::js::console::{Console, Level};
use crate::js::sources::Script;

/// One slot's state. A slot is created for every script the document asks for,
/// before any of them runs.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Slot {
    /// Ready to run: an inline script, or an external one whose body arrived.
    Ready { name: String, source: String },
    /// An external script still in flight. Nothing after it may run.
    Pending { name: String },
    /// Will never run: a failed fetch, or a `type` we do not execute. The
    /// queue steps straight over it — a hole that has settled is not a hole.
    Settled,
}

/// The page's scripts, in document order, with the position execution has
/// reached — plus the ones a script inserted, which sit after them and play by
/// a different rule (see [`ScriptQueue::insert`]).
#[derive(Default, Debug)]
pub struct ScriptQueue {
    slots: Vec<Slot>,
    /// The next slot to run. Everything before it has run or settled. Only
    /// ever walks the document's own slots — see `document_len`.
    next: usize,
    /// How many of `slots` came from the document. Everything at or past this
    /// index was inserted by a script, and `next` never reaches it.
    document_len: usize,
    /// The script elements this queue has already accounted for: the DOM's
    /// "already started" flag, which is what stops a page that reorders its
    /// own tree from re-running its analytics (M11.5).
    ///
    /// A `Vec` rather than a set on purpose — it holds one entry per script
    /// element on the page plus at most [`MAX_INSERTED_SCRIPTS`] more, which
    /// is tens of entries, and a linear scan of tens beats hashing them.
    started: Vec<NodeId>,
    /// Whether the "everything has run" moment has already been reported. A
    /// one-shot because an insertion can finish a queue that had already
    /// finished, and `DOMContentLoaded`/`load` fire once per page.
    finished_reported: bool,
}

/// What a caller should do with a slot that needs fetching.
pub struct External {
    pub slot: usize,
    /// The `src` exactly as the page wrote it; the caller resolves it.
    pub url: String,
}

/// How many scripts one page may insert by script, over its whole life
/// (M11.5).
///
/// The bound, and it is a *page* bound rather than a per-tick one on purpose:
/// each insertion asks the loop for a fresh turn, so a script that appends a
/// script that appends a script is a chain of turns, each inside M10.13's
/// 100 ms budget and each with the reader's keys served between them. Every
/// individual turn was already bounded; what was not bounded was the number of
/// them, and a per-tick cap would not have bounded it either — one insertion
/// per tick, forever, is still forever.
///
/// 32 matches [`crate::js::MAX_IN_FLIGHT`] for the same reason it was chosen
/// there: far above what any honest bootstrap chains (a loader, a library, a
/// tag manager — three or four), far below a number that could keep the loop
/// busy in a way a reader would notice.
pub const MAX_INSERTED_SCRIPTS: usize = 32;

/// What [`ScriptQueue::insert`] did with an inserted `<script>`.
pub enum Inserted {
    /// It can run now — the caller should ask for another turn.
    Ready,
    /// It has to be fetched first, on this slot.
    Fetch(External),
    /// Nothing happened: this element has already been accounted for, the
    /// page has spent its insertion budget, or the script is one we do not
    /// run and has been reported.
    Nothing,
}

impl ScriptQueue {
    /// Build the queue from a document's scripts. Returns the queue and the
    /// external slots to fetch — allocated here, before any fetch starts, so
    /// the order is fixed by the document and not by the network.
    pub fn new(scripts: Vec<(NodeId, Script)>, console: &Console) -> (ScriptQueue, Vec<External>) {
        let mut slots = Vec::with_capacity(scripts.len());
        let mut started = Vec::with_capacity(scripts.len());
        let mut externals = Vec::new();
        for (node, script) in scripts {
            // Every element the document walk found is accounted for, here and
            // now — including the ones that will never run. A page that moves
            // one of them into the tree again is reordering its own DOM, not
            // asking for a second execution (M11.5).
            started.push(node);
            match script {
                Script::Inline { name, source } => slots.push(Slot::Ready { name, source }),
                Script::External { src } => {
                    externals.push(External {
                        slot: slots.len(),
                        url: src.clone(),
                    });
                    slots.push(Slot::Pending { name: src });
                }
                // Decided against by the source walk (M10.2). Reported here
                // rather than there, because the walk is a pure function and
                // this is the one place every script the page asked for passes
                // through. It holds its place so slot numbering matches the
                // document.
                Script::Skipped { name, reason } => {
                    console.push(Level::Warn, Some(name), None, &reason);
                    slots.push(Slot::Settled);
                }
            }
        }
        (
            ScriptQueue {
                document_len: slots.len(),
                slots,
                started,
                ..ScriptQueue::default()
            },
            externals,
        )
    }

    /// A `<script>` a script put into the document (M11.5). Returns what the
    /// caller has to do about it.
    ///
    /// ## Where it goes, and whether a hole blocks it
    ///
    /// **It does not join the document-order run at all: it is `async`.** It
    /// runs the moment it is ready, ahead of any slot still in flight, and it
    /// blocks nothing behind it. That is the ordering question this queue
    /// exists to answer, asked from the other side, and the answer follows
    /// from *why* the queue's rule exists rather than from the rule itself.
    ///
    /// Nothing after a pending slot may run because the pending script may
    /// define what the next one calls. That dependency cannot exist here, in
    /// either direction. A script inserted *now* was created by code that has
    /// **already run**, so it cannot be a continuation of slot 4 when slot 3
    /// is still in flight — slot 4 has not run either. And nothing in the
    /// document can be waiting on it, because the document's order was fixed
    /// by the parse, before the page executed a line.
    ///
    /// The alternative — append it to the document run and let holes block it
    /// — has a concrete failure: one CDN script that never arrives silently
    /// swallows every script the page injects behind it, forever, including
    /// the one carrying its content. It is also not what a browser does: an
    /// element inserted by script is `async` unless the page opts out with
    /// `async = false`, and reading that opt-out is out of this task's scope —
    /// the standing `defer`/`async` deviation covers it, and every inserted
    /// script here is treated as though the page had left `async` alone.
    ///
    /// Inserted scripts do not block **each other** either, for the same
    /// reason: each runs when its own body arrives, so their order among
    /// themselves is arrival order. That is exactly what `async` means.
    pub fn insert(&mut self, node: NodeId, script: Script, console: &Console) -> Inserted {
        // The DOM's "already started" flag. A page that moves a script it has
        // already run — reordering its own tree, which pages do — must not run
        // it twice, or a reordered page fires its analytics again.
        if self.started.contains(&node) {
            return Inserted::Nothing;
        }
        // The bound (see `MAX_INSERTED_SCRIPTS`). Counted against elements the
        // queue accepted, so a page cannot spend the budget on the same
        // element twice: that case has already returned above.
        if self.started.len() - self.document_len >= MAX_INSERTED_SCRIPTS {
            console.push(
                Level::Warn,
                Some(script.name().to_string()),
                None,
                &format!(
                    "not run: this page has already inserted {MAX_INSERTED_SCRIPTS} scripts \
                     by script, which is as many as this browser will run"
                ),
            );
            return Inserted::Nothing;
        }
        self.started.push(node);
        let slot = self.slots.len();
        match script {
            Script::Inline { name, source } => {
                self.slots.push(Slot::Ready { name, source });
                Inserted::Ready
            }
            Script::External { src } => {
                self.slots.push(Slot::Pending { name: src.clone() });
                Inserted::Fetch(External { slot, url: src })
            }
            // Reported here rather than at the call site, for the same reason
            // `new` reports it here: this is the one place every script the
            // page asked for passes through.
            Script::Skipped { name, reason } => {
                console.push(Level::Warn, Some(name), None, &reason);
                self.slots.push(Slot::Settled);
                Inserted::Nothing
            }
        }
    }

    /// The element an inserted slot belongs to, so its `load`/`error` can be
    /// fired at it (M11.5). `None` for a document-order slot: a parsed
    /// `<script src>` fires no event here, because nothing can have registered
    /// a listener on it before it ran.
    ///
    /// `started` and `slots` grow in lockstep — one entry each per script,
    /// from the document walk and from every accepted insertion — so a slot
    /// index is also an index into the elements.
    pub fn element(&self, slot: usize) -> Option<NodeId> {
        (slot >= self.document_len)
            .then(|| self.started.get(slot).copied())
            .flatten()
    }

    /// A fetched body (or `None` for a fetch that will never produce one).
    /// Out-of-range slots are ignored: a message for a queue that has been
    /// replaced is not this queue's business.
    pub fn fill(&mut self, slot: usize, source: Option<String>) {
        let Some(entry) = self.slots.get_mut(slot) else {
            return;
        };
        let Slot::Pending { name } = entry else {
            return;
        };
        *entry = match source {
            Some(source) => Slot::Ready {
                name: name.clone(),
                source,
            },
            None => Slot::Settled,
        };
    }

    /// Take the longest run of slots that is ready *now*, in order, and stop
    /// at the first hole. Repeated calls resume where the last stopped, so a
    /// script arriving late unblocks everything queued behind it in one go.
    ///
    /// Then every inserted slot that is ready, whatever the document run is
    /// doing — see [`ScriptQueue::insert`] for why a hole does not hold one.
    /// They come after the document's within a call because a page's own
    /// order is the one it wrote down; between calls, they run the moment
    /// they arrive.
    pub fn take_ready_prefix(&mut self) -> Vec<(String, String)> {
        let mut ready = Vec::new();
        while self.next < self.document_len {
            match &self.slots[self.next] {
                // A hole: nothing after it may run, however ready that is.
                Slot::Pending { .. } => break,
                Slot::Settled => self.next += 1,
                Slot::Ready { name, source } => {
                    ready.push((name.clone(), source.clone()));
                    self.next += 1;
                }
            }
        }
        // Taking an inserted slot settles it: there is no cursor to walk past
        // it, so `Settled` is what stops it running twice.
        for slot in &mut self.slots[self.document_len..] {
            if let Slot::Ready { name, source } = slot {
                ready.push((std::mem::take(name), std::mem::take(source)));
                *slot = Slot::Settled;
            }
        }
        ready
    }

    /// Whether every slot has run or settled — the moment `DOMContentLoaded`
    /// and `load` are due, since nothing more can execute.
    pub fn is_finished(&self) -> bool {
        self.next >= self.document_len
            && self.slots[self.document_len..]
                .iter()
                .all(|slot| matches!(slot, Slot::Settled))
    }

    /// [`ScriptQueue::is_finished`], reported **once**.
    ///
    /// `DOMContentLoaded` and `load` fire once per page, and an insertion can
    /// un-finish a finished queue and then finish it again — a script inserted
    /// by a `load` handler does exactly that. Without the latch the page would
    /// see the pair a second time, which is a lie no browser tells.
    pub fn take_finished(&mut self) -> bool {
        if self.finished_reported || !self.is_finished() {
            return false;
        }
        self.finished_reported = true;
        true
    }

    /// How many scripts this page has inserted by script, which is what names
    /// the next one (`inserted#3`) and what the bound is counted against.
    pub fn inserted(&self) -> usize {
        self.started.len() - self.document_len
    }

    /// How many slots are still waiting on the network.
    pub fn pending(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| matches!(slot, Slot::Pending { .. }))
            .count()
    }
}

#[cfg(test)]
impl ScriptQueue {
    /// `new` without a console, for tests that are about ordering. The
    /// elements are synthesised — distinct, because "already started" is keyed
    /// on them — since these tests are about slots, not about the tree.
    fn new_for_test(scripts: Vec<Script>) -> (ScriptQueue, Vec<External>) {
        let with_nodes = scripts
            .into_iter()
            .enumerate()
            .map(|(i, script)| (NodeId(i as u32), script))
            .collect();
        ScriptQueue::new(with_nodes, &Console::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inline(name: &str, source: &str) -> Script {
        Script::Inline {
            name: name.to_string(),
            source: source.to_string(),
        }
    }

    fn external(src: &str) -> Script {
        Script::External {
            src: src.to_string(),
        }
    }

    fn names(ready: Vec<(String, String)>) -> Vec<String> {
        ready.into_iter().map(|(name, _)| name).collect()
    }

    #[test]
    fn inline_only_runs_immediately_and_finishes() {
        let (mut queue, externals) =
            ScriptQueue::new_for_test(vec![inline("inline#1", "a"), inline("inline#2", "b")]);
        assert!(externals.is_empty());
        assert_eq!(names(queue.take_ready_prefix()), ["inline#1", "inline#2"]);
        assert!(queue.is_finished());
    }

    #[test]
    fn a_pending_external_blocks_everything_after_it() {
        // The whole point of the task: an inline script written after a
        // pending external one waits, because the external one may define what
        // it calls.
        let (mut queue, externals) = ScriptQueue::new_for_test(vec![
            external("first.js"),
            inline("inline#2", "b"),
            external("third.js"),
        ]);
        assert_eq!(externals.len(), 2);
        assert_eq!(externals[0].slot, 0);
        assert_eq!(externals[1].slot, 2);

        // Nothing can run yet: slot 0 is a hole.
        assert!(queue.take_ready_prefix().is_empty());
        assert!(!queue.is_finished());

        // The *second* external arrives first. Still nothing runs.
        queue.fill(2, Some("third".into()));
        assert!(queue.take_ready_prefix().is_empty());

        // The first arrives and unblocks all three, in document order.
        queue.fill(0, Some("first".into()));
        assert_eq!(
            names(queue.take_ready_prefix()),
            ["first.js", "inline#2", "third.js"]
        );
        assert!(queue.is_finished());
    }

    #[test]
    fn a_failed_fetch_settles_its_slot_and_the_rest_proceeds() {
        let (mut queue, _) =
            ScriptQueue::new_for_test(vec![external("gone.js"), inline("inline#2", "b")]);
        assert!(queue.take_ready_prefix().is_empty());

        queue.fill(0, None);
        assert_eq!(names(queue.take_ready_prefix()), ["inline#2"]);
        assert!(queue.is_finished());
    }

    #[test]
    fn a_hole_that_never_fills_holds_the_rest_forever() {
        // A browser would not run them either: the script that never arrived
        // may have been the one that defined everything.
        let (mut queue, _) =
            ScriptQueue::new_for_test(vec![external("never.js"), inline("inline#2", "b")]);
        assert!(queue.take_ready_prefix().is_empty());
        assert!(queue.take_ready_prefix().is_empty());
        assert!(!queue.is_finished());
        assert_eq!(queue.pending(), 1);
    }

    #[test]
    fn a_skipped_script_holds_its_place_without_blocking() {
        let (mut queue, _) = ScriptQueue::new_for_test(vec![
            Script::Skipped {
                name: "<script type=module>".into(),
                reason: "not run".into(),
            },
            inline("inline#1", "a"),
        ]);
        assert_eq!(names(queue.take_ready_prefix()), ["inline#1"]);
        assert!(queue.is_finished());
    }

    #[test]
    fn a_body_for_a_slot_that_is_not_pending_changes_nothing() {
        // A second `Msg::Script` for the same slot, or one for a queue that has
        // moved on: neither may re-run a script.
        let (mut queue, _) = ScriptQueue::new_for_test(vec![external("once.js")]);
        queue.fill(0, Some("body".into()));
        assert_eq!(names(queue.take_ready_prefix()), ["once.js"]);

        queue.fill(0, Some("again".into()));
        assert!(queue.take_ready_prefix().is_empty());
        queue.fill(99, Some("nowhere".into()));
        assert!(queue.take_ready_prefix().is_empty());
    }

    // ---- scripts a script inserted (M11.5) ----

    /// An element id no document slot claimed, so each insertion is a
    /// different element.
    fn fresh(n: u32) -> NodeId {
        NodeId(1000 + n)
    }

    #[test]
    fn an_inserted_script_runs_although_a_document_slot_is_still_a_hole() {
        // **The ordering decision, pinned with a pending slot in the queue at
        // the moment of insertion.** Slot 0 is a hole and slot 1 is blocked
        // behind it; the script inserted while that is true was created by
        // code that has already run, so it cannot be waiting on slot 0 and
        // runs now.
        let (mut queue, _) =
            ScriptQueue::new_for_test(vec![external("stuck.js"), inline("inline#2", "b")]);
        assert!(queue.take_ready_prefix().is_empty());

        let inserted = Script::Inline {
            name: "inserted#1".into(),
            source: "injected".into(),
        };
        assert!(matches!(
            queue.insert(fresh(1), inserted, &Console::new()),
            Inserted::Ready
        ));
        assert_eq!(names(queue.take_ready_prefix()), ["inserted#1"]);
        // And the hole still holds the document's own order: `inline#2` has
        // not run, and will not until `stuck.js` lands.
        assert!(!queue.is_finished());
        queue.fill(0, Some("first".into()));
        assert_eq!(names(queue.take_ready_prefix()), ["stuck.js", "inline#2"]);
        assert!(queue.is_finished());
    }

    #[test]
    fn an_inserted_script_runs_exactly_once_and_never_blocks_the_next_one() {
        let (mut queue, _) = ScriptQueue::new_for_test(vec![inline("inline#1", "a")]);
        assert_eq!(names(queue.take_ready_prefix()), ["inline#1"]);

        // Two insertions, the first external and still in flight. The second
        // is inline and must not wait behind it — inserted scripts do not
        // block each other any more than a document hole blocks them.
        let waiting = queue.insert(
            fresh(1),
            Script::External {
                src: "slow.js".into(),
            },
            &Console::new(),
        );
        let Inserted::Fetch(external) = waiting else {
            panic!("an external insertion must ask to be fetched");
        };
        assert_eq!(external.url, "slow.js");
        queue.insert(
            fresh(2),
            Script::Inline {
                name: "inserted#2".into(),
                source: "b".into(),
            },
            &Console::new(),
        );
        assert_eq!(names(queue.take_ready_prefix()), ["inserted#2"]);
        // Taken is run: a second call must not hand it over again.
        assert!(queue.take_ready_prefix().is_empty());

        queue.fill(external.slot, Some("body".into()));
        assert_eq!(names(queue.take_ready_prefix()), ["slow.js"]);
        assert!(queue.is_finished());
    }

    #[test]
    fn an_element_the_queue_already_accounted_for_never_runs_again() {
        // The DOM's "already started" flag. Both halves: a document script
        // moved back into the tree, and an inserted one moved after it ran.
        let (mut queue, _) = ScriptQueue::new_for_test(vec![inline("inline#1", "a")]);
        assert_eq!(names(queue.take_ready_prefix()), ["inline#1"]);

        // `new_for_test` gave the document's one script NodeId(0).
        let again = Script::Inline {
            name: "inserted#1".into(),
            source: "a".into(),
        };
        assert!(matches!(
            queue.insert(NodeId(0), again, &Console::new()),
            Inserted::Nothing
        ));
        assert!(queue.take_ready_prefix().is_empty());

        let once = Script::Inline {
            name: "inserted#1".into(),
            source: "b".into(),
        };
        queue.insert(fresh(1), once.clone(), &Console::new());
        assert_eq!(names(queue.take_ready_prefix()), ["inserted#1"]);
        assert!(matches!(
            queue.insert(fresh(1), once, &Console::new()),
            Inserted::Nothing
        ));
        assert!(queue.take_ready_prefix().is_empty());
    }

    #[test]
    fn a_page_may_insert_only_so_many_scripts() {
        // The bound (M11.5 deliverable 7). Past it the page is told, once per
        // refusal, and the queue stops growing.
        let (mut queue, _) = ScriptQueue::new_for_test(vec![]);
        let console = Console::new();
        for n in 0..MAX_INSERTED_SCRIPTS as u32 + 5 {
            queue.insert(
                fresh(n),
                Script::Inline {
                    name: format!("inserted#{}", n + 1),
                    source: "x".into(),
                },
                &console,
            );
        }
        assert_eq!(queue.take_ready_prefix().len(), MAX_INSERTED_SCRIPTS);
        assert_eq!(
            console
                .entries()
                .iter()
                .filter(|e| e.text.contains("as many as this browser will run"))
                .count(),
            5
        );
    }

    #[test]
    fn the_finish_is_reported_once_even_when_an_insertion_un_finishes_the_queue() {
        // `DOMContentLoaded` and `load` fire once per page. A script inserted
        // by a `load` handler finishes the queue a second time, and without
        // the latch the page would see the pair twice.
        let (mut queue, _) = ScriptQueue::new_for_test(vec![inline("inline#1", "a")]);
        queue.take_ready_prefix();
        assert!(queue.take_finished());
        assert!(!queue.take_finished());

        queue.insert(
            fresh(1),
            Script::Inline {
                name: "inserted#1".into(),
                source: "b".into(),
            },
            &Console::new(),
        );
        assert!(!queue.is_finished(), "a ready insertion is not finished");
        queue.take_ready_prefix();
        assert!(queue.is_finished());
        assert!(!queue.take_finished(), "the page was told twice");
    }

    #[test]
    fn only_an_inserted_slot_names_the_element_it_came_from() {
        // What the `load`/`error` dispatch is keyed on. A document slot has
        // no element to fire at: nothing could have registered a listener on
        // a `<script src>` before the page ran.
        let (mut queue, _) = ScriptQueue::new_for_test(vec![external("doc.js")]);
        assert_eq!(queue.element(0), None);

        let Inserted::Fetch(external) = queue.insert(
            fresh(7),
            Script::External {
                src: "late.js".into(),
            },
            &Console::new(),
        ) else {
            panic!("an external insertion must ask to be fetched");
        };
        assert_eq!(queue.element(external.slot), Some(fresh(7)));
        assert_eq!(queue.element(99), None);
    }
}

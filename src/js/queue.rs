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
/// reached.
#[derive(Default, Debug)]
pub struct ScriptQueue {
    slots: Vec<Slot>,
    /// The next slot to run. Everything before it has run or settled.
    next: usize,
}

/// What a caller should do with a slot that needs fetching.
pub struct External {
    pub slot: usize,
    /// The `src` exactly as the page wrote it; the caller resolves it.
    pub url: String,
}

impl ScriptQueue {
    /// Build the queue from a document's scripts. Returns the queue and the
    /// external slots to fetch — allocated here, before any fetch starts, so
    /// the order is fixed by the document and not by the network.
    pub fn new(scripts: Vec<Script>, console: &Console) -> (ScriptQueue, Vec<External>) {
        let mut slots = Vec::with_capacity(scripts.len());
        let mut externals = Vec::new();
        for script in scripts {
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
                slots,
                ..ScriptQueue::default()
            },
            externals,
        )
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
    pub fn take_ready_prefix(&mut self) -> Vec<(String, String)> {
        let mut ready = Vec::new();
        while let Some(slot) = self.slots.get(self.next) {
            match slot {
                // A hole: nothing after it may run, however ready that is.
                Slot::Pending { .. } => break,
                Slot::Settled => self.next += 1,
                Slot::Ready { name, source } => {
                    ready.push((name.clone(), source.clone()));
                    self.next += 1;
                }
            }
        }
        ready
    }

    /// Whether every slot has run or settled — the moment `DOMContentLoaded`
    /// and `load` are due, since nothing more can execute.
    pub fn is_finished(&self) -> bool {
        self.next >= self.slots.len()
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
    /// `new` without a console, for tests that are about ordering.
    fn new_for_test(scripts: Vec<Script>) -> (ScriptQueue, Vec<External>) {
        ScriptQueue::new(scripts, &Console::new())
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
}

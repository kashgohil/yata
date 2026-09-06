//! The timer thread: `setTimeout`/`setInterval` deadlines, as messages
//! (M10.9).
//!
//! PLAN.md M10 claims the M1 message architecture absorbs timers without
//! redesign, and this is where that claim is tested. It holds: a timer is one
//! more *producer* sending into the single mpsc channel the event loop
//! `recv`s on, exactly like a fetch worker. Nothing about the loop changes.
//!
//! ## Idle really is idle
//!
//! CLAUDE.md's hard constraint is 0% idle CPU, which rules out the two easy
//! implementations — a poll loop, and `recv_timeout` on a short tick. So the
//! thread owns a deadline heap and blocks on a condvar: until the earliest
//! deadline when it has one, and **indefinitely** when it has none. A page
//! with a ten-second timer pending costs exactly one wakeup, ten seconds from
//! now, and a page with no timers costs none at all.
//!
//! One thread for the app, not one per timer: a page that schedules a thousand
//! timers must not become a thousand threads.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::msg::Msg;
use crate::net::{PageId, TabId};

/// A timer's identity within its page. Browser-compatible: a positive integer,
/// never reused, so `clearTimeout` on a fired timer is harmless rather than a
/// cancellation of somebody else.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct TimerId(pub u64);

/// The floor on a delay. `setTimeout(f, 0)` must not turn the event loop into
/// a spin, and a page that reschedules itself immediately is the shape that
/// does it.
///
/// 4 ms is what browsers clamp nested timers to, and it is the right number
/// here for a different reason: it is under half the 10 ms keypress→screen
/// budget (PLAN.md §4), so a timer firing at the floor still leaves the loop
/// most of a budget to answer a keystroke that arrives beside it. Going lower
/// buys a page nothing a terminal can show — there is no frame clock to beat.
pub const MIN_DELAY: Duration = Duration::from_millis(4);

/// What the event loop asks the timer thread to do. `App` produces these
/// during a tick and the loop hands them over, the same discipline as
/// `Effect::fetch`: `App` decides, the loop dispatches.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TimerRequest {
    /// Fire once, `delay` from now (clamped to [`MIN_DELAY`]).
    Schedule {
        page: PageId,
        id: TimerId,
        delay: Duration,
    },
    /// `clearTimeout` / `clearInterval`.
    Cancel { page: PageId, id: TimerId },
    /// Navigation: older generations in this tab are dead; other tabs keep
    /// running normally.
    CancelOthers { keep: PageId },
    /// Closing a tab eagerly removes all of its deadlines.
    CancelTab { tab: TabId },
}

/// One scheduled deadline. Ordered by time, then by insertion — two timers due
/// at the same instant fire in the order they were scheduled, which is what
/// makes a sequence of `setTimeout(f, 0)` calls run in program order.
#[derive(PartialEq, Eq)]
struct Entry {
    due: Instant,
    seq: u64,
    page: PageId,
    id: TimerId,
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.due.cmp(&other.due).then(self.seq.cmp(&other.seq))
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
struct Schedule {
    /// `Reverse`, so the binary heap gives the *earliest* deadline.
    heap: BinaryHeap<Reverse<Entry>>,
    seq: u64,
    /// Set when the app is going away, so the thread can end.
    done: bool,
}

/// The event loop's handle on the timer thread. Dropping it stops the thread.
pub struct Timers {
    shared: Arc<(Mutex<Schedule>, Condvar)>,
}

impl Timers {
    /// Start the thread. `tx` is a clone of the one channel every producer
    /// sends into — the loop remains the sole receiver (PLAN.md §2).
    pub fn spawn(tx: Sender<Msg>) -> Timers {
        let shared = Arc::new((Mutex::new(Schedule::default()), Condvar::new()));
        let worker = Arc::clone(&shared);
        thread::spawn(move || run(&worker, &tx));
        Timers { shared }
    }

    /// Apply one request from a tick.
    pub fn apply(&self, request: TimerRequest) {
        let (lock, condvar) = &*self.shared;
        let mut schedule = lock.lock().expect("the timer thread never panics");
        match request {
            TimerRequest::Schedule { page, id, delay } => {
                schedule.seq += 1;
                let entry = Entry {
                    due: Instant::now() + delay.max(MIN_DELAY),
                    seq: schedule.seq,
                    page,
                    id,
                };
                schedule.heap.push(Reverse(entry));
            }
            // Cancellation is lazy: the entry stays in the heap and is dropped
            // when it comes due. Removing from a binary heap means rebuilding
            // it, and a cancelled timer costs one comparison at its deadline
            // either way.
            TimerRequest::Cancel { page, id } => {
                schedule
                    .heap
                    .retain(|Reverse(entry)| !(entry.page == page && entry.id == id));
            }
            TimerRequest::CancelOthers { keep } => {
                schedule
                    .heap
                    .retain(|Reverse(entry)| entry.page.tab != keep.tab || entry.page == keep);
            }
            TimerRequest::CancelTab { tab } => {
                schedule.heap.retain(|Reverse(entry)| entry.page.tab != tab);
            }
        }
        // The earliest deadline may have moved closer, so the thread has to
        // re-decide how long to sleep.
        condvar.notify_one();
    }

    /// How many deadlines are outstanding. Test instrumentation, and what
    /// `--dump-js` reports as "the page scheduled work".
    pub fn pending(&self) -> usize {
        self.shared.0.lock().map(|s| s.heap.len()).unwrap_or(0)
    }
}

impl Drop for Timers {
    fn drop(&mut self) {
        let (lock, condvar) = &*self.shared;
        if let Ok(mut schedule) = lock.lock() {
            schedule.done = true;
        }
        condvar.notify_one();
    }
}

/// The thread body: sleep until the earliest deadline, send what is due,
/// repeat. Never spins — every path through the loop either blocks on the
/// condvar or has just produced a message.
fn run(shared: &Arc<(Mutex<Schedule>, Condvar)>, tx: &Sender<Msg>) {
    let (lock, condvar) = &**shared;
    let mut schedule = lock.lock().expect("the timer lock is never poisoned");
    loop {
        if schedule.done {
            return;
        }
        let now = Instant::now();
        match schedule.heap.peek().map(|Reverse(entry)| entry.due) {
            // Something is due: take everything that has come due and send it.
            Some(due) if due <= now => {
                let Some(Reverse(entry)) = schedule.heap.pop() else {
                    continue;
                };
                // Send with the lock released: the event loop may answer by
                // scheduling another timer, and it must not block on us.
                drop(schedule);
                if tx
                    .send(Msg::Timer {
                        page: entry.page,
                        id: entry.id,
                    })
                    .is_err()
                {
                    // The app is gone.
                    return;
                }
                schedule = lock.lock().expect("the timer lock is never poisoned");
            }
            // Nothing due yet: park until it is, or until a request arrives.
            Some(due) => {
                let (next, _) = condvar
                    .wait_timeout(schedule, due.saturating_duration_since(now))
                    .expect("the timer lock is never poisoned");
                schedule = next;
            }
            // No timers at all: park indefinitely. This is the state a page
            // with no script sits in forever, at no cost.
            None => {
                schedule = condvar
                    .wait(schedule)
                    .expect("the timer lock is never poisoned");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn page() -> PageId {
        PageId::headless(1)
    }

    #[test]
    fn a_due_timer_arrives_as_one_message() {
        let (tx, rx) = mpsc::channel();
        let timers = Timers::spawn(tx);
        timers.apply(TimerRequest::Schedule {
            page: page(),
            id: TimerId(1),
            delay: Duration::ZERO,
        });

        let msg = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the timer never fired");
        assert_eq!(
            msg,
            Msg::Timer {
                page: page(),
                id: TimerId(1)
            }
        );
        // Exactly one: a one-shot timer is one message, not a stream.
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn timers_fire_in_deadline_order_with_ties_broken_by_insertion() {
        let (tx, rx) = mpsc::channel();
        let timers = Timers::spawn(tx);
        // Scheduled out of order, and the last two tie at the floor.
        for (id, delay) in [(1u64, 60u64), (2, 0), (3, 0), (4, 30)] {
            timers.apply(TimerRequest::Schedule {
                page: page(),
                id: TimerId(id),
                delay: Duration::from_millis(delay),
            });
        }

        let mut fired = Vec::new();
        for _ in 0..4 {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(Msg::Timer { id, .. }) => fired.push(id.0),
                other => panic!("expected a timer, got {other:?}"),
            }
        }
        assert_eq!(fired, [2, 3, 4, 1]);
    }

    #[test]
    fn a_cancelled_timer_never_arrives() {
        let (tx, rx) = mpsc::channel();
        let timers = Timers::spawn(tx);
        timers.apply(TimerRequest::Schedule {
            page: page(),
            id: TimerId(1),
            delay: Duration::from_millis(20),
        });
        timers.apply(TimerRequest::Schedule {
            page: page(),
            id: TimerId(2),
            delay: Duration::from_millis(20),
        });
        timers.apply(TimerRequest::Cancel {
            page: page(),
            id: TimerId(1),
        });

        let Ok(Msg::Timer { id, .. }) = rx.recv_timeout(Duration::from_secs(2)) else {
            panic!("the surviving timer never fired");
        };
        assert_eq!(id, TimerId(2));
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn navigation_cancels_a_whole_generation() {
        let (tx, rx) = mpsc::channel();
        let timers = Timers::spawn(tx);
        timers.apply(TimerRequest::Schedule {
            page: PageId::headless(1),
            id: TimerId(1),
            delay: Duration::from_millis(20),
        });
        timers.apply(TimerRequest::Schedule {
            page: PageId::headless(2),
            id: TimerId(1),
            delay: Duration::from_millis(20),
        });
        timers.apply(TimerRequest::CancelOthers {
            keep: PageId::headless(2),
        });

        let Ok(Msg::Timer { page, .. }) = rx.recv_timeout(Duration::from_secs(2)) else {
            panic!("the new page's timer never fired");
        };
        assert_eq!(page, PageId::headless(2));
        assert_eq!(timers.pending(), 0);
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn a_zero_delay_is_clamped_rather_than_immediate() {
        // The clamp is what stops `setTimeout(f, 0)` re-arming itself into a
        // spin: at the floor a page gets 250 ticks a second, not as many as
        // the loop can run.
        let (tx, rx) = mpsc::channel();
        let timers = Timers::spawn(tx);
        let started = Instant::now();
        timers.apply(TimerRequest::Schedule {
            page: page(),
            id: TimerId(1),
            delay: Duration::ZERO,
        });
        rx.recv_timeout(Duration::from_secs(2)).expect("fired");
        assert!(
            started.elapsed() >= MIN_DELAY,
            "a zero delay fired in {:?}, under the {MIN_DELAY:?} floor",
            started.elapsed()
        );
    }

    #[test]
    fn n_timers_produce_exactly_n_messages() {
        // The thread wakes once per deadline and sends once — not once per
        // loop iteration, and not once per condvar notification.
        let (tx, rx) = mpsc::channel();
        let timers = Timers::spawn(tx);
        for id in 1..=20u64 {
            timers.apply(TimerRequest::Schedule {
                page: page(),
                id: TimerId(id),
                delay: Duration::from_millis(id % 5),
            });
        }
        let mut count = 0;
        while rx.recv_timeout(Duration::from_millis(500)).is_ok() {
            count += 1;
        }
        assert_eq!(count, 20);
        assert_eq!(timers.pending(), 0);
    }

    #[test]
    fn a_pending_timer_leaves_the_thread_parked() {
        // What is testable in-process about "idle is idle": with a long
        // deadline outstanding, nothing arrives in between. The real gate is
        // a CPU measurement by hand, noted in the PR.
        let (tx, rx) = mpsc::channel();
        let timers = Timers::spawn(tx);
        timers.apply(TimerRequest::Schedule {
            page: page(),
            id: TimerId(1),
            delay: Duration::from_secs(10),
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "a ten-second timer produced a message immediately"
        );
        assert_eq!(timers.pending(), 1);
    }
}

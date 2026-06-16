//! Unit tests for the IPC wait queue.
//!
//! `WaitQueue` is the rendezvous FIFO an endpoint keeps, factored out of the
//! live IPC path so it is a pure function of (its own fields, an injected link
//! array) -- the same move that makes `pick_next` testable. Here we drive it
//! over a plain local `[Option<usize>; N]` link store, with no process table,
//! endpoints, or hardware in sight. The full send/recv rendezvous is exercised
//! end-to-end by the integration smoke (pingpong/rpc); these tests pin the
//! queue mechanics the smoke cannot isolate (FIFO order, the sender-vs-receiver
//! match decision, tail bookkeeping).

use super::TestCtx;
use crate::ipc::WaitQueue;
use crate::test_assert;

/// FIFO: waiters come back out in the order they went in, then the queue is
/// empty.
pub fn wq_fifo_order(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut links = [None; 4];
    let mut q = WaitQueue::empty();
    q.enqueue(0, true, &mut links);
    q.enqueue(1, true, &mut links);
    q.enqueue(2, true, &mut links);
    test_assert!(q.dequeue(&links) == Some(0), "first out should be 0");
    test_assert!(q.dequeue(&links) == Some(1), "second out should be 1");
    test_assert!(q.dequeue(&links) == Some(2), "third out should be 2");
    test_assert!(q.dequeue(&links).is_none(), "drained queue yields None");
    Ok(())
}

/// A single waiter enqueued then dequeued leaves the queue empty (head and tail
/// both cleared, so a second dequeue is None).
pub fn wq_single(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut links = [None; 4];
    let mut q = WaitQueue::empty();
    q.enqueue(2, false, &mut links);
    test_assert!(q.dequeue(&links) == Some(2), "the one waiter comes back");
    test_assert!(q.dequeue(&links).is_none(), "now empty");
    Ok(())
}

/// `take_if` only takes a waiter when the queued side matches: a queue of
/// senders yields to a recv (wants a sender), not to a send (wants a receiver).
pub fn wq_take_matches_sender_side(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut links = [None; 4];
    let mut q = WaitQueue::empty();
    q.enqueue(1, true, &mut links); // senders waiting
    test_assert!(q.take_if(false, &links).is_none(), "send must not take a queued sender");
    test_assert!(q.take_if(true, &links) == Some(1), "recv takes the queued sender");
    Ok(())
}

/// Symmetric: a queue of receivers yields to a send, not to a recv.
pub fn wq_take_matches_receiver_side(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut links = [None; 4];
    let mut q = WaitQueue::empty();
    q.enqueue(3, false, &mut links); // receivers waiting
    test_assert!(q.take_if(true, &links).is_none(), "recv must not take a queued receiver");
    test_assert!(q.take_if(false, &links) == Some(3), "send takes the queued receiver");
    Ok(())
}

/// An empty queue takes nothing, regardless of the side asked for.
pub fn wq_take_empty(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let links = [None; 4];
    let mut q = WaitQueue::empty();
    test_assert!(q.take_if(true, &links).is_none(), "empty: no sender");
    test_assert!(q.take_if(false, &links).is_none(), "empty: no receiver");
    Ok(())
}

/// After draining one side empty, the queue can be refilled with the other
/// side: `are_senders` tracks the new occupants, not the drained ones.
pub fn wq_refill_other_side(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut links = [None; 4];
    let mut q = WaitQueue::empty();
    q.enqueue(0, true, &mut links); // a sender waits
    test_assert!(q.take_if(true, &links) == Some(0), "sender taken");
    // Queue is now empty; enqueue a receiver and confirm the side flipped.
    q.enqueue(1, false, &mut links);
    test_assert!(q.take_if(true, &links).is_none(), "stale sender side must not linger");
    test_assert!(q.take_if(false, &links) == Some(1), "receiver now takeable");
    Ok(())
}

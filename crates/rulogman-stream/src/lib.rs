//! Byte-bounded transport events with an independent shutdown path.
//!
//! Data producers wait for the consumer; control events and the terminal event
//! never wait behind that budget. Finishing wakes blocked producers, preserves
//! queued events, and appends exactly one terminal event. Dropping the consumer
//! wakes producers and releases all queued data.

use event_listener::Event;
use futures::{Stream, future::poll_fn, task::AtomicWaker};
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

/// Maximum terminal bytes waiting for the UI per session.
pub const OUTPUT_BUDGET: usize = 1024 * 1024;
/// Largest data event produced by a transport.
pub const OUTPUT_CHUNK: usize = 32 * 1024;
const BATCH_BYTES: usize = 128 * 1024;
const BATCH_EVENTS: usize = 32;

struct State<T> {
    queue: VecDeque<(usize, T)>,
    bytes: usize,
    closed: bool,
    senders: usize,
}
struct Shared<T> {
    state: Mutex<State<T>>,
    capacity: usize,
    readable: AtomicWaker,
    writable: Event,
}

/// Producer of events; cloning it does not reserve extra output capacity.
pub struct Sender<T>(Arc<Shared<T>>);
/// Single consumer, usable as a stream or in bounded batches.
pub struct Receiver<T>(Arc<Shared<T>>);

/// Creates a channel whose queued data cannot exceed `capacity` bytes.
pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert!(capacity > 0);
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            queue: VecDeque::new(),
            bytes: 0,
            closed: false,
            senders: 1,
        }),
        capacity,
        readable: AtomicWaker::new(),
        writable: Event::new(),
    });
    (Sender(Arc::clone(&shared)), Receiver(shared))
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.0.state.lock().senders += 1;
        Self(Arc::clone(&self.0))
    }
}
impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock();
        state.senders -= 1;
        if state.senders == 0 {
            state.closed = true;
        }
        drop(state);
        self.0.readable.wake();
        self.0.writable.notify(usize::MAX);
    }
}
impl<T> Sender<T> {
    /// Sends data, waiting for space. Oversized events must be split by the producer.
    pub async fn send(&self, event: T, bytes: usize) -> bool {
        if bytes > self.0.capacity {
            return false;
        }
        loop {
            let writable = self.0.writable.listen();
            {
                let mut state = self.0.state.lock();
                if state.closed {
                    return false;
                }
                if state.bytes + bytes <= self.0.capacity {
                    state.bytes += bytes;
                    state.queue.push_back((bytes, event));
                    drop(state);
                    self.0.readable.wake();
                    return true;
                }
            }
            writable.await;
        }
    }

    /// Blocking counterpart for a dedicated PTY reader thread.
    pub fn send_blocking(&self, event: T, bytes: usize) -> bool {
        futures::executor::block_on(self.send(event, bytes))
    }

    /// Publishes lifecycle information without consuming terminal-byte capacity.
    pub fn control(&self, event: T) -> bool {
        let mut state = self.0.state.lock();
        if state.closed {
            return false;
        }
        state.queue.push_back((0, event));
        drop(state);
        self.0.readable.wake();
        true
    }

    /// Appends the final event and unblocks every producer, even when data is full.
    pub fn finish(&self, event: T) -> bool {
        let mut state = self.0.state.lock();
        if state.closed {
            return false;
        }
        state.closed = true;
        state.queue.push_back((0, event));
        drop(state);
        self.0.readable.wake();
        self.0.writable.notify(usize::MAX);
        true
    }

    /// Whether this stream has ended or its consumer was dropped.
    pub fn is_closed(&self) -> bool {
        self.0.state.lock().closed
    }
}
impl<T> Stream for Receiver<T> {
    type Item = T;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.0.readable.register(cx.waker());
        let mut state = self.0.state.lock();
        if let Some((bytes, event)) = state.queue.pop_front() {
            state.bytes -= bytes;
            drop(state);
            self.0.writable.notify(usize::MAX);
            Poll::Ready(Some(event))
        } else if state.closed {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}
impl<T> Receiver<T> {
    /// Reads an already queued event without waiting.
    pub fn try_recv(&mut self) -> Result<T, futures::channel::mpsc::TryRecvError> {
        let mut state = self.0.state.lock();
        if let Some((bytes, event)) = state.queue.pop_front() {
            state.bytes -= bytes;
            drop(state);
            self.0.writable.notify(usize::MAX);
            Ok(event)
        } else if state.closed {
            Err(futures::channel::mpsc::TryRecvError::Closed)
        } else {
            Err(futures::channel::mpsc::TryRecvError::Empty)
        }
    }

    /// Waits for an event, then drains at most 32 events / 128 KiB for one UI update.
    pub async fn next_batch(&mut self) -> Option<Vec<T>> {
        poll_fn(|cx| {
            self.0.readable.register(cx.waker());
            let mut state = self.0.state.lock();
            if state.queue.is_empty() {
                return if state.closed {
                    Poll::Ready(None)
                } else {
                    Poll::Pending
                };
            }
            let mut batch = Vec::new();
            let mut bytes = 0;
            while batch.len() < BATCH_EVENTS {
                let Some((weight, _)) = state.queue.front() else {
                    break;
                };
                if !batch.is_empty() && bytes + weight > BATCH_BYTES {
                    break;
                }
                let (weight, event) = state.queue.pop_front().unwrap();
                state.bytes -= weight;
                bytes += weight;
                batch.push(event);
            }
            drop(state);
            self.0.writable.notify(usize::MAX);
            Poll::Ready(Some(batch))
        })
        .await
    }
}
impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock();
        state.closed = true;
        state.queue.clear();
        state.bytes = 0;
        drop(state);
        self.0.writable.notify(usize::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{FutureExt, StreamExt, executor::block_on};

    #[test]
    fn output_waits_for_space_without_losing_order() {
        block_on(async {
            let (tx, mut rx) = channel(4);
            assert!(tx.send(1, 4).await);
            let mut blocked = Box::pin(tx.send(2, 4));
            assert!(blocked.as_mut().now_or_never().is_none());
            assert_eq!(rx.next().await, Some(1));
            assert!(blocked.await);
            assert_eq!(rx.next().await, Some(2));
        });
    }

    #[test]
    fn finishing_a_full_queue_wakes_a_producer_and_preserves_the_final_event() {
        block_on(async {
            let (tx, mut rx) = channel(4);
            assert!(tx.send(1, 4).await);
            let mut blocked = Box::pin(tx.send(2, 4));
            assert!(blocked.as_mut().now_or_never().is_none());
            assert!(tx.control(3));
            assert!(tx.finish(4));
            assert!(!tx.finish(5));
            assert!(!blocked.await);
            assert_eq!(rx.next_batch().await, Some(vec![1, 3, 4]));
            assert_eq!(rx.next().await, None);
        });
    }

    #[test]
    fn dropping_the_receiver_unblocks_a_reader_thread() {
        let (tx, rx) = channel(4);
        assert!(tx.send_blocking(1, 4));
        let worker = std::thread::spawn(move || tx.send_blocking(2, 4));
        drop(rx);
        assert!(!worker.join().unwrap());
    }

    #[test]
    fn a_cancelled_send_does_not_consume_capacity() {
        block_on(async {
            let (tx, mut rx) = channel(4);
            assert!(tx.send(1, 4).await);
            assert!(tx.send(2, 4).now_or_never().is_none());
            assert_eq!(rx.next().await, Some(1));
            assert!(tx.send(3, 4).await);
            drop(tx);
            assert_eq!(rx.next().await, Some(3));
            assert_eq!(rx.next().await, None);
        });
    }

    #[test]
    fn batches_bound_the_work_in_one_ui_update() {
        block_on(async {
            let (tx, mut rx) = channel(OUTPUT_BUDGET);
            for i in 0..10 {
                assert!(tx.send(i, OUTPUT_CHUNK).await);
            }
            assert_eq!(rx.next_batch().await, Some(vec![0, 1, 2, 3]));
            assert_eq!(rx.next_batch().await, Some(vec![4, 5, 6, 7]));
            assert_eq!(rx.next_batch().await, Some(vec![8, 9]));
        });
    }
}

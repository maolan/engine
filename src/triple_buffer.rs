use std::cell::UnsafeCell;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const BUFFER_COUNT: usize = 3;
const INDEX_MASK: usize = 0b11;
const FRONT_SHIFT: usize = 0;
const MIDDLE_SHIFT: usize = 2;
const BACK_SHIFT: usize = 4;
const DIRTY: usize = 1 << 6;

#[derive(Debug)]
struct Inner<T> {
    buffers: [UnsafeCell<T>; BUFFER_COUNT],
    state: AtomicUsize,
}

// Safety: the producer and consumer own disjoint buffer indexes. Index handoff
// is serialized through `state`; `T` must be `Send` because the
// handles may live on different threads.
unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}

#[derive(Debug)]
pub struct TripleBufferProducer<T> {
    inner: Arc<Inner<T>>,
    back: usize,
}

#[derive(Debug)]
pub struct TripleBufferConsumer<T> {
    inner: Arc<Inner<T>>,
    front: usize,
}

pub fn triple_buffer<T: Clone>(initial: T) -> (TripleBufferProducer<T>, TripleBufferConsumer<T>) {
    let inner = Arc::new(Inner {
        buffers: std::array::from_fn(|_| UnsafeCell::new(initial.clone())),
        state: AtomicUsize::new(pack_state(0, 1, 2, false)),
    });
    (
        TripleBufferProducer {
            inner: inner.clone(),
            back: 2,
        },
        TripleBufferConsumer { inner, front: 0 },
    )
}

impl<T> TripleBufferProducer<T> {
    pub fn write_buffer(&mut self) -> &mut T {
        // Safety: `self.back` is owned only by this producer until `publish`.
        unsafe { &mut *self.inner.buffers[self.back].get() }
    }

    pub fn publish(&mut self) {
        let mut state = self.inner.state.load(Ordering::Acquire);
        loop {
            let front = front_index(state);
            let middle = middle_index(state);
            let next = pack_state(front, self.back, middle, true);
            match self.inner.state.compare_exchange_weak(
                state,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.back = middle;
                    return;
                }
                Err(current) => state = current,
            }
        }
    }
}

impl<T> TripleBufferConsumer<T> {
    pub fn refresh(&mut self) -> bool {
        let mut state = self.inner.state.load(Ordering::Acquire);
        loop {
            if !is_dirty(state) {
                return false;
            }
            let front = front_index(state);
            let middle = middle_index(state);
            let back = back_index(state);
            let next = pack_state(middle, front, back, false);
            match self.inner.state.compare_exchange_weak(
                state,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.front = middle;
                    return true;
                }
                Err(current) => state = current,
            }
        }
    }

    pub fn read_buffer(&self) -> &T {
        // Safety: `self.front` is owned only by this consumer until `refresh`.
        unsafe { &*self.inner.buffers[self.front].get() }
    }
}

fn pack_state(front: usize, middle: usize, back: usize, dirty: bool) -> usize {
    (front << FRONT_SHIFT)
        | (middle << MIDDLE_SHIFT)
        | (back << BACK_SHIFT)
        | if dirty { DIRTY } else { 0 }
}

fn front_index(state: usize) -> usize {
    (state >> FRONT_SHIFT) & INDEX_MASK
}

fn middle_index(state: usize) -> usize {
    (state >> MIDDLE_SHIFT) & INDEX_MASK
}

fn back_index(state: usize) -> usize {
    (state >> BACK_SHIFT) & INDEX_MASK
}

fn is_dirty(state: usize) -> bool {
    state & DIRTY != 0
}

#[cfg(test)]
mod tests {
    use super::triple_buffer;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread;

    #[test]
    fn consumer_reads_initial_value_before_publish() {
        let (_producer, consumer) = triple_buffer(7usize);

        assert_eq!(*consumer.read_buffer(), 7);
    }

    #[test]
    fn refresh_publishes_latest_value() {
        let (mut producer, mut consumer) = triple_buffer(0usize);

        *producer.write_buffer() = 1;
        producer.publish();
        *producer.write_buffer() = 2;
        producer.publish();

        assert!(consumer.refresh());
        assert_eq!(*consumer.read_buffer(), 2);
        assert!(!consumer.refresh());
    }

    #[test]
    fn producer_can_reuse_buffers_after_consumer_refresh() {
        let (mut producer, mut consumer) = triple_buffer(Vec::<usize>::new());

        producer.write_buffer().push(1);
        producer.publish();
        assert!(consumer.refresh());
        assert_eq!(consumer.read_buffer().as_slice(), &[1]);

        producer.write_buffer().clear();
        producer.write_buffer().extend_from_slice(&[2, 3]);
        producer.publish();
        assert!(consumer.refresh());
        assert_eq!(consumer.read_buffer().as_slice(), &[2, 3]);
    }

    #[test]
    fn concurrent_latest_value_is_monotonic() {
        let (mut producer, mut consumer) = triple_buffer(0usize);
        let done = Arc::new(AtomicBool::new(false));
        let producer_done = done.clone();

        let handle = thread::spawn(move || {
            for value in 1..=10_000 {
                *producer.write_buffer() = value;
                producer.publish();
            }
            producer_done.store(true, Ordering::Release);
        });

        let mut last = 0;
        while !done.load(Ordering::Acquire) {
            if consumer.refresh() {
                let value = *consumer.read_buffer();
                assert!(value >= last);
                last = value;
            }
        }
        while consumer.refresh() {
            let value = *consumer.read_buffer();
            assert!(value >= last);
            last = value;
        }
        handle.join().unwrap();
        assert_eq!(last, 10_000);
    }
}

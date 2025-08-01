use std::{collections::VecDeque, mem::MaybeUninit};

use crate::hints::{likely, unlikely};

/// A double-ended queue implemented with a growable ring buffer that stores up
/// to N items inline before spilling to heap
///
/// # Examples
///
/// ```
/// use cynosure::site_c::queue::Queue;
///
/// let mut queue: Queue<i32, 4> = Queue::new();
///
/// // Push to both ends
/// queue.push_back(1);
/// queue.push_back(2);
/// queue.push_front(3);
/// queue.push_front(4);
///
/// // Queue is now: [4, 3, 1, 2]
///
/// // Pop from both ends
/// assert_eq!(queue.pop_front(), Some(4));
/// assert_eq!(queue.pop_back(), Some(2));
///
/// // Iterate over references (non-consuming)
/// for value in &queue {
///     println!("Value: {}", value);
/// }
///
/// // Queue is still usable
/// assert_eq!(queue.pop_front(), Some(3));
///
/// // Consume the queue with into_iter
/// let remaining: Vec<i32> = queue.into_iter().collect();
/// assert_eq!(remaining, vec![1]);
/// ```
pub enum Queue<T, const N: usize> {
    Inline {
        buf: [MaybeUninit<T>; N],
        head: usize,
        tail: usize,
        len: usize,
    },
    Heap(VecDeque<T>),
}

impl<T, const N: usize> Default for Queue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Queue<T, N> {
    /// Creates a new empty Queue
    #[inline]
    pub fn new() -> Self {
        Self::Inline {
            buf: [const { MaybeUninit::uninit() }; N],
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    /// Adds an element to the back of the queue
    #[inline]
    pub fn push_back(&mut self, value: T) {
        match self {
            Self::Inline {
                buf,
                head,
                tail,
                len,
            } => {
                if likely(*len < N) {
                    buf[*tail] = MaybeUninit::new(value);
                    *tail = if unlikely(*tail + 1 == N) {
                        0
                    } else {
                        *tail + 1
                    };
                    *len += 1;
                } else {
                    // Spill to heap - move elements in order from circular buffer
                    let mut heap = VecDeque::with_capacity(N * 2);

                    let mut idx = *head;
                    for _ in 0..*len {
                        let val = unsafe { buf[idx].assume_init_read() };
                        heap.push_back(val);
                        idx = if unlikely(idx + 1 == N) { 0 } else { idx + 1 };
                    }
                    heap.push_back(value);
                    *len = 0; // Prevent double-drop when Self::Inline is dropped
                    *self = Self::Heap(heap);
                }
            }
            Self::Heap(vec) => vec.push_back(value),
        }
    }

    /// Adds an element to the front of the queue
    #[inline]
    pub fn push_front(&mut self, value: T) {
        match self {
            Self::Inline {
                buf,
                head,
                tail: _,
                len,
            } => {
                if likely(*len < N) {
                    *head = if unlikely(*head == 0) {
                        N - 1
                    } else {
                        *head - 1
                    };
                    buf[*head] = MaybeUninit::new(value);
                    *len += 1;
                } else {
                    // Spill to heap - move elements in order from circular buffer
                    let mut heap = VecDeque::with_capacity(N * 2);
                    heap.push_front(value);

                    let mut idx = *head;
                    for _ in 0..*len {
                        let val = unsafe { buf[idx].assume_init_read() };
                        heap.push_back(val);
                        idx = if unlikely(idx + 1 == N) { 0 } else { idx + 1 };
                    }
                    *len = 0; // Prevent double-drop when Self::Inline is dropped
                    *self = Self::Heap(heap);
                }
            }
            Self::Heap(vec) => vec.push_front(value),
        }
    }

    /// Removes and returns the element at the front of the queue
    #[inline]
    pub fn pop_front(&mut self) -> Option<T> {
        match self {
            Self::Inline { buf, head, len, .. } => {
                if unlikely(*len == 0) {
                    None
                } else {
                    let value = unsafe { buf[*head].assume_init_read() };
                    *head = if unlikely(*head + 1 == N) {
                        0
                    } else {
                        *head + 1
                    };
                    *len -= 1;
                    Some(value)
                }
            }
            Self::Heap(vec) => vec.pop_front(),
        }
    }

    /// Removes and returns the element at the back of the queue
    #[inline]
    pub fn pop_back(&mut self) -> Option<T> {
        match self {
            Self::Inline { buf, tail, len, .. } => {
                if unlikely(*len == 0) {
                    None
                } else {
                    *tail = if unlikely(*tail == 0) {
                        N - 1
                    } else {
                        *tail - 1
                    };
                    let value = unsafe { buf[*tail].assume_init_read() };
                    *len -= 1;
                    Some(value)
                }
            }
            Self::Heap(vec) => vec.pop_back(),
        }
    }

    /// Removes and returns all elements from the queue as a Vec
    pub fn into_vec(&mut self) -> Vec<T> {
        match self {
            Self::Inline {
                buf,
                head,
                tail,
                len,
            } => {
                let mut result = Vec::with_capacity(*len);
                let mut idx = *head;
                for _ in 0..*len {
                    let val = unsafe { buf[idx].assume_init_read() };
                    result.push(val);
                    idx = if unlikely(idx + 1 == N) { 0 } else { idx + 1 };
                }
                *head = 0;
                *tail = 0;
                *len = 0;
                result
            }
            Self::Heap(vec) => vec.drain(..).collect(),
        }
    }

    /// Creates an iterator that yields references to elements in FIFO order
    /// Creates an iterator that yields references to elements in FIFO order
    #[inline]
    pub fn iter(&self) -> Iter<'_, T, N> {
        match self {
            Self::Inline { buf, head, len, .. } => Iter::Inline {
                buf,
                head: *head,
                remaining: *len,
                _phantom: std::marker::PhantomData,
            },
            Self::Heap(vec) => Iter::Heap(vec.iter()),
        }
    }
}

impl<T: std::fmt::Debug, const N: usize> std::fmt::Debug for Queue<T, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: Clone, const N: usize> Clone for Queue<T, N> {
    fn clone(&self) -> Self {
        let mut new_queue = Self::new();
        for item in self.iter() {
            new_queue.push_back(item.clone());
        }
        new_queue
    }
}

impl<T: PartialEq, const N: usize> PartialEq for Queue<T, N> {
    fn eq(&self, other: &Self) -> bool {
        if self.len() != other.len() {
            return false;
        }
        self.iter().zip(other.iter()).all(|(a, b)| a == b)
    }
}

impl<T: Eq, const N: usize> Eq for Queue<T, N> {}

impl<T, const N: usize> From<Vec<T>> for Queue<T, N> {
    fn from(vec: Vec<T>) -> Self {
        let mut queue = Self::new();
        for item in vec {
            queue.push_back(item);
        }
        queue
    }
}

impl<T, const N: usize> From<VecDeque<T>> for Queue<T, N> {
    fn from(vec_deque: VecDeque<T>) -> Self {
        let mut queue = Self::new();
        for item in vec_deque {
            queue.push_back(item);
        }
        queue
    }
}

impl<T, const N: usize> Extend<T> for Queue<T, N> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for item in iter {
            self.push_back(item);
        }
    }
}

impl<T, const N: usize> FromIterator<T> for Queue<T, N> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut queue = Self::new();
        queue.extend(iter);
        queue
    }
}

impl<T, const N: usize> Queue<T, N> {
    /// Returns the number of elements in the queue
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            Self::Inline { len, .. } => *len,
            Self::Heap(vec) => vec.len(),
        }
    }

    /// Returns true if the queue is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T, const N: usize> Drop for Queue<T, N> {
    fn drop(&mut self) {
        if let Self::Inline { buf, head, len, .. } = self {
            let mut idx = *head;
            for _ in 0..*len {
                unsafe {
                    buf[idx].assume_init_drop();
                }
                idx = if idx + 1 == N { 0 } else { idx + 1 };
            }
        }
    }
}

/// Iterator that yields references to elements in the queue
pub enum Iter<'a, T, const N: usize> {
    Inline {
        buf: &'a [std::mem::MaybeUninit<T>; N],
        head: usize,
        remaining: usize,
        _phantom: std::marker::PhantomData<&'a T>,
    },
    Heap(std::collections::vec_deque::Iter<'a, T>),
}

impl<'a, T, const N: usize> Iterator for Iter<'a, T, N> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Inline {
                buf,
                head,
                remaining,
                ..
            } => {
                if *remaining == 0 {
                    None
                } else {
                    let item = unsafe { buf[*head].assume_init_ref() };
                    *head = if *head + 1 == N { 0 } else { *head + 1 };
                    *remaining -= 1;
                    Some(item)
                }
            }
            Self::Heap(iter) => iter.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Inline { remaining, .. } => (*remaining, Some(*remaining)),
            Self::Heap(iter) => iter.size_hint(),
        }
    }
}

impl<'a, T, const N: usize> ExactSizeIterator for Iter<'a, T, N> {
    fn len(&self) -> usize {
        match self {
            Self::Inline { remaining, .. } => *remaining,
            Self::Heap(iter) => iter.len(),
        }
    }
}

/// Owning iterator that yields elements from the queue
pub struct IntoIter<T> {
    inner: std::vec::IntoIter<T>,
}

impl<T> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<T> ExactSizeIterator for IntoIter<T> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<T, const N: usize> IntoIterator for Queue<T, N> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(mut self) -> Self::IntoIter {
        let vec = self.into_vec();
        IntoIter {
            inner: vec.into_iter(),
        }
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a Queue<T, N> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T, N>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_queue_is_empty() {
        let mut queue: Queue<i32, 4> = Queue::new();
        assert_eq!(queue.pop_front(), None);
        assert_eq!(queue.into_vec(), Vec::<i32>::new());
    }

    #[test]
    fn test_single_element() {
        let mut queue: Queue<i32, 4> = Queue::new();
        queue.push_back(42);
        assert_eq!(queue.pop_front(), Some(42));
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn test_fifo_order() {
        let mut queue: Queue<i32, 4> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3);

        assert_eq!(queue.pop_front(), Some(1));
        assert_eq!(queue.pop_front(), Some(2));
        assert_eq!(queue.pop_front(), Some(3));
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn test_fill_to_capacity() {
        let mut queue: Queue<i32, 3> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3);

        // Should still be inline
        if let Queue::Inline { len, .. } = &queue {
            assert_eq!(*len, 3);
        } else {
            panic!("Queue should still be inline");
        }

        assert_eq!(queue.pop_front(), Some(1));
        assert_eq!(queue.pop_front(), Some(2));
        assert_eq!(queue.pop_front(), Some(3));
    }

    #[test]
    fn test_spill_to_heap() {
        let mut queue: Queue<i32, 2> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);

        // Should be inline
        assert!(matches!(queue, Queue::Inline { .. }));

        // This should spill to heap
        queue.push_back(3);

        // Should now be heap
        assert!(matches!(queue, Queue::Heap(_)));

        assert_eq!(queue.pop_front(), Some(1));
        assert_eq!(queue.pop_front(), Some(2));
        assert_eq!(queue.pop_front(), Some(3));
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn test_operations_after_spill() {
        let mut queue: Queue<i32, 2> = Queue::new();

        // Fill and spill
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3); // Spills here

        // Continue operations
        queue.push_back(4);
        queue.push_back(5);

        assert_eq!(queue.pop_front(), Some(1));
        assert_eq!(queue.pop_front(), Some(2));

        queue.push_back(6);

        assert_eq!(queue.pop_front(), Some(3));
        assert_eq!(queue.pop_front(), Some(4));
        assert_eq!(queue.pop_front(), Some(5));
        assert_eq!(queue.pop_front(), Some(6));
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn test_circular_buffer_wraparound() {
        let mut queue: Queue<i32, 3> = Queue::new();

        // Fill the buffer
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3);

        // Pop some elements
        assert_eq!(queue.pop_front(), Some(1));
        assert_eq!(queue.pop_front(), Some(2));

        // Add more (should wrap around)
        queue.push_back(4);
        queue.push_back(5);

        // Check order is preserved
        assert_eq!(queue.pop_front(), Some(3));
        assert_eq!(queue.pop_front(), Some(4));
        assert_eq!(queue.pop_front(), Some(5));
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn test_take_all_inline() {
        let mut queue: Queue<i32, 4> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3);

        let all = queue.into_vec();
        assert_eq!(all, vec![1, 2, 3]);
        assert_eq!(queue.pop_front(), None);
        assert_eq!(queue.into_vec(), Vec::<i32>::new());
    }

    #[test]
    fn test_take_all_heap() {
        let mut queue: Queue<i32, 2> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3); // Spills to heap
        queue.push_back(4);

        let all = queue.into_vec();
        assert_eq!(all, vec![1, 2, 3, 4]);
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn test_take_all_with_wraparound() {
        let mut queue: Queue<i32, 4> = Queue::new();

        // Fill buffer
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3);
        queue.push_back(4);

        // Pop some to create wraparound
        assert_eq!(queue.pop_front(), Some(1));
        assert_eq!(queue.pop_front(), Some(2));

        // Add more
        queue.push_back(5);
        queue.push_back(6);

        let all = queue.into_vec();
        assert_eq!(all, vec![3, 4, 5, 6]);
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn test_empty_operations() {
        let mut queue: Queue<i32, 4> = Queue::new();

        assert_eq!(queue.pop_front(), None);
        assert_eq!(queue.into_vec(), Vec::<i32>::new());

        // After operations, should still work
        queue.push_back(1);
        assert_eq!(queue.pop_front(), Some(1));
    }

    #[test]
    fn test_mixed_operations_sequence() {
        let mut queue: Queue<i32, 3> = Queue::new();

        queue.push_back(1);
        assert_eq!(queue.pop_front(), Some(1));

        queue.push_back(2);
        queue.push_back(3);
        assert_eq!(queue.pop_front(), Some(2));

        queue.push_back(4);
        queue.push_back(5);

        assert_eq!(queue.pop_front(), Some(3));
        assert_eq!(queue.pop_front(), Some(4));

        queue.push_back(6);
        assert_eq!(queue.pop_front(), Some(5));
        assert_eq!(queue.pop_front(), Some(6));
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn test_capacity_one() {
        let mut queue: Queue<i32, 1> = Queue::new();

        queue.push_back(1);
        assert_eq!(queue.pop_front(), Some(1));

        queue.push_back(2);
        queue.push_back(3); // Should spill to heap

        assert!(matches!(queue, Queue::Heap(_)));
        assert_eq!(queue.pop_front(), Some(2));
        assert_eq!(queue.pop_front(), Some(3));
    }

    #[test]
    fn test_large_sequence() {
        let mut queue: Queue<i32, 4> = Queue::new();

        // Add many elements to test heap behavior
        for i in 0..100 {
            queue.push_back(i);
        }

        // Remove half
        for i in 0..50 {
            assert_eq!(queue.pop_front(), Some(i));
        }

        // Add more
        for i in 100..150 {
            queue.push_back(i);
        }

        // Take all remaining
        let remaining = queue.into_vec();
        let expected: Vec<i32> = (50..100).chain(100..150).collect();
        assert_eq!(remaining, expected);
    }

    #[test]
    fn test_string_elements() {
        let mut queue: Queue<String, 2> = Queue::new();

        queue.push_back("hello".to_string());
        queue.push_back("world".to_string());

        assert_eq!(queue.pop_front(), Some("hello".to_string()));

        queue.push_back("!".to_string()); // Should spill to heap

        assert_eq!(queue.pop_front(), Some("world".to_string()));
        assert_eq!(queue.pop_front(), Some("!".to_string()));
    }

    #[test]
    fn test_clone_elements() {
        #[derive(Clone, PartialEq, Debug)]
        struct TestStruct(i32);

        let mut queue: Queue<TestStruct, 2> = Queue::new();

        queue.push_back(TestStruct(1));
        queue.push_back(TestStruct(2));
        queue.push_back(TestStruct(3)); // Spill to heap

        assert_eq!(queue.pop_front(), Some(TestStruct(1)));
        assert_eq!(queue.pop_front(), Some(TestStruct(2)));
        assert_eq!(queue.pop_front(), Some(TestStruct(3)));
    }

    #[test]
    fn test_zero_capacity_compiles() {
        // This should compile but immediately spill to heap
        let mut queue: Queue<i32, 0> = Queue::new();
        queue.push_back(1);
        assert!(matches!(queue, Queue::Heap(_)));
        assert_eq!(queue.pop_front(), Some(1));
    }

    #[test]
    fn test_alternating_push_pop() {
        let mut queue: Queue<i32, 3> = Queue::new();

        for i in 0..10 {
            queue.push_back(i);
            assert_eq!(queue.pop_front(), Some(i));
        }

        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn test_take_all_after_wraparound_edge_case() {
        let mut queue: Queue<i32, 3> = Queue::new();

        // Create a specific wraparound scenario
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3);

        // Pop all but one
        assert_eq!(queue.pop_front(), Some(1));
        assert_eq!(queue.pop_front(), Some(2));

        // Add to wrap around
        queue.push_back(4);
        queue.push_back(5);

        // Now we have: [4, 5, 3] with head pointing to index 2 (element 3)
        let all = queue.into_vec();
        assert_eq!(all, vec![3, 4, 5]);
    }

    #[test]
    fn test_reuse_after_take_all() {
        let mut queue: Queue<i32, 3> = Queue::new();

        queue.push_back(1);
        queue.push_back(2);
        let _ = queue.into_vec();

        // Should be reset and reusable
        queue.push_back(3);
        queue.push_back(4);
        assert_eq!(queue.pop_front(), Some(3));
        assert_eq!(queue.pop_front(), Some(4));
    }

    #[test]
    fn test_iter_inline() {
        let mut queue: Queue<i32, 4> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3);

        let items: Vec<&i32> = queue.iter().collect();
        assert_eq!(items, vec![&1, &2, &3]);

        // Original queue should be unchanged
        assert_eq!(queue.pop_front(), Some(1));
    }

    #[test]
    fn test_iter_heap() {
        let mut queue: Queue<i32, 2> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3); // Spills to heap

        let items: Vec<&i32> = queue.iter().collect();
        assert_eq!(items, vec![&1, &2, &3]);

        // Original queue should be unchanged
        assert_eq!(queue.pop_front(), Some(1));
    }

    #[test]
    fn test_iter_with_wraparound() {
        let mut queue: Queue<i32, 4> = Queue::new();

        // Fill buffer
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3);
        queue.push_back(4);

        // Pop some to create wraparound
        assert_eq!(queue.pop_front(), Some(1));
        assert_eq!(queue.pop_front(), Some(2));

        // Add more
        queue.push_back(5);
        queue.push_back(6);

        let items: Vec<&i32> = queue.iter().collect();
        assert_eq!(items, vec![&3, &4, &5, &6]);
    }

    #[test]
    fn test_iter_empty() {
        let queue: Queue<i32, 4> = Queue::new();
        let items: Vec<&i32> = queue.iter().collect();
        assert_eq!(items, Vec::<&i32>::new());
    }

    #[test]
    fn test_into_iter_inline() {
        let mut queue: Queue<i32, 4> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3);

        let items: Vec<i32> = queue.into_iter().collect();
        assert_eq!(items, vec![1, 2, 3]);
    }

    #[test]
    fn test_into_iter_heap() {
        let mut queue: Queue<i32, 2> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3); // Spills to heap

        let items: Vec<i32> = queue.into_iter().collect();
        assert_eq!(items, vec![1, 2, 3]);
    }

    #[test]
    fn test_iter_size_hint() {
        let mut queue: Queue<i32, 3> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);

        let mut iter = queue.iter();
        assert_eq!(iter.size_hint(), (2, Some(2)));
        assert_eq!(iter.len(), 2);

        iter.next();
        assert_eq!(iter.size_hint(), (1, Some(1)));
        assert_eq!(iter.len(), 1);

        iter.next();
        assert_eq!(iter.size_hint(), (0, Some(0)));
        assert_eq!(iter.len(), 0);
    }

    #[test]
    fn test_into_iter_size_hint() {
        let mut queue: Queue<i32, 3> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);

        let mut iter = queue.into_iter();
        assert_eq!(iter.size_hint(), (2, Some(2)));
        assert_eq!(iter.len(), 2);

        iter.next();
        assert_eq!(iter.size_hint(), (1, Some(1)));
        assert_eq!(iter.len(), 1);
    }

    #[test]
    fn test_iter_for_loop() {
        let mut queue: Queue<i32, 3> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3);

        let mut sum = 0;
        for &value in &queue {
            sum += value;
        }
        assert_eq!(sum, 6);

        // Queue should be unchanged
        assert_eq!(queue.iter().count(), 3);
    }

    #[test]
    fn test_into_iter_for_loop() {
        let mut queue: Queue<i32, 3> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3);

        let mut sum = 0;
        for value in queue {
            sum += value;
        }
        assert_eq!(sum, 6);
    }

    #[test]
    fn test_debug() {
        let mut queue: Queue<i32, 3> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3);

        let debug_str = format!("{:?}", queue);
        assert_eq!(debug_str, "[1, 2, 3]");
    }

    #[test]
    fn test_clone() {
        let mut queue: Queue<i32, 3> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);

        let cloned = queue.clone();
        assert_eq!(queue, cloned);

        // Verify they're independent
        queue.push_back(3);
        assert_ne!(queue, cloned);
    }

    #[test]
    fn test_partial_eq() {
        let mut queue1: Queue<i32, 3> = Queue::new();
        let mut queue2: Queue<i32, 3> = Queue::new();

        queue1.push_back(1);
        queue1.push_back(2);
        queue2.push_back(1);
        queue2.push_back(2);

        assert_eq!(queue1, queue2);

        queue2.push_back(3);
        assert_ne!(queue1, queue2);
    }

    #[test]
    fn test_default() {
        let queue: Queue<i32, 3> = Queue::default();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn test_from_vec() {
        let vec = vec![1, 2, 3, 4];
        let queue: Queue<i32, 2> = Queue::from(vec);

        let items: Vec<i32> = queue.into_iter().collect();
        assert_eq!(items, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_from_vec_deque() {
        let mut vec_deque = VecDeque::new();
        vec_deque.push_back(1);
        vec_deque.push_back(2);
        vec_deque.push_back(3);

        let queue: Queue<i32, 2> = Queue::from(vec_deque);
        let items: Vec<i32> = queue.into_iter().collect();
        assert_eq!(items, vec![1, 2, 3]);
    }

    #[test]
    fn test_extend() {
        let mut queue: Queue<i32, 2> = Queue::new();
        queue.push_back(1);

        queue.extend(vec![2, 3, 4]);
        let items: Vec<i32> = queue.into_iter().collect();
        assert_eq!(items, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_from_iterator() {
        let queue: Queue<i32, 2> = (1..=4).collect();
        let items: Vec<i32> = queue.into_iter().collect();
        assert_eq!(items, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_len_and_is_empty() {
        let mut queue: Queue<i32, 3> = Queue::new();
        assert_eq!(queue.len(), 0);
        assert!(queue.is_empty());

        queue.push_back(1);
        assert_eq!(queue.len(), 1);
        assert!(!queue.is_empty());

        queue.push_back(2);
        queue.push_back(3);
        assert_eq!(queue.len(), 3);

        queue.pop_front();
        assert_eq!(queue.len(), 2);
        assert!(!queue.is_empty());
    }

    #[test]
    fn test_len_and_is_empty_after_spill() {
        let mut queue: Queue<i32, 2> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3); // Spills to heap

        assert_eq!(queue.len(), 3);
        assert!(!queue.is_empty());

        queue.pop_front();
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn test_eq_with_different_storage() {
        let mut inline_queue: Queue<i32, 4> = Queue::new();
        inline_queue.push_back(1);
        inline_queue.push_back(2);

        let mut heap_queue: Queue<i32, 1> = Queue::new();
        heap_queue.push_back(1);
        heap_queue.push_back(2); // Spills to heap, contains [1, 2]

        // Compare their contents since they have different N values
        let inline_contents: Vec<_> = inline_queue.iter().collect();
        let heap_contents: Vec<_> = heap_queue.iter().collect();
        assert_eq!(inline_contents, heap_contents);
    }

    #[test]
    fn test_push_front_basic() {
        let mut queue: Queue<i32, 4> = Queue::new();
        queue.push_front(1);
        queue.push_front(2);
        queue.push_front(3);

        assert_eq!(queue.pop_front(), Some(3));
        assert_eq!(queue.pop_front(), Some(2));
        assert_eq!(queue.pop_front(), Some(1));
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn test_pop_back_basic() {
        let mut queue: Queue<i32, 4> = Queue::new();
        queue.push_back(1);
        queue.push_back(2);
        queue.push_back(3);

        assert_eq!(queue.pop_back(), Some(3));
        assert_eq!(queue.pop_back(), Some(2));
        assert_eq!(queue.pop_back(), Some(1));
        assert_eq!(queue.pop_back(), None);
    }

    #[test]
    fn test_mixed_push_front_back() {
        let mut queue: Queue<i32, 4> = Queue::new();
        queue.push_back(1);
        queue.push_front(2);
        queue.push_back(3);
        queue.push_front(4);

        // Queue should be: [4, 2, 1, 3]
        assert_eq!(queue.pop_front(), Some(4));
        assert_eq!(queue.pop_back(), Some(3));
        assert_eq!(queue.pop_front(), Some(2));
        assert_eq!(queue.pop_back(), Some(1));
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn test_push_front_spill_to_heap() {
        let mut queue: Queue<i32, 3> = Queue::new();
        queue.push_front(1);
        queue.push_front(2);
        queue.push_front(3);

        // This should trigger spill to heap
        queue.push_front(4);

        // Verify order is preserved
        assert_eq!(queue.pop_front(), Some(4));
        assert_eq!(queue.pop_front(), Some(3));
        assert_eq!(queue.pop_front(), Some(2));
        assert_eq!(queue.pop_front(), Some(1));
    }

    #[test]
    fn test_wraparound_with_push_front_pop_back() {
        let mut queue: Queue<i32, 4> = Queue::new();

        // Fill partially and create wraparound
        queue.push_back(1);
        queue.push_back(2);
        assert_eq!(queue.pop_front(), Some(1));

        queue.push_front(3);
        queue.push_back(4);
        queue.push_front(5);

        // Queue should be: [5, 3, 2, 4]
        assert_eq!(queue.pop_back(), Some(4));
        assert_eq!(queue.pop_front(), Some(5));
        assert_eq!(queue.pop_back(), Some(2));
        assert_eq!(queue.pop_front(), Some(3));
    }

    #[test]
    fn test_alternating_all_operations() {
        let mut queue: Queue<i32, 5> = Queue::new();

        queue.push_back(1);
        queue.push_front(2);
        assert_eq!(queue.pop_back(), Some(1));

        queue.push_back(3);
        queue.push_front(4);
        queue.push_back(5);
        assert_eq!(queue.pop_front(), Some(4));

        queue.push_front(6);
        assert_eq!(queue.pop_back(), Some(5));

        // Queue should now have: [6, 2, 3]
        assert_eq!(queue.len(), 3);

        let vec: Vec<i32> = queue.iter().copied().collect();
        assert_eq!(vec, vec![6, 2, 3]);
    }

    #[test]
    fn test_push_front_pop_back_after_spill() {
        let mut queue: Queue<i32, 2> = Queue::new();

        // Fill to capacity
        queue.push_back(1);
        queue.push_back(2);

        // Spill to heap
        queue.push_back(3);

        // Test operations work correctly on heap storage
        queue.push_front(4);
        queue.push_back(5);

        assert_eq!(queue.pop_back(), Some(5));
        assert_eq!(queue.pop_front(), Some(4));
        assert_eq!(queue.len(), 3);
    }
}

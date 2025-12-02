use core::ptr::NonNull;

use branches::likely;

use crate::Signal;

pub(crate) struct SignalQueue {
    first: Option<NonNull<Signal>>,
    last: Option<NonNull<Signal>>,
}

unsafe impl Send for SignalQueue {}

impl SignalQueue {
    /// Creates a new empty SignalQueue.
    #[inline(always)]
    pub const fn new() -> Self {
        Self {
            first: None,
            last: None,
        }
    }

    /// Pushes a signal entry into the queue.
    /// 
    /// # Safety
    /// 
    /// The caller must ensure that the entry lives long enough in the
    /// queue or is removed from the queue on drop, caller must guarantee
    /// entry.next is None.
    #[inline(always)]
    pub unsafe fn push(&mut self, entry: NonNull<Signal>) -> bool {
        if let Some(mut old) = self.last.take() {
            // SAFETY: self.last was guaranteed to be valid
            unsafe {
                old.as_mut().next = Some(entry);
            }
            self.last = Some(entry);
            false
        } else {
            self.first = Some(entry);
            self.last = Some(entry);
            true
        }
    }

    /// Pops a signal entry from the front of the queue.
    #[inline(always)]
    pub fn pop(&mut self) -> Option<NonNull<Signal>> {
        // Take the first element; return None if the queue is empty.
        let first = self.first?;
        // SAFETY: `first` is a valid `NonNull<Signal>` because it came from the queue.
        let entry = unsafe { first.as_ref() };
        let next = entry.next;
        if let Some(next_nn) = next {
            // There is a next element; update the head of the queue.
            self.first = Some(next_nn);
        } else {
            // Queue becomes empty; clear both pointers.
            self.first = None;
            self.last = None;
        }
        // Return the raw pointer to the popped signal.
        Some(first)
    }

    /// Removes a specific signal entry from the queue.
    /// Returns true if the entry was found and removed, false otherwise.
    #[inline(always)]
    pub fn remove(&mut self, entry: NonNull<Signal>) -> bool {
        let mut cur = self.first;
        let mut prev: Option<NonNull<Signal>> = None;
        while likely(cur.is_some()) {
            let mut cur_ptr = cur.unwrap();
            if cur_ptr == entry {
                if let Some(mut prev) = prev {
                    unsafe {
                        // SAFETY: prev is not null and guaranteed to be valid
                        prev.as_mut().next = cur_ptr.as_mut().next;
                    }
                } else {
                    self.first = unsafe { cur_ptr.as_mut().next };
                }
                if self.last == Some(cur_ptr) {
                    self.last = prev;
                }
                return true;
            }
            prev = Some(cur_ptr);
            cur = unsafe {
                // SAFETY: current is not null and guaranteed to be valid
                cur_ptr.as_mut().next
            };
        }
        false
    }
}

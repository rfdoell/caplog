use std::collections::LinkedList;
use std::mem::MaybeUninit;
use std::cell::UnsafeCell;
use std::sync::{RwLock, atomic::{AtomicU32, Ordering}};

const BLOCK_SIZE: usize = 128;

pub struct StableList<T> {
    list_lock: RwLock<LinkedList<*const [UnsafeCell<MaybeUninit<T>>; BLOCK_SIZE]>>,
    /// Index just past the last initialized item in the StableList
    ///
    /// The item pointed to by this idx is uninitialized or may not exist.
    last_global_idx: AtomicU32,
}

unsafe impl<T> Send for StableList<T> where T: Send {}
unsafe impl<T> Sync for StableList<T> {}

impl<T> StableList<T> {
    pub fn new() -> Self {
        let mut list: LinkedList<*const _> = LinkedList::new();
        let block: [UnsafeCell<MaybeUninit<T>>; BLOCK_SIZE] = unsafe { MaybeUninit::uninit().assume_init() };
        list.push_back(Box::into_raw(Box::new(block)));
        StableList{
            list_lock: RwLock::new(list),
            last_global_idx: AtomicU32::new(0),
        }
    }

    pub fn push(&self, item: T) {
       let mut list = match self.list_lock.write() {
           Ok(lock) => lock,
           Err(_) => panic!("StableList's internal mutex has been poisoned"),
       };
       // make sure to get the most recent value, don't move this before the lock
       let global_idx = self.last_global_idx.load(Ordering::SeqCst) as usize;
       if global_idx == u32::MAX as usize {
           panic!("list is full, cannot index past 2^32");
       }
       if global_idx % BLOCK_SIZE == 0 && global_idx != 0 {
            // we have all full blocks and have to add a new one
            // Safety: We are telling the compiler to assume initialization of the MaybeUninit values
            // *not* the T inside them. MaybeUninit requires no initialization.
            let block: [UnsafeCell<MaybeUninit<T>>; BLOCK_SIZE] = unsafe { MaybeUninit::uninit().assume_init() };
            list.push_back(Box::into_raw(Box::new(block)));
       }
       let last_block = list.iter_mut().last().expect("no block in list even though we tried to add one");
       unsafe { *(**last_block)[global_idx % BLOCK_SIZE].get() = MaybeUninit::new(item) };
       // Safety: only modify last_global_idx while we have the lock
       self.last_global_idx.fetch_add(1, Ordering::SeqCst);
    }

    pub fn len(&self) -> usize {
       self.last_global_idx.load(Ordering::SeqCst) as usize
    }

    unsafe fn get_chunk(&self, idx: usize) -> Option<*const [UnsafeCell<MaybeUninit<T>>; BLOCK_SIZE]> {
        println!("stable: fetching {} chunk", idx);
        match self.list_lock.read() {
            Ok(lock) => lock.iter().nth(idx).map(|&val| val),
            Err(_) => panic!("StableList's internal mutex has been poisoned"),
        }
    }

    fn get(&self, idx: usize) -> Option<&T> {
        if idx < self.last_global_idx.load(Ordering::SeqCst) as usize {
            let list = match self.list_lock.read() {
               Ok(lock) => lock,
               Err(_) => panic!("StableList's internal mutex has been poisoned"),
            };
            // Safety: All values before last_global_idx are guaranteed to be initialized
            list.iter().nth(idx / BLOCK_SIZE)
                .map(|ch| unsafe { &*(*(&**ch)[idx % BLOCK_SIZE].get()).as_ptr() } )
        } else {
            None
        }
    }

    fn iter(&self) -> StableListIterator<'_, T> {
        let list = match self.list_lock.read() {
           Ok(lock) => lock,
           Err(_) => panic!("StableList's internal mutex has been poisoned"),
        };
        let global_idx = self.last_global_idx.load(Ordering::SeqCst) as usize;
        StableListIterator {
            global_idx: global_idx,
            chunk: *list.iter().last().unwrap(),
            list: self,
            first_item_given: false,
        }
    }
}

// TODO impl Drop for StableList, by default dropping MaybeUninit does nothing resulting in the
// internal values leaking if they are heap allocated

struct StableListIterator<'a, T: 'a> {
    global_idx: usize,
    chunk: *const [UnsafeCell<MaybeUninit<T>>; BLOCK_SIZE],
    list: &'a StableList<T>,
    first_item_given: bool,
}

impl<'a, T: std::fmt::Debug> Iterator for StableListIterator<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.first_item_given && self.list.len() == 0 {
            return None;
        }
        dbg!(self.global_idx);
        if self.global_idx + 1 == self.list.len() {
            // no values to return right now
            return None;
        }

        if self.global_idx % BLOCK_SIZE + 1 == BLOCK_SIZE {
            // this would be a lot simpler if LinkedList exposed a way to hold a reference to a
            // node, the proposed cursor API might be what is necessary: https://github.com/rust-lang/rust/issues/58533
            // TODO safety
            match unsafe { self.list.get_chunk(self.global_idx / BLOCK_SIZE + 1) } {
                None => return None,
                Some(chunk) => {
                    dbg!(chunk);
                    self.global_idx += 1;
                    self.chunk = chunk;
                },
            }
        } else if self.first_item_given {
            self.global_idx += 1;
        } else {
            self.first_item_given = true;
        }
        let val = unsafe { (&*self.chunk)[self.global_idx % BLOCK_SIZE].get().as_ref().unwrap() };
        return dbg!(Some(unsafe { &*val.as_ptr().as_ref().unwrap() }));
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use proptest::prelude::*;

    #[test]
    // Test that an item pushed into StableList can then be retrieved via `get`
    fn push_and_check_single_item() {
        let list = StableList::new();
        assert_eq!(list.get(0), None);
        list.push(1002);
        assert_eq!(list.get(0), Some(&1002));
    }

    fn push_and_check_full_chunk() {
        let list = StableList::new();
        assert_eq!(list.get(0), None);
        for i in 0..BLOCK_SIZE {
            list.push(100 + i);
        }
        for i in 0..BLOCK_SIZE {
            assert_eq!(list.get(i), Some(&(100+i)));
        }
    }

    fn push_and_check_multiple_chunks() {
        let list = StableList::new();
        assert_eq!(list.get(0), None);
        for i in 0..(BLOCK_SIZE*2) {
            list.push(100 + i);
        }
        for i in 0..(BLOCK_SIZE*2) {
            assert_eq!(list.get(i), Some(&(100+i)));
        }
    }

    #[test]
    // Test that populating the list then iterating over it works
    fn populate_and_iterate_simple() {
        let list = StableList::new();
        let mut iter = list.iter();
        let arb_values = BLOCK_SIZE * 2 + 1;
        for i in 0..(arb_values) {
            list.push(i * 10);
        }
        assert_eq!(list.len(), arb_values);
        let mut values_found = 0;
        for (exp, val) in (0..).zip(iter) {
            dbg!((exp*10, val, list.get(exp)));
            assert_eq!(exp*10, *val);
            values_found += 1;
        }
        assert_eq!(values_found, arb_values);
    }

    #[test]
    // Test that iterator will return values again after returning None if new values are added to
    // base list
    fn iterator_resumption() {
        let mut list = StableList::new();
        let mut iter = list.iter();
        assert_eq!(list.len(), 0);
        assert_eq!(iter.next(), None);
        list.push(1000);
        assert_eq!(list.len(), 1);
        assert_eq!(iter.next(), Some(&1000));
    }

    //proptest! {
    //    #[test]
    //    fn test_calc_local_idx(last_global: usize, block_size: std::num::NonZeroUsize) {
    //        let idx = StableList::calc_local_idx(last_global, block_size);
    //        assert!(idx < block_size);
    //        assert!(idx < block_size);
    //        assert!(idx == last_global % block_size);
    //    }
    //}
}

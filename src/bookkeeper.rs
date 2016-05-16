//! Memory bookkeeping primitives.
//!
//! Blocks are the main unit for the memory bookkeeping. A block is a simple construct with a
//! `Unique` pointer and a size. Occupied (non-free) blocks are represented by a zero-sized block.

use block::Block;
use sys;

use core::mem::{align_of, size_of};
use core::ptr::Unique;
use core::{ops, ptr, slice, cmp};

#[cfg(debug_assertions)]
use core::fmt;

/// An address representing an "empty" or non-allocated value on the heap.
const EMPTY_HEAP: *mut u8 = 0x1 as *mut _;

/// The memory bookkeeper.
///
/// This is the main primitive in ralloc. Its job is to keep track of the free blocks in a
/// structured manner, such that allocation, reallocation, and deallocation are all efficient.
/// Parituclarly, it keeps a list of blocks, commonly called the "block vector". This list is kept.
/// Entries in the block vector can be "empty", meaning that you can overwrite the entry without
/// breaking consistency.
///
/// For details about the internals, see [`BlockVec`](./struct.BlockVec.html) (requires the docs
/// to be rendered with private item exposed).
pub struct Bookkeeper {
    /// The internal block vector.
    ///
    /// Guarantees
    /// ==========
    ///
    /// Certain guarantees are made:
    ///
    /// 1. The list is always sorted with respect to the block's pointers.
    /// 2. No two blocks overlap.
    /// 3. No two free blocks are adjacent.
    inner: BlockVec,
}

impl Bookkeeper {
    /// Construct a new, empty bookkeeper.
    ///
    /// No allocations or BRKs are done.
    pub fn new() -> Bookkeeper {
        Bookkeeper {
            inner: BlockVec::new(),
        }
    }

    /// Allocate a chunk of memory.
    ///
    /// This function takes a size and an alignment. From these a fitting block is found, to which
    /// a pointer is returned. The pointer returned has the following guarantees:
    ///
    /// 1. It is aligned to `align`: In particular, `align` divides the address.
    /// 2. The chunk can be safely read and written, up to `size`. Reading or writing out of this
    ///    bound is undefined behavior.
    /// 3. It is a valid, unique, non-null pointer, until `free` is called again.
    pub fn alloc(&mut self, size: usize, align: usize) -> Unique<u8> {
        self.inner.alloc(size, align)
    }

    /// Reallocate memory.
    ///
    /// If necessary it will allocate a new buffer and deallocate the old one.
    ///
    /// The following guarantees are made:
    ///
    /// 1. The returned pointer is valid and aligned to `align`.
    /// 2. The returned pointer points to a buffer containing the same data byte-for-byte as the
    ///    original buffer.
    /// 3. Reading and writing up to the bound, `new_size`, is valid.
    pub fn realloc(&mut self, block: Block, new_size: usize, align: usize) -> Unique<u8> {
        self.inner.realloc(block, new_size, align)
    }

    /// Free a memory block.
    ///
    /// After this have been called, no guarantees are made about the passed pointer. If it want
    /// to, it could begin shooting laser beams.
    ///
    /// Freeing an invalid block will drop all future guarantees about this bookkeeper.
    pub fn free(&mut self, block: Block) {
        self.inner.free(block)
    }
}

/// Calculate the aligner.
///
/// The aligner is what we add to a pointer to align it to a given value.
fn aligner(ptr: *const u8, align: usize) -> usize {
    align - ptr as usize % align
}

/// Canonicalize a BRK request.
///
/// Syscalls can be expensive, which is why we would rather accquire more memory than necessary,
/// than having many syscalls acquiring memory stubs. Memory stubs are small blocks of memory,
/// which are essentially useless until merge with another block.
///
/// To avoid many syscalls and accumulating memory stubs, we BRK a little more memory than
/// necessary. This function calculate the memory to be BRK'd based on the necessary memory.
///
/// The return value is always greater than or equals to the argument.
fn canonicalize_brk(size: usize) -> usize {
    const BRK_MULTIPLIER: usize = 1;
    const BRK_MIN: usize = 200;
    const BRK_MIN_EXTRA: usize = 10000; // TODO tune this?

    let res = cmp::max(BRK_MIN, size.saturating_add(cmp::min(BRK_MULTIPLIER * size, BRK_MIN_EXTRA)));

    debug_assert!(res >= size, "Canonicalized BRK space is smaller than the one requested.");

    res
}

/// A block vector.
///
/// This primitive is used for keeping track of the free blocks.
///
/// Only making use of only [`alloc`](#method.alloc), [`free`](#method.free),
/// [`realloc`](#method.realloc) (and following their respective assumptions) guarantee that no
/// buffer overrun, segfault, arithmetic overflow, or otherwise unexpected crash.
struct BlockVec {
    /// The capacity of the block vector.
    cap: usize,
    /// The length of the block vector.
    len: usize,
    /// The segment end.
    ///
    /// This points to the end of the heap.
    seg_end: Unique<u8>,
    /// The pointer to the first element in the block vector.
    ptr: Unique<Block>,
}

impl BlockVec {
    /// Create a new, empty block vector.
    ///
    /// This will make no allocations or BRKs.
    fn new() -> BlockVec {
        BlockVec {
            cap: 0,
            len: 0,
            seg_end: unsafe { Unique::new(EMPTY_HEAP as *mut _) },
            ptr: unsafe { Unique::new(EMPTY_HEAP as *mut _) },
        }
    }

    /// Initialize the block vector.
    ///
    /// This will do some basic initial allocation, and a bunch of other things as well. It is
    /// necessary to avoid meta-circular dependency.
    // TODO can this be done in a more elegant way?
    fn init(&mut self) {
        debug_assert!(self.cap == 0, "Capacity is non-zero on initialization.");

        /// The initial capacity.
        const INITIAL_CAPACITY: usize = 16;

        let size = INITIAL_CAPACITY * size_of::<Block>() + align_of::<Block>();
        // Use SYSBRK to allocate extra data segment.
        let ptr = unsafe {
            sys::inc_brk(size).unwrap_or_else(|x| x.handle())
        };

        // Calculate the aligner.
        let aligner = aligner(*ptr, align_of::<Block>());

        // The alignment is used as precursor for our allocated block. This ensures that it is
        // properly memory aligned to the requested value.
        let alignment_block = Block {
            size: aligner,
            ptr: unsafe { Unique::new(*ptr) },
        };

        // Set the initial capacity.
        self.cap = INITIAL_CAPACITY;
        // Update the pointer.
        self.ptr = unsafe { Unique::new(*alignment_block.end() as *mut _) };

        // We have a stub in the end, which we will store as well.
        let stub = Block {
            size: align_of::<Block>() - aligner,
            ptr: Block {
                size: self.cap * size_of::<Block>(),
                ptr: alignment_block.end(),
            }.end(),
        };
        // Set the new segment end.
        self.seg_end = stub.end();

        // Add it to the list. This will not change the order, since the pointer is higher than all
        // the previous blocks.
        self.push(alignment_block);

        if stub.size != 0 {
            self.push(stub);
        }

        // Check consistency.
        self.check();
        debug_assert!(*self.ptr as usize % align_of::<Block>() == 0, "Alignment in `init` failed.");
        debug_assert!(self.iter().map(|x| x.size).sum::<usize>() + self.cap * size_of::<Block>() ==
                      size, "BRK memory leaked in `init`.");
    }

    /// *[See `Bookkeeper`'s respective method.](./struct.Bookkeeper.html#method.alloc)*
    ///
    /// # Example
    ///
    /// We start with our initial segment.
    ///
    /// ```notrust
    ///    Address space
    ///   I---------------------------------I
    /// B
    /// l
    /// k
    /// s
    /// ```
    ///
    /// We then split it at the [aligner](./fn.aligner.html), which is used for making sure that
    /// the pointer is aligned properly.
    ///
    /// ```notrust
    ///    Address space
    ///   I------I
    /// B   ^    I--------------------------I
    /// l  al
    /// k
    /// s
    /// ```
    ///
    /// We then use the remaining block, but leave the excessive space.
    ///
    /// ```notrust
    ///    Address space
    ///   I------I
    /// B                           I--------I
    /// l        \_________________/
    /// k        our allocated block.
    /// s
    /// ```
    ///
    /// The pointer to the marked area is then returned.
    fn alloc(&mut self, size: usize, align: usize) -> Unique<u8> {
        // This variable will keep block, we will return as allocated memory.
        let mut block = None;

        // We run right-to-left, since new blocks tend to get added to the right.
        for (n, i) in self.iter_mut().enumerate().rev() {
            let aligner = aligner(*i.ptr as *const _, align);

            if i.size >= size + aligner {
                // To catch dumb logic errors.
                debug_assert!(i.is_free(), "Block is not free (What the fuck, Richard?)");

                // Use this block as the one, we use for our allocation.
                block = Some((n, Block {
                    size: i.size,
                    ptr: unsafe { Unique::new((*i.ptr as usize + aligner) as *mut _) },
                }));

                // Leave the stub behind.
                if aligner == 0 {
                    // Since the stub is empty, we are not interested in keeping it marked as free.
                    i.set_free();
                } else {
                    i.size = aligner;
                }

                break;
            }
        }

        if let Some((n, b)) = block {
            if b.size != size {
                // Mark the excessive space as free.
                self.insert(n, Block {
                    size: b.size - size,
                    ptr: unsafe { Unique::new((*b.ptr as usize + size) as *mut _) },
                });
            }

            // Check consistency.
            self.check();
            debug_assert!(*b.ptr as usize % align == 0, "Alignment in `alloc` failed.");

            b.ptr
        } else {
            // No fitting block found. Allocate a new block.
            self.alloc_fresh(size, align)
        }
    }

    /// Push to the block vector.
    ///
    /// This will append a block entry to the end of the block vector. Make sure that this entry has
    /// a value higher than any of the elements in the list, to keep it sorted.
    fn push(&mut self, block: Block) {
        // Some assertions.
        debug_assert!(block.size != 0, "Pushing a zero sized block.");
        debug_assert!(self.last().map_or(0, |x| *x.ptr as usize) <= *block.ptr as usize, "The \
                      previous last block is higher than the new.");

        {
            let len = self.len;
            // This is guaranteed not to overflow, since `len` is bounded by the address space, since
            // each entry represent at minimum one byte, meaning that `len` is bounded by the address
            // space.
            self.reserve(len + 1);
        }

        unsafe {
            ptr::write((*self.ptr as usize + size_of::<Block>() * self.len) as *mut _, block);
        }

        self.len += 1;

        // Check consistency.
        self.check();
    }

    /// Find a block's index through binary search.
    ///
    /// If it fails, the value will be where the block could be inserted to keep the list sorted.
    fn search(&self, block: &Block) -> Result<usize, usize> {
        self.binary_search_by(|x| x.cmp(block))
    }

    /// Allocate _fresh_ space.
    ///
    /// "Fresh" means that the space is allocated through a BRK call to the kernel.
    ///
    /// The following guarantees are made:
    ///
    /// 1. The returned pointer is aligned to `align`.
    /// 2. The returned pointer points to a _valid buffer of size `size` (in bytes)_.
    /// 3. The returned pointer is equal to the old segment end, if the align is one.
    fn alloc_fresh(&mut self, size: usize, align: usize) -> Unique<u8> {
        // Calculate the canonical size (extra space is allocated to limit the number of system calls).
        let can_size = canonicalize_brk(size);
        let brk_size = can_size.checked_add(align).unwrap_or_else(|| sys::oom());
        // Use SYSBRK to allocate extra data segment.
        let ptr = unsafe {
            sys::inc_brk(brk_size).unwrap_or_else(|x| x.handle())
        };

        // Calculate the aligner.
        let aligner = aligner(*ptr, align);

        // The alignment is used as precursor for our allocated block. This ensures that it is
        // properly memory aligned to the requested value.
        let alignment_block = Block {
            size: aligner,
            ptr: ptr,
        };
        let res = Block {
            size: size,
            ptr: alignment_block.end(),
        };

        // Calculate the excessive space.
        let excessive = Block {
            // This won't overflow, since `can_size` is bounded by `size`
            size: can_size - size,
            ptr: res.end(),
        };

        // Make some assertions.
        debug_assert!(*res.ptr as usize % align == 0, "Alignment in `alloc_fresh` failed.");
        debug_assert!(res.size + alignment_block.size + excessive.size == brk_size, "BRK memory \
                      leak in fresh allocation.");

        // Set the segment end.
        self.seg_end = excessive.end();

        // Add it to the list. This will not change the order, since the pointer is higher than all
        // the previous blocks.
        self.push(alignment_block);

        // Push the excessive space to the end of the block vector.
        self.push(excessive);

        // Check consistency.
        self.check();

        res.ptr
    }

    /// Reallocate inplace.
    ///
    /// This will try to reallocate a buffer inplace, meaning that the buffers length is merely
    /// extended, and not copied to a new buffer.
    ///
    /// This _won't_ shrink the block.
    ///
    /// Returns `Err(())` if the buffer extension couldn't be done, `Err(())` otherwise.
    ///
    /// The following guarantees are made:
    ///
    /// 1. If this function returns `Ok(())`, it is valid to read and write within the bound of the
    ///    new size.
    /// 2. No changes are made to the allocated buffer itself.
    /// 3. On failure, the state of the allocator is left unmodified.
    fn realloc_inplace(&mut self, ind: usize, block: &Block, new_size: usize) -> Result<(), ()> {
        // Make sure that invariants aren't broken.
        debug_assert!(new_size > block.size, "`realloc_inplace` cannot be used for shrinking!");

        let res;

        // Evil hack to emulate post-function hooks.
        // TODO make this more idiomatic.
        loop {
            {
                // We check if `ind` is the end of the array.
                if let Some(entry) = self.get_mut(ind + 1) {
                    // Note that we are sure that no segments in the array are adjacent (unless they have size
                    // 0). This way we know that we will, at maximum, need one and only one block for extending
                    // the current block.
                    if block.left_to(*entry.ptr) && entry.size + block.size >= new_size {
                        // There is space for inplace reallocation.

                        // Set the following block.
                        entry.size -= new_size - block.size;
                        // We now move the block to the new appropriate place.
                        entry.ptr = entry.end();

                        res = Ok(());
                        break;
                    } else { return Err(()) }
                }
            }

            // We are in the left outermost index, therefore we can extend the segment to the
            // right.
            if block.left_to(*self.seg_end) {
                // We make a fresh allocation (BRK), since we are in the end of the segment, and
                // thus an extension will simply extend our buffer.
                let ptr = self.alloc_fresh(new_size - block.size, 1);

                // Check consistency.
                debug_assert!(block.left_to(*ptr));

                res = Ok(());
                break;
            } else { return Err(()) }
        }

        // Run a consistency check.
        self.check();

        res
    }

    /// *[See `Bookkeeper`'s respective method.](./struct.Bookkeeper.html#method.realloc)*
    ///
    /// Example
    /// =======
    ///
    /// We will first try to perform an in-place reallocation, and if that fails, we will use
    /// memmove.
    ///
    /// ```notrust
    ///    Address space
    ///   I------I
    /// B \~~~~~~~~~~~~~~~~~~~~~/
    /// l     needed
    /// k
    /// s
    /// ```
    ///
    /// We simply find the block next to our initial block. If this block is free and have
    /// sufficient size, we will simply merge it into our initial block, and leave the excessive
    /// space as free. If these conditions are not met, we have to allocate a new list, and then
    /// deallocate the old one, after which we use memmove to copy the data over to the newly
    /// allocated list.
    fn realloc(&mut self, block: Block, new_size: usize, align: usize) -> Unique<u8> {
        if new_size <= block.size {
            // Shrink the block.

            let ind = self.find(&block);
            self.free_ind(ind, Block {
                size: new_size - block.size,
                ptr: unsafe { Unique::new((*block.ptr as usize + new_size) as *mut u8) },
            });

            debug_assert!(self[self.find(&block)].size == new_size, "Block wasn't shrinked properly.");
            block.ptr
        } else if {
            // Try to do an inplace reallocation.
            let ind = self.find(&block);
            self.realloc_inplace(ind, &block, new_size).is_ok()
        } {
            block.ptr
        } else {
            // Reallocation cannot be done inplace.

            // Allocate a new block with the same size.
            let ptr = self.alloc(new_size, align);

            // Copy the old data to the new location.
            unsafe { ptr::copy(*block.ptr, *ptr, block.size); }

            // Free the old block.
            self.free(block);

            // Check consistency.
            self.check();
            debug_assert!(*ptr as usize % align == 0, "Alignment in `realloc` failed.");

            ptr
        }
    }

    /// Reserve space for the block vector.
    ///
    /// This will extend the capacity to a number greater than or equals to `needed`, potentially
    /// reallocating the block vector.
    fn reserve(&mut self, needed: usize) {
        /* TODO remove this.
        if needed > self.cap {
            // Set the new capacity.
            self.cap = cmp::max(30, self.cap.saturating_mul(2));

            // Reallocate the block vector.
            self.ptr = unsafe {
                let block = Block {
                    ptr: Unique::new(*self.ptr as *mut _),
                    size: self.cap,
                };

                let cap = self.cap;
                Unique::new(*self.realloc(block, cap, align_of::<Block>()) as *mut _)
            };

            // Check consistency.
            self.check();
        }
        */

        // Initialize if necessary.
        if *self.ptr == EMPTY_HEAP as *mut _ { self.init() }

        if needed > self.cap {
            let block = Block {
                ptr: unsafe { Unique::new(*self.ptr as *mut _) },
                size: self.cap,
            };
            let ind = self.find(&block);
            // TODO allow BRK-free non-inplace reservations.

            // Reallocate the block vector.

            // We first try inplace.
            if self.realloc_inplace(ind, &block, needed).is_ok() {
                self.cap = needed;
            } else {
                // Inplace alloc failed, so we have to BRK some new space.

                let old_ptr = *self.ptr;

                // Make a fresh allocation.
                self.cap = needed.saturating_add(cmp::min(self.cap, 200 + self.cap / 2));
                unsafe {
                    let cap = self.cap;
                    self.ptr = Unique::new(*self.alloc_fresh(cap, align_of::<Block>()) as *mut _);

                    // Copy the content.
                    ptr::copy_nonoverlapping(old_ptr as *const _, *self.ptr, self.len);
                }
            }

            // Check consistency.
            self.check();
        }
    }

    /// Perform a binary search to find the appropriate place where the block can be insert or is
    /// located.
    fn find(&self, block: &Block) -> usize {
        match self.search(block) {
            Ok(x) => x,
            Err(x) => x,
        }
    }

    /// *[See `Bookkeeper`'s respective method.](./struct.Bookkeeper.html#method.free)*
    ///
    /// # Example
    ///
    /// ```notrust
    ///    Address space
    ///   I------I
    /// B                                  I--------I
    /// l        \_________________/
    /// k     the used block we want to deallocate.
    /// s
    /// ```
    ///
    /// If the blocks are adjacent, we merge them:
    ///
    /// ```notrust
    ///    Address space
    ///   I------I
    /// B        I-----------------I
    /// l                                  I--------I
    /// k
    /// s
    /// ```
    ///
    /// This gives us:
    ///
    /// ```notrust
    ///    Address space
    ///   I------------------------I
    /// B                                  I--------I
    /// l
    /// k
    /// s
    /// ```
    ///
    /// And we're done. If it cannot be done, we insert the block, while keeping the list sorted.
    /// See [`insert`](#method.insert) for details.
    fn free(&mut self, block: Block) {
        let ind = self.find(&block);
        self.free_ind(ind, block);
    }

    /// Free a block placed on some index.
    ///
    /// See [`free`](#method.free) for more information.
    fn free_ind(&mut self, ind: usize, block: Block) {
        // We use loops as an evil hack to make local returns.
        // TODO: do this in a better way.
        loop {
            {
                let len = self.len;
                let entry = &mut self[ind];

                // Make some handy assertions.
                debug_assert!(*entry.ptr != *block.ptr || !entry.is_free(), "Double free.");

                // Try to merge right.
                if entry.is_free() && ind + 1 < len && entry.left_to(*block.ptr) {
                    entry.size += block.size;
                    break;
                }
            }

            if ind != 0 {
                let prev_entry = &mut self[ind - 1];
                // Try to merge left. Note that `entry` is not free, by the conditional above.
                if prev_entry.is_free() && prev_entry.left_to(*block.ptr) {
                    prev_entry.size += block.size;
                    break;
                }
            }

            // We will have to insert it in a normal manner.
            self.insert(ind, block);
            break;
        }

        // Check consistency.
        self.check();
    }

    /// Insert a block entry at some index.
    ///
    /// If the space is non-empty, the elements will be pushed filling out the empty gaps to the
    /// right. If all places to the right is occupied, it will reserve additional space to the
    /// block vector.
    ///
    /// # Example
    /// We want to insert the block denoted by the tildes into our list. Perform a binary search to
    /// find where insertion is appropriate.
    ///
    /// ```notrust
    ///    Address space
    ///   I------I
    /// B < here                      I--------I
    /// l                                              I------------I
    /// k
    /// s                                                             I---I
    ///                  I~~~~~~~~~~I
    /// ```
    ///
    /// We keep pushing the blocks to the right to the next entry until a empty entry is reached:
    ///
    /// ```notrust
    ///    Address space
    ///   I------I
    /// B < here                      I--------I <~ this one cannot move down, due to being blocked.
    /// l
    /// k                                              I------------I <~ thus we have moved this one down.
    /// s                                                             I---I
    ///              I~~~~~~~~~~I
    /// ```
    ///
    /// Repeating yields:
    ///
    /// ```notrust
    ///    Address space
    ///   I------I
    /// B < here
    /// l                             I--------I <~ this one cannot move down, due to being blocked.
    /// k                                              I------------I <~ thus we have moved this one down.
    /// s                                                             I---I
    ///              I~~~~~~~~~~I
    /// ```
    ///
    /// Now an empty space is left out, meaning that we can insert the block:
    ///
    /// ```notrust
    ///    Address space
    ///   I------I
    /// B            I----------I
    /// l                             I--------I
    /// k                                              I------------I
    /// s                                                             I---I
    /// ```
    ///
    /// The insertion is now completed.
    fn insert(&mut self, ind: usize, block: Block) {
        // Some assertions...
        debug_assert!(block >= self[ind.saturating_sub(1)], "Inserting at {} will make the list \
                      unsorted.", ind);
        debug_assert!(self.find(&block) == ind, "Block is not inserted at the appropriate index.");

        // TODO consider moving right before searching left.

        // Find the next gap, where a used block were.
        let n = self.iter()
            .skip(ind)
            .enumerate()
            .filter(|&(_, x)| x.is_free())
            .next().map(|x| x.0)
            .unwrap_or_else(|| {
                let len = self.len;

                // No gap was found, so we need to reserve space for new elements.
                self.reserve(len + 1);
                // Increment the length, since a gap haven't been found.
                self.len += 1;
                len
            });

        // Memmove the blocks to close in that gap.
        unsafe {
            ptr::copy(self[ind..].as_ptr(), self[ind + 1..].as_mut_ptr(), self.len - n);
        }

        // Check that the inserted block doesn't overlap the following ones.
        debug_assert!(*block.end() <= *self[ind + 1].ptr, "The inserted block overlaps with the \
                      following blocks.");

        // Place the block left to the moved line.
        self[ind] = block;

        // Check consistency.
        self.check();
    }

    /// No-op in release mode.
    #[cfg(not(debug_assertions))]
    fn check(&self) {}

    /// Perform consistency checks.
    ///
    /// This will check for the following conditions:
    ///
    /// 1. The list is sorted.
    /// 2. No entries are overlapping.
    /// 3. The length does not exceed the capacity.
    #[cfg(debug_assertions)]
    fn check(&self) {
        if let Some(x) = self.first() {
            let mut prev = *x.ptr;
            let mut end = *x.ptr;
            for (n, i) in self.iter().enumerate().skip(1) {
                // Check if sorted.
                assert!(*i.ptr >= prev, "The block vector is not sorted at index, {}: 0x{:x} ≤ \
                        0x{:x}.", n, *i.ptr as usize, prev as usize);
                // Check if overlapping.
                assert!(*i.ptr > end || i.is_free() && *i.ptr == end, "Two blocks are \
                        overlapping/adjacent at index, {}.", n);
                // Check if bounded by seg_end
                assert!(*i.end() <= *self.seg_end, "The {}th element in the block list is placed \
                        outside the segment.", n);
                prev = *i.ptr;
                end = *i.end();
            }

            // Check that the length is lower than or equals to the capacity.
            assert!(self.len <= self.cap, "The capacity does not cover the length.");
        }
    }

    /// Dump the contents into a format writer.
    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    fn dump<W: fmt::Write>(&self, mut fmt: W) {
        writeln!(fmt, "len: {}", self.len).unwrap();
        writeln!(fmt, "cap: {}", self.cap).unwrap();
        writeln!(fmt, "content:").unwrap();
        for i in &**self {
            writeln!(fmt, "  - {:x} .. {}", *i.ptr as usize, i.size).unwrap();
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use block::Block;

    use core::ptr;

    #[test]
    fn test_alloc() {
        let mut bk = Bookkeeper::new();
        let mem = bk.alloc(1000, 4);

        unsafe {
            ptr::write(*mem as *mut _, [1u8; 1000]);
        }

        bk.free(Block {
            size: 1000,
            ptr: mem,
        });
    }
}

impl ops::Deref for BlockVec {
    type Target = [Block];

    fn deref(&self) -> &[Block] {
        unsafe {
            slice::from_raw_parts(*self.ptr as *const _, self.len)
        }
    }
}
impl ops::DerefMut for BlockVec {
    fn deref_mut(&mut self) -> &mut [Block] {
        unsafe {
            slice::from_raw_parts_mut(*self.ptr, self.len)
        }
    }
}

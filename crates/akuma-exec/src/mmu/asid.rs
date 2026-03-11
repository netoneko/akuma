//! ASID (Address Space Identifier) allocator.
//!
//! Pure bit manipulation - fully host-testable with no architecture dependencies.

const MAX_ASID: u16 = 256;

pub struct AsidAllocator {
    next_asid: u16,
    used: [u64; 4],
}

impl AsidAllocator {
    pub const fn new() -> Self {
        Self {
            next_asid: 1,
            used: [0; 4],
        }
    }

    pub fn alloc(&mut self) -> Option<u16> {
        let start = self.next_asid;
        let mut asid = start;
        loop {
            let word = (asid / 64) as usize;
            let bit = asid % 64;
            if word < self.used.len() && (self.used[word] & (1 << bit)) == 0 {
                self.used[word] |= 1 << bit;
                self.next_asid = if asid + 1 >= MAX_ASID { 1 } else { asid + 1 };
                return Some(asid);
            }
            asid = if asid + 1 >= MAX_ASID { 1 } else { asid + 1 };
            if asid == start { return None; }
        }
    }

    pub fn free(&mut self, asid: u16) {
        if asid > 0 && asid < MAX_ASID {
            let word = (asid / 64) as usize;
            let bit = asid % 64;
            if word < self.used.len() { self.used[word] &= !(1 << bit); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_alloc_returns_one() {
        let mut alloc = AsidAllocator::new();
        assert_eq!(alloc.alloc(), Some(1));
    }

    #[test]
    fn sequential_allocations_unique() {
        let mut alloc = AsidAllocator::new();
        let mut seen = [false; 256];
        for _ in 0..10 {
            let asid = alloc.alloc().unwrap();
            assert!(!seen[asid as usize], "ASID {} allocated twice", asid);
            seen[asid as usize] = true;
        }
    }

    #[test]
    fn free_makes_asid_available() {
        let mut alloc = AsidAllocator::new();
        for _ in 0..255 {
            alloc.alloc().unwrap();
        }
        assert_eq!(alloc.alloc(), None);
        alloc.free(100);
        assert_eq!(alloc.alloc(), Some(100));
    }

    #[test]
    fn allocate_all_then_none() {
        let mut alloc = AsidAllocator::new();
        let mut asids = alloc::vec::Vec::new();
        for _ in 0..255 {
            asids.push(alloc.alloc().unwrap());
        }
        assert_eq!(alloc.alloc(), None);
    }

    #[test]
    fn free_then_realloc_returns_freed() {
        let mut alloc = AsidAllocator::new();
        for _ in 0..255 {
            alloc.alloc().unwrap();
        }
        alloc.free(50);
        alloc.free(100);
        let a = alloc.alloc().unwrap();
        let b = alloc.alloc().unwrap();
        assert!((a == 50 && b == 100) || (a == 100 && b == 50));
        assert_eq!(alloc.alloc(), None);
    }
}

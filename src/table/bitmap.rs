//! A packed, growable bit vector — a column's validity and presence
//! (`plans/columnar.org` D-C2), and a `Bool` column's values.

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Bitmap {
    words: Vec<u64>,
    len: usize,
}

impl Bitmap {
    /// `len` bits, every one `bit`.
    pub(crate) fn filled(len: usize, bit: bool) -> Bitmap {
        let fill = if bit { u64::MAX } else { 0 };
        let mut words = vec![fill; len.div_ceil(64)];
        // Bits past `len` stay clear, so two bitmaps of equal bits compare equal.
        if bit && len % 64 != 0 {
            *words.last_mut().unwrap() = (1u64 << (len % 64)) - 1;
        }
        Bitmap { words, len }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn push(&mut self, bit: bool) {
        if self.len % 64 == 0 {
            self.words.push(0);
        }
        if bit {
            self.words[self.len / 64] |= 1 << (self.len % 64);
        }
        self.len += 1;
    }

    pub(crate) fn get(&self, i: usize) -> bool {
        debug_assert!(i < self.len);
        self.words[i / 64] >> (i % 64) & 1 == 1
    }

    pub(crate) fn set(&mut self, i: usize, bit: bool) {
        debug_assert!(i < self.len);
        let mask = 1 << (i % 64);
        match bit {
            true => self.words[i / 64] |= mask,
            false => self.words[i / 64] &= !mask,
        }
    }

    /// Whether some bit set here is `bit` in `other`, which is as long.
    pub(crate) fn any_where(&self, other: &Bitmap, bit: bool) -> bool {
        debug_assert_eq!(self.len, other.len);
        // Bits past `len` are clear here, so they never match.
        self.words.iter().zip(&other.words).any(|(&a, &b)| a & if bit { b } else { !b } != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushed_and_filled_bits_read_back() {
        let mut b = Bitmap::default();
        for i in 0..130 {
            b.push(i % 3 == 0);
        }
        assert_eq!(b.len(), 130);
        assert!((0..130).all(|i| b.get(i) == (i % 3 == 0)));

        let full = Bitmap::filled(70, true);
        assert!((0..70).all(|i| full.get(i)));
        let mut grown = full.clone();
        grown.push(false);
        assert!(!grown.get(70));
        assert!((0..70).all(|i| !Bitmap::filled(70, false).get(i)));
    }

    #[test]
    fn equal_bits_compare_equal_however_built() {
        let mut pushed = Bitmap::default();
        for _ in 0..70 {
            pushed.push(true);
        }
        assert_eq!(pushed, Bitmap::filled(70, true));
    }
}

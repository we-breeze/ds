//! Strings that stay inline while they fit and spill to the heap beyond that.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::mem::size_of;
use std::ops::Deref;
use std::str;

/// Content bytes held inline before a value spills to the heap.
///
/// The inline arm is one byte wider than this, holding the length, and the
/// discriminant brings the whole value to 64 bytes: one cache line.
const INLINE_BYTES: usize = 62;

/// A string that stays inline while it fits and spills to the heap past
/// [`SmolStr::INLINE_CAPACITY`] bytes, one 64-byte cache line wide whether or
/// not it has spilled.
///
/// Nothing is ever truncated, so a caller that needs a bounded field bounds it
/// itself. The inline arm copies its bytes rather than borrowing them, so a
/// value outlives the buffer it was read from.
///
/// Equality, ordering, and hashing compare the text and not the
/// representation, so the same content compares and hashes identically whether
/// it is inline or on the heap.
pub struct SmolStr(Repr);

enum Repr {
    Inline { bytes: [u8; INLINE_BYTES], len: u8 },
    Heap(String),
}

/// The layout is a promise to consumers, which size their own frames around it,
/// rather than an implementation detail: one 64-byte cache line.
const _: () = assert!(
    size_of::<SmolStr>() == 64,
    "SmolStr must stay one cache line wide; adjust INLINE_BYTES if a compiler \
     release changed the representation"
);

impl SmolStr {
    /// Content bytes stored inline before a value spills to the heap.
    pub const INLINE_CAPACITY: usize = INLINE_BYTES;

    /// Creates an empty value, which holds no allocation.
    #[must_use]
    pub const fn new() -> Self {
        Self(Repr::Inline {
            bytes: [0; INLINE_BYTES],
            len: 0,
        })
    }

    /// Borrows the text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match &self.0 {
            Repr::Inline { bytes, len } => {
                // Only whole `&str` and `char` encodings are ever copied in, so
                // this prefix is always valid UTF-8 and needs no unsafe to hold.
                str::from_utf8(&bytes[..usize::from(*len)]).expect("inline value holds UTF-8")
            }
            Repr::Heap(heap) => heap.as_str(),
        }
    }

    /// Whether the value has spilled to the heap, named after
    /// [`EphemeralBytes::is_heap_allocated`](crate::EphemeralBytes::is_heap_allocated).
    #[must_use]
    pub const fn is_heap_allocated(&self) -> bool {
        matches!(self.0, Repr::Heap(_))
    }

    /// Length of the text in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.0 {
            Repr::Inline { len, .. } => usize::from(*len),
            Repr::Heap(heap) => heap.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends `value`, moving the bytes already held to the heap when they no
    /// longer fit inline. The write that spills is served by one allocation.
    pub fn push_str(&mut self, value: &str) {
        if let Repr::Heap(heap) = &mut self.0 {
            heap.push_str(value);
            return;
        }
        if !self.push_inline(value) {
            self.spill(value);
        }
    }

    /// Appends one character, spilling like [`push_str`](Self::push_str).
    ///
    /// A character is never split across the boundary: one that crosses it
    /// spills the value rather than writing a partial encoding.
    pub fn push_char(&mut self, value: char) {
        let mut scratch = [0; 4];
        self.push_str(value.encode_utf8(&mut scratch));
    }

    /// Empties the value, keeping any heap allocation for reuse.
    pub fn clear(&mut self) {
        match &mut self.0 {
            Repr::Inline { len, .. } => *len = 0,
            Repr::Heap(heap) => heap.clear(),
        }
    }

    /// Returns `false` when `value` does not fit inline, leaving none of it
    /// behind.
    fn push_inline(&mut self, value: &str) -> bool {
        let Repr::Inline { bytes, len } = &mut self.0 else {
            return false;
        };
        let start = usize::from(*len);
        let Some(end) = start
            .checked_add(value.len())
            .filter(|end| *end <= INLINE_BYTES)
        else {
            return false;
        };
        bytes[start..end].copy_from_slice(value.as_bytes());
        *len = end as u8;
        true
    }

    /// Moves the bytes held inline to the heap so writing can continue.
    fn spill(&mut self, value: &str) {
        let Repr::Inline { bytes, len } = &self.0 else {
            return;
        };
        let len = usize::from(*len);
        // Sized for exactly this write; later writes grow the `String`.
        let mut heap = String::with_capacity(len + value.len());
        heap.push_str(str::from_utf8(&bytes[..len]).expect("inline value holds UTF-8"));
        heap.push_str(value);
        self.0 = Repr::Heap(heap);
    }
}

impl Default for SmolStr {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for SmolStr {
    fn clone(&self) -> Self {
        match &self.0 {
            Repr::Inline { bytes, len } => Self(Repr::Inline {
                bytes: *bytes,
                len: *len,
            }),
            Repr::Heap(heap) => Self(Repr::Heap(heap.clone())),
        }
    }
}

impl From<&str> for SmolStr {
    fn from(value: &str) -> Self {
        let mut out = Self::new();
        out.push_str(value);
        out
    }
}

impl From<String> for SmolStr {
    fn from(value: String) -> Self {
        if value.len() > INLINE_BYTES {
            // Already on the heap and already the right shape: move it.
            return Self(Repr::Heap(value));
        }
        Self::from(value.as_str())
    }
}

impl From<&String> for SmolStr {
    fn from(value: &String) -> Self {
        Self::from(value.as_str())
    }
}

impl Deref for SmolStr {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for SmolStr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Lets a `HashMap<SmolStr, _>` be read with a `&str` key.
impl Borrow<str> for SmolStr {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for SmolStr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), formatter)
    }
}

impl fmt::Debug for SmolStr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), formatter)
    }
}

/// Never fails: text that does not fit inline spills to the heap.
impl fmt::Write for SmolStr {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.push_str(value);
        Ok(())
    }
}

impl PartialEq for SmolStr {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for SmolStr {}

impl PartialEq<str> for SmolStr {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for SmolStr {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// Hashing the representation instead would place equal inline and heap values
/// in different buckets.
impl Hash for SmolStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialOrd for SmolStr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SmolStr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::SmolStr;
    use std::collections::HashMap;
    use std::collections::hash_map::DefaultHasher;
    use std::fmt::Write;
    use std::hash::{Hash, Hasher};
    use std::mem::size_of;

    const CAPACITY: usize = SmolStr::INLINE_CAPACITY;

    fn text(len: usize) -> String {
        "x".repeat(len)
    }

    fn digest(value: &impl Hash) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn the_value_is_one_cache_line_wide() {
        assert_eq!(
            SmolStr::INLINE_CAPACITY,
            62,
            "the inline arm carries a length byte"
        );
        assert_eq!(size_of::<SmolStr>(), 64, "one cache line");
    }

    #[test]
    fn values_are_inline_until_they_pass_the_capacity() {
        for len in [0, 1, CAPACITY - 1, CAPACITY] {
            let value = SmolStr::from(text(len).as_str());
            assert!(!value.is_heap_allocated(), "{len} bytes must stay inline");
            assert_eq!(value.as_str().len(), len);
        }

        let value = SmolStr::from(text(CAPACITY + 1).as_str());
        assert!(value.is_heap_allocated());
        assert_eq!(value.len(), CAPACITY + 1);
    }

    #[test]
    fn a_character_is_never_split_across_the_boundary() {
        let mut value = SmolStr::from(text(CAPACITY - 1).as_str());
        // Two bytes, and only one byte of room: the value spills whole.
        value.push_char('é');
        assert!(value.is_heap_allocated());

        let mut value = SmolStr::from(text(CAPACITY - 2).as_str());
        value.push_char('é');
        assert!(!value.is_heap_allocated());
        assert_eq!(value.as_str().chars().next_back(), Some('é'));
    }

    #[test]
    fn writing_past_the_capacity_keeps_every_byte() {
        let mut value = SmolStr::new();
        for _ in 0..40 {
            value.push_str("abcd");
        }
        assert!(value.is_heap_allocated());
        assert_eq!(value.as_str().len(), 160);
        assert_eq!(value.as_str(), "abcd".repeat(40));
    }

    #[test]
    fn write_never_fails_and_extends_a_spilled_value() {
        let mut value = SmolStr::from(text(CAPACITY + 4).as_str());
        write!(value, "-{}", 7).expect("writing into a spilled value");
        assert!(value.is_heap_allocated());
        assert!(value.as_str().ends_with("-7"));
    }

    #[test]
    fn equal_content_compares_and_hashes_the_same_in_both_arms() {
        let short = SmolStr::from("abc");
        let long = SmolStr::from(text(CAPACITY + 8).as_str());
        let long_again = SmolStr::from(long.as_str().to_owned());

        assert_eq!(short, "abc");
        assert_eq!(long, long_again);
        assert_eq!(digest(&long), digest(&long_again));
        assert_eq!(digest(&short), digest(&SmolStr::from("abc")));

        let mut map = HashMap::new();
        map.insert(long.clone(), 1);
        assert_eq!(map.get(long.as_str()), Some(&1));
    }

    #[test]
    fn clone_and_clear_keep_the_text_that_is_expected() {
        let mut value = SmolStr::from(text(CAPACITY + 8).as_str());
        let clone = value.clone();
        assert_eq!(value, clone);

        value.clear();
        assert!(value.is_empty());
        assert!(value.is_heap_allocated(), "clear keeps the allocation");

        let mut inline = SmolStr::from("abc");
        inline.clear();
        assert!(inline.is_empty());
        assert!(!inline.is_heap_allocated());
    }

    #[test]
    fn a_long_string_is_moved_into_the_heap_instead_of_copied() {
        let owned = text(CAPACITY + 1);
        let address = owned.as_ptr();
        let value = SmolStr::from(owned);
        assert!(value.is_heap_allocated());
        assert_eq!(value.as_str().as_ptr(), address, "the buffer is reused");
    }

    #[test]
    fn ordering_follows_the_text() {
        let mut values = [
            SmolStr::from(text(CAPACITY + 2).as_str()),
            SmolStr::from("abc"),
            SmolStr::from("abd"),
        ];
        values.sort();
        assert_eq!(values[0], "abc");
        assert_eq!(values[1], "abd");
    }
}

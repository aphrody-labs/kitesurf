//! Versioned node identifier shared between the Blitz crates.

/// A versioned identifier for a node in a Blitz DOM tree.
///
/// A `NodeId` packs a 32-bit slot index (low bits) and a 32-bit version
/// (high bits). Node storage bumps a slot's version when the slot is
/// reused, so ids referring to a dropped node no longer resolve (lookups
/// return `None` rather than aliasing the new node occupying the slot).
///
/// The `Default` value is a null id which never resolves to a node.
// Two halves rather than a u64 keep the id 4-byte aligned on 32-bit targets, so structs
// holding ids (every DOM node) carry no alignment padding.
#[derive(Copy, Clone, Default, Eq, PartialEq)]
pub struct NodeId {
    index: u32,
    version: u32,
}

impl NodeId {
    /// Convert this id to its raw `u64` representation (index + version).
    ///
    /// The value round-trips through [`NodeId::from_u64`]. This is useful for
    /// interop with APIs which use integer ids (e.g. Taffy or AccessKit).
    #[inline(always)]
    pub fn as_u64(self) -> u64 {
        u64::from(self.version) << 32 | u64::from(self.index)
    }

    /// Reconstruct a `NodeId` from the raw `u64` representation produced by
    /// [`NodeId::as_u64`].
    ///
    /// Passing a value that did not come from `as_u64` will produce an id
    /// which fails to resolve (it will not alias an unrelated live node).
    #[inline(always)]
    pub fn from_u64(raw: u64) -> Self {
        Self {
            index: raw as u32,
            version: (raw >> 32) as u32,
        }
    }

    /// The slot index part of this id.
    #[inline(always)]
    fn index(self) -> u32 {
        self.index
    }

    /// The version part of this id.
    #[inline(always)]
    fn version(self) -> u32 {
        self.version
    }
}

/// Hashes the raw `u64` rather than the two halves, so hash values and map iteration order do
/// not depend on the representation.
impl std::hash::Hash for NodeId {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_u64().hash(state);
    }
}

/// Ordered by slot index first, then by version, so that ordering roughly
/// matches node creation order (as with the previous `Slab`-backed storage)
/// rather than being dominated by the version bits.
impl Ord for NodeId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.index(), self.version()).cmp(&(other.index(), other.version()))
    }
}

impl PartialOrd for NodeId {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Debug for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NodeId({}v{})", self.index(), self.version())
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[cfg(test)]
mod tests {
    use super::NodeId;
    use std::hash::{BuildHasher, RandomState};

    #[test]
    fn halves_round_trip_through_the_raw_u64() {
        let raw = 7 << 32 | 9;
        let id = NodeId::from_u64(raw);

        assert_eq!(id.as_u64(), raw);
        assert_eq!((id.index(), id.version()), (9, 7));
        assert_eq!(format!("{id}"), "NodeId(9v7)");
    }

    #[test]
    fn is_aligned_like_a_u32() {
        assert_eq!(align_of::<NodeId>(), align_of::<u32>());
    }

    #[test]
    fn hashes_like_the_raw_u64() {
        let raw = 7 << 32 | 9;
        let hasher = RandomState::new();

        assert_eq!(hasher.hash_one(NodeId::from_u64(raw)), hasher.hash_one(raw));
    }

    #[test]
    fn orders_by_index_before_version() {
        assert!(NodeId::from_u64(2 << 32 | 1) < NodeId::from_u64(1 << 32 | 2));
    }
}

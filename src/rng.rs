//! A seeded PRNG, so that label assignment and session ids are reproducible
//! from a run id.
//!
//! magi needs exactly two random things — which candidate gets which label, and
//! a UUID for each Claude session — and both must be replayable when a run is
//! resumed. A 20-line SplitMix64 covers that without a dependency whose version
//! could change the shuffle out from under a resumed run.

/// SplitMix64. Fixed algorithm: the sequence for a given seed is part of magi's
/// on-disk contract, because a resumed run must recompute the same labels.
#[derive(Debug, Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// Seed the generator.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Seed from an arbitrary string (FNV-1a), for per-seat derivation.
    pub fn from_key(s: &str) -> Self {
        Self::new(fnv1a(s))
    }

    /// Next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform-enough value in `0..n`. `n` must be non-zero.
    pub fn below(&mut self, n: usize) -> usize {
        debug_assert!(n > 0);
        (self.next_u64() % n as u64) as usize
    }

    /// Fisher-Yates, in place.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }

    /// A syntactically valid RFC 4122 version 4 UUID.
    ///
    /// `claude --session-id` rejects anything else, and minting the id
    /// ourselves means the first turn and every resume agree on it without
    /// having to parse it back out of the CLI's output.
    pub fn uuid_v4(&mut self) -> String {
        let mut b = [0u8; 16];
        for chunk in b.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        let h = |r: &[u8]| r.iter().map(|x| format!("{x:02x}")).collect::<String>();
        format!(
            "{}-{}-{}-{}-{}",
            h(&b[0..4]),
            h(&b[4..6]),
            h(&b[6..8]),
            h(&b[8..10]),
            h(&b[10..16])
        )
    }
}

/// FNV-1a over the bytes of `s`.
pub fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.as_bytes() {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Entropy for a fresh run id: wall clock nanoseconds mixed with the pid.
pub fn entropy() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut r = SplitMix64::new(nanos ^ (u64::from(std::process::id()) << 32));
    r.next_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_shuffle() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        let mut xs = [0, 1, 2, 3, 4, 5, 6, 7];
        let mut ys = xs;
        a.shuffle(&mut xs);
        b.shuffle(&mut ys);
        assert_eq!(xs, ys);
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut r = SplitMix64::new(7);
        let mut xs: Vec<usize> = (0..64).collect();
        r.shuffle(&mut xs);
        let mut sorted = xs.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..64).collect::<Vec<_>>());
        assert_ne!(xs, sorted, "a 64-element shuffle should move something");
    }

    #[test]
    fn shuffle_handles_degenerate_lengths() {
        let mut r = SplitMix64::new(1);
        let mut empty: [u8; 0] = [];
        r.shuffle(&mut empty);
        let mut one = [9];
        r.shuffle(&mut one);
        assert_eq!(one, [9]);
    }

    #[test]
    fn uuid_v4_shape_and_variant_bits() {
        let mut r = SplitMix64::new(99);
        let id = r.uuid_v4();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_eq!(&parts[2][..1], "4", "version nibble");
        assert!(
            matches!(&parts[3][..1], "8" | "9" | "a" | "b"),
            "variant nibble: {id}"
        );
        assert_ne!(id, SplitMix64::new(100).uuid_v4());
    }

    #[test]
    fn seat_seeds_differ_per_seat() {
        assert_ne!(
            SplitMix64::from_key("judge-1").uuid_v4(),
            SplitMix64::from_key("judge-2").uuid_v4()
        );
    }
}

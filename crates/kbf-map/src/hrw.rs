//! The weighted rendezvous score: which member wins a digest, and by how much.
//!
//! Rendezvous (highest-random-weight) hashing is Thaler and Ravishankar, "Using
//! name-based mappings to increase hit rates" (IEEE/ACM Transactions on Networking,
//! 1998). The weighted form scores a member `-weight / ln(u)`, where `u` is a hash of
//! (digest, member) read as a number in (0, 1); the member with the highest score wins.
//! `-ln(u) / weight` is an exponential variable with rate `weight`, and the member with
//! the smallest one wins with probability `weight / total weight`; this is the form in
//! Resch, "Stateless Distributed Object Location" (2015) and the Wikipedia article
//! "Rendezvous hashing". A member's score for a digest does not depend on any other
//! member, which is why removing a member moves only the digests it won.
//!
//! The ranking must not change across builds or platforms, so nothing here is left to
//! the host: the hash is SipHash-2-4 with the fixed keys below over a byte string this
//! module lays out, and the logarithm is [`neg_ln`], built from `+ - * /` on `f64`,
//! which IEEE 754 defines exactly, not `f64::ln`, whose last bit varies between math
//! libraries. Rust does not fuse a multiply and an add.

use std::hash::Hasher;

use kbf_types::Digest;
use siphasher::sip::SipHasher24;

use crate::MemberId;

/// The keys of the hash, spelled as text: changing one bit moves every owner list.
const KEY0: u64 = u64::from_be_bytes(*b"kbf-map1");
const KEY1: u64 = u64::from_be_bytes(*b"rendezvo");

/// SipHash-2-4 over the digest's hash (32 bytes), its size (8, little endian) and the
/// member id (8, little endian). The size is part of the key because REAPI makes it part
/// of the digest; the hash function is not, as SHA-256 is the only one.
pub(crate) fn hash(digest: &Digest, id: MemberId) -> u64 {
    let mut bytes = [0u8; 48];
    bytes[..32].copy_from_slice(&digest.hash);
    bytes[32..40].copy_from_slice(&digest.size_bytes.to_le_bytes());
    bytes[40..].copy_from_slice(&id.0.to_le_bytes());
    let mut h = SipHasher24::new_with_keys(KEY0, KEY1);
    h.write(&bytes);
    h.finish()
}

/// The score of a member for a digest: higher wins. Positive and finite for a weight of
/// at least 1.
pub(crate) fn score(digest: &Digest, id: MemberId, weight: u32) -> f64 {
    // The top 52 bits plus one half: in [2^-53, 1 - 2^-53], exact in an f64, never 0 or 1.
    let u = ((hash(digest, id) >> 12) as f64 + 0.5) / 4_503_599_627_370_496.0;
    f64::from(weight) / neg_ln(u)
}

/// `-ln(u)` for `u` in (0, 1), to better than 1e-15 relative (the worst of 300,000 values,
/// measured against a 60-digit logarithm, was 3.8e-16), by `ln(m 2^e) = e ln 2 + 2
/// atanh((m-1)/(m+1))` with `m` in [sqrt(1/2), sqrt(2)] and a fixed number of series terms.
fn neg_ln(u: f64) -> f64 {
    const MANTISSA: u64 = (1 << 52) - 1;
    const ONE_EXPONENT: u64 = 1023 << 52;
    let bits = u.to_bits();
    let mut e = ((bits >> 52) & 0x7ff) as i32 - 1023;
    let mut m = f64::from_bits((bits & MANTISSA) | ONE_EXPONENT);
    if m > std::f64::consts::SQRT_2 {
        m /= 2.0;
        e += 1;
    }
    let s = (m - 1.0) / (m + 1.0);
    let s2 = s * s;
    // |s| <= 0.172, so s^25 < 1e-19: twelve terms of 1 + s^2/3 + s^4/5 + ...
    let mut series = 0.0;
    for k in (0..12).rev() {
        series = series * s2 + 1.0 / f64::from(2 * k + 1);
    }
    -(f64::from(e) * std::f64::consts::LN_2 + 2.0 * s * series)
}

#[cfg(test)]
mod tests {
    use kbf_types::DigestFunction;

    use super::*;

    fn digest(byte: u8, size: u64) -> Digest {
        Digest::new(DigestFunction::Sha256, [byte; 32], size)
    }

    // Catches: a changed seed, byte layout or hash. Both values come from an independent
    // Python SipHash-2-4 (checked against the paper's test vector) over the same layout.
    #[test]
    fn the_hash_is_pinned() {
        assert_eq!(hash(&digest(0, 0), MemberId(11)), 0xeb10_4cb6_3da5_aeb0);
        let d = Digest::new(
            DigestFunction::Sha256,
            core::array::from_fn(|i| i as u8),
            4096,
        );
        assert_eq!(hash(&d, MemberId(16)), 0xbbae_35bc_774b_3c68);
    }

    // Catches: the SipHash dependency changing its output for the same bytes (the
    // reference vector of the SipHash paper, appendix A).
    #[test]
    fn siphash_matches_its_reference_vector() {
        let mut h = SipHasher24::new_with_keys(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
        h.write(&core::array::from_fn::<u8, 15, _>(|i| i as u8));
        assert_eq!(h.finish(), 0xa129_ca61_49be_45e5);
    }

    // Catches: a series too short or a wrong reduction, which skews scores (and the
    // weights' shares) by more than float noise. Checked against the host's `ln`, which
    // is itself good to an ulp, so 1e-15 (the measured worst case is 3.8e-16).
    #[test]
    fn neg_ln_agrees_with_the_host_logarithm() {
        let mut us = vec![0.5, 0.25, 0.75, 2f64.powi(-53), 1.0 - 2f64.powi(-53)];
        us.extend((1..2000).map(|i| f64::from(i) / 2000.0));
        us.extend((0..60).map(|i| 0.5f64.powi(i) * 1.37));
        for u in us.into_iter().filter(|u| *u < 1.0) {
            let (got, want) = (neg_ln(u), -u.ln());
            assert!(
                ((got - want) / want).abs() < 1e-15,
                "u {u}: {got} vs {want}"
            );
        }
    }

    // Catches: any change to `neg_ln` or `score` that moves a single bit, on any platform:
    // a coefficient, the order of the terms, a fused multiply-add, a different reduction.
    // The 1e-15 test above would not see a change of one ulp; the owner lists would. The
    // constants come from a Python script that repeats this module's operations one for
    // one in Python floats (IEEE binary64): the same split of `u` into exponent and
    // mantissa, the same reduction by sqrt(2), the same twelve Horner steps from the
    // highest term down, the same divisions, the same final sum. It does not call
    // `math.log`; it does check each result against it to 1e-15. test-x86 and test-arm
    // both run this, so equal bits there are the proof that the two platforms rank alike.
    //
    // `u` is `((h >> 12) + 0.5) / 2^52`, so its real extremes are 2^-53 (h >> 12 = 0) and
    // 1 - 2^-53 (h >> 12 = 2^52 - 1), and both are tested. The score has no way to reach
    // them from a chosen digest, so the scores below are the pinned cases of the hash test.
    #[test]
    fn neg_ln_and_score_are_bit_exact() {
        let neg_ln_cases: [(f64, u64); 12] = [
            (1.0 / 9_007_199_254_740_992.0, 0x4042_5e4f_7b27_37fa),
            (1e-9, 0x4034_b927_f32b_ffb8),
            (0.001, 0x401b_a18a_998f_ffa0),
            (0.0625, 0x4006_2e42_fefa_39ef),
            (0.25, 0x3ff6_2e42_fefa_39ef),
            (0.3, 0x3ff3_4378_fcbd_a720),
            (0.5, 0x3fe6_2e42_fefa_39ef),
            (0.7, 0x3fd6_d3c3_24e1_3f4f),
            (0.75, 0x3fd2_6962_1134_db92),
            (0.9, 0x3fba_f8e8_210a_415b),
            (0.999, 0x3f50_6467_0d97_9b73),
            (1.0 - 1.0 / 9_007_199_254_740_992.0, 0x3ca0_0000_0000_0000),
        ];
        // The extremes of `u`, computed as `score` does from the smallest and the largest
        // value of `h >> 12`.
        let u_of = |top: u64| (top as f64 + 0.5) / 4_503_599_627_370_496.0;
        assert_eq!(u_of(0), neg_ln_cases[0].0);
        assert_eq!(u_of((1 << 52) - 1), neg_ln_cases[11].0);
        for (u, want) in neg_ln_cases {
            assert_eq!(neg_ln(u).to_bits(), want, "neg_ln({u:e})");
        }
        let d = |byte: u8, size| digest(byte, size);
        let ranged = Digest::new(
            DigestFunction::Sha256,
            core::array::from_fn(|i| i as u8),
            4096,
        );
        let score_cases: [(Digest, u64, u32, u64); 4] = [
            (d(0, 0), 11, 1, 0x4027_70dd_845c_ed70),
            (ranged, 16, 2, 0x4019_c52a_5ac5_51c8),
            (d(0xff, 1), 14, 3, 0x4003_90c3_a7bf_7381),
            (d(7, 123_456), 13, u32::MAX, 0x41e4_498d_86b9_a319),
        ];
        for (dig, id, weight, want) in score_cases {
            assert_eq!(
                score(&dig, MemberId(id), weight).to_bits(),
                want,
                "score of m{id} weight {weight}"
            );
        }
    }

    // Catches: a score that is not positive and finite at the ends of the range.
    #[test]
    fn scores_are_positive_and_finite() {
        for (byte, size) in [(0u8, 0u64), (0xff, u64::MAX), (7, 1)] {
            let s = score(&digest(byte, size), MemberId(u64::MAX), u32::MAX);
            assert!(s.is_finite());
            assert!(s > 0.0);
        }
    }
}

//! 256-bit intermediate for multiply-then-divide (eng review E2).
//!
//! `raw(u64) × multiplier(u128) × price(u64)` can need all 256 bits, so valuation computes the full product
//! here and divides once. Dividing by `a` then by `b` with floor division equals one floor division by `a × b`,
//! so a chain of divisions still rounds exactly once, down.
//!
//! Only what valuation needs: full multiply, checked multiply, divide by a `u128`. Never panics.

use crate::{CoreError, Result};

const LOW64: u128 = u64::MAX as u128;
/// Largest power of ten that fits in `u64`, so power-of-ten divisions stay on the fast limb path.
const POW10_CHUNK_DIV: u32 = 19;
/// Largest power of ten that fits in `u128`.
const POW10_CHUNK_MUL: u32 = 38;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct U256 {
    hi: u128,
    lo: u128,
}

impl U256 {
    pub(crate) const ZERO: U256 = U256 { hi: 0, lo: 0 };

    /// Full product of two `u128`s. Cannot overflow: the result is below 2^256.
    pub(crate) fn full_mul(a: u128, b: u128) -> U256 {
        let (a1, a0) = (a >> 64, a & LOW64);
        let (b1, b0) = (b >> 64, b & LOW64);
        let p00 = a0 * b0;
        let p01 = a0 * b1;
        let p10 = a1 * b0;
        let p11 = a1 * b1;
        // At most 3 × (2^64 − 1): fits in u128.
        let middle = (p00 >> 64) + (p01 & LOW64) + (p10 & LOW64);
        U256 {
            hi: p11 + (p01 >> 64) + (p10 >> 64) + (middle >> 64),
            lo: (middle << 64) | (p00 & LOW64),
        }
    }

    /// `self × b`, or `Overflow` past 2^256 − 1.
    pub(crate) fn checked_mul(self, b: u128) -> Result<U256> {
        let low = U256::full_mul(self.lo, b);
        if self.hi == 0 {
            return Ok(low);
        }
        let high = U256::full_mul(self.hi, b);
        if high.hi != 0 {
            return Err(CoreError::Overflow);
        }
        let hi = low.hi.checked_add(high.lo).ok_or(CoreError::Overflow)?;
        Ok(U256 { hi, lo: low.lo })
    }

    /// `self × 10^n`, or `Overflow` past 2^256 − 1.
    pub(crate) fn checked_mul_pow10(self, mut n: u32) -> Result<U256> {
        let mut x = self;
        while n > 0 && x != U256::ZERO {
            let k = n.min(POW10_CHUNK_MUL);
            x = x.checked_mul(10u128.pow(k))?;
            n -= k;
        }
        Ok(x)
    }

    /// Floor of `self / d`, and the remainder.
    pub(crate) fn div_rem(self, d: u128) -> Result<(U256, u128)> {
        if d == 0 {
            return Err(CoreError::DivideByZero);
        }
        if self.hi == 0 {
            return Ok((U256::from(self.lo / d), self.lo % d));
        }
        if d <= LOW64 {
            Ok(self.div_rem_limbs(d))
        } else {
            Ok(self.div_rem_long(d))
        }
    }

    /// Floor of `self / 10^n`.
    pub(crate) fn div_pow10(self, mut n: u32) -> Result<U256> {
        let mut x = self;
        while n > 0 && x != U256::ZERO {
            let k = n.min(POW10_CHUNK_DIV);
            x = x.div_rem(10u128.pow(k))?.0;
            n -= k;
        }
        Ok(x)
    }

    pub(crate) fn to_u64(self) -> Result<u64> {
        if self.hi != 0 {
            return Err(CoreError::Overflow);
        }
        u64::try_from(self.lo).map_err(|_| CoreError::Overflow)
    }

    /// Schoolbook division over 64-bit limbs, for a divisor that fits in `u64`. Each step divides a value below
    /// `d × 2^64`, so every partial quotient fits in one limb.
    fn div_rem_limbs(self, d: u128) -> (U256, u128) {
        let limbs = [
            self.hi >> 64,
            self.hi & LOW64,
            self.lo >> 64,
            self.lo & LOW64,
        ];
        let mut q = [0u128; 4];
        let mut rem = 0u128;
        for (i, limb) in limbs.iter().enumerate() {
            let cur = (rem << 64) | limb;
            q[i] = cur / d;
            rem = cur % d;
        }
        (
            U256 {
                hi: (q[0] << 64) | q[1],
                lo: (q[2] << 64) | q[3],
            },
            rem,
        )
    }

    /// Bitwise long division, for a divisor above `u64`. The running remainder is below `2d`, so one carry bit
    /// past `u128` is all it ever needs.
    fn div_rem_long(self, d: u128) -> (U256, u128) {
        let mut q = U256::ZERO;
        let mut rem = 0u128;
        for i in (0..256).rev() {
            let bit = if i >= 128 {
                (self.hi >> (i - 128)) & 1
            } else {
                (self.lo >> i) & 1
            };
            let carry = rem >> 127;
            rem = (rem << 1) | bit;
            if carry == 1 || rem >= d {
                rem = rem.wrapping_sub(d);
                if i >= 128 {
                    q.hi |= 1 << (i - 128);
                } else {
                    q.lo |= 1 << i;
                }
            }
        }
        (q, rem)
    }
}

impl From<u128> for U256 {
    fn from(lo: u128) -> Self {
        U256 { hi: 0, lo }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: U256 = U256 {
        hi: u128::MAX,
        lo: u128::MAX,
    };

    fn add(a: U256, b: u128) -> U256 {
        let (lo, carry) = a.lo.overflowing_add(b);
        U256 {
            hi: a.hi + carry as u128,
            lo,
        }
    }

    #[test]
    fn full_mul_at_the_extremes() {
        // (2^128 − 1)^2 = 2^256 − 2^129 + 1
        assert_eq!(
            U256::full_mul(u128::MAX, u128::MAX),
            U256 {
                hi: u128::MAX - 1,
                lo: 1
            }
        );
        assert_eq!(U256::full_mul(0, u128::MAX), U256::ZERO);
        assert_eq!(U256::full_mul(1 << 127, 2), U256 { hi: 1, lo: 0 });
    }

    #[test]
    fn checked_mul_overflows_only_past_256_bits() {
        assert_eq!(MAX.checked_mul(1), Ok(MAX));
        assert_eq!(MAX.checked_mul(2), Err(CoreError::Overflow));
        let two_pow_255 = U256 {
            hi: 1 << 127,
            lo: 0,
        };
        assert_eq!(two_pow_255.checked_mul(2), Err(CoreError::Overflow));
        assert_eq!(
            U256 { hi: 1, lo: 0 }.checked_mul(u128::MAX),
            Ok(U256 {
                hi: u128::MAX,
                lo: 0
            })
        );
        assert_eq!(
            U256::from(1).checked_mul_pow10(77).map(|_| ()),
            Ok(()),
            "10^77 < 2^256"
        );
        assert_eq!(
            U256::from(1).checked_mul_pow10(78),
            Err(CoreError::Overflow)
        );
        assert_eq!(U256::ZERO.checked_mul_pow10(255), Ok(U256::ZERO));
    }

    #[test]
    fn division_errors_and_trivial_cases() {
        assert_eq!(MAX.div_rem(0), Err(CoreError::DivideByZero));
        assert_eq!(MAX.div_rem(1), Ok((MAX, 0)));
        assert_eq!(MAX.div_pow10(255), Ok(U256::ZERO));
        assert_eq!(U256::from(u64::MAX as u128).to_u64(), Ok(u64::MAX));
        assert_eq!(
            U256::from(u64::MAX as u128 + 1).to_u64(),
            Err(CoreError::Overflow)
        );
        assert_eq!(U256 { hi: 1, lo: 0 }.to_u64(), Err(CoreError::Overflow));
    }

    /// Both slow paths reconstruct the dividend exactly, and agree with each other where both apply.
    #[test]
    fn division_paths_reconstruct_the_dividend() {
        let mut state = 0x5AFE_u64;
        let mut next = || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        for _ in 0..2_000 {
            let x = U256 {
                hi: ((next() as u128) << 64) | next() as u128,
                lo: ((next() as u128) << 64) | next() as u128,
            };
            let small = (next() as u128).max(1);
            let big = (((next() as u128) << 64) | next() as u128).max(LOW64 + 1);
            for d in [small, big] {
                let (q, r) = x.div_rem(d).unwrap();
                assert!(r < d);
                assert_eq!(add(q.checked_mul(d).unwrap(), r), x);
            }
            assert_eq!(x.div_rem_limbs(small), x.div_rem_long(small));
        }
    }
}

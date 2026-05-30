//! Software 80-bit x87 extended-precision float (`long double` on i386).
//!
//! Why this exists: the x87 stack used to be modeled as `f64` (53-bit
//! mantissa). Real hardware keeps a 64-bit mantissa, and musl's
//! `long double`-based `strtod`/`printf` (`fmt_fp`) rely on it — with only
//! 53 bits their cancellation-heavy digit loops drift, so e.g.
//! `printf '%.17g' 0.1` printed `0.099999999999994315`. Each individual x87
//! op was bit-exact at f64; the gap was purely the missing 11 mantissa bits
//! (see the `fpu_dtoa_path_ops_are_individually_correct` test and the
//! README x87 row). This type provides a real 64-bit-mantissa float so the
//! FPU stack can carry full extended precision.
//!
//! Representation: a finite nonzero value is `(-1)^sign · mant · 2^(exp-63)`
//! with `mant` normalized so bit 63 is set (`mant ∈ [2^63, 2^64)`), i.e.
//! `exp = floor(log2|value|)`. Zero / infinity / NaN are explicit classes.
//! `exp` is an `i32`, far wider than the 15-bit hardware exponent — the
//! 80-bit *memory* encoding clamps on the way out; internally we never
//! over/underflow the exponent. Arithmetic uses `u128` intermediates and
//! rounds the 64-bit mantissa to nearest, ties-to-even.
//!
//! This is Phase 1: the type is standalone and unit-tested but NOT yet wired
//! into the FPU (that swap is Phase 2). Pure Rust, no `f128`/libgcc, so it
//! builds for the WASM target too.

use core::cmp::Ordering;
use core::ops::{Add, Div, Mul, Neg, Sub};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cls {
    Zero,
    Normal,
    Inf,
    Nan,
}

/// 80-bit extended-precision float. See module docs for the representation.
///
/// `PartialEq`/`Eq` are STRUCTURAL (do the stored bits match) — used for
/// snapshot/rollback checks. Values are always normalized so this coincides
/// with numeric equality, except it treats the two signed zeros as distinct
/// and any two NaNs as equal. For IEEE numeric ordering use [`F80::partial_cmp`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct F80 {
    sign: bool,
    cls: Cls,
    /// Unbiased binary exponent (valid for `Normal`).
    exp: i32,
    /// 64-bit significand with bit 63 set (valid for `Normal`).
    mant: u64,
}

impl F80 {
    pub const ZERO: F80 = F80 {
        sign: false,
        cls: Cls::Zero,
        exp: 0,
        mant: 0,
    };

    fn zero(sign: bool) -> F80 {
        F80 {
            sign,
            cls: Cls::Zero,
            exp: 0,
            mant: 0,
        }
    }
    fn inf(sign: bool) -> F80 {
        F80 {
            sign,
            cls: Cls::Inf,
            exp: 0,
            mant: 0,
        }
    }
    fn nan() -> F80 {
        F80 {
            sign: false,
            cls: Cls::Nan,
            exp: 0,
            mant: 0,
        }
    }

    pub fn is_nan(self) -> bool {
        self.cls == Cls::Nan
    }
    pub fn is_inf(self) -> bool {
        self.cls == Cls::Inf
    }
    pub fn is_zero(self) -> bool {
        self.cls == Cls::Zero
    }

    pub fn abs(self) -> F80 {
        F80 {
            sign: false,
            ..self
        }
    }

    /// Build a normalized `Normal` (or zero) from `value = (-1)^sign · sig ·
    /// 2^e`, where `sig` is a `u128` integer (binary point at bit 0) and
    /// `sticky` records whether any 1-bits were already dropped below bit 0.
    /// Keeps the top 64 bits as the mantissa, rounding to nearest-even.
    fn round_from(sign: bool, e: i32, sig: u128, sticky: bool) -> F80 {
        if sig == 0 {
            // Only reachable from exact cancellation in add/sub. IEEE
            // round-to-nearest yields +0 (unless the sticky says otherwise,
            // which can't happen for an exact-zero significand).
            return F80::zero(false);
        }
        let n = 128 - sig.leading_zeros() as i32; // bit length (MSB index + 1)
        let mut exp = e + n - 1; // value ∈ [2^exp, 2^(exp+1))
        let mut mant: u64;
        let round_bit;
        let sticky_bit;
        if n > 64 {
            let drop = (n - 64) as u32;
            mant = (sig >> drop) as u64;
            let half = 1u128 << (drop - 1);
            let rem = sig & ((1u128 << drop) - 1);
            round_bit = rem & half != 0;
            sticky_bit = (rem & (half - 1)) != 0 || sticky;
        } else {
            // sig already fits in 64 bits; shift up so bit 63 is set.
            mant = (sig as u64) << (64 - n as u32);
            round_bit = false;
            sticky_bit = sticky;
        }
        // Round to nearest, ties to even.
        if round_bit && (sticky_bit || (mant & 1) != 0) {
            mant = mant.wrapping_add(1);
            if mant == 0 {
                // carried out of bit 63 → 2^64; renormalize.
                mant = 0x8000_0000_0000_0000;
                exp += 1;
            }
        }
        F80 {
            sign,
            cls: Cls::Normal,
            exp,
            mant,
        }
    }

    // ---- conversions --------------------------------------------------

    pub fn from_f64(v: f64) -> F80 {
        let bits = v.to_bits();
        let sign = bits >> 63 != 0;
        let exp = ((bits >> 52) & 0x7ff) as i32;
        let frac = bits & 0x000f_ffff_ffff_ffff;
        if exp == 0x7ff {
            return if frac == 0 {
                F80::inf(sign)
            } else {
                F80::nan()
            };
        }
        if exp == 0 {
            if frac == 0 {
                return F80::zero(sign);
            }
            // subnormal f64: value = frac · 2^(-1022-52). round_from
            // normalizes it (frac < 2^52, so n ≤ 52, shifted up — exact).
            return F80::round_from(sign, -1022 - 52, frac as u128, false);
        }
        // normal f64: significand = (1<<52)|frac, value = sig · 2^(exp-1023-52).
        let sig = (1u64 << 52) | frac;
        F80::round_from(sign, exp - 1023 - 52, sig as u128, false)
    }

    pub fn from_f32(v: f32) -> F80 {
        F80::from_f64(v as f64)
    }

    pub fn from_i64(v: i64) -> F80 {
        if v == 0 {
            return F80::zero(false);
        }
        let sign = v < 0;
        let mag = v.unsigned_abs() as u128;
        F80::round_from(sign, 0, mag, false)
    }
    pub fn from_i32(v: i32) -> F80 {
        F80::from_i64(v as i64)
    }
    pub fn from_i16(v: i16) -> F80 {
        F80::from_i64(v as i64)
    }

    /// Demote to f64 (round to nearest). Powers of two are applied in
    /// bounded chunks so a tiny/huge value doesn't under/overflow the 2^k
    /// factor independently of the mantissa.
    pub fn to_f64(self) -> f64 {
        match self.cls {
            Cls::Zero => {
                if self.sign {
                    -0.0
                } else {
                    0.0
                }
            }
            Cls::Inf => {
                if self.sign {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                }
            }
            Cls::Nan => f64::NAN,
            Cls::Normal => {
                // value = mant · 2^(exp-63). `mant as f64` rounds 64→53 bits
                // to nearest-even; multiplying by the exact power of two does
                // not round again, so the result is correctly rounded.
                let mut r = self.mant as f64;
                let mut k = self.exp - 63;
                while k < -512 {
                    r *= 2f64.powi(-512);
                    k += 512;
                }
                while k > 512 {
                    r *= 2f64.powi(512);
                    k -= 512;
                }
                let m = r * 2f64.powi(k);
                if self.sign {
                    -m
                } else {
                    m
                }
            }
        }
    }

    /// Round to a 64-bit integer, ties to even (x87 default rounding).
    /// Out-of-range / NaN return the x87 "integer indefinite" (i64::MIN).
    pub fn to_i64_round(self) -> i64 {
        self.to_int(false)
    }
    /// Truncate toward zero (what a C `(int)` cast / FISTTP do).
    pub fn to_i64_trunc(self) -> i64 {
        self.to_int(true)
    }

    fn to_int(self, trunc: bool) -> i64 {
        match self.cls {
            Cls::Zero => 0,
            Cls::Inf | Cls::Nan => i64::MIN, // integer indefinite
            Cls::Normal => {
                if self.exp < 0 {
                    // |value| < 1
                    if trunc {
                        return 0;
                    }
                    // round-to-nearest-even of a value in (0,1)
                    let rounded = if self.exp == -1 {
                        // |value| ∈ [0.5,1): tie at exactly 0.5 → 0 (even)
                        if self.mant == 0x8000_0000_0000_0000 {
                            0
                        } else {
                            1
                        }
                    } else {
                        0
                    };
                    return if self.sign { -rounded } else { rounded };
                }
                if self.exp >= 63 {
                    // |value| ≥ 2^63 — out of i64 range (2^63 itself is
                    // i64::MIN's magnitude; treat as indefinite/overflow).
                    return i64::MIN;
                }
                let shift = 63 - self.exp as u32;
                let intpart = self.mant >> shift;
                let fracmask = (1u64 << shift) - 1;
                let frac = self.mant & fracmask;
                let mut mag = intpart;
                if !trunc {
                    let half = 1u64 << (shift - 1);
                    if frac > half || (frac == half && (intpart & 1) != 0) {
                        mag += 1;
                    }
                }
                if self.sign {
                    -(mag as i64)
                } else {
                    mag as i64
                }
            }
        }
    }

    // ---- comparison ---------------------------------------------------

    /// IEEE ordering; `None` if either is NaN (unordered).
    pub fn partial_cmp(self, other: F80) -> Option<Ordering> {
        if self.cls == Cls::Nan || other.cls == Cls::Nan {
            return None;
        }
        // Both zero (any sign) compare equal.
        if self.cls == Cls::Zero && other.cls == Cls::Zero {
            return Some(Ordering::Equal);
        }
        // Reduce to magnitude comparison given signs.
        let neg = self.sign;
        if self.sign != other.sign {
            // different signs: negative < positive (zeros handled above)
            return Some(if self.sign {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
        // same sign: compare magnitudes, then flip if both negative.
        let mag = self.cmp_mag(other);
        Some(if neg { mag.reverse() } else { mag })
    }

    /// Compare magnitudes ignoring sign (treats zero as smallest).
    fn cmp_mag(self, other: F80) -> Ordering {
        let rank = |f: &F80| match f.cls {
            Cls::Zero => 0,
            Cls::Normal => 1,
            Cls::Inf => 2,
            Cls::Nan => 3,
        };
        match rank(&self).cmp(&rank(&other)) {
            Ordering::Equal => {
                if self.cls == Cls::Normal {
                    self.exp.cmp(&other.exp).then(self.mant.cmp(&other.mant))
                } else {
                    Ordering::Equal
                }
            }
            o => o,
        }
    }

    pub fn is_sign_negative(self) -> bool {
        self.sign
    }
    /// True for an 80-bit *subnormal* (exponent below the normal minimum).
    pub fn is_subnormal(self) -> bool {
        self.cls == Cls::Normal && self.exp < -16382
    }

    // ---- 80-bit memory format (the `m80`/FXSAVE encoding) -------------

    /// Decode the 80-bit memory form: `mant` is the 64-bit significand
    /// (explicit integer bit at bit 63), `se` the sign+exponent word
    /// (bit 15 = sign, bits 0..14 = biased exponent). Precision-preserving
    /// — the full 64-bit mantissa is kept (unlike the old f64 demotion).
    pub fn from_f80_parts(mant: u64, se: u16) -> F80 {
        let sign = se & 0x8000 != 0;
        let exp_field = (se & 0x7fff) as i32;
        if exp_field == 0x7fff {
            return if mant & 0x7fff_ffff_ffff_ffff == 0 {
                F80::inf(sign)
            } else {
                // Preserve the sign so a NaN round-trips through the m80
                // encoding (NaN sign is otherwise architecturally inert).
                F80 {
                    sign,
                    cls: Cls::Nan,
                    exp: 0,
                    mant: 0,
                }
            };
        }
        if mant == 0 {
            return F80::zero(sign);
        }
        // value = mant · 2^(eff - 16383 - 63); subnormals use eff = 1.
        let eff = if exp_field == 0 { 1 } else { exp_field };
        F80::round_from(sign, eff - 16383 - 63, mant as u128, false)
    }

    /// Encode to the 80-bit memory form `(mantissa, sign+exponent)`.
    /// Out-of-range exponents clamp to ∞ / subnormal / zero.
    pub fn to_f80_parts(self) -> (u64, u16) {
        let sign16 = if self.sign { 0x8000u16 } else { 0 };
        match self.cls {
            Cls::Zero => (0, sign16),
            Cls::Inf => (0x8000_0000_0000_0000, sign16 | 0x7fff),
            Cls::Nan => (0xC000_0000_0000_0000, sign16 | 0x7fff),
            Cls::Normal => {
                let biased = self.exp + 16383;
                if biased >= 0x7fff {
                    (0x8000_0000_0000_0000, sign16 | 0x7fff) // overflow → ∞
                } else if biased <= 0 {
                    let sh = 1 - biased;
                    if sh >= 64 {
                        (0, sign16)
                    } else {
                        (self.mant >> sh as u32, sign16) // subnormal (exp field 0)
                    }
                } else {
                    (self.mant, sign16 | biased as u16)
                }
            }
        }
    }

    // ---- integer rounding / x87 helpers -------------------------------

    /// True if a `Normal` has a nonzero fractional part.
    fn has_fraction(self) -> bool {
        match self.cls {
            Cls::Normal => {
                if self.exp < 0 {
                    true // 0 < |v| < 1
                } else if self.exp >= 63 {
                    false // integer
                } else {
                    let shift = 63 - self.exp as u32;
                    (self.mant & ((1u64 << shift) - 1)) != 0
                }
            }
            _ => false,
        }
    }

    /// Round to i64 per the x87 control-word rounding-control field
    /// (0 = nearest-even, 1 = −∞, 2 = +∞, 3 = truncate).
    pub fn to_i64_rc(self, rc: u8) -> i64 {
        match rc & 3 {
            0 => self.to_i64_round(),
            3 => self.to_i64_trunc(),
            1 => {
                let t = self.to_i64_trunc();
                if self.sign && self.has_fraction() {
                    t - 1
                } else {
                    t
                }
            }
            _ => {
                let t = self.to_i64_trunc();
                if !self.sign && self.has_fraction() {
                    t + 1
                } else {
                    t
                }
            }
        }
    }

    /// FRNDINT: round to an integer-valued F80 per the rounding mode.
    pub fn round_to_integer(self, rc: u8) -> F80 {
        match self.cls {
            Cls::Normal if self.exp < 63 => F80::from_i64(self.to_i64_rc(rc)),
            _ => self, // already integer, or zero/inf/nan
        }
    }

    /// Multiply by 2^n exactly (FSCALE).
    pub fn scale2(self, n: i32) -> F80 {
        match self.cls {
            Cls::Normal => F80 {
                exp: self.exp.saturating_add(n),
                ..self
            },
            _ => self,
        }
    }

    /// FXTRACT exponent part: the unbiased exponent as an integer-valued
    /// F80 (−∞ for zero, mirroring x87).
    pub fn exponent_f80(self) -> F80 {
        match self.cls {
            Cls::Normal => F80::from_i64(self.exp as i64),
            Cls::Zero => F80::inf(true),
            Cls::Inf => F80::inf(false),
            Cls::Nan => F80::nan(),
        }
    }
    /// FXTRACT significand part: the mantissa scaled into [1, 2).
    pub fn significand(self) -> F80 {
        match self.cls {
            Cls::Normal => F80 { exp: 0, ..self },
            _ => self,
        }
    }
}

// ---- arithmetic as operator traits (so the FPU reads like math) ---------

impl Neg for F80 {
    type Output = F80;
    fn neg(self) -> F80 {
        F80 {
            sign: !self.sign,
            ..self
        }
    }
}

impl Mul for F80 {
    type Output = F80;
    fn mul(self, other: F80) -> F80 {
        let sign = self.sign ^ other.sign;
        use Cls::*;
        match (self.cls, other.cls) {
            (Nan, _) | (_, Nan) => F80::nan(),
            (Inf, Zero) | (Zero, Inf) => F80::nan(), // 0·∞
            (Inf, _) | (_, Inf) => F80::inf(sign),
            (Zero, _) | (_, Zero) => F80::zero(sign),
            (Normal, Normal) => {
                let p = (self.mant as u128) * (other.mant as u128);
                // value = p · 2^(self.exp-63 + other.exp-63)
                F80::round_from(sign, self.exp + other.exp - 126, p, false)
            }
        }
    }
}

impl Add for F80 {
    type Output = F80;
    fn add(self, other: F80) -> F80 {
        use Cls::*;
        match (self.cls, other.cls) {
            (Nan, _) | (_, Nan) => return F80::nan(),
            (Inf, Inf) => {
                return if self.sign == other.sign {
                    F80::inf(self.sign)
                } else {
                    F80::nan() // ∞ + (−∞)
                };
            }
            (Inf, _) => return F80::inf(self.sign),
            (_, Inf) => return F80::inf(other.sign),
            (Zero, Zero) => {
                // −0 + −0 = −0; otherwise +0 (round-to-nearest).
                return F80::zero(self.sign && other.sign);
            }
            (Zero, _) => return other,
            (_, Zero) => return self,
            (Normal, Normal) => {}
        }
        // Order so `a` has the larger (or equal) magnitude.
        let (a, b) = if self.cmp_mag(other) == Ordering::Less {
            (other, self)
        } else {
            (self, other)
        };
        let diff = (a.exp - b.exp) as u32;
        // Place a's mantissa with its MSB at bit 126 (63 guard bits below),
        // leaving bit 127 free so two of them can sum without overflowing
        // u128. value_a = a.mant · 2^(a.exp-63) = (a.mant<<63) · 2^(a.exp-126).
        let av = (a.mant as u128) << 63;
        // Align b by shifting right `diff`; capture dropped bits as sticky.
        let (bv, bsticky) = if diff >= 128 {
            (0u128, true)
        } else {
            let full = (b.mant as u128) << 63;
            let shifted = full >> diff;
            let dropped = full & ((1u128 << diff) - 1);
            (shifted, dropped != 0)
        };
        let base_e = a.exp - 126; // value = (significand) · 2^base_e
        if a.sign == b.sign {
            // magnitudes add
            let sig = av + bv;
            F80::round_from(a.sign, base_e, sig, bsticky)
        } else {
            // magnitudes subtract: a ≥ b, so a - b ≥ 0. The dropped bits of
            // b make the true subtrahend slightly larger; borrow them.
            let mut sig = av - bv;
            if bsticky {
                sig -= 1; // account for the (positive) dropped fraction
                          // the new sticky is the complement, but at 64-bit output
                          // precision this ≤½ulp correction is immaterial — and we
                          // pass sticky=true so ties resolve as inexact.
            }
            if sig == 0 {
                return F80::zero(false);
            }
            F80::round_from(a.sign, base_e, sig, bsticky)
        }
    }
}

impl Sub for F80 {
    type Output = F80;
    fn sub(self, other: F80) -> F80 {
        self.add(other.neg())
    }
}

impl Div for F80 {
    type Output = F80;
    fn div(self, other: F80) -> F80 {
        let sign = self.sign ^ other.sign;
        use Cls::*;
        match (self.cls, other.cls) {
            (Nan, _) | (_, Nan) => F80::nan(),
            (Inf, Inf) | (Zero, Zero) => F80::nan(),
            (Inf, _) => F80::inf(sign),
            (_, Inf) => F80::zero(sign),
            (_, Zero) => F80::inf(sign), // finite/0 = ∞
            (Zero, _) => F80::zero(sign),
            (Normal, Normal) => {
                // quotient = (a.mant / b.mant) · 2^(a.exp-b.exp). Scale the
                // numerator up by 64 bits, then append 2 guard bits from the
                // remainder so the significand ALWAYS has ≥66 bits — the bare
                // quotient can be exactly 64 bits (q ∈ [2^63,2^64)), where a
                // boolean sticky can't express round-vs-sticky and the result
                // would never round (audit finding). With the guard bits
                // round_from sees a real round bit.
                let num = (self.mant as u128) << 64;
                let den = other.mant as u128;
                let q = num / den; // ∈ [2^63, 2^65)
                let rem = num % den;
                let guard = (rem << 2) / den; // next 2 quotient bits, 0..3
                let sig = (q << 2) | guard; // = floor(4·num/den), ≥ 2^65
                let sticky = !(rem << 2).is_multiple_of(den);
                // value = sig · 2^(a.exp-b.exp-64-2)
                F80::round_from(sign, self.exp - other.exp - 66, sig, sticky)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(v: f64) -> f64 {
        F80::from_f64(v).to_f64()
    }

    #[test]
    fn f64_roundtrip_is_exact() {
        for &v in &[
            0.0,
            -0.0,
            1.0,
            -1.0,
            0.1,
            0.5,
            1.6,
            std::f64::consts::PI,
            1e300,
            -1e-300,
            1234.5678,
            f64::MIN_POSITIVE,
            5e-324, // smallest subnormal
            f64::MAX,
        ] {
            assert_eq!(rt(v).to_bits(), v.to_bits(), "round-trip {v:e}");
        }
        assert!(rt(f64::NAN).is_nan());
        assert_eq!(rt(f64::INFINITY), f64::INFINITY);
        assert_eq!(rt(f64::NEG_INFINITY), f64::NEG_INFINITY);
    }

    // For operands exactly representable in f64, F80 results (which carry ≥
    // f64 precision) must demote back to exactly the f64 result.
    fn check_binop(a: f64, b: f64) {
        let fa = F80::from_f64(a);
        let fb = F80::from_f64(b);
        assert_eq!(fa.add(fb).to_f64().to_bits(), (a + b).to_bits(), "{a}+{b}");
        assert_eq!(fa.sub(fb).to_f64().to_bits(), (a - b).to_bits(), "{a}-{b}");
        assert_eq!(fa.mul(fb).to_f64().to_bits(), (a * b).to_bits(), "{a}*{b}");
        if b != 0.0 {
            assert_eq!(fa.div(fb).to_f64().to_bits(), (a / b).to_bits(), "{a}/{b}");
        }
    }

    #[test]
    fn arithmetic_matches_f64_for_exact_operands() {
        let vals = [
            1.0, 2.0, 0.5, 0.25, 3.0, -4.0, 1.5, 100.0, 0.125, -2.5, 1024.0, 7.0, -0.75, 6.0,
        ];
        for &a in &vals {
            for &b in &vals {
                check_binop(a, b);
            }
        }
    }

    #[test]
    fn arithmetic_matches_f64_on_inexact_values() {
        // 0.1, 0.3 etc. aren't exact, but f80 ⊇ f64 means a single op on
        // the f64-rounded inputs, demoted, equals the f64 op (no double
        // rounding for +,−,× since f80 has ≥11 extra bits; division is
        // correctly rounded via the u128 quotient).
        let vals = [0.1, 0.3, 0.7, std::f64::consts::PI, 1.0 / 3.0, 1e16, 9.0];
        for &a in &vals {
            for &b in &vals {
                check_binop(a, b);
            }
        }
    }

    #[test]
    fn extra_precision_beyond_f64() {
        // 1 + 2^-60 is NOT representable in f64 (needs 61 mantissa bits) but
        // IS in f80 (64-bit mantissa). Build it via f80 arithmetic and show
        // it differs from 1.0 — whereas the f64 computation collapses to 1.0.
        let one = F80::from_f64(1.0);
        let tiny = F80::from_f64(2f64.powi(-60));
        let sum = one.add(tiny); // exact in f80
        assert_eq!(sum.partial_cmp(one), Some(Ordering::Greater));
        // subtracting 1 recovers exactly 2^-60
        let back = sum.sub(one);
        assert_eq!(back.to_f64().to_bits(), 2f64.powi(-60).to_bits());
        // the equivalent f64 computation loses it:
        assert_eq!(1.0f64 + 2f64.powi(-60), 1.0);
    }

    #[test]
    fn integer_conversions() {
        for &v in &[
            0i64,
            1,
            -1,
            42,
            -42,
            1_000_000,
            -123456789,
            i32::MAX as i64,
            i32::MIN as i64,
        ] {
            assert_eq!(F80::from_i64(v).to_i64_round(), v, "i64 {v}");
            assert_eq!(F80::from_i64(v).to_i64_trunc(), v, "i64 trunc {v}");
        }
        // rounding vs truncation of fractions
        assert_eq!(F80::from_f64(2.7).to_i64_trunc(), 2);
        assert_eq!(F80::from_f64(2.7).to_i64_round(), 3);
        assert_eq!(F80::from_f64(-2.7).to_i64_trunc(), -2);
        assert_eq!(F80::from_f64(-2.7).to_i64_round(), -3);
        assert_eq!(F80::from_f64(2.5).to_i64_round(), 2, "tie to even");
        assert_eq!(F80::from_f64(3.5).to_i64_round(), 4, "tie to even");
        assert_eq!(F80::from_f64(0.5).to_i64_round(), 0, "tie to even");
        assert_eq!(F80::from_f64(0.4).to_i64_trunc(), 0);
        assert_eq!(F80::from_f64(600000000.9).to_i64_trunc(), 600000000);
    }

    #[test]
    fn comparison_and_specials() {
        let a = F80::from_f64(1.5);
        let b = F80::from_f64(2.5);
        assert_eq!(a.partial_cmp(b), Some(Ordering::Less));
        assert_eq!(b.partial_cmp(a), Some(Ordering::Greater));
        assert_eq!(a.partial_cmp(a), Some(Ordering::Equal));
        assert_eq!(
            F80::from_f64(-1.0).partial_cmp(F80::from_f64(1.0)),
            Some(Ordering::Less)
        );
        assert_eq!(
            F80::from_f64(0.0).partial_cmp(F80::from_f64(-0.0)),
            Some(Ordering::Equal)
        );
        assert_eq!(F80::nan().partial_cmp(a), None);
        // inf arithmetic
        assert!(F80::from_f64(1.0).div(F80::ZERO).is_inf());
        assert!(F80::ZERO.div(F80::ZERO).is_nan());
        assert!(F80::inf(false).add(F80::inf(true)).is_nan());
        assert!(F80::inf(false).mul(F80::ZERO).is_nan());
    }

    // ---- regression cases from the adversarial F80 audit --------------

    fn norm(mant: u64, exp: i32) -> F80 {
        F80 {
            sign: false,
            cls: Cls::Normal,
            exp,
            mant,
        }
    }

    #[test]
    fn audit_div_rounds_exact_64bit_quotient() {
        // a/b ∈ (0.5,1): the quotient fills exactly 64 bits and the
        // remainder is > den/2, so it must round UP to all-ones. (Audit
        // found round_from set round_bit=false for n=64 → no rounding.)
        let r = norm(0xd555_5555_5555_5555, 0).div(norm(0xd555_5555_5555_5556, 0));
        assert_eq!(r.cls, Cls::Normal);
        assert_eq!(r.exp, -1);
        assert_eq!(
            r.mant, 0xffff_ffff_ffff_ffff,
            "n=64 quotient with rem>den/2 must round up"
        );
    }

    #[test]
    fn audit_sub_borrow_is_correct() {
        // 1.0 + (−(2^64−1)·2^-128) = 1 − 2^-64 + 2^-128, rounds to 1−2^-64
        // (mant all-ones, exp −1). Exercises the `sig -= 1` borrow path
        // (bsticky true via diff=65); the audit suspected it corrupts
        // rounding — verify it doesn't (no cancellation when bsticky).
        let r = norm(0x8000_0000_0000_0000, 0).add(F80 {
            sign: true,
            cls: Cls::Normal,
            exp: -65,
            mant: 0xffff_ffff_ffff_ffff,
        });
        assert_eq!(r.cls, Cls::Normal);
        assert_eq!(r.exp, -1);
        assert_eq!(r.mant, 0xffff_ffff_ffff_ffff);
    }

    #[test]
    fn audit_nan_sign_roundtrips_through_m80() {
        let n = F80::from_f80_parts(0xC000_0000_0000_0000, 0xFFFF); // signed NaN
        assert!(n.is_nan());
        let (_, se) = n.to_f80_parts();
        assert_eq!(
            se & 0x8000,
            0x8000,
            "NaN sign must survive the m80 round-trip"
        );
    }
}

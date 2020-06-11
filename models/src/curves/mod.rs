use snarkos_utilities::{
    biginteger::*,
    bytes::{FromBytes, ToBytes},
    serialize::{
        CanonicalDeserialize,
        CanonicalDeserializeWithFlags,
        CanonicalSerialize,
        CanonicalSerializeWithFlags,
        ConstantSerializedSize,
    },
};

use std::fmt::Debug;

#[macro_use]
mod macros;

pub mod field;
pub use self::field::*;

pub mod fp_256;
pub use self::fp_256::*;

pub mod fp_320;
pub use self::fp_320::*;

pub mod fp_384;
pub use self::fp_384::*;

pub mod fp_768;
pub use self::fp_768::*;

pub mod fp_832;
pub use self::fp_832::*;

pub mod fp2;
pub use self::fp2::*;

pub mod fp3;
pub use self::fp3::*;

pub mod fp6_2over3;
pub use self::fp6_2over3::*;

pub mod fp6_3over2;
pub use self::fp6_3over2::*;

pub mod fp12_2over3over2;
pub use self::fp12_2over3over2::*;

pub mod fp_parameters;
pub use self::fp_parameters::*;

pub mod group;
pub use self::group::*;

pub mod pairing_engine;
pub use self::pairing_engine::*;

pub mod primefield;
pub use self::primefield::*;

pub mod tests_field;

pub mod tests_group;

pub mod tests_curve;

pub mod to_field_vec;

#[macro_export]
macro_rules! field {
    ($name:ident, $c0:expr) => {
        $name {
            0: $c0,
            1: std::marker::PhantomData,
        }
    };
    ($name:ident, $c0:expr, $c1:expr $(,)?) => {
        $name {
            c0: $c0,
            c1: $c1,
            _parameters: std::marker::PhantomData,
        }
    };
    ($name:ident, $c0:expr, $c1:expr, $c2:expr $(,)?) => {
        $name {
            c0: $c0,
            c1: $c1,
            c2: $c2,
            _parameters: std::marker::PhantomData,
        }
    };
}

/// The interface for a field that supports an efficient square-root operation.
pub trait SquareRootField: Field {
    /// Returns the Legendre symbol.
    fn legendre(&self) -> LegendreSymbol;

    /// Returns the square root of self, if it exists.
    #[must_use]
    fn sqrt(&self) -> Option<Self>;

    /// Sets `self` to be the square root of `self`, if it exists.
    fn sqrt_in_place(&mut self) -> Option<&mut Self>;
}

#[derive(Debug, PartialEq)]
pub enum LegendreSymbol {
    Zero = 0,
    QuadraticResidue = 1,
    QuadraticNonResidue = -1,
}

impl LegendreSymbol {
    pub fn is_zero(&self) -> bool {
        *self == LegendreSymbol::Zero
    }

    pub fn is_qnr(&self) -> bool {
        *self == LegendreSymbol::QuadraticNonResidue
    }

    pub fn is_qr(&self) -> bool {
        *self == LegendreSymbol::QuadraticResidue
    }
}

impl_field_into_bigint!(Fp256, BigInteger256, Fp256Parameters);
impl_field_into_bigint!(Fp320, BigInteger320, Fp320Parameters);
impl_field_into_bigint!(Fp384, BigInteger384, Fp384Parameters);
impl_field_into_bigint!(Fp768, BigInteger768, Fp768Parameters);
impl_field_into_bigint!(Fp832, BigInteger832, Fp832Parameters);

impl_prime_field_serializer!(Fp256, Fp256Parameters, 32);
impl_prime_field_serializer!(Fp320, Fp320Parameters, 40);
impl_prime_field_serializer!(Fp384, Fp384Parameters, 48);
impl_prime_field_serializer!(Fp768, Fp768Parameters, 96);
impl_prime_field_serializer!(Fp832, Fp832Parameters, 104);

pub fn batch_inversion<F: Field>(v: &mut [F]) {
    // Montgomery’s Trick and Fast Implementation of Masked AES
    // Genelle, Prouff and Quisquater
    // Section 3.2

    // First pass: compute [a, ab, abc, ...]
    let mut prod = Vec::with_capacity(v.len());
    let mut tmp = F::one();
    for f in v.iter().filter(|f| !f.is_zero()) {
        tmp.mul_assign(&f);
        prod.push(tmp);
    }

    // Invert `tmp`.
    tmp = tmp.inverse().unwrap(); // Guaranteed to be nonzero.

    // Second pass: iterate backwards to compute inverses
    for (f, s) in v
        .iter_mut()
        // Backwards
        .rev()
        // Ignore normalized elements
        .filter(|f| !f.is_zero())
        // Backwards, skip last element, fill in one for last term.
        .zip(prod.into_iter().rev().skip(1).chain(Some(F::one())))
    {
        // tmp := tmp * g.z; g.z := tmp * s = 1/z
        let newtmp = tmp * &f;
        *f = tmp * &s;
        tmp = newtmp;
    }
}

//! Structs and constants specific to the Sapling shielded pool.

pub mod group_hash;
pub mod keys;
pub mod note;
pub mod note_encryption;
pub mod pedersen_hash;
pub mod prover;
pub mod redjubjub;
pub mod tree;
pub mod util;

use blake2s_simd::Params as Blake2sParams;
use borsh::{BorshDeserialize, BorshSerialize};
use ff::PrimeField;
use group::{cofactor::CofactorGroup, Group, GroupEncoding};
use rand_core::{CryptoRng, RngCore};
use std::{
    cmp::Ordering,
    convert::TryFrom,
    fmt::{Display, Formatter},
    hash::{Hash, Hasher},
    io::{self, Read, Write},
    str::FromStr,
};
use subtle::CtOption;

use crate::{
    asset_type::AssetType,
    constants::{self, SPENDING_KEY_GENERATOR},
    transaction::components::amount::MAX_MONEY,
};

use self::{
    group_hash::group_hash,
    redjubjub::{PrivateKey, PublicKey, Signature},
};

pub use crate::sapling::tree::{Node, SAPLING_COMMITMENT_TREE_DEPTH};

/// Create the spendAuthSig for a Sapling SpendDescription.
pub fn spend_sig<R: RngCore + CryptoRng>(
    ask: PrivateKey,
    ar: jubjub::Fr,
    sighash: &[u8; 32],
    rng: &mut R,
) -> Signature {
    spend_sig_internal(ask, ar, sighash, rng)
}

pub(crate) fn spend_sig_internal<R: RngCore>(
    ask: PrivateKey,
    ar: jubjub::Fr,
    sighash: &[u8; 32],
    rng: &mut R,
) -> Signature {
    // We compute `rsk`...
    let rsk = ask.randomize(ar);

    // We compute `rk` from there (needed for key prefixing)
    let rk = PublicKey::from_private(&rsk, SPENDING_KEY_GENERATOR);

    // Compute the signature's message for rk/spend_auth_sig
    let mut data_to_be_signed = [0u8; 64];
    data_to_be_signed[0..32].copy_from_slice(&rk.0.to_bytes());
    data_to_be_signed[32..64].copy_from_slice(&sighash[..]);

    // Do the signing
    rsk.sign(&data_to_be_signed, rng, SPENDING_KEY_GENERATOR)
}

#[derive(Clone)]
pub struct ValueCommitment {
    pub asset_generator: jubjub::ExtendedPoint,
    pub value: u64,
    pub randomness: jubjub::Fr,
}

impl ValueCommitment {
    pub fn commitment(&self) -> jubjub::SubgroupPoint {
        (CofactorGroup::clear_cofactor(&self.asset_generator) * jubjub::Fr::from(self.value))
            + (constants::VALUE_COMMITMENT_RANDOMNESS_GENERATOR * self.randomness)
    }
}

#[derive(Clone)]
pub struct ProofGenerationKey {
    pub ak: jubjub::SubgroupPoint,
    pub nsk: jubjub::Fr,
}

impl ProofGenerationKey {
    pub fn to_viewing_key(&self) -> ViewingKey {
        ViewingKey {
            ak: self.ak,
            nk: NullifierDerivingKey(constants::PROOF_GENERATION_KEY_GENERATOR * self.nsk),
        }
    }
}

/// A key used to derive the nullifier for a Sapling note.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct NullifierDerivingKey(pub jubjub::SubgroupPoint);

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct ViewingKey {
    pub ak: jubjub::SubgroupPoint,
    pub nk: NullifierDerivingKey,
}

impl Hash for ViewingKey {
    fn hash<H>(&self, state: &mut H)
    where
        H: Hasher,
    {
        self.ak.to_bytes().hash(state);
        self.nk.0.to_bytes().hash(state);
    }
}

impl ViewingKey {
    pub fn rk(&self, ar: jubjub::Fr) -> jubjub::SubgroupPoint {
        self.ak + constants::SPENDING_KEY_GENERATOR * ar
    }

    pub fn ivk(&self) -> SaplingIvk {
        let mut h = [0; 32];
        h.copy_from_slice(
            Blake2sParams::new()
                .hash_length(32)
                .personal(constants::CRH_IVK_PERSONALIZATION)
                .to_state()
                .update(&self.ak.to_bytes())
                .update(&self.nk.0.to_bytes())
                .finalize()
                .as_bytes(),
        );

        // Drop the most significant five bits, so it can be interpreted as a scalar.
        h[31] &= 0b0000_0111;

        SaplingIvk(jubjub::Fr::from_repr(h).unwrap())
    }

    pub fn to_payment_address(&self, diversifier: Diversifier) -> Option<PaymentAddress> {
        self.ivk().to_payment_address(diversifier)
    }

    pub fn read<R: Read>(mut reader: R) -> io::Result<Self> {
        let ak = {
            let mut buf = [0u8; 32];
            reader.read_exact(&mut buf)?;
            jubjub::SubgroupPoint::from_bytes(&buf).and_then(|p| CtOption::new(p, !p.is_identity()))
        };
        let nk = {
            let mut buf = [0u8; 32];
            reader.read_exact(&mut buf)?;
            jubjub::SubgroupPoint::from_bytes(&buf)
        };
        if ak.is_none().into() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ak not of prime order",
            ));
        }
        if nk.is_none().into() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "nk not in prime-order subgroup",
            ));
        }
        let ak = ak.unwrap();
        let nk = nk.unwrap();

        Ok(ViewingKey {
            ak,
            nk: NullifierDerivingKey(nk),
        })
    }

    pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
        writer.write_all(&self.ak.to_bytes())?;
        writer.write_all(&self.nk.0.to_bytes())?;

        Ok(())
    }

    pub fn to_bytes(&self) -> [u8; 64] {
        let mut result = [0u8; 64];
        self.write(&mut result[..])
            .expect("should be able to serialize a ViewingKey");
        result
    }
}

impl BorshSerialize for ViewingKey {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::maybestd::io::Result<()> {
        self.write(writer)
    }
}

impl BorshDeserialize for ViewingKey {
    fn deserialize(buf: &mut &[u8]) -> borsh::maybestd::io::Result<Self> {
        Self::read(buf)
    }
}

impl PartialOrd for ViewingKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.to_bytes().partial_cmp(&other.to_bytes())
    }
}

impl Ord for ViewingKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.to_bytes().cmp(&other.to_bytes())
    }
}

#[derive(Debug, Clone)]
pub struct SaplingIvk(pub jubjub::Fr);

impl SaplingIvk {
    pub fn to_payment_address(&self, diversifier: Diversifier) -> Option<PaymentAddress> {
        diversifier.g_d().and_then(|g_d| {
            let pk_d = g_d * self.0;

            PaymentAddress::from_parts(diversifier, pk_d)
        })
    }

    pub fn to_repr(&self) -> [u8; 32] {
        self.0.to_repr()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub struct Diversifier(pub [u8; 11]);

impl Diversifier {
    pub fn g_d(&self) -> Option<jubjub::SubgroupPoint> {
        group_hash(&self.0, constants::KEY_DIVERSIFICATION_PERSONALIZATION)
    }
}

/// A Sapling payment address.
///
/// # Invariants
///
/// `pk_d` is guaranteed to be prime-order (i.e. in the prime-order subgroup of Jubjub,
/// and not the identity).
#[derive(Clone, Copy, Debug)]
pub struct PaymentAddress {
    pk_d: jubjub::SubgroupPoint,
    diversifier: Diversifier,
}

impl PartialEq for PaymentAddress {
    fn eq(&self, other: &Self) -> bool {
        self.pk_d == other.pk_d && self.diversifier == other.diversifier
    }
}

impl Eq for PaymentAddress {}

impl PaymentAddress {
    /// Constructs a PaymentAddress from a diversifier and a Jubjub point.
    ///
    /// Returns None if `pk_d` is the identity.
    pub fn from_parts(diversifier: Diversifier, pk_d: jubjub::SubgroupPoint) -> Option<Self> {
        if pk_d.is_identity().into() {
            None
        } else {
            Some(PaymentAddress { pk_d, diversifier })
        }
    }

    /// Constructs a PaymentAddress from a diversifier and a Jubjub point.
    ///
    /// Only for test code, as this explicitly bypasses the invariant.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn from_parts_unchecked(
        diversifier: Diversifier,
        pk_d: jubjub::SubgroupPoint,
    ) -> Self {
        PaymentAddress { pk_d, diversifier }
    }

    /// Parses a PaymentAddress from bytes.
    pub fn from_bytes(bytes: &[u8; 43]) -> Option<Self> {
        let diversifier = {
            let mut tmp = [0; 11];
            tmp.copy_from_slice(&bytes[0..11]);
            Diversifier(tmp)
        };
        // Check that the diversifier is valid
        diversifier.g_d()?;

        let pk_d = jubjub::SubgroupPoint::from_bytes(bytes[11..43].try_into().unwrap());
        if pk_d.is_some().into() {
            PaymentAddress::from_parts(diversifier, pk_d.unwrap())
        } else {
            None
        }
    }

    /// Returns the byte encoding of this `PaymentAddress`.
    pub fn to_bytes(&self) -> [u8; 43] {
        let mut bytes = [0; 43];
        bytes[0..11].copy_from_slice(&self.diversifier.0);
        bytes[11..].copy_from_slice(&self.pk_d.to_bytes());
        bytes
    }

    /// Returns the [`Diversifier`] for this `PaymentAddress`.
    pub fn diversifier(&self) -> &Diversifier {
        &self.diversifier
    }

    /// Returns `pk_d` for this `PaymentAddress`.
    pub fn pk_d(&self) -> &jubjub::SubgroupPoint {
        &self.pk_d
    }

    pub fn g_d(&self) -> Option<jubjub::SubgroupPoint> {
        self.diversifier.g_d()
    }

    pub fn create_note(&self, asset_type: AssetType, value: u64, rseed: Rseed) -> Option<Note> {
        self.g_d().map(|g_d| Note {
            asset_type,
            value,
            rseed,
            g_d,
            pk_d: self.pk_d,
        })
    }
}

impl Display for PaymentAddress {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{}", hex::encode(self.to_bytes()))
    }
}

impl FromStr for PaymentAddress {
    type Err = io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let vec = hex::decode(s).map_err(|x| io::Error::new(io::ErrorKind::InvalidData, x))?;
        BorshDeserialize::try_from_slice(&vec)
    }
}

impl PartialOrd for PaymentAddress {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.to_bytes().partial_cmp(&other.to_bytes())
    }
}
impl Ord for PaymentAddress {
    fn cmp(&self, other: &Self) -> Ordering {
        self.to_bytes().cmp(&other.to_bytes())
    }
}
impl Hash for PaymentAddress {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.to_bytes().hash(state)
    }
}

impl BorshSerialize for PaymentAddress {
    fn serialize<W: Write>(&self, writer: &mut W) -> borsh::maybestd::io::Result<()> {
        writer.write(self.to_bytes().as_ref()).and(Ok(()))
    }
}
impl BorshDeserialize for PaymentAddress {
    fn deserialize(buf: &mut &[u8]) -> borsh::maybestd::io::Result<Self> {
        let data = buf
            .get(..43)
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        let res = Self::from_bytes(data.try_into().unwrap());
        let pa = res.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
        *buf = &buf[43..];
        Ok(pa)
    }
}

pub use crate::sapling::note::nullifier::Nullifier;
pub use crate::sapling::note::Rseed;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoteValue(u64);

impl TryFrom<u64> for NoteValue {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if value <= MAX_MONEY as u64 {
            Ok(NoteValue(value))
        } else {
            Err(())
        }
    }
}

impl From<NoteValue> for u64 {
    fn from(value: NoteValue) -> u64 {
        value.0
    }
}
pub use crate::sapling::note::Note;

#[cfg(any(test, feature = "test-dependencies"))]
pub mod testing {
    use proptest::prelude::*;
    use std::cmp::min;

    use crate::transaction::components::amount::MAX_MONEY;

    use super::{
        keys::testing::arb_full_viewing_key, Diversifier, Node, Note, NoteValue, PaymentAddress,
        Rseed, SaplingIvk,
    };

    prop_compose! {
        pub fn arb_note_value()(value in 0u64..=MAX_MONEY as u64) -> NoteValue {
            NoteValue::try_from(value).unwrap()
        }
    }

    prop_compose! {
        /// The
        pub fn arb_positive_note_value(bound: u64)(
            value in 1u64..=(min(bound, MAX_MONEY as u64))
        ) -> NoteValue {
            NoteValue::try_from(value).unwrap()
        }
    }

    prop_compose! {
        pub fn arb_incoming_viewing_key()(fvk in arb_full_viewing_key()) -> SaplingIvk {
            fvk.vk.ivk()
        }
    }

    pub fn arb_payment_address() -> impl Strategy<Value = PaymentAddress> {
        arb_incoming_viewing_key().prop_flat_map(|ivk: SaplingIvk| {
            any::<[u8; 11]>().prop_filter_map(
                "Sampled diversifier must generate a valid Sapling payment address.",
                move |d| ivk.to_payment_address(Diversifier(d)),
            )
        })
    }

    prop_compose! {
        pub fn arb_node()(value in prop::array::uniform32(prop::num::u8::ANY)) -> Node {
            Node {
                repr: value
            }
        }
    }

    prop_compose! {
        pub fn arb_note(value: NoteValue)(
            asset_type in crate::asset_type::testing::arb_asset_type(),
            addr in arb_payment_address(),
            rseed in prop::array::uniform32(prop::num::u8::ANY).prop_map(Rseed::AfterZip212)
                ) -> Note {
            Note {
                value: value.into(),
                g_d: addr.g_d().unwrap(), // this unwrap is safe because arb_payment_address always generates an address with a valid g_d
                pk_d: *addr.pk_d(),
                rseed,
                asset_type
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        sapling::testing::{arb_note, arb_positive_note_value},
        sapling::Note,
        transaction::components::amount::MAX_MONEY,
    };
    use borsh::{BorshDeserialize, BorshSerialize};
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10))]
        #[test]
        fn note_serialization(note in arb_positive_note_value(MAX_MONEY as u64).prop_flat_map(arb_note)) {
            // BorshSerialize
            let borsh = note.try_to_vec().unwrap();
            // BorshDeserialize
            let de_note: Note = BorshDeserialize::deserialize(&mut borsh.as_ref()).unwrap();
            prop_assert_eq!(note, de_note);
        }
    }
}

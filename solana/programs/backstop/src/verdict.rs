//! Oracle verdict verification (eng review D1 + D2).
//!
//! The oracle signs a verdict off-chain with a throwaway Ed25519 key. The transaction carries a native
//! Ed25519 precompile instruction **immediately before** the backstop instruction. The runtime verifies the
//! signature itself; this module proves that the verified signature is the one we expect: exactly one
//! signature, every offset pointing into that same precompile instruction, signed by the configured oracle,
//! over exactly the domain-separated message rebuilt from the instruction arguments.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{get_stack_height, TRANSACTION_LEVEL_STACK_HEIGHT};
use solana_instructions_sysvar::{load_current_index_checked, load_instruction_at_checked};

use crate::errors::VerdictError;

/// Domain tag (v2: the oracle signs facts, not a payout amount — see the LOCKED verdict spec).
/// Bump the version when the message layout changes; old signatures then stop verifying.
pub const DOMAIN: &[u8; 17] = b"SAFU_STOCKLANA_V2";
/// DOMAIN(17) + program id(32) + cluster(1) + liquidation record(32) + borrower(32) +
/// ref_at_liq(8) + ref_after(8) + after_ts(8) + evidence hash(32) + deadline(8).
pub const MESSAGE_LEN: usize = 178;
/// A signed verdict must be submitted within this window of being signed.
pub const MAX_VERDICT_TTL_SECS: i64 = 86_400;

pub const CLUSTER_LOCALNET: u8 = 0;
pub const CLUSTER_DEVNET: u8 = 1;
pub const CLUSTER_MAINNET: u8 = 2;

// Layout of the Ed25519 precompile instruction, from solana-ed25519-program 3.0.0.
const SIGNATURE_OFFSETS_START: usize = 2;
const SIGNATURE_OFFSETS_SERIALIZED_SIZE: usize = 14;
const DATA_START: usize = SIGNATURE_OFFSETS_START + SIGNATURE_OFFSETS_SERIALIZED_SIZE;
const PUBKEY_LEN: usize = 32;
const SIGNATURE_LEN: usize = 64;

/// The oracle-signed facts (LOCKED verdict spec): a reference price at the liquidation, a reference
/// price after a wait to prove the move didn't hold, and an evidence hash. No payout amount, no
/// tier — the program computes and bounds the loss itself from these plus on-chain state.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct FactsArgs {
    pub liquidation_record: Pubkey,
    pub borrower: Pubkey,
    /// USD per whole share, 8 decimals, independent of the feed that liquidated.
    pub ref_at_liq: u64,
    /// Same reference source, sampled `after_ts`.
    pub ref_after: u64,
    /// Unix seconds the second sample was taken. Must be `MIN_AFTER_WAIT_SECS..=MAX_AFTER_WAIT_SECS`
    /// after the liquidation.
    pub after_ts: i64,
    /// sha256 over the verdict evidence, including the archived reference-price responses.
    pub evidence_hash: [u8; 32],
    /// Unix seconds. The submission must land at or before this time.
    pub deadline: i64,
}

/// The exact bytes the oracle signs.
pub fn encode_message(program_id: &Pubkey, cluster_tag: u8, args: &FactsArgs) -> Vec<u8> {
    let mut m = Vec::with_capacity(MESSAGE_LEN);
    m.extend_from_slice(DOMAIN);
    m.extend_from_slice(program_id.as_ref());
    m.push(cluster_tag);
    m.extend_from_slice(args.liquidation_record.as_ref());
    m.extend_from_slice(args.borrower.as_ref());
    m.extend_from_slice(&args.ref_at_liq.to_le_bytes());
    m.extend_from_slice(&args.ref_after.to_le_bytes());
    m.extend_from_slice(&args.after_ts.to_le_bytes());
    m.extend_from_slice(&args.evidence_hash);
    m.extend_from_slice(&args.deadline.to_le_bytes());
    debug_assert_eq!(m.len(), MESSAGE_LEN);
    m
}

fn read_u16(data: &[u8], at: usize) -> Result<u16> {
    let bytes = data
        .get(
            at..at
                .checked_add(2)
                .ok_or(VerdictError::MalformedEd25519Instruction)?,
        )
        .ok_or(VerdictError::MalformedEd25519Instruction)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn slice_at(data: &[u8], offset: u16, len: usize) -> Result<&[u8]> {
    let start = offset as usize;
    let end = start
        .checked_add(len)
        .ok_or(VerdictError::MalformedEd25519Instruction)?;
    Ok(data
        .get(start..end)
        .ok_or(VerdictError::MalformedEd25519Instruction)?)
}

/// Checks the raw data of the Ed25519 precompile instruction found at `own_index`.
/// Pure function: no sysvar access, so every malformed shape is unit-testable.
pub fn check_ed25519_data(
    data: &[u8],
    own_index: u16,
    expected_signer: &Pubkey,
    expected_message: &[u8],
) -> Result<()> {
    require!(
        data.len() >= DATA_START,
        VerdictError::MalformedEd25519Instruction
    );
    require!(data[0] == 1, VerdictError::WrongSignatureCount);
    require!(data[1] == 0, VerdictError::MalformedEd25519Instruction);

    let signature_offset = read_u16(data, 2)?;
    let signature_ix = read_u16(data, 4)?;
    let pubkey_offset = read_u16(data, 6)?;
    let pubkey_ix = read_u16(data, 8)?;
    let message_offset = read_u16(data, 10)?;
    let message_size = read_u16(data, 12)?;
    let message_ix = read_u16(data, 14)?;

    // D2: the verified signature, key and message must all live inside this very instruction, so nothing
    // verified elsewhere in the transaction can be passed off as our verdict.
    require!(
        signature_ix == own_index && pubkey_ix == own_index && message_ix == own_index,
        VerdictError::OffsetsOutsideEd25519Instruction
    );

    slice_at(data, signature_offset, SIGNATURE_LEN)?;
    let pubkey = slice_at(data, pubkey_offset, PUBKEY_LEN)?;
    require!(
        pubkey == expected_signer.as_ref(),
        VerdictError::WrongVerdictSigner
    );

    require!(
        message_size as usize == expected_message.len(),
        VerdictError::VerdictMessageMismatch
    );
    let message = slice_at(data, message_offset, message_size as usize)?;
    require!(
        message == expected_message,
        VerdictError::VerdictMessageMismatch
    );
    Ok(())
}

/// Finds the Ed25519 precompile instruction directly before the current top-level instruction and checks it.
pub fn verify_preceding_ed25519(
    instructions_sysvar: &AccountInfo,
    expected_signer: &Pubkey,
    expected_message: &[u8],
) -> Result<()> {
    // Top-level only: the sysvar's "current index" names the outer instruction, so a CPI caller could
    // otherwise borrow a neighbouring precompile instruction it does not control the meaning of.
    require!(
        get_stack_height() == TRANSACTION_LEVEL_STACK_HEIGHT,
        VerdictError::VerdictNotTopLevel
    );
    let current = load_current_index_checked(instructions_sysvar)?;
    let own_index = current
        .checked_sub(1)
        .ok_or(VerdictError::MissingEd25519Instruction)?;
    let ix = load_instruction_at_checked(own_index as usize, instructions_sysvar)
        .map_err(|_| VerdictError::MissingEd25519Instruction)?;
    require_keys_eq!(
        ix.program_id,
        solana_sdk_ids::ed25519_program::ID,
        VerdictError::MissingEd25519Instruction
    );
    require!(
        ix.accounts.is_empty(),
        VerdictError::MalformedEd25519Instruction
    );
    check_ed25519_data(&ix.data, own_index, expected_signer, expected_message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> FactsArgs {
        FactsArgs {
            liquidation_record: Pubkey::new_from_array([7; 32]),
            borrower: Pubkey::new_from_array([8; 32]),
            ref_at_liq: 40_000_000_000,
            ref_after: 40_100_000_000,
            after_ts: 1_000_003_600,
            evidence_hash: [9; 32],
            deadline: 1_800_000_000,
        }
    }

    /// Builds precompile-shaped data with one signature. The signature bytes are dummies: the runtime
    /// verifies them, this parser only checks structure, key and message.
    fn data(signer: &Pubkey, message: &[u8], indexes: [u16; 3]) -> Vec<u8> {
        let sig_off = DATA_START as u16;
        let pk_off = sig_off + SIGNATURE_LEN as u16;
        let msg_off = pk_off + PUBKEY_LEN as u16;
        let mut d = vec![1u8, 0u8];
        for v in [
            sig_off,
            indexes[0],
            pk_off,
            indexes[1],
            msg_off,
            message.len() as u16,
            indexes[2],
        ] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.extend_from_slice(&[0xAB; SIGNATURE_LEN]);
        d.extend_from_slice(signer.as_ref());
        d.extend_from_slice(message);
        d
    }

    fn code(r: Result<()>) -> u32 {
        match r {
            Err(anchor_lang::error::Error::AnchorError(e)) => e.error_code_number,
            other => panic!("expected an anchor error, got {other:?}"),
        }
    }

    fn err(e: VerdictError) -> u32 {
        anchor_lang::error::ERROR_CODE_OFFSET + e as u32
    }

    #[test]
    fn message_is_exactly_178_bytes_and_starts_with_domain() {
        let m = encode_message(&Pubkey::new_from_array([1; 32]), CLUSTER_DEVNET, &args());
        assert_eq!(m.len(), MESSAGE_LEN);
        assert_eq!(&m[..17], DOMAIN);
    }

    #[test]
    fn every_field_changes_the_message() {
        let pid = Pubkey::new_from_array([1; 32]);
        let base = encode_message(&pid, CLUSTER_DEVNET, &args());
        let mut variants = Vec::new();
        variants.push(encode_message(
            &Pubkey::new_from_array([2; 32]),
            CLUSTER_DEVNET,
            &args(),
        ));
        variants.push(encode_message(&pid, CLUSTER_MAINNET, &args()));
        for f in 0..7 {
            let mut a = args();
            match f {
                0 => a.liquidation_record = Pubkey::new_from_array([70; 32]),
                1 => a.borrower = Pubkey::new_from_array([80; 32]),
                2 => a.ref_at_liq += 1,
                3 => a.ref_after += 1,
                4 => a.after_ts += 1,
                5 => a.evidence_hash[0] ^= 1,
                _ => a.deadline += 1,
            }
            variants.push(encode_message(&pid, CLUSTER_DEVNET, &a));
        }
        for v in variants {
            assert_ne!(v, base);
        }
    }

    #[test]
    fn valid_shape_passes() {
        let signer = Pubkey::new_from_array([5; 32]);
        let msg = encode_message(&Pubkey::new_from_array([1; 32]), CLUSTER_DEVNET, &args());
        assert!(check_ed25519_data(&data(&signer, &msg, [3, 3, 3]), 3, &signer, &msg).is_ok());
    }

    #[test]
    fn truncated_header_rejected() {
        let signer = Pubkey::new_from_array([5; 32]);
        for len in 0..DATA_START {
            let d = vec![1u8; len];
            assert_eq!(
                code(check_ed25519_data(&d, 0, &signer, b"x")),
                err(VerdictError::MalformedEd25519Instruction)
            );
        }
    }

    #[test]
    fn signature_count_other_than_one_rejected() {
        let signer = Pubkey::new_from_array([5; 32]);
        let msg = b"m".to_vec();
        for count in [0u8, 2, 255] {
            let mut d = data(&signer, &msg, [0, 0, 0]);
            d[0] = count;
            assert_eq!(
                code(check_ed25519_data(&d, 0, &signer, &msg)),
                err(VerdictError::WrongSignatureCount)
            );
        }
    }

    #[test]
    fn nonzero_padding_rejected() {
        let signer = Pubkey::new_from_array([5; 32]);
        let msg = b"m".to_vec();
        let mut d = data(&signer, &msg, [0, 0, 0]);
        d[1] = 1;
        assert_eq!(
            code(check_ed25519_data(&d, 0, &signer, &msg)),
            err(VerdictError::MalformedEd25519Instruction)
        );
    }

    #[test]
    fn each_instruction_index_pointing_elsewhere_rejected() {
        let signer = Pubkey::new_from_array([5; 32]);
        let msg = b"m".to_vec();
        for which in 0..3 {
            let mut idx = [2u16, 2, 2];
            idx[which] = 1;
            assert_eq!(
                code(check_ed25519_data(
                    &data(&signer, &msg, idx),
                    2,
                    &signer,
                    &msg
                )),
                err(VerdictError::OffsetsOutsideEd25519Instruction)
            );
        }
    }

    #[test]
    fn out_of_bounds_and_overflowing_offsets_rejected() {
        let signer = Pubkey::new_from_array([5; 32]);
        let msg = b"m".to_vec();
        for field_at in [2usize, 6, 10] {
            for bad in [u16::MAX, u16::MAX - 1, 60_000] {
                let mut d = data(&signer, &msg, [0, 0, 0]);
                d[field_at..field_at + 2].copy_from_slice(&bad.to_le_bytes());
                assert_eq!(
                    code(check_ed25519_data(&d, 0, &signer, &msg)),
                    err(VerdictError::MalformedEd25519Instruction)
                );
            }
        }
        // message size larger than the data that remains
        let mut d = data(&signer, &msg, [0, 0, 0]);
        d[12..14].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(check_ed25519_data(&d, 0, &signer, &msg).is_err());
    }

    #[test]
    fn wrong_signer_rejected() {
        let signer = Pubkey::new_from_array([5; 32]);
        let msg = b"m".to_vec();
        let other = Pubkey::new_from_array([6; 32]);
        assert_eq!(
            code(check_ed25519_data(
                &data(&other, &msg, [0, 0, 0]),
                0,
                &signer,
                &msg
            )),
            err(VerdictError::WrongVerdictSigner)
        );
    }

    #[test]
    fn different_or_resized_message_rejected() {
        let signer = Pubkey::new_from_array([5; 32]);
        let msg = b"verdict".to_vec();
        assert_eq!(
            code(check_ed25519_data(
                &data(&signer, b"verdicT", [0, 0, 0]),
                0,
                &signer,
                &msg
            )),
            err(VerdictError::VerdictMessageMismatch)
        );
        assert_eq!(
            code(check_ed25519_data(
                &data(&signer, b"verdict!", [0, 0, 0]),
                0,
                &signer,
                &msg
            )),
            err(VerdictError::VerdictMessageMismatch)
        );
    }
}

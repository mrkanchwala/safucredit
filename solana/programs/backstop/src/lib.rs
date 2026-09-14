use anchor_lang::prelude::*;

pub mod errors;
pub mod state;
pub mod verdict;

use errors::VerdictError;
use state::*;
use verdict::{VerdictArgs, MAX_VERDICT_TTL_SECS};

declare_id!("H1hApKnNkYPsqQ9WVZGzqZQZYirxCLpXDj2uEmzMK3YV");

#[program]
pub mod backstop {
    use super::*;

    /// Records an oracle-signed wrongful-liquidation verdict. The transaction must place the oracle's
    /// Ed25519 precompile instruction directly before this one. Anyone may submit; the signature is the
    /// authorization. The payout itself happens in a later instruction that consumes this attestation.
    pub fn attest_verdict(ctx: Context<AttestVerdict>, args: VerdictArgs) -> Result<()> {
        require!(args.payout > 0, VerdictError::ZeroPayout);
        require!((1..=3).contains(&args.tier), VerdictError::InvalidTier);

        let now = Clock::get()?.unix_timestamp;
        require!(now <= args.deadline, VerdictError::VerdictExpired);
        require!(
            args.deadline
                <= now
                    .checked_add(MAX_VERDICT_TTL_SECS)
                    .ok_or(VerdictError::VerdictDeadlineTooFar)?,
            VerdictError::VerdictDeadlineTooFar
        );

        let config = &ctx.accounts.config;
        let message = verdict::encode_message(&crate::ID, config.cluster_tag, &args);
        verdict::verify_preceding_ed25519(
            &ctx.accounts.instructions_sysvar.to_account_info(),
            &config.verdict_oracle,
            &message,
        )?;

        let attestation = &mut ctx.accounts.attestation;
        attestation.version = ACCOUNT_VERSION;
        attestation.liquidation_record = args.liquidation_record;
        attestation.borrower = args.borrower;
        attestation.payout = args.payout;
        attestation.tier = args.tier;
        attestation.verdict_hash = args.verdict_hash;
        attestation.attested_at = now;
        attestation.bump = ctx.bumps.attestation;

        emit!(VerdictAttested {
            liquidation_record: args.liquidation_record,
            borrower: args.borrower,
            payout: args.payout,
            tier: args.tier,
            verdict_hash: args.verdict_hash,
        });
        Ok(())
    }
}

#[derive(Accounts)]
#[instruction(args: VerdictArgs)]
pub struct AttestVerdict<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, BackstopConfig>,

    #[account(
        init,
        payer = payer,
        space = 8 + VerdictAttestation::INIT_SPACE,
        seeds = [ATTESTATION_SEED, args.liquidation_record.as_ref()],
        bump,
    )]
    pub attestation: Account<'info, VerdictAttestation>,

    /// CHECK: address-constrained to the instructions sysvar; read only through the checked loaders.
    #[account(address = solana_instructions_sysvar::ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

#[event]
pub struct VerdictAttested {
    pub liquidation_record: Pubkey,
    pub borrower: Pubkey,
    pub payout: u64,
    pub tier: u8,
    pub verdict_hash: [u8; 32],
}

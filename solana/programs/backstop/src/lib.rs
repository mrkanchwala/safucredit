use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

pub mod errors;
pub mod state;
pub mod verdict;

use errors::VerdictError;
use state::*;
use verdict::{VerdictArgs, MAX_VERDICT_TTL_SECS};

/// `amount × numerator / denominator`, rounded down.
fn mul_div_floor(amount: u128, numerator: u128, denominator: u128) -> Result<u128> {
    amount
        .checked_mul(numerator)
        .and_then(|v| v.checked_div(denominator))
        .ok_or_else(|| VerdictError::MathOverflow.into())
}

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

        // Backers cannot exit between a verdict being attested and it being paid (A5 §9).
        let config = &mut ctx.accounts.config;
        config.open_claims = config
            .open_claims
            .checked_add(1)
            .ok_or(VerdictError::MathOverflow)?;

        emit!(VerdictAttested {
            liquidation_record: args.liquidation_record,
            borrower: args.borrower,
            payout: args.payout,
            tier: args.tier,
            verdict_hash: args.verdict_hash,
        });
        Ok(())
    }

    /// NOTE: the first caller becomes admin, matching the lending vault. Gating this on the
    /// program's upgrade authority lands with upgradeability (stage 2 item 5); until then deploy
    /// and initialize in the same session.
    pub fn initialize_backstop(
        ctx: Context<InitializeBackstop>,
        verdict_oracle: Pubkey,
        cluster_tag: u8,
        per_claim_cap_bps: u32,
        withdraw_delay_secs: i64,
    ) -> Result<()> {
        require!(
            per_claim_cap_bps > 0 && per_claim_cap_bps <= MAX_PER_CLAIM_CAP_BPS,
            VerdictError::InvalidParams
        );
        require!(
            (0..=30 * 86_400).contains(&withdraw_delay_secs),
            VerdictError::InvalidParams
        );
        let config = &mut ctx.accounts.config;
        config.version = ACCOUNT_VERSION;
        config.admin = ctx.accounts.admin.key();
        config.verdict_oracle = verdict_oracle;
        config.cluster_tag = cluster_tag;
        config.bump = ctx.bumps.config;
        config.usdc_mint = ctx.accounts.usdc_mint.key();
        config.cash = 0;
        config.total_shares = 0;
        config.per_claim_cap_bps = per_claim_cap_bps;
        config.open_claims = 0;
        config.withdraw_delay_secs = withdraw_delay_secs;
        config.reserved = [0; 32];
        Ok(())
    }

    /// Rotates the verdict oracle key (U5). It is a throwaway per-chain Ed25519 key, so a leak has
    /// to be recoverable on-chain: without this, replacing it would need a program upgrade.
    /// Signatures already attested stay valid; unattested ones signed by the old key stop verifying.
    pub fn set_verdict_oracle(ctx: Context<AdminOnly>, verdict_oracle: Pubkey) -> Result<()> {
        ctx.accounts.config.verdict_oracle = verdict_oracle;
        emit!(VerdictOracleRotated { verdict_oracle });
        Ok(())
    }

    /// Admin-settable within hard bounds (U2), so neither knob can be set somewhere unsafe: a zero
    /// per-claim cap would freeze every payout, and an unbounded withdrawal delay would trap backers.
    pub fn set_backstop_params(
        ctx: Context<AdminOnly>,
        per_claim_cap_bps: u32,
        withdraw_delay_secs: i64,
    ) -> Result<()> {
        require!(
            per_claim_cap_bps > 0 && per_claim_cap_bps <= MAX_PER_CLAIM_CAP_BPS,
            VerdictError::InvalidParams
        );
        require!(
            (0..=30 * 86_400).contains(&withdraw_delay_secs),
            VerdictError::InvalidParams
        );
        let config = &mut ctx.accounts.config;
        config.per_claim_cap_bps = per_claim_cap_bps;
        config.withdraw_delay_secs = withdraw_delay_secs;
        Ok(())
    }

    /// Hands the admin role over (U5). No renounce path: an unowned backstop could never rotate a
    /// leaked oracle key.
    pub fn set_admin(ctx: Context<AdminOnly>, admin: Pubkey) -> Result<()> {
        require_keys_neq!(admin, Pubkey::default(), VerdictError::InvalidParams);
        ctx.accounts.config.admin = admin;
        Ok(())
    }

    pub fn open_backer(ctx: Context<OpenBacker>) -> Result<()> {
        let backer = &mut ctx.accounts.backer;
        backer.version = ACCOUNT_VERSION;
        backer.bump = ctx.bumps.backer;
        backer.owner = ctx.accounts.owner.key();
        backer.shares = 0;
        backer.withdraw_shares = 0;
        backer.withdraw_requested_at = 0;
        Ok(())
    }

    /// Backer capital in. Shares are priced off `config.cash`, never the token account balance, so
    /// a raw transfer into the vault cannot move the share price.
    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        require!(amount > 0, VerdictError::ZeroAmount);
        let config = &ctx.accounts.config;
        let shares = if config.total_shares == 0 {
            amount as u128
        } else {
            // Unreachable while every outgoing path is capped below 100% (see `cover_bad_debt`);
            // kept as an explicit guard because the alternative is minting against a zero
            // denominator, and a wiped pool must fail loudly rather than silently hand a new
            // depositor's capital to worthless legacy shares.
            require!(config.cash > 0, VerdictError::InsufficientBalance);
            mul_div_floor(amount as u128, config.total_shares, config.cash as u128)?
        };
        require!(shares > 0, VerdictError::ZeroAmount);

        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.usdc_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.owner_usdc.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.usdc_vault.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.usdc_mint.decimals,
        )?;

        let config = &mut ctx.accounts.config;
        config.cash = config
            .cash
            .checked_add(amount)
            .ok_or(VerdictError::MathOverflow)?;
        config.total_shares = config
            .total_shares
            .checked_add(shares)
            .ok_or(VerdictError::MathOverflow)?;
        let backer = &mut ctx.accounts.backer;
        backer.shares = backer
            .shares
            .checked_add(shares)
            .ok_or(VerdictError::MathOverflow)?;
        emit!(Deposited {
            owner: backer.owner,
            amount,
            shares
        });
        Ok(())
    }

    /// Queues an exit. The delay exists so capital cannot leave in front of a verdict that is
    /// already being prepared off-chain but not yet attested.
    pub fn request_withdraw(ctx: Context<BackerOnly>, shares: u128) -> Result<()> {
        require!(shares > 0, VerdictError::ZeroAmount);
        let backer = &mut ctx.accounts.backer;
        require!(shares <= backer.shares, VerdictError::InsufficientBalance);
        backer.withdraw_shares = shares;
        backer.withdraw_requested_at = Clock::get()?.unix_timestamp;
        emit!(WithdrawRequested {
            owner: backer.owner,
            shares
        });
        Ok(())
    }

    /// Takes the money out, but never while a claim is outstanding: an attested verdict is a
    /// liability the remaining backers would otherwise be left holding alone.
    pub fn finalize_withdraw(ctx: Context<FinalizeWithdraw>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let config_bump = ctx.accounts.config.bump;
        let (cash, total_shares, delay, open_claims) = {
            let c = &ctx.accounts.config;
            (c.cash, c.total_shares, c.withdraw_delay_secs, c.open_claims)
        };
        require!(open_claims == 0, VerdictError::ClaimsOutstanding);

        let backer = &mut ctx.accounts.backer;
        let shares = backer.withdraw_shares;
        require!(shares > 0, VerdictError::NoWithdrawalPending);
        require!(
            now >= backer.withdraw_requested_at.saturating_add(delay),
            VerdictError::WithdrawalNotReady
        );
        require!(shares <= backer.shares, VerdictError::InsufficientBalance);
        require!(total_shares > 0, VerdictError::InsufficientBalance);

        let amount = u64::try_from(mul_div_floor(shares, cash as u128, total_shares)?)
            .map_err(|_| VerdictError::MathOverflow)?;
        require!(
            amount > 0 && amount <= cash,
            VerdictError::InsufficientBalance
        );

        backer.shares -= shares;
        backer.withdraw_shares = 0;
        backer.withdraw_requested_at = 0;
        let owner = backer.owner;

        let seeds: &[&[&[u8]]] = &[&[CONFIG_SEED, &[config_bump]]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.usdc_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.usdc_vault.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.owner_usdc.to_account_info(),
                    authority: ctx.accounts.config.to_account_info(),
                },
                seeds,
            ),
            amount,
            ctx.accounts.usdc_mint.decimals,
        )?;

        let config = &mut ctx.accounts.config;
        config.cash -= amount;
        config.total_shares -= shares;
        emit!(WithdrawFinalized {
            owner,
            amount,
            shares
        });
        Ok(())
    }

    /// Pays a borrower back for a liquidation the verdict engine proved was wrongful. Permissionless:
    /// the oracle signature recorded in the attestation is the authorization, not the caller.
    pub fn pay_wrongful_liquidation(ctx: Context<PayWrongfulLiquidation>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let record = &ctx.accounts.liquidation_record;
        let attestation = &ctx.accounts.attestation;

        require_keys_eq!(
            attestation.liquidation_record,
            record.key(),
            VerdictError::RecordMismatch
        );
        require_keys_eq!(
            attestation.borrower,
            record.borrower,
            VerdictError::RecordMismatch
        );
        require_keys_eq!(
            record.borrower,
            ctx.accounts.borrower.key(),
            VerdictError::RecordMismatch
        );
        // Spec 14a: an issuer freeze, pause or seizure is never covered, and the record captured
        // whether the issuer had intervened at the moment of the liquidation.
        require!(!record.issuer_halt, VerdictError::IssuerHaltedLiquidation);
        // Spec 13c: a borrower liquidating themselves is not a wrongful liquidation.
        require_keys_neq!(
            record.liquidator,
            record.borrower,
            VerdictError::SelfDealtLiquidation
        );

        // A4: someone backing the pool cannot also be paid out of it. An absent account is fine; an
        // existing one must hold nothing, including nothing queued for exit.
        let backer_info = ctx.accounts.borrower_backer.to_account_info();
        if !backer_info.data_is_empty() {
            require_keys_eq!(
                *backer_info.owner,
                crate::ID,
                VerdictError::BorrowerIsABacker
            );
            let data = backer_info.try_borrow_data()?;
            let backer = Backer::try_deserialize(&mut &data[..])?;
            require!(
                backer.shares == 0 && backer.withdraw_shares == 0,
                VerdictError::BorrowerIsABacker
            );
        }

        let (cash, cap_bps, config_bump) = {
            let c = &ctx.accounts.config;
            (c.cash, c.per_claim_cap_bps, c.bump)
        };
        // E4: capped and disclosed rather than queued pro-rata. The shortfall is recorded on the
        // receipt so it is visible rather than silently dropped.
        let cap = safu_core::apply_bps(cash, cap_bps).map_err(|_| VerdictError::MathOverflow)?;
        let paid = attestation.payout.min(cap);
        require!(paid > 0, VerdictError::NothingToPay);

        let seeds: &[&[&[u8]]] = &[&[CONFIG_SEED, &[config_bump]]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.usdc_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.usdc_vault.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.borrower_usdc.to_account_info(),
                    authority: ctx.accounts.config.to_account_info(),
                },
                seeds,
            ),
            paid,
            ctx.accounts.usdc_mint.decimals,
        )?;

        let receipt = &mut ctx.accounts.receipt;
        receipt.version = ACCOUNT_VERSION;
        receipt.bump = ctx.bumps.receipt;
        receipt.liquidation_record = record.key();
        receipt.borrower = record.borrower;
        receipt.attested = attestation.payout;
        receipt.paid = paid;
        receipt.verdict_hash = attestation.verdict_hash;
        receipt.paid_at = now;
        receipt.reserved = [0; 32];

        let config = &mut ctx.accounts.config;
        config.cash -= paid;
        config.open_claims = config.open_claims.saturating_sub(1);

        emit!(WrongfulLiquidationPaid {
            liquidation_record: receipt.liquidation_record,
            borrower: receipt.borrower,
            attested: receipt.attested,
            paid,
        });
        Ok(())
    }

    pub fn open_bad_debt_cover(ctx: Context<OpenBadDebtCover>) -> Result<()> {
        let cover = &mut ctx.accounts.cover;
        cover.version = ACCOUNT_VERSION;
        cover.bump = ctx.bumps.cover;
        cover.market = ctx.accounts.market.key();
        cover.total_paid = 0;
        Ok(())
    }

    /// Reimburses a lending market for debt its collateral could not cover. Permissionless.
    ///
    /// What is owed is the market's own monotonic write-off total minus what this backstop has
    /// already paid it, so repeat calls settle the remainder and never double-pay — no epochs to
    /// keep in step. The USDC simply lands in the market's vault; the market recognises it through
    /// its own `absorb_bad_debt_cover`, which is bounded by its outstanding bad debt.
    pub fn cover_bad_debt(ctx: Context<CoverBadDebt>) -> Result<()> {
        let market = &ctx.accounts.market;
        let market_key = market.key();
        // The destination must be the market's own USDC vault, derived from the vault program.
        let (expected_vault, _) = Pubkey::find_program_address(
            &[stock_vault::state::USDC_VAULT_SEED, market_key.as_ref()],
            &stock_vault::ID,
        );
        require_keys_eq!(
            ctx.accounts.market_usdc_vault.key(),
            expected_vault,
            VerdictError::RecordMismatch
        );

        let cover = &ctx.accounts.cover;
        let owed = market.bad_debt_cumulative.saturating_sub(cover.total_paid);
        require!(owed > 0, VerdictError::NoBadDebt);

        let (cash, cap_bps, config_bump) = {
            let c = &ctx.accounts.config;
            (c.cash, c.per_claim_cap_bps, c.bump)
        };
        // Bounded by the same per-call share as a claim, and for a second reason beyond fairness:
        // an uncapped payment could take the pool to exactly zero while shares were still
        // outstanding, leaving the share price undefined and no deposit ever able to price itself
        // again. With every outgoing path capped below 100%, cash cannot reach zero unless the last
        // backer withdraws, which clears the shares with it.
        let cap = safu_core::apply_bps(cash, cap_bps).map_err(|_| VerdictError::MathOverflow)?;
        let paid = owed.min(cap);
        require!(paid > 0, VerdictError::NothingToPay);

        let seeds: &[&[&[u8]]] = &[&[CONFIG_SEED, &[config_bump]]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.usdc_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.usdc_vault.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.market_usdc_vault.to_account_info(),
                    authority: ctx.accounts.config.to_account_info(),
                },
                seeds,
            ),
            paid,
            ctx.accounts.usdc_mint.decimals,
        )?;

        ctx.accounts.cover.total_paid = ctx
            .accounts
            .cover
            .total_paid
            .checked_add(paid)
            .ok_or(VerdictError::MathOverflow)?;
        let config = &mut ctx.accounts.config;
        config.cash -= paid;

        emit!(BadDebtReimbursed {
            market: market_key,
            paid,
            cumulative: ctx.accounts.cover.total_paid,
        });
        Ok(())
    }
}

#[derive(Accounts)]
#[instruction(args: VerdictArgs)]
pub struct AttestVerdict<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
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

#[derive(Accounts)]
pub struct InitializeBackstop<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + BackstopConfig::INIT_SPACE, seeds = [CONFIG_SEED], bump)]
    pub config: Box<Account<'info, BackstopConfig>>,
    #[account(mint::token_program = usdc_token_program)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(
        init,
        payer = admin,
        seeds = [USDC_VAULT_SEED],
        bump,
        token::mint = usdc_mint,
        token::authority = config,
        token::token_program = usdc_token_program,
    )]
    pub usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ VerdictError::Unauthorized)]
    pub config: Box<Account<'info, BackstopConfig>>,
}

#[derive(Accounts)]
pub struct OpenBacker<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, BackstopConfig>>,
    #[account(
        init,
        payer = owner,
        space = 8 + Backer::INIT_SPACE,
        seeds = [BACKER_SEED, owner.key().as_ref()],
        bump,
    )]
    pub backer: Box<Account<'info, Backer>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    pub owner: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, BackstopConfig>>,
    #[account(mut, seeds = [BACKER_SEED, owner.key().as_ref()], bump = backer.bump, has_one = owner)]
    pub backer: Box<Account<'info, Backer>>,
    #[account(address = config.usdc_mint, mint::token_program = usdc_token_program)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = usdc_mint, token::authority = owner, token::token_program = usdc_token_program)]
    pub owner_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [USDC_VAULT_SEED], bump, token::token_program = usdc_token_program)]
    pub usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct BackerOnly<'info> {
    pub owner: Signer<'info>,
    #[account(mut, seeds = [BACKER_SEED, owner.key().as_ref()], bump = backer.bump, has_one = owner)]
    pub backer: Box<Account<'info, Backer>>,
}

#[derive(Accounts)]
pub struct FinalizeWithdraw<'info> {
    pub owner: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, BackstopConfig>>,
    #[account(mut, seeds = [BACKER_SEED, owner.key().as_ref()], bump = backer.bump, has_one = owner)]
    pub backer: Box<Account<'info, Backer>>,
    #[account(address = config.usdc_mint, mint::token_program = usdc_token_program)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = usdc_mint, token::authority = owner, token::token_program = usdc_token_program)]
    pub owner_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [USDC_VAULT_SEED], bump, token::token_program = usdc_token_program)]
    pub usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct PayWrongfulLiquidation<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, BackstopConfig>>,
    #[account(
        seeds = [ATTESTATION_SEED, attestation.liquidation_record.as_ref()],
        bump = attestation.bump,
    )]
    pub attestation: Box<Account<'info, VerdictAttestation>>,
    /// Owned by the lending vault program — Anchor's `Owner` check is what proves this record was
    /// written by the vault and not fabricated.
    pub liquidation_record: Box<Account<'info, stock_vault::state::LiquidationRecord>>,
    /// `init` is the idempotency: a replayed payout transaction cannot create this twice.
    #[account(
        init,
        payer = payer,
        space = 8 + ClaimReceipt::INIT_SPACE,
        seeds = [CLAIM_SEED, liquidation_record.key().as_ref()],
        bump,
    )]
    pub receipt: Box<Account<'info, ClaimReceipt>>,
    /// CHECK: matched against the liquidation record's borrower; holds no data we read.
    pub borrower: UncheckedAccount<'info>,
    #[account(mut, token::mint = usdc_mint, token::authority = borrower, token::token_program = usdc_token_program)]
    pub borrower_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: the borrower's Backer PDA. May legitimately not exist; when it does, it is decoded and
    /// required to hold nothing (A4 self-dealing).
    #[account(seeds = [BACKER_SEED, borrower.key().as_ref()], bump)]
    pub borrower_backer: UncheckedAccount<'info>,
    #[account(address = config.usdc_mint, mint::token_program = usdc_token_program)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, seeds = [USDC_VAULT_SEED], bump, token::token_program = usdc_token_program)]
    pub usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct OpenBadDebtCover<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub market: Box<Account<'info, stock_vault::state::Market>>,
    #[account(
        init,
        payer = payer,
        space = 8 + BadDebtCover::INIT_SPACE,
        seeds = [BAD_DEBT_SEED, market.key().as_ref()],
        bump,
    )]
    pub cover: Box<Account<'info, BadDebtCover>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CoverBadDebt<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, BackstopConfig>>,
    /// Owned by the lending vault program, so its bad-debt counter cannot be fabricated.
    pub market: Box<Account<'info, stock_vault::state::Market>>,
    #[account(mut, seeds = [BAD_DEBT_SEED, market.key().as_ref()], bump = cover.bump, has_one = market)]
    pub cover: Box<Account<'info, BadDebtCover>>,
    /// CHECK: required to be the market's own USDC vault PDA, derived in the instruction.
    #[account(mut)]
    pub market_usdc_vault: UncheckedAccount<'info>,
    #[account(address = config.usdc_mint, mint::token_program = usdc_token_program)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, seeds = [USDC_VAULT_SEED], bump, token::token_program = usdc_token_program)]
    pub usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
}

#[event]
pub struct VerdictOracleRotated {
    pub verdict_oracle: Pubkey,
}

#[event]
pub struct Deposited {
    pub owner: Pubkey,
    pub amount: u64,
    pub shares: u128,
}

#[event]
pub struct WithdrawRequested {
    pub owner: Pubkey,
    pub shares: u128,
}

#[event]
pub struct WithdrawFinalized {
    pub owner: Pubkey,
    pub amount: u64,
    pub shares: u128,
}

#[event]
pub struct WrongfulLiquidationPaid {
    pub liquidation_record: Pubkey,
    pub borrower: Pubkey,
    /// What the oracle said was owed; `paid` is lower when the per-claim cap bit.
    pub attested: u64,
    pub paid: u64,
}

#[event]
pub struct BadDebtReimbursed {
    pub market: Pubkey,
    pub paid: u64,
    pub cumulative: u64,
}

#[event]
pub struct VerdictAttested {
    pub liquidation_record: Pubkey,
    pub borrower: Pubkey,
    pub payout: u64,
    pub tier: u8,
    pub verdict_hash: [u8; 32],
}

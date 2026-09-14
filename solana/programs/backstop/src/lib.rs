use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

pub mod errors;
pub mod state;
pub mod verdict;

use errors::VerdictError;
use state::*;
use verdict::{FactsArgs, MAX_VERDICT_TTL_SECS};

/// `amount × numerator / denominator`, rounded down.
fn mul_div_floor(amount: u128, numerator: u128, denominator: u128) -> Result<u128> {
    amount
        .checked_mul(numerator)
        .and_then(|v| v.checked_div(denominator))
        .ok_or_else(|| VerdictError::MathOverflow.into())
}

/// `reserved_total` as bps of `cash`. `u128::MAX` when cash is zero, so every band treats an empty
/// pool as maximally utilised rather than dividing by zero.
fn utilisation_bps(reserved_total: u64, cash: u64) -> u128 {
    if cash == 0 {
        return u128::MAX;
    }
    (reserved_total as u128 * 10_000) / cash as u128
}

/// Daily admission cap band (LOCKED verdict spec: 25/10/3% by utilisation).
fn admission_cap_bps(reserved_total: u64, cash: u64) -> u32 {
    let u = utilisation_bps(reserved_total, cash);
    if u < UTIL_LOW_BPS {
        ADMISSION_CAP_LOW_BPS
    } else if u < UTIL_MID_BPS {
        ADMISSION_CAP_MID_BPS
    } else {
        ADMISSION_CAP_HIGH_BPS
    }
}

/// Daily outflow cap band, per claim (LOCKED verdict spec: 5/3/1% by utilisation).
fn outflow_cap_bps(reserved_total: u64, cash: u64) -> u32 {
    let u = utilisation_bps(reserved_total, cash);
    if u < UTIL_LOW_BPS {
        OUTFLOW_CAP_LOW_BPS
    } else if u < UTIL_MID_BPS {
        OUTFLOW_CAP_MID_BPS
    } else {
        OUTFLOW_CAP_HIGH_BPS
    }
}

fn roll_admission_day(config: &mut BackstopConfig, now: i64) {
    let today = now.div_euclid(DAY_SECS);
    if config.admission_day != today {
        config.admission_day = today;
        config.admitted_today = 0;
    }
}

/// Attempts to admit `claim` (already priced, `claim.loss` set) against `config`'s current caps
/// and solvency. Never errors on "doesn't fit" — over any limit queues rather than rejects
/// (LOCKED verdict spec); returns whether it was admitted.
fn try_admit(config: &mut BackstopConfig, claim: &mut Claim, now: i64) -> Result<bool> {
    roll_admission_day(config, now);
    let loss = claim.loss;
    let cap_bps = admission_cap_bps(config.reserved_total, config.cash);
    let admission_cap =
        safu_core::apply_bps(config.cash, cap_bps).map_err(|_| VerdictError::MathOverflow)?;
    let per_claim_cap = safu_core::apply_bps(config.cash, config.per_claim_cap_bps)
        .map_err(|_| VerdictError::MathOverflow)?;
    let fits = loss <= per_claim_cap
        && config
            .admitted_today
            .checked_add(loss)
            .is_some_and(|v| v <= admission_cap)
        && config
            .reserved_total
            .checked_add(loss)
            .is_some_and(|v| v <= config.cash);
    if fits {
        config.reserved_total = config
            .reserved_total
            .checked_add(loss)
            .ok_or(VerdictError::MathOverflow)?;
        config.admitted_today = config
            .admitted_today
            .checked_add(loss)
            .ok_or(VerdictError::MathOverflow)?;
        claim.status = ClaimStatus::Active;
        claim.admitted_at = now;
        claim.cooldown_end = now
            .checked_add(COOLDOWN_SECS)
            .ok_or(VerdictError::MathOverflow)?;
        claim.stream_end = claim
            .cooldown_end
            .checked_add(STREAM_SECS)
            .ok_or(VerdictError::MathOverflow)?;
        claim.snapshot_cash = config.cash;
        claim.last_activity_ts = now;
        Ok(true)
    } else {
        claim.status = ClaimStatus::Queued;
        Ok(false)
    }
}

declare_id!("H1hApKnNkYPsqQ9WVZGzqZQZYirxCLpXDj2uEmzMK3YV");

#[program]
pub mod backstop {
    use super::*;

    /// Only the program's upgrade authority may initialize, so nobody watching the deploy can call it
    /// first and take admin. Initialize before any `--final`: a program with no upgrade authority can
    /// never be initialized.
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
        config.reserved_total = 0;
        config.admission_day = 0;
        config.admitted_today = 0;
        config.withdraw_delay_secs = withdraw_delay_secs;
        config.reserved = [0; 32];
        Ok(())
    }

    /// Rotates the verdict oracle key (U5). It is a throwaway per-chain Ed25519 key, so a leak has
    /// to be recoverable on-chain: without this, replacing it would need a program upgrade.
    /// Claims already submitted stay valid; unsubmitted facts signed by the old key stop verifying.
    pub fn set_verdict_oracle(ctx: Context<AdminOnly>, verdict_oracle: Pubkey) -> Result<()> {
        ctx.accounts.config.verdict_oracle = verdict_oracle;
        emit!(VerdictOracleRotated { verdict_oracle });
        Ok(())
    }

    /// Admin-settable within hard bounds (U2), so neither knob can be set somewhere unsafe: a zero
    /// per-claim cap would freeze every admission, and an unbounded withdrawal delay would trap backers.
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
    /// already being prepared off-chain but not yet submitted.
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

    /// Takes the money out, bounded by `cash − reserved_total`: only the claim amount an `Active`
    /// claim actually reserves is off-limits, not the whole pool while any claim merely exists
    /// (queued or held claims reserve nothing until they are actually admitted).
    pub fn finalize_withdraw(ctx: Context<FinalizeWithdraw>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let config_bump = ctx.accounts.config.bump;
        let (cash, total_shares, delay, reserved_total) = {
            let c = &ctx.accounts.config;
            (
                c.cash,
                c.total_shares,
                c.withdraw_delay_secs,
                c.reserved_total,
            )
        };

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
        let available = cash.saturating_sub(reserved_total);
        require!(
            amount > 0 && amount <= available,
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

    /// Opens the single-slot claim tracker for one (market, borrower) pair. Permissionless — anyone
    /// may open it ahead of a borrower's first claim.
    pub fn open_borrower_claims(ctx: Context<OpenBorrowerClaims>) -> Result<()> {
        let bc = &mut ctx.accounts.borrower_claims;
        bc.version = ACCOUNT_VERSION;
        bc.bump = ctx.bumps.borrower_claims;
        bc.market = ctx.accounts.market.key();
        bc.borrower = ctx.accounts.borrower.key();
        bc.open = Pubkey::default();
        bc.reserved = [0; 32];
        Ok(())
    }

    /// Records an oracle-signed facts payload for one liquidation and runs the verdict rules
    /// on-chain (LOCKED verdict spec). The transaction must place the oracle's Ed25519 precompile
    /// instruction directly before this one. Anyone may submit; the signature is the authorization.
    ///
    /// A structurally invalid submission (bad signature, expired deadline, mismatched record,
    /// issuer-halted liquidation, self-dealt liquidation, a privileged payout address, or a
    /// borrower with an unresolved claim already open) reverts the transaction. A submission that
    /// checks out structurally but fails the wrongfulness rules — the price wasn't actually wrong,
    /// the move held, or the loss nets to zero — is recorded on-chain as `Denied` with a reason
    /// code, rather than reverting: a claims decision must be visible, not silently dropped.
    pub fn submit_facts(ctx: Context<SubmitFacts>, args: FactsArgs) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(now <= args.deadline, VerdictError::VerdictExpired);
        require!(
            args.deadline
                <= now
                    .checked_add(MAX_VERDICT_TTL_SECS)
                    .ok_or(VerdictError::VerdictDeadlineTooFar)?,
            VerdictError::VerdictDeadlineTooFar
        );

        let record = &ctx.accounts.liquidation_record;
        require!(
            (MIN_AFTER_WAIT_SECS..=MAX_AFTER_WAIT_SECS)
                .contains(&args.after_ts.saturating_sub(record.ts)),
            VerdictError::InvalidAfterWindow
        );

        let config = &ctx.accounts.config;
        let message = verdict::encode_message(&crate::ID, config.cluster_tag, &args);
        verdict::verify_preceding_ed25519(
            &ctx.accounts.instructions_sysvar.to_account_info(),
            &config.verdict_oracle,
            &message,
        )?;

        let record = &ctx.accounts.liquidation_record;
        require_keys_eq!(
            args.liquidation_record,
            record.key(),
            VerdictError::RecordMismatch
        );
        require_keys_eq!(args.borrower, record.borrower, VerdictError::RecordMismatch);
        require_keys_eq!(
            ctx.accounts.borrower.key(),
            record.borrower,
            VerdictError::RecordMismatch
        );
        require_keys_eq!(
            ctx.accounts.market.key(),
            record.market,
            VerdictError::RecordMismatch
        );
        require!(
            now.saturating_sub(record.ts) <= CLAIM_WINDOW_SECS,
            VerdictError::ClaimWindowExpired
        );
        // Spec 14a: an issuer freeze, pause or seizure is never covered.
        require!(!record.issuer_halt, VerdictError::IssuerHaltedLiquidation);
        // Spec 13c: a borrower liquidating themselves is not a wrongful liquidation.
        require_keys_neq!(
            record.liquidator,
            record.borrower,
            VerdictError::SelfDealtLiquidation
        );

        // D2: the frozen payout address can never be a privileged role.
        require_keys_neq!(
            record.payout,
            ctx.accounts.config.key(),
            VerdictError::PrivilegedPayout
        );
        require_keys_neq!(record.payout, config.admin, VerdictError::PrivilegedPayout);
        require_keys_neq!(
            record.payout,
            config.verdict_oracle,
            VerdictError::PrivilegedPayout
        );
        // A4, applied to the payout rather than the borrower: whoever actually receives the money
        // cannot also share in what they are being paid from.
        let backer_info = ctx.accounts.payout_backer.to_account_info();
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

        // E8: one unresolved claim per (market, borrower) in this phase. A terminal occupant frees
        // the slot for the new claim.
        let bc = &mut ctx.accounts.borrower_claims;
        if bc.open != Pubkey::default() {
            let existing_info = ctx.accounts.existing_claim.to_account_info();
            require_keys_eq!(existing_info.key(), bc.open, VerdictError::RecordMismatch);
            require_keys_eq!(
                *existing_info.owner,
                crate::ID,
                VerdictError::RecordMismatch
            );
            let data = existing_info.try_borrow_data()?;
            let existing = Claim::try_deserialize(&mut &data[..])?;
            drop(data);
            require!(
                existing.status.is_terminal(),
                VerdictError::BorrowerClaimsFull
            );
        }

        let cap_bps = ctx.accounts.market.params.deviation_cap_bps;
        let price_was_wrong =
            safu_core::price::deviation_exceeded(record.price_fp, args.ref_at_liq, cap_bps)
                .map_err(|_| VerdictError::MathOverflow)?;
        let move_did_not_hold =
            safu_core::price::deviation_exceeded(record.price_fp, args.ref_after, cap_bps)
                .map_err(|_| VerdictError::MathOverflow)?;
        let loss = safu_core::loss::wrongful_loss(
            record.seized_raw,
            record.collateral_decimals,
            record.multiplier_fp,
            args.ref_at_liq,
            record.debt_repaid,
        )
        .map_err(|_| VerdictError::MathOverflow)?;

        let claim = &mut ctx.accounts.claim;
        claim.version = ACCOUNT_VERSION;
        claim.bump = ctx.bumps.claim;
        claim.market = ctx.accounts.market.key();
        claim.borrower = record.borrower;
        claim.liquidation_record = record.key();
        claim.payout = record.payout;
        claim.submitted_at = now;
        claim.liquidated_at = record.ts;
        claim.evidence_hash = args.evidence_hash;
        claim.streamed = 0;
        claim.releasable_at = 0;
        claim.admitted_at = 0;
        claim.cooldown_end = 0;
        claim.stream_end = 0;
        claim.snapshot_cash = 0;
        claim.last_pull_day = 0;
        claim.pulled_today = 0;
        claim.last_activity_ts = 0;
        claim.reserved = [0; 24];

        let deny = if !price_was_wrong {
            Some(deny_reason::PRICE_NOT_WRONG)
        } else if !move_did_not_hold {
            Some(deny_reason::MOVE_HELD)
        } else if loss == 0 {
            Some(deny_reason::ZERO_LOSS)
        } else {
            None
        };

        if let Some(reason) = deny {
            claim.status = ClaimStatus::Denied;
            claim.deny_reason = reason;
            claim.loss = 0;
        } else {
            claim.loss = loss;
            claim.deny_reason = deny_reason::NONE;
            let loan_age = record.ts.saturating_sub(record.borrow_age_ts);
            if loan_age < GATE_SECS {
                claim.status = ClaimStatus::PendingTime;
                claim.releasable_at = record.borrow_age_ts.saturating_add(GATE_SECS);
            } else {
                let config = &mut ctx.accounts.config;
                try_admit(config, claim, now)?;
            }
        }

        ctx.accounts.borrower_claims.open = ctx.accounts.claim.key();

        let claim = &ctx.accounts.claim;
        emit!(FactsSubmitted {
            claim: claim.key(),
            liquidation_record: claim.liquidation_record,
            borrower: claim.borrower,
            status: claim.status,
            deny_reason: claim.deny_reason,
            loss: claim.loss,
        });
        Ok(())
    }

    /// Releases a `PendingTime` claim once the 60-day gate has elapsed, attempting admission at
    /// full value. Permissionless.
    pub fn unlock_claim(ctx: Context<UpdateClaim>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let claim = &mut ctx.accounts.claim;
        require!(
            claim.status == ClaimStatus::PendingTime,
            VerdictError::WrongClaimStatus
        );
        require!(now >= claim.releasable_at, VerdictError::GateNotElapsed);
        let config = &mut ctx.accounts.config;
        try_admit(config, claim, now)?;
        emit!(ClaimStatusChanged {
            claim: claim.key(),
            status: claim.status
        });
        Ok(())
    }

    /// Re-checks a `Queued` claim against the current caps and solvency. Permissionless; callable
    /// as often as anyone likes — cash growing or utilisation falling is what lets it through.
    pub fn try_release_queued(ctx: Context<UpdateClaim>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let claim = &mut ctx.accounts.claim;
        require!(
            claim.status == ClaimStatus::Queued,
            VerdictError::WrongClaimStatus
        );
        let config = &mut ctx.accounts.config;
        try_admit(config, claim, now)?;
        emit!(ClaimStatusChanged {
            claim: claim.key(),
            status: claim.status
        });
        Ok(())
    }

    /// A `Queued` claim that never fit inside the claim window expires. It reserved nothing, so
    /// nothing is released.
    pub fn expire_queued(ctx: Context<UpdateClaim>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let claim = &mut ctx.accounts.claim;
        require!(
            claim.status == ClaimStatus::Queued,
            VerdictError::WrongClaimStatus
        );
        require!(
            now.saturating_sub(claim.liquidated_at) > CLAIM_WINDOW_SECS,
            VerdictError::NotYetExpired
        );
        claim.status = ClaimStatus::Expired;
        emit!(ClaimStatusChanged {
            claim: claim.key(),
            status: claim.status
        });
        Ok(())
    }

    /// An `Active` claim with no pull for `INACTIVITY_EXPIRY_SECS` returns its unpaid remainder to
    /// the backstop by simply un-reserving it — nothing was ever transferred for that remainder.
    pub fn expire_stale(ctx: Context<UpdateClaim>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let claim = &mut ctx.accounts.claim;
        require!(
            claim.status == ClaimStatus::Active,
            VerdictError::WrongClaimStatus
        );
        require!(
            now.saturating_sub(claim.last_activity_ts) >= INACTIVITY_EXPIRY_SECS,
            VerdictError::NotYetStale
        );
        let remaining = claim.loss.saturating_sub(claim.streamed);
        claim.status = ClaimStatus::Expired;
        let config = &mut ctx.accounts.config;
        config.reserved_total = config.reserved_total.saturating_sub(remaining);
        emit!(ClaimStatusChanged {
            claim: claim.key(),
            status: claim.status
        });
        Ok(())
    }

    /// Pulls whatever has vested and fits today's outflow cap to the claim's frozen payout ATA.
    /// Permissionless — the payout address, not the caller, receives the funds.
    pub fn claim_stream(ctx: Context<ClaimStream>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let claim = &mut ctx.accounts.claim;
        require!(
            claim.status == ClaimStatus::Active,
            VerdictError::WrongClaimStatus
        );
        require!(now >= claim.cooldown_end, VerdictError::CooldownNotElapsed);

        let elapsed = now.saturating_sub(claim.cooldown_end).max(0);
        let vested = if elapsed >= STREAM_SECS {
            claim.loss
        } else {
            u64::try_from(mul_div_floor(
                claim.loss as u128,
                elapsed as u128,
                STREAM_SECS as u128,
            )?)
            .map_err(|_| VerdictError::MathOverflow)?
        };
        let available = vested.saturating_sub(claim.streamed);
        require!(available > 0, VerdictError::NothingToPay);

        let today = now.div_euclid(DAY_SECS);
        if claim.last_pull_day != today {
            claim.last_pull_day = today;
            claim.pulled_today = 0;
        }

        let (cash, reserved_total, config_bump) = {
            let c = &ctx.accounts.config;
            (c.cash, c.reserved_total, c.bump)
        };
        let cap_bps = outflow_cap_bps(reserved_total, cash);
        let base = cash.max(claim.snapshot_cash);
        let outflow_cap =
            safu_core::apply_bps(base, cap_bps).map_err(|_| VerdictError::MathOverflow)?;
        let room = outflow_cap.saturating_sub(claim.pulled_today);
        let pull = available.min(room).min(cash);
        require!(pull > 0, VerdictError::NothingToPay);

        let seeds: &[&[&[u8]]] = &[&[CONFIG_SEED, &[config_bump]]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.usdc_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.usdc_vault.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.payout_usdc.to_account_info(),
                    authority: ctx.accounts.config.to_account_info(),
                },
                seeds,
            ),
            pull,
            ctx.accounts.usdc_mint.decimals,
        )?;

        claim.streamed = claim
            .streamed
            .checked_add(pull)
            .ok_or(VerdictError::MathOverflow)?;
        claim.pulled_today = claim
            .pulled_today
            .checked_add(pull)
            .ok_or(VerdictError::MathOverflow)?;
        claim.last_activity_ts = now;
        let completed = claim.streamed == claim.loss;
        if completed {
            claim.status = ClaimStatus::Completed;
        }
        let streamed = claim.streamed;
        let claim_key = claim.key();

        let config = &mut ctx.accounts.config;
        config.cash = config
            .cash
            .checked_sub(pull)
            .ok_or(VerdictError::MathOverflow)?;
        config.reserved_total = config
            .reserved_total
            .checked_sub(pull)
            .ok_or(VerdictError::MathOverflow)?;

        emit!(ClaimStreamed {
            claim: claim_key,
            pull,
            streamed,
            completed
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

        let (cash, reserved_total, cap_bps, config_bump) = {
            let c = &ctx.accounts.config;
            (c.cash, c.reserved_total, c.per_claim_cap_bps, c.bump)
        };
        // Bounded by the same per-call share as a claim admission, and applied to cash already net
        // of every Active claim's reservation — bad-debt cover must not eat into money a claim has
        // already reserved, or `claim_stream` would be short later.
        let available = cash.saturating_sub(reserved_total);
        let cap =
            safu_core::apply_bps(available, cap_bps).map_err(|_| VerdictError::MathOverflow)?;
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
pub struct InitializeBackstop<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    /// This program. Proves `program_data` belongs to it, not to a program the caller deployed.
    #[account(constraint = program.programdata_address()? == Some(program_data.key()) @ VerdictError::NotUpgradeAuthority)]
    pub program: Program<'info, crate::program::Backstop>,
    #[account(constraint = program_data.upgrade_authority_address == Some(admin.key()) @ VerdictError::NotUpgradeAuthority)]
    pub program_data: Account<'info, ProgramData>,
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
pub struct OpenBorrowerClaims<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub market: Box<Account<'info, stock_vault::state::Market>>,
    /// CHECK: identifies which borrower this tracks. No signature required — opening the tracker
    /// is permissionless and holds no funds.
    pub borrower: UncheckedAccount<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + BorrowerClaims::INIT_SPACE,
        seeds = [BORROWER_CLAIMS_SEED, market.key().as_ref(), borrower.key().as_ref()],
        bump,
    )]
    pub borrower_claims: Box<Account<'info, BorrowerClaims>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(args: FactsArgs)]
pub struct SubmitFacts<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    // `mut`: an old-enough loan admits straight into `Active` during this instruction, which
    // writes `reserved_total` and the admission-day counters — without `mut` Anchor never
    // serializes those writes back, and they are silently lost.
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, BackstopConfig>>,

    pub market: Box<Account<'info, stock_vault::state::Market>>,

    /// Owned by the lending vault program — Anchor's `Owner` check is what proves this record was
    /// written by the vault and not fabricated.
    pub liquidation_record: Box<Account<'info, stock_vault::state::LiquidationRecord>>,

    /// CHECK: matched against the liquidation record's borrower.
    pub borrower: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [BORROWER_CLAIMS_SEED, market.key().as_ref(), borrower.key().as_ref()],
        bump = borrower_claims.bump,
    )]
    pub borrower_claims: Box<Account<'info, BorrowerClaims>>,

    /// CHECK: only read when `borrower_claims.open` is non-default; deserialized and validated
    /// manually as a `Claim` owned by this program. Any account may be passed when the slot is
    /// free — it is never touched in that branch.
    pub existing_claim: UncheckedAccount<'info>,

    /// `init` is the idempotency: a second facts submission for the same liquidation cannot create
    /// this twice, so a replayed transaction cannot open a second claim.
    #[account(
        init,
        payer = payer,
        space = 8 + Claim::INIT_SPACE,
        seeds = [CLAIM_SEED, args.liquidation_record.as_ref()],
        bump,
    )]
    pub claim: Box<Account<'info, Claim>>,

    /// CHECK: the payout address's Backer PDA. May legitimately not exist; when it does, it is
    /// decoded and required to hold nothing (A4, applied to the payout rather than the borrower).
    #[account(seeds = [BACKER_SEED, liquidation_record.payout.as_ref()], bump)]
    pub payout_backer: UncheckedAccount<'info>,

    /// CHECK: address-constrained to the instructions sysvar; read only through the checked loaders.
    #[account(address = solana_instructions_sysvar::ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateClaim<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, BackstopConfig>>,
    #[account(mut, seeds = [CLAIM_SEED, claim.liquidation_record.as_ref()], bump = claim.bump)]
    pub claim: Box<Account<'info, Claim>>,
}

#[derive(Accounts)]
pub struct ClaimStream<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, BackstopConfig>>,
    #[account(mut, seeds = [CLAIM_SEED, claim.liquidation_record.as_ref()], bump = claim.bump)]
    pub claim: Box<Account<'info, Claim>>,
    #[account(address = config.usdc_mint, mint::token_program = usdc_token_program)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = usdc_mint, token::authority = claim.payout, token::token_program = usdc_token_program)]
    pub payout_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [USDC_VAULT_SEED], bump, token::token_program = usdc_token_program)]
    pub usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
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
pub struct BadDebtReimbursed {
    pub market: Pubkey,
    pub paid: u64,
    pub cumulative: u64,
}

#[event]
pub struct FactsSubmitted {
    pub claim: Pubkey,
    pub liquidation_record: Pubkey,
    pub borrower: Pubkey,
    pub status: ClaimStatus,
    pub deny_reason: u16,
    pub loss: u64,
}

#[event]
pub struct ClaimStatusChanged {
    pub claim: Pubkey,
    pub status: ClaimStatus,
}

#[event]
pub struct ClaimStreamed {
    pub claim: Pubkey,
    pub pull: u64,
    pub streamed: u64,
    pub completed: bool,
}

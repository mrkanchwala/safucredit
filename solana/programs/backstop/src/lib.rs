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

/// Separate day bucket from `roll_admission_day`: claim admissions and pool-liquidation draws are
/// different budgets (phase 3).
fn roll_liq_day(config: &mut BackstopConfig, now: i64) {
    let today = now.div_euclid(DAY_SECS);
    if config.liq_day != today {
        config.liq_day = today;
        config.liq_spent_today = 0;
    }
}

/// Attempts to admit `claim` (already priced, `claim.loss` set) against `config`'s current caps
/// and solvency. Never errors on "doesn't fit" — over any limit queues rather than rejects
/// (LOCKED verdict spec); returns whether it was admitted.
///
/// Reads `claim.streamed` as "already paid, not to be re-reserved" (phase 4: an overridden claim
/// carrying a prior `Active` claim's `streamed` forward). For every ordinary caller
/// (`submit_facts`, `unlock_claim`, `try_release_queued`) `claim.streamed` is always 0 at this
/// point — a claim cannot stream before its first admission — so this is a strict, provably
/// backward-compatible generalisation, not a behaviour change for the existing paths.
fn try_admit(config: &mut BackstopConfig, claim: &mut Claim, now: i64) -> Result<bool> {
    roll_admission_day(config, now);
    let loss = claim.loss;
    let already_streamed = claim.streamed;
    let remaining = loss.saturating_sub(already_streamed);
    let cap_bps = admission_cap_bps(config.reserved_total, config.cash);
    let admission_cap =
        safu_core::apply_bps(config.cash, cap_bps).map_err(|_| VerdictError::MathOverflow)?;
    // The per-claim cap bounds the claim's total size, not just what remains to be reserved: a
    // correction that already paid most of a claim still represents a claim of its full size.
    let per_claim_cap = safu_core::apply_bps(config.cash, config.per_claim_cap_bps)
        .map_err(|_| VerdictError::MathOverflow)?;
    let fits = loss <= per_claim_cap
        && config
            .admitted_today
            .checked_add(remaining)
            .is_some_and(|v| v <= admission_cap)
        && config
            .reserved_total
            .checked_add(remaining)
            .is_some_and(|v| v <= config.cash);
    if fits {
        config.reserved_total = config
            .reserved_total
            .checked_add(remaining)
            .ok_or(VerdictError::MathOverflow)?;
        config.admitted_today = config
            .admitted_today
            .checked_add(remaining)
            .ok_or(VerdictError::MathOverflow)?;
        claim.status = ClaimStatus::Active;
        claim.admitted_at = now;
        // Phase 5: the config's timings at admission are frozen onto the claim (forward-only).
        claim.cooldown_end = now
            .checked_add(config.cooldown_secs)
            .ok_or(VerdictError::MathOverflow)?;
        claim.stream_end = claim
            .cooldown_end
            .checked_add(config.stream_secs)
            .ok_or(VerdictError::MathOverflow)?;
        claim.inactivity_secs = config.inactivity_secs;
        // Default: vesting is measured from `cooldown_end` itself, identical to before this field
        // existed. `approve_override` shifts this backward when carrying `streamed` forward.
        claim.vest_origin = claim.cooldown_end;
        claim.snapshot_cash = config.cash;
        claim.last_activity_ts = now;
        Ok(true)
    } else {
        claim.status = ClaimStatus::Queued;
        Ok(false)
    }
}

/// Pairwise role separation (LOCKED verdict spec: "admin ≠ oracle ≠ co-signer, at init and on
/// every setter"). Checked after every mutation to any of the three, so no setter can leave the
/// config in a state initialization itself would have refused.
fn require_distinct_roles(admin: Pubkey, verdict_oracle: Pubkey, co_signer: Pubkey) -> Result<()> {
    require!(
        admin != verdict_oracle && admin != co_signer && verdict_oracle != co_signer,
        VerdictError::RoleCollision
    );
    Ok(())
}

/// Eng review A5: per-cluster bounds on every claim timing. Localnet/devnet may go to one second so the
/// demo runs in minutes; mainnet keeps floors no admin can undercut. Ceilings are the LOCKED spec values
/// (inactivity: one year), so an admin can only ever make a claim resolve faster than the spec, never
/// hold a borrower's money longer.
fn require_claim_timing(
    cluster_tag: u8,
    gate_secs: i64,
    cooldown_secs: i64,
    stream_secs: i64,
    inactivity_secs: i64,
    min_after_wait_secs: i64,
) -> Result<()> {
    let mainnet = cluster_tag == verdict::CLUSTER_MAINNET;
    let floor = |mainnet_min: i64| {
        if mainnet {
            mainnet_min
        } else {
            TEST_CLUSTER_MIN_SECS
        }
    };
    require!(
        (floor(MAINNET_MIN_GATE_SECS)..=DEFAULT_GATE_SECS).contains(&gate_secs)
            && (floor(MAINNET_MIN_COOLDOWN_SECS)..=DEFAULT_COOLDOWN_SECS).contains(&cooldown_secs)
            && (floor(MAINNET_MIN_STREAM_SECS)..=DEFAULT_STREAM_SECS).contains(&stream_secs)
            && (floor(MAINNET_MIN_INACTIVITY_SECS)..=MAX_INACTIVITY_SECS)
                .contains(&inactivity_secs)
            && (floor(MAINNET_MIN_MIN_AFTER_WAIT_SECS)..=DEFAULT_MIN_AFTER_WAIT_SECS)
                .contains(&min_after_wait_secs),
        VerdictError::InvalidParams
    );
    // An admitted claim can't be pulled before its cooldown ends, and every pull resets the inactivity
    // clock; an inactivity window no longer than the cooldown would expire claims nobody could collect.
    require!(inactivity_secs > cooldown_secs, VerdictError::InvalidParams);
    Ok(())
}

fn min_resale_floor_secs(cluster_tag: u8) -> i64 {
    if cluster_tag == verdict::CLUSTER_MAINNET {
        MAINNET_MIN_RESALE_FLOOR_SECS
    } else {
        MIN_RESALE_FLOOR_SECS
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
        co_signer: Pubkey,
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
        require_distinct_roles(ctx.accounts.admin.key(), verdict_oracle, co_signer)?;
        require!(
            cluster_tag <= verdict::CLUSTER_MAINNET,
            VerdictError::InvalidParams
        );
        let config = &mut ctx.accounts.config;
        config.version = ACCOUNT_VERSION;
        config.admin = ctx.accounts.admin.key();
        config.verdict_oracle = verdict_oracle;
        config.co_signer = co_signer;
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
        config.inventory_cost_total = 0;
        config.per_liq_cap_bps = DEFAULT_PER_LIQ_CAP_BPS;
        config.daily_liq_cap_bps = DEFAULT_DAILY_LIQ_CAP_BPS;
        config.liq_day = 0;
        config.liq_spent_today = 0;
        config.resale_discount_bps = DEFAULT_RESALE_DISCOUNT_BPS;
        config.resale_floor_secs = DEFAULT_RESALE_FLOOR_SECS;
        config.fee_share_bps = DEFAULT_FEE_SHARE_BPS;
        config.min_pool_repay = DEFAULT_MIN_POOL_REPAY;
        config.gate_secs = DEFAULT_GATE_SECS;
        config.cooldown_secs = DEFAULT_COOLDOWN_SECS;
        config.stream_secs = DEFAULT_STREAM_SECS;
        config.inactivity_secs = DEFAULT_INACTIVITY_SECS;
        config.min_after_wait_secs = DEFAULT_MIN_AFTER_WAIT_SECS;
        config.reserved = [0; 16];
        Ok(())
    }

    /// Phase 5 (A5): sets every claim timing at once, bounded per cluster by `require_claim_timing`.
    /// Forward-only: a held claim keeps the `releasable_at` it was given at submission, and an admitted
    /// claim keeps the cooldown, stream and inactivity window it was admitted under.
    pub fn set_claim_timing(
        ctx: Context<AdminOnly>,
        gate_secs: i64,
        cooldown_secs: i64,
        stream_secs: i64,
        inactivity_secs: i64,
        min_after_wait_secs: i64,
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;
        require_claim_timing(
            config.cluster_tag,
            gate_secs,
            cooldown_secs,
            stream_secs,
            inactivity_secs,
            min_after_wait_secs,
        )?;
        config.gate_secs = gate_secs;
        config.cooldown_secs = cooldown_secs;
        config.stream_secs = stream_secs;
        config.inactivity_secs = inactivity_secs;
        config.min_after_wait_secs = min_after_wait_secs;
        emit!(ClaimTimingSet {
            gate_secs,
            cooldown_secs,
            stream_secs,
            inactivity_secs,
            min_after_wait_secs,
        });
        Ok(())
    }

    /// Rotates the verdict oracle key (U5). It is a throwaway per-chain Ed25519 key, so a leak has
    /// to be recoverable on-chain: without this, replacing it would need a program upgrade.
    /// Claims already submitted stay valid; unsubmitted facts signed by the old key stop verifying.
    pub fn set_verdict_oracle(ctx: Context<AdminOnly>, verdict_oracle: Pubkey) -> Result<()> {
        let config = &mut ctx.accounts.config;
        require_distinct_roles(config.admin, verdict_oracle, config.co_signer)?;
        config.verdict_oracle = verdict_oracle;
        emit!(VerdictOracleRotated { verdict_oracle });
        Ok(())
    }

    /// Rotates the 2-of-2 override co-signer (phase 4). Same recoverability rationale as
    /// `set_verdict_oracle`: a leaked co-signer key must be replaceable without a program upgrade.
    pub fn set_co_signer(ctx: Context<AdminOnly>, co_signer: Pubkey) -> Result<()> {
        let config = &mut ctx.accounts.config;
        require_distinct_roles(config.admin, config.verdict_oracle, co_signer)?;
        config.co_signer = co_signer;
        emit!(CoSignerRotated { co_signer });
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

    /// Pool-liquidation params, bounded (phase 3). A5's per-cluster floors land in phase 5; these are
    /// generic ceilings only, so the demo can run fast today without leaving a mainnet admin free to
    /// set anything unsafe later.
    #[allow(clippy::too_many_arguments)]
    pub fn set_pool_liquidation_params(
        ctx: Context<AdminOnly>,
        per_liq_cap_bps: u32,
        daily_liq_cap_bps: u32,
        resale_discount_bps: u32,
        resale_floor_secs: i64,
        fee_share_bps: u32,
        min_pool_repay: u64,
    ) -> Result<()> {
        require!(
            per_liq_cap_bps > 0 && per_liq_cap_bps <= MAX_PER_LIQ_CAP_BPS,
            VerdictError::InvalidParams
        );
        require!(
            daily_liq_cap_bps >= per_liq_cap_bps && daily_liq_cap_bps <= MAX_DAILY_LIQ_CAP_BPS,
            VerdictError::InvalidParams
        );
        require!(
            resale_discount_bps <= MAX_RESALE_DISCOUNT_BPS,
            VerdictError::InvalidParams
        );
        require!(
            (min_resale_floor_secs(ctx.accounts.config.cluster_tag)..=MAX_RESALE_FLOOR_SECS)
                .contains(&resale_floor_secs),
            VerdictError::InvalidParams
        );
        require!(
            fee_share_bps <= MAX_FEE_SHARE_BPS,
            VerdictError::InvalidParams
        );
        require!(
            min_pool_repay <= MAX_MIN_POOL_REPAY,
            VerdictError::InvalidParams
        );
        let config = &mut ctx.accounts.config;
        config.per_liq_cap_bps = per_liq_cap_bps;
        config.daily_liq_cap_bps = daily_liq_cap_bps;
        config.resale_discount_bps = resale_discount_bps;
        config.resale_floor_secs = resale_floor_secs;
        config.fee_share_bps = fee_share_bps;
        config.min_pool_repay = min_pool_repay;
        Ok(())
    }

    /// Hands the admin role over (U5). No renounce path: an unowned backstop could never rotate a
    /// leaked oracle key.
    pub fn set_admin(ctx: Context<AdminOnly>, admin: Pubkey) -> Result<()> {
        require_keys_neq!(admin, Pubkey::default(), VerdictError::InvalidParams);
        let config = &mut ctx.accounts.config;
        require_distinct_roles(admin, config.verdict_oracle, config.co_signer)?;
        config.admin = admin;
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
        require!(
            config.inventory_cost_total == 0,
            VerdictError::PausedForInventory
        );
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
        let (cash, total_shares, delay, reserved_total, inventory_cost_total) = {
            let c = &ctx.accounts.config;
            (
                c.cash,
                c.total_shares,
                c.withdraw_delay_secs,
                c.reserved_total,
                c.inventory_cost_total,
            )
        };
        require!(inventory_cost_total == 0, VerdictError::PausedForInventory);

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
        bc.penalty_since = 0;
        bc.penalty_until = 0;
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
            (ctx.accounts.config.min_after_wait_secs..=MAX_AFTER_WAIT_SECS)
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

        // Phase 4: a specific signed-but-not-yet-submitted attestation the admin revoked. Presence
        // alone is the signal — never deserialized, the seeds already bind it to this exact
        // (liquidation_record, evidence_hash) pair.
        require!(
            *ctx.accounts.revoked.owner != crate::ID,
            VerdictError::AttestationRevoked
        );

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

        // D2: the frozen payout address can never be a privileged role. Roles (LOCKED verdict
        // spec): "borrower and payout wallet can't be any role".
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
        require_keys_neq!(
            record.payout,
            config.co_signer,
            VerdictError::PrivilegedPayout
        );
        require_keys_neq!(
            ctx.accounts.borrower.key(),
            config.admin,
            VerdictError::PrivilegedBorrower
        );
        require_keys_neq!(
            ctx.accounts.borrower.key(),
            config.verdict_oracle,
            VerdictError::PrivilegedBorrower
        );
        require_keys_neq!(
            ctx.accounts.borrower.key(),
            config.co_signer,
            VerdictError::PrivilegedBorrower
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
        claim.suspended = false;
        claim.suspended_since = 0;
        claim.suspended_secs = 0;
        claim.vest_origin = 0;
        claim.inactivity_secs = 0;
        claim.reserved = [0; 8];

        // Phase 4: "no coverage on new loans for 365 days" — a liquidation whose debt-weighted
        // loan age falls inside the penalty window is denied, visibly, same as every other
        // wrongfulness check here. The lower bound excludes loans that predate the penalty
        // entirely (see `BorrowerClaims::penalty_since` doc comment).
        let bc_penalty = &ctx.accounts.borrower_claims;
        let penalty_active = bc_penalty.penalty_since <= record.borrow_age_ts
            && record.borrow_age_ts < bc_penalty.penalty_until;

        let deny = if !price_was_wrong {
            Some(deny_reason::PRICE_NOT_WRONG)
        } else if !move_did_not_hold {
            Some(deny_reason::MOVE_HELD)
        } else if penalty_active {
            Some(deny_reason::PENALTY_ACTIVE)
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
            let gate_secs = ctx.accounts.config.gate_secs;
            let loan_age = record.ts.saturating_sub(record.borrow_age_ts);
            if loan_age < gate_secs {
                claim.status = ClaimStatus::PendingTime;
                claim.releasable_at = record.borrow_age_ts.saturating_add(gate_secs);
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
        require!(!claim.suspended, VerdictError::ClaimSuspended);
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
        require!(!claim.suspended, VerdictError::ClaimSuspended);
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
        require!(!claim.suspended, VerdictError::ClaimSuspended);
        require!(
            claim.status == ClaimStatus::Queued,
            VerdictError::WrongClaimStatus
        );
        // `suspended_secs` discounts every second this claim ever spent frozen, so a suspension
        // genuinely stops this clock rather than merely delaying when it is checked.
        let elapsed = now.saturating_sub(claim.liquidated_at);
        let effective = elapsed.saturating_sub(claim.suspended_secs).max(0);
        require!(effective > CLAIM_WINDOW_SECS, VerdictError::NotYetExpired);
        claim.status = ClaimStatus::Expired;
        emit!(ClaimStatusChanged {
            claim: claim.key(),
            status: claim.status
        });
        Ok(())
    }

    /// An `Active` claim with no pull for its admitted `inactivity_secs` returns its unpaid remainder to
    /// the backstop by simply un-reserving it — nothing was ever transferred for that remainder.
    pub fn expire_stale(ctx: Context<UpdateClaim>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let claim = &mut ctx.accounts.claim;
        require!(!claim.suspended, VerdictError::ClaimSuspended);
        require!(
            claim.status == ClaimStatus::Active,
            VerdictError::WrongClaimStatus
        );
        let elapsed = now.saturating_sub(claim.last_activity_ts);
        let effective = elapsed.saturating_sub(claim.suspended_secs).max(0);
        require!(
            effective >= claim.inactivity_secs,
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
        require!(!claim.suspended, VerdictError::ClaimSuspended);
        require!(
            claim.status == ClaimStatus::Active,
            VerdictError::WrongClaimStatus
        );
        require!(now >= claim.cooldown_end, VerdictError::CooldownNotElapsed);

        // `vest_origin` defaults to `cooldown_end` (identical to before this field existed); an
        // override that carried a prior `streamed` amount forward shifts it backward so vesting
        // resumes from where it left off instead of re-streaming the paid portion again.
        let elapsed = now.saturating_sub(claim.vest_origin).max(0);
        // Phase 5: the stream length this claim was admitted under, not the config's current value.
        let stream_secs = claim.stream_end.saturating_sub(claim.cooldown_end);
        let vested = if elapsed >= stream_secs {
            claim.loss
        } else {
            u64::try_from(mul_div_floor(
                claim.loss as u128,
                elapsed as u128,
                stream_secs as u128,
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

    /// Opens the collateral-inventory tracker and its token vault for one market (phase 3).
    /// Permissionless, one-time, ahead of that market's first pool liquidation.
    pub fn open_inventory(ctx: Context<OpenInventory>) -> Result<()> {
        let inv = &mut ctx.accounts.inventory;
        inv.version = ACCOUNT_VERSION;
        inv.bump = ctx.bumps.inventory;
        inv.market = ctx.accounts.market.key();
        inv.raw = 0;
        inv.cost_total = 0;
        inv.last_acquired_at = 0;
        inv.reserved = [0; 32];
        Ok(())
    }

    /// Opens the interest-recognition counter for one market (phase 3). Permissionless, one-time.
    pub fn open_interest_absorbed(ctx: Context<OpenInterestAbsorbed>) -> Result<()> {
        let ia = &mut ctx.accounts.interest_absorbed;
        ia.version = ACCOUNT_VERSION;
        ia.bump = ctx.bumps.interest_absorbed;
        ia.market = ctx.accounts.market.key();
        ia.total_absorbed = 0;
        ia.reserved = [0; 32];
        Ok(())
    }

    /// Liquidates a position through the vault with the pool itself as liquidator
    /// (backstop-as-liquidator lock). A permissionless crank; the caller pays the new
    /// `LiquidationRecord`'s rent (A1 -- the pool's key is a program PDA, it cannot fund an
    /// account) and earns a slice of the bonus (D6). Bounded by the per-liquidation and daily
    /// caps -- over either, this pays what caps and cash allow (E1): the position stays
    /// liquidatable and an outside liquidator may act once the vault's own grace period elapses.
    pub fn pool_liquidate(ctx: Context<PoolLiquidate>, repay_amount: u64) -> Result<()> {
        require!(repay_amount > 0, VerdictError::ZeroAmount);
        let now = Clock::get()?.unix_timestamp;
        let config_bump = ctx.accounts.config.bump;

        let (cash, per_liq_cap_bps, daily_cap_bps, min_repay) = {
            let c = &ctx.accounts.config;
            (
                c.cash,
                c.per_liq_cap_bps,
                c.daily_liq_cap_bps,
                c.min_pool_repay,
            )
        };
        roll_liq_day(&mut ctx.accounts.config, now);
        let liq_spent_today = ctx.accounts.config.liq_spent_today;

        let per_liq_cap =
            safu_core::apply_bps(cash, per_liq_cap_bps).map_err(|_| VerdictError::MathOverflow)?;
        let daily_cap =
            safu_core::apply_bps(cash, daily_cap_bps).map_err(|_| VerdictError::MathOverflow)?;
        let daily_room = daily_cap.saturating_sub(liq_spent_today);
        let capped = repay_amount.min(per_liq_cap).min(daily_room).min(cash);
        require!(capped >= min_repay, VerdictError::BelowMinPoolRepay);

        let seeds: &[&[&[u8]]] = &[&[CONFIG_SEED, &[config_bump]]];
        let cpi_accounts = stock_vault::cpi::accounts::Liquidate {
            payer: ctx.accounts.payer.to_account_info(),
            liquidator: ctx.accounts.config.to_account_info(),
            config: ctx.accounts.vault_config.to_account_info(),
            market: ctx.accounts.market.to_account_info(),
            position: ctx.accounts.position.to_account_info(),
            record: ctx.accounts.record.to_account_info(),
            collateral_mint: ctx.accounts.collateral_mint.to_account_info(),
            usdc_mint: ctx.accounts.usdc_mint.to_account_info(),
            liquidator_usdc: ctx.accounts.pool_usdc_vault.to_account_info(),
            liquidator_collateral: ctx.accounts.inventory_vault.to_account_info(),
            collateral_vault: ctx.accounts.vault_collateral_vault.to_account_info(),
            usdc_vault: ctx.accounts.vault_usdc_vault.to_account_info(),
            collateral_token_program: ctx.accounts.collateral_token_program.to_account_info(),
            usdc_token_program: ctx.accounts.usdc_token_program.to_account_info(),
            system_program: ctx.accounts.system_program.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.stock_vault_program.key(),
            cpi_accounts,
            seeds,
        );
        stock_vault::cpi::liquidate(cpi_ctx, capped)?;

        // `liquidate` may have paid less than `capped` (partial seizure) -- read what actually
        // happened from the record it just wrote, rather than assume the requested amount landed.
        let (seized_raw, debt_repaid, bonus_bps) = {
            let data = ctx.accounts.record.try_borrow_data()?;
            let record = stock_vault::state::LiquidationRecord::try_deserialize(&mut &data[..])?;
            (record.seized_raw, record.debt_repaid, record.bonus_bps)
        };

        // D6: pay × bonus_bps × fee_share / 10^8, so the pool never pays more than the bonus it
        // just captured. bonus_bps and fee_share_bps are both <= 10_000 by construction, so the
        // product with debt_repaid (a u64) always fits u128.
        let fee_share_bps = ctx.accounts.config.fee_share_bps;
        let fee = u64::try_from(
            (debt_repaid as u128) * (bonus_bps as u128) * (fee_share_bps as u128) / 100_000_000u128,
        )
        .map_err(|_| VerdictError::MathOverflow)?;

        if fee > 0 {
            token_interface::transfer_checked(
                CpiContext::new_with_signer(
                    ctx.accounts.usdc_token_program.key(),
                    TransferChecked {
                        from: ctx.accounts.pool_usdc_vault.to_account_info(),
                        mint: ctx.accounts.usdc_mint.to_account_info(),
                        to: ctx.accounts.crank_usdc.to_account_info(),
                        authority: ctx.accounts.config.to_account_info(),
                    },
                    seeds,
                ),
                fee,
                ctx.accounts.usdc_mint.decimals,
            )?;
        }

        let inventory = &mut ctx.accounts.inventory;
        inventory.raw = inventory
            .raw
            .checked_add(seized_raw)
            .ok_or(VerdictError::MathOverflow)?;
        inventory.cost_total = inventory
            .cost_total
            .checked_add(debt_repaid)
            .ok_or(VerdictError::MathOverflow)?;
        inventory.last_acquired_at = now;

        let config = &mut ctx.accounts.config;
        config.cash = config
            .cash
            .checked_sub(debt_repaid)
            .and_then(|v| v.checked_sub(fee))
            .ok_or(VerdictError::MathOverflow)?;
        config.liq_spent_today = config
            .liq_spent_today
            .checked_add(debt_repaid)
            .ok_or(VerdictError::MathOverflow)?;
        config.inventory_cost_total = config
            .inventory_cost_total
            .checked_add(debt_repaid)
            .ok_or(VerdictError::MathOverflow)?;

        emit!(PoolLiquidated {
            market: ctx.accounts.market.key(),
            seized_raw,
            debt_repaid,
            bonus_bps,
            fee,
        });
        Ok(())
    }

    /// Buys collateral out of a market's inventory at the healthy price minus the discount, never
    /// below the pool's own cost for the first `resale_floor_secs` after acquiring it, and never
    /// while the price is flagged or a corporate-action hold is active (D5). Bounded by the
    /// buyer's own `max_price_per_share` (slippage).
    pub fn buy_inventory(
        ctx: Context<BuyInventory>,
        raw: u64,
        max_price_per_share: u64,
    ) -> Result<()> {
        require!(raw > 0, VerdictError::ZeroAmount);
        let now = Clock::get()?.unix_timestamp;
        require_keys_eq!(
            ctx.accounts.inventory.market,
            ctx.accounts.market.key(),
            VerdictError::MarketMismatch
        );
        require!(
            raw <= ctx.accounts.inventory.raw,
            VerdictError::NothingToBuy
        );

        let market = &ctx.accounts.market;
        let mint_info = ctx.accounts.collateral_mint.to_account_info();
        require!(
            !market.issuer_halt && !stock_vault::logic::issuer_blocks(&mint_info),
            VerdictError::ResaleBlocked
        );
        let schedule = stock_vault::logic::MultiplierSchedule::read(&mint_info)?;
        require!(
            !stock_vault::logic::corporate_action_hold(market, &schedule, now)
                && !stock_vault::logic::unobserved_multiplier_change(market, &schedule, now)
                // The market is read-only here, so a price history still quoted in a pre-split multiplier
                // cannot be re-quoted; selling off it would misprice the pool's inventory by the split ratio.
                // The next vault price update re-quotes it and resale resumes.
                && stock_vault::logic::price_units_current(market, &schedule, now),
            VerdictError::ResaleBlocked
        );
        let multiplier_fp = schedule.effective(now);
        let healthy =
            stock_vault::logic::risk_price(market, now, stock_vault::logic::PriceUse::Liquidate)?;
        let discounted =
            safu_core::apply_bps(healthy, 10_000 - ctx.accounts.config.resale_discount_bps)
                .map_err(|_| VerdictError::MathOverflow)?;
        let discounted_value =
            stock_vault::logic::value_of(market, raw, multiplier_fp, discounted)?;

        let inventory = &ctx.accounts.inventory;
        let prorated_cost = u64::try_from(
            mul_div_floor(
                inventory.cost_total as u128,
                raw as u128,
                inventory.raw as u128,
            )
            .map_err(|_| VerdictError::MathOverflow)?,
        )
        .map_err(|_| VerdictError::MathOverflow)?;
        let in_floor_window = now
            < inventory
                .last_acquired_at
                .saturating_add(ctx.accounts.config.resale_floor_secs);
        let sale_value = if in_floor_window {
            discounted_value.max(prorated_cost)
        } else {
            discounted_value
        };
        require!(sale_value > 0, VerdictError::ZeroAmount);

        let max_allowed_value =
            stock_vault::logic::value_of(market, raw, multiplier_fp, max_price_per_share)?;
        require!(sale_value <= max_allowed_value, VerdictError::PriceAboveMax);

        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.usdc_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.buyer_usdc.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.pool_usdc_vault.to_account_info(),
                    authority: ctx.accounts.buyer.to_account_info(),
                },
            ),
            sale_value,
            ctx.accounts.usdc_mint.decimals,
        )?;

        let config_bump = ctx.accounts.config.bump;
        let seeds: &[&[&[u8]]] = &[&[CONFIG_SEED, &[config_bump]]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.collateral_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.inventory_vault.to_account_info(),
                    mint: ctx.accounts.collateral_mint.to_account_info(),
                    to: ctx.accounts.buyer_collateral.to_account_info(),
                    authority: ctx.accounts.config.to_account_info(),
                },
                seeds,
            ),
            raw,
            ctx.accounts.collateral_mint.decimals,
        )?;

        let inventory = &mut ctx.accounts.inventory;
        inventory.raw -= raw;
        let config = &mut ctx.accounts.config;
        if inventory.raw == 0 {
            // Dust-safe close-out: whatever cost basis remains after floor division leaves the
            // aggregate pause signal stuck above zero forever otherwise.
            config.inventory_cost_total = config
                .inventory_cost_total
                .saturating_sub(inventory.cost_total);
            inventory.cost_total = 0;
        } else {
            inventory.cost_total -= prorated_cost;
            config.inventory_cost_total -= prorated_cost;
        }
        config.cash = config
            .cash
            .checked_add(sale_value)
            .ok_or(VerdictError::MathOverflow)?;

        emit!(InventorySold {
            market: ctx.accounts.market.key(),
            raw,
            sale_value,
        });
        Ok(())
    }

    /// Admin-only, bounded to a market the issuer has actually frozen or halted (E3): the only
    /// case where `buy_inventory` can never clear the position on its own, because the token
    /// account itself is frozen. Zeroes this market's cost basis so deposits/withdrawals can
    /// resume; backers take the loss, same as any other issuer-seizure risk they carry.
    pub fn write_off_inventory(ctx: Context<WriteOffInventory>) -> Result<()> {
        let issuer_acted = ctx.accounts.market.issuer_halt
            || stock_vault::logic::issuer_blocks(&ctx.accounts.collateral_mint.to_account_info());
        require!(issuer_acted, VerdictError::NotFrozenOrHalted);

        let inventory = &mut ctx.accounts.inventory;
        let written_off = inventory.cost_total;
        require!(written_off > 0, VerdictError::ZeroAmount);
        inventory.cost_total = 0;

        let config = &mut ctx.accounts.config;
        config.inventory_cost_total = config.inventory_cost_total.saturating_sub(written_off);

        emit!(InventoryWrittenOff {
            market: ctx.accounts.market.key(),
            raw_stranded: inventory.raw,
            cost_written_off: written_off,
        });
        Ok(())
    }

    /// Recognises USDC `pay_backer_interest` has already moved into the pool's vault. Permissionless,
    /// bounded by the unaccounted surplus actually sitting in the token account (2c pattern, in
    /// reverse) -- a raw donation is never counted, and repeat calls settle only the remainder.
    pub fn absorb_interest(ctx: Context<AbsorbInterest>) -> Result<()> {
        let vault_amount = ctx.accounts.pool_usdc_vault.amount;
        let owed = ctx
            .accounts
            .market
            .backer_interest_paid_cumulative
            .saturating_sub(ctx.accounts.interest_absorbed.total_absorbed);
        let surplus = vault_amount.saturating_sub(ctx.accounts.config.cash);
        let credit = owed.min(surplus);
        require!(credit > 0, VerdictError::ZeroAmount);

        ctx.accounts.config.cash = ctx
            .accounts
            .config
            .cash
            .checked_add(credit)
            .ok_or(VerdictError::MathOverflow)?;
        ctx.accounts.interest_absorbed.total_absorbed = ctx
            .accounts
            .interest_absorbed
            .total_absorbed
            .checked_add(credit)
            .ok_or(VerdictError::MathOverflow)?;

        emit!(InterestAbsorbedEvent {
            market: ctx.accounts.market.key(),
            credited: credit,
        });
        Ok(())
    }

    // ---------------------------------------------------------------- phase 4: overrides

    /// Admin-only. Cancels a held (`PendingTime`), `Queued`, or `Active` claim with a public
    /// reason code (LOCKED verdict spec). An `Active` claim's unstreamed reservation is released
    /// exactly as it would be on expiry. The 365-day no-coverage penalty applies only if real
    /// money had actually left the pool for this claim (`streamed > 0`) — catching a mistake
    /// before any payout moved costs the borrower nothing.
    pub fn cancel_claim(ctx: Context<CancelClaim>, reason_code: u16) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let claim = &mut ctx.accounts.claim;
        require!(
            matches!(
                claim.status,
                ClaimStatus::PendingTime | ClaimStatus::Queued | ClaimStatus::Active
            ),
            VerdictError::ClaimNotCancellable
        );
        let penalize = claim.status == ClaimStatus::Active && claim.streamed > 0;
        if claim.status == ClaimStatus::Active {
            let unreserved = claim.loss.saturating_sub(claim.streamed);
            let config = &mut ctx.accounts.config;
            config.reserved_total = config.reserved_total.saturating_sub(unreserved);
        }
        claim.status = ClaimStatus::Cancelled;
        // Reused, not a new field: on a Cancelled claim this slot holds the admin's public
        // cancellation reason rather than one of the `deny_reason::` wrongfulness codes — the
        // status itself disambiguates which meaning applies.
        claim.deny_reason = reason_code;
        let claim_key = claim.key();

        if penalize {
            let bc = &mut ctx.accounts.borrower_claims;
            bc.penalty_since = now;
            bc.penalty_until = now
                .checked_add(PENALTY_LOCK_SECS)
                .ok_or(VerdictError::MathOverflow)?;
        }

        emit!(ClaimCancelled {
            claim: claim_key,
            reason_code,
            penalized: penalize,
        });
        Ok(())
    }

    /// Admin-only. Freezes a claim: every permissionless progression instruction (`unlock_claim`,
    /// `try_release_queued`, `expire_queued`, `expire_stale`, `claim_stream`) refuses while
    /// suspended.
    pub fn suspend_claim(ctx: Context<SuspendClaim>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let claim = &mut ctx.accounts.claim;
        require!(!claim.status.is_terminal(), VerdictError::WrongClaimStatus);
        require!(!claim.suspended, VerdictError::AlreadySuspended);
        claim.suspended = true;
        claim.suspended_since = now;
        emit!(ClaimSuspendChanged {
            claim: claim.key(),
            suspended: true,
        });
        Ok(())
    }

    /// Admin-only. Un-freezes a claim, folding the just-ended suspension's duration into
    /// `suspended_secs` so `expire_queued`/`expire_stale` never count time this claim spent
    /// suspended (LOCKED verdict spec: "expiry clocks stop while suspended and reset on
    /// unsuspend").
    pub fn unsuspend_claim(ctx: Context<SuspendClaim>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let claim = &mut ctx.accounts.claim;
        require!(claim.suspended, VerdictError::NotSuspended);
        let paused = now.saturating_sub(claim.suspended_since).max(0);
        claim.suspended_secs = claim.suspended_secs.saturating_add(paused);
        claim.suspended = false;
        claim.suspended_since = 0;
        emit!(ClaimSuspendChanged {
            claim: claim.key(),
            suspended: false,
        });
        Ok(())
    }

    /// Admin-only. Marks one specific signed-but-not-yet-submitted attestation revoked, so a
    /// leaked-oracle-key scenario has an on-chain block even before anyone tries to submit it.
    pub fn revoke_attestation(
        ctx: Context<RevokeAttestation>,
        liquidation_record: Pubkey,
        evidence_hash: [u8; 32],
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let revoked = &mut ctx.accounts.revoked;
        revoked.version = ACCOUNT_VERSION;
        revoked.bump = ctx.bumps.revoked;
        revoked.liquidation_record = liquidation_record;
        revoked.evidence_hash = evidence_hash;
        revoked.revoked_at = now;
        revoked.reserved = [0; 16];
        emit!(AttestationRevokedEvent {
            liquidation_record,
            evidence_hash,
        });
        Ok(())
    }

    /// Permissionless. Opens the 2-of-2 override request for one liquidation, ahead of the first
    /// `approve_override` call.
    pub fn open_override(ctx: Context<OpenOverride>) -> Result<()> {
        let req = &mut ctx.accounts.override_request;
        req.version = ACCOUNT_VERSION;
        req.bump = ctx.bumps.override_request;
        req.liquidation_record = ctx.accounts.liquidation_record.key();
        req.ref_at_liq = 0;
        req.admin_approver = None;
        req.co_signer_approver = None;
        req.executed = false;
        req.reserved = [0; 16];
        Ok(())
    }

    /// Admin or co-signer only. The 2-of-2 override (LOCKED verdict spec): each approver submits
    /// the same `ref_at_liq` once; the second matching, current-key approval executes. Skips
    /// checks (a)/(b)/(c) and the 60-day gate. Never skips: record + borrower match, the
    /// issuer-action exclusion, one payout per liquidation (a `Completed` claim is refused), the
    /// on-chain loss formula, the per-claim cap, or solvency — `ref_at_liq` is supplied, not the
    /// loss itself, and the loss is still computed on-chain via `safu_core::wrongful_loss`, then
    /// still run through `try_admit`'s cap/solvency logic exactly like an ordinary admission.
    /// A prior `Active` claim's unstreamed reservation is released and its `streamed` carried
    /// forward, so the correction can never pay out on top of what already streamed.
    pub fn approve_override(ctx: Context<ApproveOverride>, ref_at_liq: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let admin = ctx.accounts.config.admin;
        let co_signer = ctx.accounts.config.co_signer;
        let caller = ctx.accounts.caller.key();
        require!(
            caller == admin || caller == co_signer,
            VerdictError::CallerNotAdminOrCoSigner
        );

        let record = &ctx.accounts.liquidation_record;
        require_keys_eq!(
            record.market,
            ctx.accounts.market.key(),
            VerdictError::RecordMismatch
        );
        require_keys_eq!(
            record.borrower,
            ctx.accounts.claim.borrower,
            VerdictError::RecordMismatch
        );
        require_keys_eq!(
            ctx.accounts.claim.liquidation_record,
            record.key(),
            VerdictError::RecordMismatch
        );
        require!(!record.issuer_halt, VerdictError::IssuerHaltedLiquidation);
        require!(
            ctx.accounts.claim.status != ClaimStatus::Completed,
            VerdictError::ClaimAlreadyCompleted
        );

        require_keys_neq!(
            record.payout,
            ctx.accounts.config.key(),
            VerdictError::PrivilegedPayout
        );
        require_keys_neq!(record.payout, admin, VerdictError::PrivilegedPayout);
        require_keys_neq!(
            record.payout,
            ctx.accounts.config.verdict_oracle,
            VerdictError::PrivilegedPayout
        );
        require_keys_neq!(record.payout, co_signer, VerdictError::PrivilegedPayout);
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

        let req = &mut ctx.accounts.override_request;
        require!(!req.executed, VerdictError::OverrideAlreadyExecuted);
        require_keys_eq!(
            req.liquidation_record,
            record.key(),
            VerdictError::RecordMismatch
        );
        if req.admin_approver.is_none() && req.co_signer_approver.is_none() {
            req.ref_at_liq = ref_at_liq;
        } else {
            require!(
                req.ref_at_liq == ref_at_liq,
                VerdictError::OverrideParamsMismatch
            );
        }
        if caller == admin {
            req.admin_approver = Some(caller);
        }
        if caller == co_signer {
            req.co_signer_approver = Some(caller);
        }

        // Bound to the CURRENT admin/co_signer, not whoever approved earlier: a rotation between
        // the two approvals correctly un-readies a stale approval rather than letting it carry
        // over silently.
        let ready = req.admin_approver == Some(admin) && req.co_signer_approver == Some(co_signer);
        if !ready {
            return Ok(());
        }

        let loss = safu_core::loss::wrongful_loss(
            record.seized_raw,
            record.collateral_decimals,
            record.multiplier_fp,
            ref_at_liq,
            record.debt_repaid,
        )
        .map_err(|_| VerdictError::MathOverflow)?;
        require!(loss > 0, VerdictError::ZeroAmount);

        let claim_status = ctx.accounts.claim.status;
        let carried_streamed = if claim_status == ClaimStatus::Active {
            ctx.accounts.claim.streamed
        } else {
            0
        };
        if claim_status == ClaimStatus::Active {
            let unreserved = ctx
                .accounts
                .claim
                .loss
                .saturating_sub(ctx.accounts.claim.streamed);
            ctx.accounts.config.reserved_total = ctx
                .accounts
                .config
                .reserved_total
                .saturating_sub(unreserved);
        }
        ctx.accounts.claim.loss = loss;
        ctx.accounts.claim.deny_reason = deny_reason::NONE;
        ctx.accounts.claim.streamed = carried_streamed;

        let claim = &mut ctx.accounts.claim;
        let config = &mut ctx.accounts.config;
        try_admit(config, claim, now)?;
        if claim.status == ClaimStatus::Active && carried_streamed > 0 {
            let stream_secs = claim.stream_end.saturating_sub(claim.cooldown_end);
            let shift = (carried_streamed as i128 * stream_secs as i128 / loss as i128) as i64;
            claim.vest_origin = claim.vest_origin.saturating_sub(shift);
        }
        let claim_key = claim.key();
        let claim_status = claim.status;

        ctx.accounts.override_request.executed = true;

        emit!(ClaimOverridden {
            claim: claim_key,
            ref_at_liq,
            loss,
            status: claim_status,
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

    /// CHECK: phase 4 — existence alone (owner == this program) means `revoke_attestation`
    /// marked this exact (liquidation_record, evidence_hash) pair revoked; never deserialized.
    #[account(seeds = [REVOKE_SEED, args.liquidation_record.as_ref(), args.evidence_hash.as_ref()], bump)]
    pub revoked: UncheckedAccount<'info>,

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

// ------------------------------------------------------------------ phase 3: pool-as-liquidator

#[derive(Accounts)]
pub struct OpenInventory<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub market: Box<Account<'info, stock_vault::state::Market>>,
    #[account(
        init,
        payer = payer,
        space = 8 + Inventory::INIT_SPACE,
        seeds = [INVENTORY_SEED, market.key().as_ref()],
        bump,
    )]
    pub inventory: Box<Account<'info, Inventory>>,
    #[account(address = market.collateral_mint, mint::token_program = collateral_token_program)]
    pub collateral_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, BackstopConfig>>,
    #[account(
        init,
        payer = payer,
        seeds = [INVENTORY_VAULT_SEED, market.key().as_ref()],
        bump,
        token::mint = collateral_mint,
        token::authority = config,
        token::token_program = collateral_token_program,
    )]
    pub inventory_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub collateral_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct OpenInterestAbsorbed<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub market: Box<Account<'info, stock_vault::state::Market>>,
    #[account(
        init,
        payer = payer,
        space = 8 + InterestAbsorbed::INIT_SPACE,
        seeds = [INTEREST_SEED, market.key().as_ref()],
        bump,
    )]
    pub interest_absorbed: Box<Account<'info, InterestAbsorbed>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct PoolLiquidate<'info> {
    /// Pays the new LiquidationRecord's rent (A1) and receives the crank fee.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, BackstopConfig>>,
    /// The vault's own config, required by the CPI; re-validated there, not here.
    pub vault_config: Box<Account<'info, stock_vault::state::VaultConfig>>,
    #[account(mut)]
    pub market: Box<Account<'info, stock_vault::state::Market>>,
    /// CHECK: not independently constrained here -- the CPI'd `liquidate` re-derives and checks it
    /// against its own seeds, so a wrong position simply fails inside the CPI.
    #[account(mut)]
    pub position: UncheckedAccount<'info>,
    /// CHECK: the vault's own `init` constraint creates this during the CPI. Derived here with the
    /// vault's own seeds so a wrong address fails before the CPI runs, not inside it.
    #[account(
        mut,
        seeds = [stock_vault::state::LIQ_RECORD_SEED, market.key().as_ref(), market.liq_seq.to_le_bytes().as_ref()],
        bump,
        seeds::program = stock_vault::ID,
    )]
    pub record: UncheckedAccount<'info>,
    #[account(address = market.collateral_mint, mint::token_program = collateral_token_program)]
    pub collateral_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address = market.usdc_mint, mint::token_program = usdc_token_program)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    /// The pool's own USDC -- doubles as the CPI's `liquidator_usdc`.
    #[account(mut, seeds = [USDC_VAULT_SEED], bump, token::token_program = usdc_token_program)]
    pub pool_usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [INVENTORY_SEED, market.key().as_ref()], bump = inventory.bump, has_one = market)]
    pub inventory: Box<Account<'info, Inventory>>,
    /// The pool's own collateral holding -- doubles as the CPI's `liquidator_collateral`.
    #[account(mut, seeds = [INVENTORY_VAULT_SEED, market.key().as_ref()], bump, token::token_program = collateral_token_program)]
    pub inventory_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: the vault's own seeds; re-validated by the CPI callee.
    #[account(mut, seeds = [stock_vault::state::COLL_VAULT_SEED, market.key().as_ref()], bump, seeds::program = stock_vault::ID)]
    pub vault_collateral_vault: UncheckedAccount<'info>,
    /// CHECK: the vault's own seeds; re-validated by the CPI callee.
    #[account(mut, seeds = [stock_vault::state::USDC_VAULT_SEED, market.key().as_ref()], bump, seeds::program = stock_vault::ID)]
    pub vault_usdc_vault: UncheckedAccount<'info>,
    #[account(mut, token::mint = usdc_mint, token::authority = payer, token::token_program = usdc_token_program)]
    pub crank_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    pub collateral_token_program: Interface<'info, TokenInterface>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
    pub stock_vault_program: Program<'info, stock_vault::program::StockVault>,
}

#[derive(Accounts)]
pub struct BuyInventory<'info> {
    #[account(mut)]
    pub buyer: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, BackstopConfig>>,
    pub market: Box<Account<'info, stock_vault::state::Market>>,
    #[account(mut, seeds = [INVENTORY_SEED, market.key().as_ref()], bump = inventory.bump, has_one = market)]
    pub inventory: Box<Account<'info, Inventory>>,
    #[account(address = market.collateral_mint, mint::token_program = collateral_token_program)]
    pub collateral_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address = market.usdc_mint, mint::token_program = usdc_token_program)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, seeds = [INVENTORY_VAULT_SEED, market.key().as_ref()], bump, token::token_program = collateral_token_program)]
    pub inventory_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [USDC_VAULT_SEED], bump, token::token_program = usdc_token_program)]
    pub pool_usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = usdc_mint, token::authority = buyer, token::token_program = usdc_token_program)]
    pub buyer_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = collateral_mint, token::authority = buyer, token::token_program = collateral_token_program)]
    pub buyer_collateral: Box<InterfaceAccount<'info, TokenAccount>>,
    pub collateral_token_program: Interface<'info, TokenInterface>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct WriteOffInventory<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ VerdictError::Unauthorized)]
    pub config: Box<Account<'info, BackstopConfig>>,
    pub market: Box<Account<'info, stock_vault::state::Market>>,
    /// CHECK: only read for `issuer_blocks`; address-pinned to the market's own mint.
    #[account(address = market.collateral_mint)]
    pub collateral_mint: UncheckedAccount<'info>,
    #[account(mut, seeds = [INVENTORY_SEED, market.key().as_ref()], bump = inventory.bump, has_one = market)]
    pub inventory: Box<Account<'info, Inventory>>,
}

#[derive(Accounts)]
pub struct AbsorbInterest<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, BackstopConfig>>,
    pub market: Box<Account<'info, stock_vault::state::Market>>,
    #[account(mut, seeds = [INTEREST_SEED, market.key().as_ref()], bump = interest_absorbed.bump, has_one = market)]
    pub interest_absorbed: Box<Account<'info, InterestAbsorbed>>,
    #[account(seeds = [USDC_VAULT_SEED], bump, token::token_program = usdc_token_program)]
    pub pool_usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
}

// ------------------------------------------------------------------------- phase 4: overrides

#[derive(Accounts)]
pub struct CancelClaim<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ VerdictError::Unauthorized)]
    pub config: Box<Account<'info, BackstopConfig>>,
    #[account(mut, seeds = [CLAIM_SEED, claim.liquidation_record.as_ref()], bump = claim.bump, has_one = market)]
    pub claim: Box<Account<'info, Claim>>,
    pub market: Box<Account<'info, stock_vault::state::Market>>,
    #[account(mut, seeds = [BORROWER_CLAIMS_SEED, market.key().as_ref(), claim.borrower.as_ref()], bump = borrower_claims.bump)]
    pub borrower_claims: Box<Account<'info, BorrowerClaims>>,
}

#[derive(Accounts)]
pub struct SuspendClaim<'info> {
    pub admin: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ VerdictError::Unauthorized)]
    pub config: Box<Account<'info, BackstopConfig>>,
    #[account(mut, seeds = [CLAIM_SEED, claim.liquidation_record.as_ref()], bump = claim.bump)]
    pub claim: Box<Account<'info, Claim>>,
}

#[derive(Accounts)]
#[instruction(liquidation_record: Pubkey, evidence_hash: [u8; 32])]
pub struct RevokeAttestation<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ VerdictError::Unauthorized)]
    pub config: Box<Account<'info, BackstopConfig>>,
    #[account(
        init,
        payer = admin,
        space = 8 + RevokedAttestation::INIT_SPACE,
        seeds = [REVOKE_SEED, liquidation_record.as_ref(), evidence_hash.as_ref()],
        bump,
    )]
    pub revoked: Box<Account<'info, RevokedAttestation>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct OpenOverride<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub liquidation_record: Box<Account<'info, stock_vault::state::LiquidationRecord>>,
    #[account(
        init,
        payer = payer,
        space = 8 + OverrideRequest::INIT_SPACE,
        seeds = [OVERRIDE_SEED, liquidation_record.key().as_ref()],
        bump,
    )]
    pub override_request: Box<Account<'info, OverrideRequest>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ApproveOverride<'info> {
    pub caller: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, BackstopConfig>>,
    pub market: Box<Account<'info, stock_vault::state::Market>>,
    pub liquidation_record: Box<Account<'info, stock_vault::state::LiquidationRecord>>,
    #[account(mut, seeds = [CLAIM_SEED, liquidation_record.key().as_ref()], bump = claim.bump, has_one = market)]
    pub claim: Box<Account<'info, Claim>>,
    #[account(mut, seeds = [OVERRIDE_SEED, liquidation_record.key().as_ref()], bump = override_request.bump)]
    pub override_request: Box<Account<'info, OverrideRequest>>,
    /// CHECK: the payout address's Backer PDA — same A4 pattern as `SubmitFacts`.
    #[account(seeds = [BACKER_SEED, liquidation_record.payout.as_ref()], bump)]
    pub payout_backer: UncheckedAccount<'info>,
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

#[event]
pub struct PoolLiquidated {
    pub market: Pubkey,
    pub seized_raw: u64,
    pub debt_repaid: u64,
    pub bonus_bps: u32,
    pub fee: u64,
}

#[event]
pub struct InventorySold {
    pub market: Pubkey,
    pub raw: u64,
    pub sale_value: u64,
}

#[event]
pub struct InventoryWrittenOff {
    pub market: Pubkey,
    pub raw_stranded: u64,
    pub cost_written_off: u64,
}

#[event]
pub struct InterestAbsorbedEvent {
    pub market: Pubkey,
    pub credited: u64,
}

#[event]
pub struct ClaimTimingSet {
    pub gate_secs: i64,
    pub cooldown_secs: i64,
    pub stream_secs: i64,
    pub inactivity_secs: i64,
    pub min_after_wait_secs: i64,
}

#[event]
pub struct CoSignerRotated {
    pub co_signer: Pubkey,
}

#[event]
pub struct ClaimCancelled {
    pub claim: Pubkey,
    pub reason_code: u16,
    pub penalized: bool,
}

#[event]
pub struct ClaimSuspendChanged {
    pub claim: Pubkey,
    pub suspended: bool,
}

#[event]
pub struct AttestationRevokedEvent {
    pub liquidation_record: Pubkey,
    pub evidence_hash: [u8; 32],
}

#[event]
pub struct ClaimOverridden {
    pub claim: Pubkey,
    pub ref_at_liq: u64,
    pub loss: u64,
    pub status: ClaimStatus,
}

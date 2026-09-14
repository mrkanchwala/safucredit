use anchor_lang::prelude::*;
use anchor_spl::token_2022::spl_token_2022::state::AccountState;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use safu_core::lending::{
    current_ltv_bps, debt_for_shares, liquidation_bonus_bps, max_borrow, max_repay,
    repay_for_seized, seize_for_repay, shares_for_borrow, shares_for_repay, INDEX_SCALE,
};

pub mod errors;
pub mod logic;
pub mod state;

use errors::VaultError;
use logic::*;
use state::*;

declare_id!("GkQw6VGDKYBWJtgtWUmFDkHNqGrnyjSviQeQzVMW35K2");

#[program]
pub mod stock_vault {
    use super::*;

    /// Only the program's upgrade authority may initialize, so nobody watching the deploy can call it
    /// first and take admin. Initialize before any `--final`: a program with no upgrade authority can
    /// never be initialized.
    pub fn initialize_config(ctx: Context<InitializeConfig>, feed_authority: Pubkey) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.version = ACCOUNT_VERSION;
        config.admin = ctx.accounts.admin.key();
        config.feed_authority = feed_authority;
        config.paused = false;
        config.bump = ctx.bumps.config;
        config.pool_liquidator = Pubkey::default();
        config.fallback_grace_secs = DEFAULT_FALLBACK_GRACE_SECS;
        config.reserved = [0; 24];
        Ok(())
    }

    pub fn set_paused(ctx: Context<AdminOnly>, paused: bool) -> Result<()> {
        ctx.accounts.config.paused = paused;
        emit!(PausedSet { paused });
        Ok(())
    }

    /// Key rotation (U5).
    pub fn set_feed_authority(ctx: Context<AdminOnly>, feed_authority: Pubkey) -> Result<()> {
        ctx.accounts.config.feed_authority = feed_authority;
        Ok(())
    }

    /// Registers the backstop pool's liquidator key (its config PDA). Default unregisters it, which leaves the
    /// grace rule applying to every liquidator. Never the vault admin or the feed authority.
    pub fn set_pool_liquidator(ctx: Context<AdminOnly>, pool_liquidator: Pubkey) -> Result<()> {
        let config = &mut ctx.accounts.config;
        require!(
            pool_liquidator != config.admin && pool_liquidator != config.feed_authority,
            VaultError::InvalidPoolLiquidator
        );
        config.pool_liquidator = pool_liquidator;
        Ok(())
    }

    /// How long a position must be liquidatable before outside liquidators may act. Bounded 1 min to 1 day.
    pub fn set_fallback_grace(ctx: Context<AdminOnly>, secs: i64) -> Result<()> {
        require!(
            (MIN_FALLBACK_GRACE_SECS..=MAX_FALLBACK_GRACE_SECS).contains(&secs),
            VaultError::InvalidFallbackGrace
        );
        ctx.accounts.config.fallback_grace_secs = secs;
        Ok(())
    }

    /// Permissionless. Records that a position can be liquidated right now (every liquidation guard passes and
    /// it is past its line), or clears the record if it cannot. The pool may liquidate at any time; outside
    /// liquidators only once the position has been marked for the grace period and the mark is still fresh.
    /// A mark older than twice the grace period restarts the clock, so a position that recovered and fell
    /// again gets the pool's full head start.
    pub fn mark_liquidatable(ctx: Context<MarkLiquidatable>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let mint_info = ctx.accounts.collateral_mint.to_account_info();
        let vault_amount = ctx.accounts.collateral_vault.amount;
        let vault_frozen = ctx.accounts.collateral_vault.state == AccountState::Frozen;
        let (paused, grace) = (
            ctx.accounts.config.paused,
            ctx.accounts.config.fallback_grace_secs,
        );
        let market = &mut ctx.accounts.market;
        let position = &mut ctx.accounts.position;
        let open = !paused
            && liquidation_view(
                market,
                position,
                &mint_info,
                vault_amount,
                vault_frozen,
                now,
            )
            .is_ok();
        if open {
            let stale =
                now.saturating_sub(position.liquidatable_last_seen) > grace.saturating_mul(2);
            if position.liquidatable_first_seen == 0 || stale {
                position.liquidatable_first_seen = now;
            }
            position.liquidatable_last_seen = now;
        } else {
            position.liquidatable_first_seen = 0;
            position.liquidatable_last_seen = 0;
        }
        emit!(LiquidatableMarked {
            position: position.key(),
            first_seen: position.liquidatable_first_seen,
            last_seen: position.liquidatable_last_seen,
        });
        Ok(())
    }

    /// Admin rotation (U5), matching the backstop. Refuses the default key, which would leave the
    /// vault with no reachable admin.
    pub fn set_admin(ctx: Context<AdminOnly>, admin: Pubkey) -> Result<()> {
        require_keys_neq!(admin, Pubkey::default(), VaultError::InvalidAdmin);
        ctx.accounts.config.admin = admin;
        Ok(())
    }

    /// Clears a hold raised by `unobserved_multiplier_change`. The feed authority or the admin
    /// attests that the price feed and the token's multiplier are back in step, and the market
    /// resumes against the new value.
    ///
    /// This is an operational gate on an unannounced corporate action, not a claims decision: it
    /// cannot move anyone's money, cannot raise or lower a payout, and is recorded on-chain by the
    /// event below.
    pub fn acknowledge_multiplier(ctx: Context<AcknowledgeMultiplier>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let mint_info = ctx.accounts.collateral_mint.to_account_info();
        let schedule = MultiplierSchedule::read(&mint_info)?;
        let market_key = ctx.accounts.market.key();
        let market = &mut ctx.accounts.market;
        // Attesting against a missing or flagged price would defeat the point of the hold.
        require!(
            market.price.count > 0 && !market.price.flagged,
            VaultError::PriceUnavailable
        );
        let previous = market.observed_multiplier_fp;
        let effective = schedule.effective(now);
        market.observed_multiplier_fp = effective;
        emit!(MultiplierAcknowledged {
            market: market_key,
            previous,
            effective,
        });
        Ok(())
    }

    pub fn create_market(ctx: Context<CreateMarket>, params: MarketParams) -> Result<()> {
        params.validate()?;

        let collateral = ctx.accounts.collateral_mint.to_account_info();
        require_keys_eq!(
            *collateral.owner,
            anchor_spl::token_2022::ID,
            VaultError::UnsupportedCollateralMint
        );
        let schedule = MultiplierSchedule::read(&collateral)?;
        reject_risky_mint(&collateral)?;
        let usdc = ctx.accounts.usdc_mint.to_account_info();
        if *usdc.owner == anchor_spl::token_2022::ID {
            reject_risky_mint(&usdc)?;
        }

        let now = Clock::get()?.unix_timestamp;
        let market = &mut ctx.accounts.market;
        market.version = ACCOUNT_VERSION;
        market.bump = ctx.bumps.market;
        market.collateral_mint = ctx.accounts.collateral_mint.key();
        market.usdc_mint = ctx.accounts.usdc_mint.key();
        market.collateral_decimals = ctx.accounts.collateral_mint.decimals;
        market.usdc_decimals = ctx.accounts.usdc_mint.decimals;
        market.params = params;
        market.price = PriceState::default();
        market.cash = 0;
        market.total_supply_shares = 0;
        market.total_borrow_shares = 0;
        market.borrow_index = INDEX_SCALE;
        market.last_accrual_ts = now;
        market.total_collateral_raw = 0;
        market.bad_debt = 0;
        market.bad_debt_cumulative = 0;
        market.issuer_halt = false;
        market.liq_seq = 0;
        market.observed_multiplier_fp = schedule.effective(now);
        market.ramp_from = params.liquidation_terms();
        market.ramp_start_ts = now;
        market.reserved = [0; 12];

        emit!(MarketCreated {
            market: market.key(),
            collateral_mint: market.collateral_mint,
            usdc_mint: market.usdc_mint,
        });
        Ok(())
    }

    /// U2/U3: interest accrues at the old rate up to now; open loans keep their coverage snapshot.
    pub fn update_market_params(ctx: Context<UpdateMarket>, params: MarketParams) -> Result<()> {
        params.validate()?;
        let now = Clock::get()?.unix_timestamp;
        let market = &mut ctx.accounts.market;
        accrue(market, now)?;
        // U3: loosening applies at once; tightening reaches existing loans over seven days, starting
        // from the terms in force right now (so a change made mid-ramp never jumps).
        let live = effective_liquidation_terms(market, now);
        let target = params.liquidation_terms();
        market.ramp_from = ramp_start_terms(live, target);
        market.ramp_start_ts = now;
        market.params = params;
        if market.ramp_from != target {
            emit!(LiquidationTermsTightening {
                market: market.key(),
                from: market.ramp_from,
                to: target,
                starts: now,
                completes: now + LIQUIDATION_TERMS_RAMP_SECS,
            });
        }
        Ok(())
    }

    pub fn push_price(
        ctx: Context<PushPrice>,
        price: u64,
        market_open: bool,
        last_close: u64,
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let accepted = apply_price(
            &mut ctx.accounts.market,
            price,
            market_open,
            last_close,
            now,
        )?;
        emit!(PriceUpdated {
            market: ctx.accounts.market.key(),
            price,
            market_open,
            accepted,
        });
        Ok(())
    }

    pub fn open_supplier(ctx: Context<OpenSupplier>) -> Result<()> {
        let supplier = &mut ctx.accounts.supplier;
        supplier.version = ACCOUNT_VERSION;
        supplier.bump = ctx.bumps.supplier;
        supplier.market = ctx.accounts.market.key();
        supplier.owner = ctx.accounts.owner.key();
        supplier.shares = 0;
        Ok(())
    }

    pub fn open_position(ctx: Context<OpenPosition>) -> Result<()> {
        let position = &mut ctx.accounts.position;
        position.version = ACCOUNT_VERSION;
        position.bump = ctx.bumps.position;
        position.market = ctx.accounts.market.key();
        position.owner = ctx.accounts.owner.key();
        position.raw_collateral = 0;
        position.debt_shares = 0;
        position.coverage_bps = 0;
        position.borrow_age_ts = 0;
        position.payout = ctx.accounts.owner.key();
        position.liquidatable_first_seen = 0;
        position.liquidatable_last_seen = 0;
        position.reserved = [0; 16];
        Ok(())
    }

    /// Where this position's wrongful-liquidation payback goes. Owner only. Refuses the default key and
    /// the vault's own admin and feed keys (the backstop refuses its oracle and co-signer at payout). A
    /// liquidation record freezes the address, so a later change, or a stolen key, cannot redirect a
    /// payback already owed.
    pub fn set_payout_address(ctx: Context<SetPayoutAddress>, payout: Pubkey) -> Result<()> {
        let config = &ctx.accounts.config;
        require!(
            payout != Pubkey::default()
                && payout != config.admin
                && payout != config.feed_authority,
            VaultError::InvalidPayoutAddress
        );
        let position = &mut ctx.accounts.position;
        position.payout = payout;
        emit!(PayoutAddressSet {
            position: position.key(),
            payout
        });
        Ok(())
    }

    pub fn supply(ctx: Context<Supply>, amount: u64) -> Result<()> {
        require!(!ctx.accounts.config.paused, VaultError::Paused);
        require!(amount > 0, VaultError::ZeroAmount);
        let now = Clock::get()?.unix_timestamp;
        let market = &mut ctx.accounts.market;
        accrue(market, now)?;

        let shares = if market.total_supply_shares == 0 {
            amount as u128
        } else {
            let assets = total_assets(market)?;
            require!(assets > 0, VaultError::InsufficientBalance);
            mul_div_floor(amount as u128, market.total_supply_shares, assets as u128)?
        };
        require!(shares > 0, VaultError::ZeroAmount);

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

        market.cash = market
            .cash
            .checked_add(amount)
            .ok_or(VaultError::MathOverflow)?;
        market.total_supply_shares = market
            .total_supply_shares
            .checked_add(shares)
            .ok_or(VaultError::MathOverflow)?;
        let supplier = &mut ctx.accounts.supplier;
        supplier.shares = supplier
            .shares
            .checked_add(shares)
            .ok_or(VaultError::MathOverflow)?;
        emit!(Supplied {
            owner: supplier.owner,
            amount,
            shares
        });
        Ok(())
    }

    pub fn withdraw_supply(ctx: Context<WithdrawSupply>, shares: u128) -> Result<()> {
        require!(!ctx.accounts.config.paused, VaultError::Paused);
        require!(shares > 0, VaultError::ZeroAmount);
        require!(
            shares <= ctx.accounts.supplier.shares,
            VaultError::InsufficientBalance
        );
        let now = Clock::get()?.unix_timestamp;
        let market_info = ctx.accounts.market.to_account_info();
        let market = &mut ctx.accounts.market;
        accrue(market, now)?;

        let amount = u64::try_from(mul_div_floor(
            shares,
            total_assets(market)? as u128,
            market.total_supply_shares,
        )?)
        .map_err(|_| VaultError::MathOverflow)?;
        require!(amount > 0, VaultError::ZeroAmount);
        require!(amount <= market.cash, VaultError::InsufficientCash);

        market.cash -= amount;
        market.total_supply_shares -= shares;
        ctx.accounts.supplier.shares -= shares;

        let collateral_mint = market.collateral_mint;
        let bump = [market.bump];
        let seeds: &[&[&[u8]]] = &[&[MARKET_SEED, collateral_mint.as_ref(), &bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.usdc_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.usdc_vault.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.owner_usdc.to_account_info(),
                    authority: market_info,
                },
                seeds,
            ),
            amount,
            ctx.accounts.usdc_mint.decimals,
        )?;
        emit!(SupplyWithdrawn {
            owner: ctx.accounts.owner.key(),
            amount,
            shares
        });
        Ok(())
    }

    /// Adding collateral only reduces risk, so it is allowed while paused.
    pub fn deposit_collateral(ctx: Context<DepositCollateral>, amount: u64) -> Result<()> {
        require!(amount > 0, VaultError::ZeroAmount);
        let market = &mut ctx.accounts.market;
        let new_total = market
            .total_collateral_raw
            .checked_add(amount)
            .ok_or(VaultError::MathOverflow)?;
        require!(
            new_total <= market.params.collateral_cap_raw,
            VaultError::CapReached
        );

        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.collateral_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.owner_collateral.to_account_info(),
                    mint: ctx.accounts.collateral_mint.to_account_info(),
                    to: ctx.accounts.collateral_vault.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.collateral_mint.decimals,
        )?;

        market.total_collateral_raw = new_total;
        let position = &mut ctx.accounts.position;
        position.raw_collateral = position
            .raw_collateral
            .checked_add(amount)
            .ok_or(VaultError::MathOverflow)?;
        emit!(CollateralDeposited {
            owner: position.owner,
            amount
        });
        Ok(())
    }

    pub fn withdraw_collateral(ctx: Context<WithdrawCollateral>, amount: u64) -> Result<()> {
        require!(!ctx.accounts.config.paused, VaultError::Paused);
        require!(amount > 0, VaultError::ZeroAmount);
        require!(
            amount <= ctx.accounts.position.raw_collateral,
            VaultError::InsufficientBalance
        );
        let now = Clock::get()?.unix_timestamp;
        let market_info = ctx.accounts.market.to_account_info();
        let mint_info = ctx.accounts.collateral_mint.to_account_info();
        let market = &mut ctx.accounts.market;
        accrue(market, now)?;

        let position = &ctx.accounts.position;
        let debt = logic::core(debt_for_shares(position.debt_shares, market.borrow_index))?;
        if debt > 0 {
            require!(
                !market.issuer_halt && !issuer_blocks(&mint_info),
                VaultError::IssuerHalt
            );
            let schedule = MultiplierSchedule::read(&mint_info)?;
            require!(
                !corporate_action_hold(market, &schedule, now)
                    && !unobserved_multiplier_change(market, &schedule, now),
                VaultError::CorporateActionHold
            );
            market.observed_multiplier_fp = schedule.effective(now);
            let price = risk_price(market, now, PriceUse::Borrow)?;
            let remaining = position.raw_collateral - amount;
            let value_after = value_of(market, remaining, schedule.effective(now), price)?;
            let limit = logic::core(max_borrow(value_after, market.params.ltv_bps))?;
            require!(debt <= limit, VaultError::ExceedsLtv);
        }

        market.total_collateral_raw -= amount;
        ctx.accounts.position.raw_collateral -= amount;

        let collateral_mint = market.collateral_mint;
        let bump = [market.bump];
        let seeds: &[&[&[u8]]] = &[&[MARKET_SEED, collateral_mint.as_ref(), &bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.collateral_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.collateral_vault.to_account_info(),
                    mint: mint_info,
                    to: ctx.accounts.owner_collateral.to_account_info(),
                    authority: market_info,
                },
                seeds,
            ),
            amount,
            ctx.accounts.collateral_mint.decimals,
        )?;
        emit!(CollateralWithdrawn {
            owner: ctx.accounts.owner.key(),
            amount
        });
        Ok(())
    }

    pub fn borrow(ctx: Context<Borrow>, amount: u64) -> Result<()> {
        require!(!ctx.accounts.config.paused, VaultError::Paused);
        require!(amount > 0, VaultError::ZeroAmount);
        let now = Clock::get()?.unix_timestamp;
        let market_info = ctx.accounts.market.to_account_info();
        let mint_info = ctx.accounts.collateral_mint.to_account_info();
        let market = &mut ctx.accounts.market;
        accrue(market, now)?;

        require!(
            !market.issuer_halt && !issuer_blocks(&mint_info),
            VaultError::IssuerHalt
        );
        let schedule = MultiplierSchedule::read(&mint_info)?;
        require!(
            !corporate_action_hold(market, &schedule, now)
                && !unobserved_multiplier_change(market, &schedule, now),
            VaultError::CorporateActionHold
        );
        market.observed_multiplier_fp = schedule.effective(now);
        let price = risk_price(market, now, PriceUse::Borrow)?;

        let position = &mut ctx.accounts.position;
        let value = value_of(
            market,
            position.raw_collateral,
            schedule.effective(now),
            price,
        )?;
        let debt = logic::core(debt_for_shares(position.debt_shares, market.borrow_index))?;
        let new_debt = debt.checked_add(amount).ok_or(VaultError::MathOverflow)?;
        let limit = logic::core(max_borrow(value, market.params.ltv_bps))?;
        require!(new_debt <= limit, VaultError::ExceedsLtv);
        require!(amount <= market.cash, VaultError::InsufficientCash);
        let borrowed_after = total_borrows(market)?
            .checked_add(amount)
            .ok_or(VaultError::MathOverflow)?;
        require!(
            borrowed_after <= market.params.borrow_cap,
            VaultError::CapReached
        );

        if position.debt_shares == 0 {
            position.coverage_bps = FULL_COVERAGE_BPS;
        }
        // Loan age for the backstop's 60-day gate: a new loan starts now; a top-up pulls the age toward
        // now in proportion to its size (so a tiny early loan cannot make a large late one look old).
        position.borrow_age_ts = if position.debt_shares == 0 {
            now
        } else {
            weighted_borrow_age(debt, position.borrow_age_ts, amount, now)?
        };
        let shares = logic::core(shares_for_borrow(amount, market.borrow_index))?;
        position.debt_shares = position
            .debt_shares
            .checked_add(shares)
            .ok_or(VaultError::MathOverflow)?;
        market.total_borrow_shares = market
            .total_borrow_shares
            .checked_add(shares)
            .ok_or(VaultError::MathOverflow)?;
        market.cash -= amount;

        let collateral_mint = market.collateral_mint;
        let bump = [market.bump];
        let seeds: &[&[&[u8]]] = &[&[MARKET_SEED, collateral_mint.as_ref(), &bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.usdc_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.usdc_vault.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.owner_usdc.to_account_info(),
                    authority: market_info,
                },
                seeds,
            ),
            amount,
            ctx.accounts.usdc_mint.decimals,
        )?;
        emit!(Borrowed {
            owner: ctx.accounts.owner.key(),
            amount,
            shares
        });
        Ok(())
    }

    /// Anyone may repay a position. Allowed while paused (it only reduces risk).
    pub fn repay(ctx: Context<Repay>, amount: u64) -> Result<()> {
        require!(amount > 0, VaultError::ZeroAmount);
        let now = Clock::get()?.unix_timestamp;
        let market = &mut ctx.accounts.market;
        accrue(market, now)?;

        let position = &mut ctx.accounts.position;
        let debt = logic::core(debt_for_shares(position.debt_shares, market.borrow_index))?;
        require!(debt > 0, VaultError::InsufficientBalance);
        let pay = amount.min(debt);
        let burn = if pay == debt {
            position.debt_shares
        } else {
            logic::core(shares_for_repay(pay, market.borrow_index))?.min(position.debt_shares)
        };

        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.usdc_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.payer_usdc.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.usdc_vault.to_account_info(),
                    authority: ctx.accounts.payer.to_account_info(),
                },
            ),
            pay,
            ctx.accounts.usdc_mint.decimals,
        )?;

        position.debt_shares -= burn;
        if position.debt_shares == 0 {
            // Fully repaid: the next borrow is a new loan with a fresh age.
            position.borrow_age_ts = 0;
        }
        market.total_borrow_shares = market.total_borrow_shares.saturating_sub(burn);
        market.cash = market
            .cash
            .checked_add(pay)
            .ok_or(VaultError::MathOverflow)?;
        emit!(Repaid {
            owner: position.owner,
            amount: pay,
            shares: burn
        });
        Ok(())
    }

    /// Permissionless. Re-derives the issuer-controlled facts the market must respect and records
    /// them, so a halt can be raised (or lifted) without waiting for someone to attempt a risk
    /// action. It reflects observable on-chain state only, which is why it needs no authority.
    ///
    /// Known limit: a burn by the mint's permanent delegate leaves `reconciliation_short` true
    /// forever, because the collateral really is gone. Clearing that needs an admin write-down,
    /// which lands with upgradeability (stage 2 item 5). Until then the halt is permanent and
    /// correct — the market genuinely cannot honour the collateral it has recorded.
    pub fn sync_issuer_state(ctx: Context<SyncIssuerState>) -> Result<()> {
        let mint_info = ctx.accounts.collateral_mint.to_account_info();
        let mint_blocked = issuer_blocks(&mint_info);
        let vault_frozen = ctx.accounts.collateral_vault.state == AccountState::Frozen;
        let vault_amount = ctx.accounts.collateral_vault.amount;
        let market_key = ctx.accounts.market.key();
        let market = &mut ctx.accounts.market;
        let short = reconciliation_short(vault_amount, market);
        let halted = mint_blocked || vault_frozen || short;
        market.issuer_halt = halted;
        emit!(IssuerHaltSet {
            market: market_key,
            halted,
            mint_blocked,
            vault_frozen,
            reconciliation_short: short,
        });
        Ok(())
    }

    /// Credits USDC sitting in the market's vault beyond what the market has accounted for, against
    /// bad debt. Permissionless and authority-free by construction.
    ///
    /// The backstop reimburses bad debt by simply transferring USDC into this market's USDC vault.
    /// Cash is tracked internally precisely so a raw transfer cannot move the share price, which
    /// means that reimbursement would otherwise sit unaccounted forever. This instruction is the one
    /// place unaccounted surplus may be recognised, and it is bounded by `bad_debt` — so a stranger
    /// donating tokens can repair a write-off but can never inflate the pool beyond it. Anything
    /// above the outstanding bad debt stays unaccounted, exactly as before.
    pub fn absorb_bad_debt_cover(ctx: Context<AbsorbBadDebtCover>) -> Result<()> {
        let vault_amount = ctx.accounts.usdc_vault.amount;
        let market_key = ctx.accounts.market.key();
        let market = &mut ctx.accounts.market;
        let surplus = vault_amount.saturating_sub(market.cash);
        let absorbed = surplus.min(market.bad_debt);
        require!(absorbed > 0, VaultError::ZeroAmount);
        market.cash = market
            .cash
            .checked_add(absorbed)
            .ok_or(VaultError::MathOverflow)?;
        market.bad_debt -= absorbed;
        emit!(BadDebtCovered {
            market: market_key,
            absorbed,
            bad_debt_remaining: market.bad_debt,
        });
        Ok(())
    }

    /// Permissionless liquidation of a position past its threshold.
    ///
    /// The liquidator repays USDC on the borrower's behalf and receives collateral at a bonus that
    /// grows with how far the position has drifted, bounded by solvency, the close factor, and a
    /// chunk cap sized from measured liquidity. Everything used to price the seizure is snapshotted
    /// into a `LiquidationRecord`: the wrongful-liquidation verdict is decided off-chain against
    /// exactly these inputs, and must never be re-derived from a later feed state.
    pub fn liquidate(ctx: Context<Liquidate>, repay_amount: u64) -> Result<()> {
        require!(!ctx.accounts.config.paused, VaultError::Paused);
        require!(repay_amount > 0, VaultError::ZeroAmount);
        let clock = Clock::get()?;
        let (now, slot) = (clock.unix_timestamp, clock.slot);

        let market_info = ctx.accounts.market.to_account_info();
        let mint_info = ctx.accounts.collateral_mint.to_account_info();
        let vault_amount = ctx.accounts.collateral_vault.amount;
        let vault_frozen = ctx.accounts.collateral_vault.state == AccountState::Frozen;
        let market_key = ctx.accounts.market.key();
        let liquidator_key = ctx.accounts.liquidator.key();

        let (pool, grace) = (
            ctx.accounts.config.pool_liquidator,
            ctx.accounts.config.fallback_grace_secs,
        );
        let market = &mut ctx.accounts.market;
        let position = &mut ctx.accounts.position;
        let LiquidationView {
            multiplier_fp,
            price,
            value,
            debt,
            terms,
        } = liquidation_view(
            market,
            position,
            &mint_info,
            vault_amount,
            vault_frozen,
            now,
        )?;
        // Pool priority (backstop-as-liquidator lock): the pool acts at once; anyone else only after the position
        // has been liquidatable for the grace period, which covers a pool that is empty, capped, paused or broken.
        if liquidator_key != pool {
            require!(
                fallback_open(
                    position.liquidatable_first_seen,
                    position.liquidatable_last_seen,
                    grace,
                    now
                ),
                VaultError::PoolPriority
            );
        }
        // Frozen into the record below, before any reset: the gate and the payback use these as they were.
        let (borrow_age_ts, payout) = (position.borrow_age_ts, position.payout);

        let ltv_bps = logic::core(current_ltv_bps(debt, value))?;
        let bonus_bps = logic::core(liquidation_bonus_bps(
            ltv_bps,
            terms.liq_threshold_bps,
            terms.min_liq_bonus_bps,
            terms.max_liq_bonus_bps,
        ))?;
        let allowed = logic::core(max_repay(
            debt,
            ltv_bps,
            terms.insolvency_ltv_bps,
            terms.close_factor_bps,
            market.params.max_liquidation_debt,
        ))?;
        let mut pay = repay_amount.min(allowed);
        require!(pay > 0, VaultError::ZeroAmount);

        let wanted = logic::core(seize_for_repay(
            pay,
            bonus_bps,
            market.collateral_decimals,
            multiplier_fp,
            price,
        ))?;
        // The position may hold less than the repayment would buy. Seize what is there and cut the
        // repayment to match, so the liquidator never pays for collateral that does not exist —
        // and, just as importantly, so the debt the collateral could not cover survives as bad debt
        // instead of being silently cleared by an overpayment.
        let seized = wanted.min(position.raw_collateral);
        if seized < wanted {
            let seized_value = value_of(market, seized, multiplier_fp, price)?;
            pay = logic::core(repay_for_seized(seized_value, bonus_bps))?.min(pay);
        }
        require!(seized > 0 && pay > 0, VaultError::NothingSeized);

        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.usdc_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.liquidator_usdc.to_account_info(),
                    mint: ctx.accounts.usdc_mint.to_account_info(),
                    to: ctx.accounts.usdc_vault.to_account_info(),
                    authority: ctx.accounts.liquidator.to_account_info(),
                },
            ),
            pay,
            ctx.accounts.usdc_mint.decimals,
        )?;

        let collateral_mint = market.collateral_mint;
        let bump = [market.bump];
        let seeds: &[&[&[u8]]] = &[&[MARKET_SEED, collateral_mint.as_ref(), &bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.collateral_token_program.key(),
                TransferChecked {
                    from: ctx.accounts.collateral_vault.to_account_info(),
                    mint: mint_info,
                    to: ctx.accounts.liquidator_collateral.to_account_info(),
                    authority: market_info,
                },
                seeds,
            ),
            seized,
            ctx.accounts.collateral_mint.decimals,
        )?;

        let burn = if pay >= debt {
            position.debt_shares
        } else {
            logic::core(shares_for_repay(pay, market.borrow_index))?.min(position.debt_shares)
        };
        position.debt_shares -= burn;
        market.total_borrow_shares = market.total_borrow_shares.saturating_sub(burn);
        market.cash = market
            .cash
            .checked_add(pay)
            .ok_or(VaultError::MathOverflow)?;
        position.raw_collateral -= seized;
        market.total_collateral_raw -= seized;

        // Collateral exhausted with debt still outstanding: write it off now rather than let it
        // accrue interest nobody will ever pay. Removing the shares is what charges the loss to
        // suppliers; `bad_debt` is the memo of what the backstop owes them back.
        let mut bad_debt = 0u64;
        if position.raw_collateral == 0 && position.debt_shares > 0 {
            bad_debt = logic::core(debt_for_shares(position.debt_shares, market.borrow_index))?;
            market.total_borrow_shares = market
                .total_borrow_shares
                .saturating_sub(position.debt_shares);
            position.debt_shares = 0;
            market.bad_debt = market
                .bad_debt
                .checked_add(bad_debt)
                .ok_or(VaultError::MathOverflow)?;
            market.bad_debt_cumulative = market
                .bad_debt_cumulative
                .checked_add(bad_debt)
                .ok_or(VaultError::MathOverflow)?;
        }

        if position.debt_shares == 0 {
            position.borrow_age_ts = 0;
            position.liquidatable_first_seen = 0;
            position.liquidatable_last_seen = 0;
        }

        let seq = market.liq_seq;
        market.liq_seq = seq.checked_add(1).ok_or(VaultError::MathOverflow)?;
        let issuer_halt = market.issuer_halt;
        let collateral_decimals = market.collateral_decimals;

        let record = &mut ctx.accounts.record;
        record.version = ACCOUNT_VERSION;
        record.bump = ctx.bumps.record;
        record.market = market_key;
        record.seq = seq;
        record.borrower = position.owner;
        record.liquidator = liquidator_key;
        record.seized_raw = seized;
        record.debt_repaid = pay;
        record.multiplier_fp = multiplier_fp;
        record.price_fp = price;
        record.collateral_decimals = collateral_decimals;
        record.bonus_bps = bonus_bps;
        record.ltv_bps = ltv_bps;
        record.coverage_bps = position.coverage_bps;
        record.borrow_age_ts = borrow_age_ts;
        record.payout = payout;
        record.bad_debt = bad_debt;
        record.ts = now;
        record.slot = slot;
        record.issuer_halt = issuer_halt;
        record.reserved = [0; 32];

        emit!(Liquidated {
            market: market_key,
            seq,
            borrower: record.borrower,
            liquidator: liquidator_key,
            seized_raw: seized,
            debt_repaid: pay,
            bonus_bps,
            ltv_bps,
            price_fp: price,
            bad_debt,
        });
        Ok(())
    }
}

// ------------------------------------------------------------------ accounts

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    /// This program. Proves `program_data` belongs to it, not to a program the caller deployed.
    #[account(constraint = program.programdata_address()? == Some(program_data.key()) @ VaultError::NotUpgradeAuthority)]
    pub program: Program<'info, crate::program::StockVault>,
    #[account(constraint = program_data.upgrade_authority_address == Some(admin.key()) @ VaultError::NotUpgradeAuthority)]
    pub program_data: Account<'info, ProgramData>,
    #[account(init, payer = admin, space = 8 + VaultConfig::INIT_SPACE, seeds = [VCONFIG_SEED], bump)]
    pub config: Account<'info, VaultConfig>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [VCONFIG_SEED], bump = config.bump, has_one = admin @ VaultError::Unauthorized)]
    pub config: Account<'info, VaultConfig>,
}

#[derive(Accounts)]
pub struct CreateMarket<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(seeds = [VCONFIG_SEED], bump = config.bump, has_one = admin @ VaultError::Unauthorized)]
    pub config: Account<'info, VaultConfig>,
    #[account(mint::token_program = collateral_token_program)]
    pub collateral_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mint::token_program = usdc_token_program)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(
        init,
        payer = admin,
        space = 8 + Market::INIT_SPACE,
        seeds = [MARKET_SEED, collateral_mint.key().as_ref()],
        bump,
    )]
    pub market: Box<Account<'info, Market>>,
    #[account(
        init,
        payer = admin,
        seeds = [COLL_VAULT_SEED, market.key().as_ref()],
        bump,
        token::mint = collateral_mint,
        token::authority = market,
        token::token_program = collateral_token_program,
    )]
    pub collateral_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        init,
        payer = admin,
        seeds = [USDC_VAULT_SEED, market.key().as_ref()],
        bump,
        token::mint = usdc_mint,
        token::authority = market,
        token::token_program = usdc_token_program,
    )]
    pub usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub collateral_token_program: Interface<'info, TokenInterface>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AcknowledgeMultiplier<'info> {
    pub authority: Signer<'info>,
    #[account(
        seeds = [VCONFIG_SEED],
        bump = config.bump,
        constraint = config.feed_authority == authority.key() || config.admin == authority.key()
            @ VaultError::Unauthorized,
    )]
    pub config: Account<'info, VaultConfig>,
    #[account(mut, seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    /// Read for the live multiplier schedule.
    #[account(address = market.collateral_mint)]
    pub collateral_mint: Box<InterfaceAccount<'info, Mint>>,
}

#[derive(Accounts)]
pub struct UpdateMarket<'info> {
    pub admin: Signer<'info>,
    #[account(seeds = [VCONFIG_SEED], bump = config.bump, has_one = admin @ VaultError::Unauthorized)]
    pub config: Account<'info, VaultConfig>,
    #[account(mut, seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
}

#[derive(Accounts)]
pub struct PushPrice<'info> {
    pub feed_authority: Signer<'info>,
    #[account(
        seeds = [VCONFIG_SEED],
        bump = config.bump,
        constraint = config.feed_authority == feed_authority.key() @ VaultError::Unauthorized,
    )]
    pub config: Account<'info, VaultConfig>,
    #[account(mut, seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
}

#[derive(Accounts)]
pub struct OpenSupplier<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(
        init,
        payer = owner,
        space = 8 + Supplier::INIT_SPACE,
        seeds = [SUPPLIER_SEED, market.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub supplier: Account<'info, Supplier>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct MarkLiquidatable<'info> {
    #[account(seeds = [VCONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, VaultConfig>,
    #[account(mut, seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(
        mut,
        seeds = [POSITION_SEED, market.key().as_ref(), position.owner.as_ref()],
        bump = position.bump,
        has_one = market,
    )]
    pub position: Box<Account<'info, Position>>,
    #[account(address = market.collateral_mint, mint::token_program = collateral_token_program)]
    pub collateral_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(seeds = [COLL_VAULT_SEED, market.key().as_ref()], bump, token::token_program = collateral_token_program)]
    pub collateral_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub collateral_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct SetPayoutAddress<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [VCONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, VaultConfig>,
    #[account(
        mut,
        seeds = [POSITION_SEED, position.market.as_ref(), position.owner.as_ref()],
        bump = position.bump,
        has_one = owner @ VaultError::Unauthorized,
    )]
    pub position: Account<'info, Position>,
}

#[derive(Accounts)]
pub struct OpenPosition<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(
        init,
        payer = owner,
        space = 8 + Position::INIT_SPACE,
        seeds = [POSITION_SEED, market.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub position: Account<'info, Position>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Supply<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [VCONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, VaultConfig>,
    #[account(mut, seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(
        mut,
        seeds = [SUPPLIER_SEED, market.key().as_ref(), owner.key().as_ref()],
        bump = supplier.bump,
        has_one = owner,
        has_one = market,
    )]
    pub supplier: Account<'info, Supplier>,
    #[account(address = market.usdc_mint, mint::token_program = usdc_token_program)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = usdc_mint, token::authority = owner, token::token_program = usdc_token_program)]
    pub owner_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [USDC_VAULT_SEED, market.key().as_ref()], bump, token::token_program = usdc_token_program)]
    pub usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct WithdrawSupply<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [VCONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, VaultConfig>,
    #[account(mut, seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(
        mut,
        seeds = [SUPPLIER_SEED, market.key().as_ref(), owner.key().as_ref()],
        bump = supplier.bump,
        has_one = owner,
        has_one = market,
    )]
    pub supplier: Account<'info, Supplier>,
    #[account(address = market.usdc_mint, mint::token_program = usdc_token_program)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = usdc_mint, token::authority = owner, token::token_program = usdc_token_program)]
    pub owner_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [USDC_VAULT_SEED, market.key().as_ref()], bump, token::token_program = usdc_token_program)]
    pub usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct DepositCollateral<'info> {
    pub owner: Signer<'info>,
    #[account(mut, seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(
        mut,
        seeds = [POSITION_SEED, market.key().as_ref(), owner.key().as_ref()],
        bump = position.bump,
        has_one = owner,
        has_one = market,
    )]
    pub position: Account<'info, Position>,
    #[account(address = market.collateral_mint, mint::token_program = collateral_token_program)]
    pub collateral_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = collateral_mint, token::authority = owner, token::token_program = collateral_token_program)]
    pub owner_collateral: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [COLL_VAULT_SEED, market.key().as_ref()], bump, token::token_program = collateral_token_program)]
    pub collateral_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub collateral_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct WithdrawCollateral<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [VCONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, VaultConfig>,
    #[account(mut, seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(
        mut,
        seeds = [POSITION_SEED, market.key().as_ref(), owner.key().as_ref()],
        bump = position.bump,
        has_one = owner,
        has_one = market,
    )]
    pub position: Account<'info, Position>,
    #[account(address = market.collateral_mint, mint::token_program = collateral_token_program)]
    pub collateral_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = collateral_mint, token::authority = owner, token::token_program = collateral_token_program)]
    pub owner_collateral: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [COLL_VAULT_SEED, market.key().as_ref()], bump, token::token_program = collateral_token_program)]
    pub collateral_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub collateral_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct Borrow<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [VCONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, VaultConfig>,
    #[account(mut, seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(
        mut,
        seeds = [POSITION_SEED, market.key().as_ref(), owner.key().as_ref()],
        bump = position.bump,
        has_one = owner,
        has_one = market,
    )]
    pub position: Account<'info, Position>,
    /// Read for the live multiplier schedule and issuer controls.
    #[account(address = market.collateral_mint)]
    pub collateral_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address = market.usdc_mint, mint::token_program = usdc_token_program)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = usdc_mint, token::authority = owner, token::token_program = usdc_token_program)]
    pub owner_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [USDC_VAULT_SEED, market.key().as_ref()], bump, token::token_program = usdc_token_program)]
    pub usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct AbsorbBadDebtCover<'info> {
    #[account(mut, seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(seeds = [USDC_VAULT_SEED, market.key().as_ref()], bump)]
    pub usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}

#[derive(Accounts)]
pub struct SyncIssuerState<'info> {
    #[account(mut, seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(address = market.collateral_mint)]
    pub collateral_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(seeds = [COLL_VAULT_SEED, market.key().as_ref()], bump)]
    pub collateral_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}

#[derive(Accounts)]
pub struct Liquidate<'info> {
    /// Pays the record's rent. Separate from `liquidator` so the backstop pool, whose key is a program-owned
    /// PDA that cannot fund an account, can liquidate with a crank wallet paying the rent (eng review A1).
    #[account(mut)]
    pub payer: Signer<'info>,
    /// Authorizes the USDC in and receives the collateral out.
    pub liquidator: Signer<'info>,
    #[account(seeds = [VCONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, VaultConfig>,
    #[account(mut, seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(
        mut,
        seeds = [POSITION_SEED, market.key().as_ref(), position.owner.as_ref()],
        bump = position.bump,
        has_one = market,
    )]
    pub position: Box<Account<'info, Position>>,
    /// Seeded by the market's own counter, so each liquidation gets its own immutable record and a
    /// replayed transaction cannot overwrite an earlier one.
    #[account(
        init,
        payer = payer,
        space = 8 + LiquidationRecord::INIT_SPACE,
        seeds = [LIQ_RECORD_SEED, market.key().as_ref(), market.liq_seq.to_le_bytes().as_ref()],
        bump,
    )]
    pub record: Box<Account<'info, LiquidationRecord>>,
    #[account(address = market.collateral_mint, mint::token_program = collateral_token_program)]
    pub collateral_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address = market.usdc_mint, mint::token_program = usdc_token_program)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = usdc_mint, token::authority = liquidator, token::token_program = usdc_token_program)]
    pub liquidator_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = collateral_mint, token::authority = liquidator, token::token_program = collateral_token_program)]
    pub liquidator_collateral: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [COLL_VAULT_SEED, market.key().as_ref()], bump, token::token_program = collateral_token_program)]
    pub collateral_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [USDC_VAULT_SEED, market.key().as_ref()], bump, token::token_program = usdc_token_program)]
    pub usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub collateral_token_program: Interface<'info, TokenInterface>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Repay<'info> {
    pub payer: Signer<'info>,
    #[account(mut, seeds = [MARKET_SEED, market.collateral_mint.as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,
    #[account(
        mut,
        seeds = [POSITION_SEED, market.key().as_ref(), position.owner.as_ref()],
        bump = position.bump,
        has_one = market,
    )]
    pub position: Account<'info, Position>,
    #[account(address = market.usdc_mint, mint::token_program = usdc_token_program)]
    pub usdc_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = usdc_mint, token::authority = payer, token::token_program = usdc_token_program)]
    pub payer_usdc: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [USDC_VAULT_SEED, market.key().as_ref()], bump, token::token_program = usdc_token_program)]
    pub usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
}

// ------------------------------------------------------------------ events

#[event]
pub struct MarketCreated {
    pub market: Pubkey,
    pub collateral_mint: Pubkey,
    pub usdc_mint: Pubkey,
}

/// A parameter change tightened the liquidation terms. Existing loans reach `to` gradually, completing
/// at `completes` (U3). Announced on-chain so borrowers can add collateral or repay in time.
#[event]
pub struct LiquidationTermsTightening {
    pub market: Pubkey,
    pub from: LiquidationTerms,
    pub to: LiquidationTerms,
    pub starts: i64,
    pub completes: i64,
}

#[event]
pub struct LiquidatableMarked {
    pub position: Pubkey,
    pub first_seen: i64,
    pub last_seen: i64,
}

#[event]
pub struct PayoutAddressSet {
    pub position: Pubkey,
    pub payout: Pubkey,
}

#[event]
pub struct PausedSet {
    pub paused: bool,
}

#[event]
pub struct MultiplierAcknowledged {
    pub market: Pubkey,
    pub previous: u128,
    pub effective: u128,
}

#[event]
pub struct PriceUpdated {
    pub market: Pubkey,
    pub price: u64,
    pub market_open: bool,
    pub accepted: bool,
}

#[event]
pub struct Supplied {
    pub owner: Pubkey,
    pub amount: u64,
    pub shares: u128,
}

#[event]
pub struct SupplyWithdrawn {
    pub owner: Pubkey,
    pub amount: u64,
    pub shares: u128,
}

#[event]
pub struct CollateralDeposited {
    pub owner: Pubkey,
    pub amount: u64,
}

#[event]
pub struct CollateralWithdrawn {
    pub owner: Pubkey,
    pub amount: u64,
}

#[event]
pub struct Borrowed {
    pub owner: Pubkey,
    pub amount: u64,
    pub shares: u128,
}

#[event]
pub struct BadDebtCovered {
    pub market: Pubkey,
    pub absorbed: u64,
    pub bad_debt_remaining: u64,
}

#[event]
pub struct IssuerHaltSet {
    pub market: Pubkey,
    pub halted: bool,
    pub mint_blocked: bool,
    pub vault_frozen: bool,
    pub reconciliation_short: bool,
}

/// Everything the off-chain verdict engine needs to start from, without reading the record account.
#[event]
pub struct Liquidated {
    pub market: Pubkey,
    pub seq: u64,
    pub borrower: Pubkey,
    pub liquidator: Pubkey,
    pub seized_raw: u64,
    pub debt_repaid: u64,
    pub bonus_bps: u32,
    pub ltv_bps: u32,
    pub price_fp: u64,
    pub bad_debt: u64,
}

#[event]
pub struct Repaid {
    pub owner: Pubkey,
    pub amount: u64,
    pub shares: u128,
}

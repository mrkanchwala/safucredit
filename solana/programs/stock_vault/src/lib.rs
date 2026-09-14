use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use safu_core::lending::{
    debt_for_shares, max_borrow, shares_for_borrow, shares_for_repay, INDEX_SCALE,
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

    /// NOTE: the first caller becomes admin. Gating this on the program's upgrade authority lands with
    /// upgradeability (stage 2 item 5); until then deploy and initialize in the same session.
    pub fn initialize_config(ctx: Context<InitializeConfig>, feed_authority: Pubkey) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.version = ACCOUNT_VERSION;
        config.admin = ctx.accounts.admin.key();
        config.feed_authority = feed_authority;
        config.paused = false;
        config.bump = ctx.bumps.config;
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
        market.issuer_halt = false;
        market.liq_seq = 0;
        market.observed_multiplier_fp = schedule.effective(now);
        market.reserved = [0; 48];

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
        market.params = params;
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
}

// ------------------------------------------------------------------ accounts

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
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
pub struct Repaid {
    pub owner: Pubkey,
    pub amount: u64,
    pub shares: u128,
}

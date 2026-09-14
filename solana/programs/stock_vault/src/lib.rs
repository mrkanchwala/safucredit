use anchor_lang::prelude::*;

declare_id!("GkQw6VGDKYBWJtgtWUmFDkHNqGrnyjSviQeQzVMW35K2");

#[program]
pub mod stock_vault {
    use super::*;

    pub fn ping(_ctx: Context<Ping>) -> Result<()> {
        Ok(())
    }
}

#[derive(Accounts)]
pub struct Ping {}

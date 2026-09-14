use anchor_lang::prelude::*;

declare_id!("H1hApKnNkYPsqQ9WVZGzqZQZYirxCLpXDj2uEmzMK3YV");

#[program]
pub mod backstop {
    use super::*;

    pub fn ping(_ctx: Context<Ping>) -> Result<()> {
        Ok(())
    }
}

#[derive(Accounts)]
pub struct Ping {}

use anchor_lang::prelude::*;

declare_id!("F9Nkem7AXzDMy3Tq5FVs6zjzvRu4vP1ctg6EUZTJaKTM");

#[program]
pub mod sol_goal_tmp {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        msg!("Greetings from: {:?}", ctx.program_id);
        Ok(())
    }
}

#[derive(Accounts)]
pub struct Initialize {}

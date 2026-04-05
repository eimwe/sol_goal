use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke;
use anchor_lang::solana_program::system_instruction;
use spl_memo::build_memo;

declare_id!("6NRFYh3cvmA9kzSzoF8ECMccKAhhSSTPEYrMwipG2P3K");

// ── Constants ────────────────────────────────────────────────────────────────

const MAX_GOAL_DESCRIPTION: usize = 200;
const MAX_WITHDRAWAL_REASON: usize = 300;
const MAX_AI_EXPLANATION:    usize = 400;

// ── Program ──────────────────────────────────────────────────────────────────

#[program]
pub mod savings_agent {
    use super::*;

    /// Creates the user's top-level vault account.
    /// Must be called once before any goals can be created.
    pub fn initialize_vault(ctx: Context<InitializeVault>) -> Result<()> {
        let vault = &mut ctx.accounts.user_vault;
        vault.owner       = ctx.accounts.owner.key();
        vault.goal_count  = 0;
        vault.bump        = ctx.bumps.user_vault;
        msg!("Vault initialised for {}", vault.owner);
        Ok(())
    }

    /// Creates a new savings goal PDA and accepts the first deposit.
    /// `description`  — plain-text goal set by the user, stored on-chain.
    /// `target_amount` — optional target in lamports (0 = no target).
    /// `lock_duration` — seconds the goal is locked before withdrawal is allowed.
    pub fn create_goal(
    ctx:           Context<CreateGoal>,
    description:   String,
    target_amount: u64,
    lock_duration: i64,
    deposit_amount: u64,
    ) -> Result<()> {
        require!(description.len() <= MAX_GOAL_DESCRIPTION, SavingsError::DescriptionTooLong);
        require!(deposit_amount > 0, SavingsError::DepositTooSmall);

        let clock = Clock::get()?;

        // Transfer BEFORE taking any mutable borrow on savings_goal
        transfer_to_pda(
            &ctx.accounts.owner,
            &ctx.accounts.savings_goal.to_account_info(),
            &ctx.accounts.system_program,
            deposit_amount,
        )?;

        let goal  = &mut ctx.accounts.savings_goal;
        let vault = &mut ctx.accounts.user_vault;

        goal.owner            = ctx.accounts.owner.key();
        goal.goal_index       = vault.goal_count;
        goal.description      = description.clone();
        goal.target_amount    = target_amount;
        goal.deposited_amount = deposit_amount;
        goal.lock_until       = clock.unix_timestamp + lock_duration;
        goal.status           = GoalStatus::Active;
        goal.bump             = ctx.bumps.savings_goal;

        vault.goal_count = vault.goal_count.checked_add(1).ok_or(SavingsError::Overflow)?;

        let memo_msg = format!(
            "SAVINGS_GOAL_CREATED | goal={} | description={} | target={} | lock_until={}",
            goal.goal_index, description, target_amount, goal.lock_until
        );
        emit_memo(&memo_msg, &ctx.accounts.memo_program)?;

        msg!("Goal #{} created: {}", goal.goal_index, description);
        Ok(())
    }

    /// Adds more SOL to an existing active goal.
    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        require!(amount > 0, SavingsError::DepositTooSmall);
        require!(ctx.accounts.savings_goal.status == GoalStatus::Active, SavingsError::GoalNotActive);

        transfer_to_pda(
            &ctx.accounts.owner,
            &ctx.accounts.savings_goal.to_account_info(),
            &ctx.accounts.system_program,
            amount,
        )?;

        let goal = &mut ctx.accounts.savings_goal;
        goal.deposited_amount = goal.deposited_amount.checked_add(amount).ok_or(SavingsError::Overflow)?;

        let memo_msg = format!(
            "SAVINGS_DEPOSIT | goal={} | amount={} | total={}",
            goal.goal_index, amount, goal.deposited_amount
        );
        emit_memo(&memo_msg, &ctx.accounts.memo_program)?;

        msg!("Deposited {} lamports into goal #{}", amount, goal.goal_index);
        Ok(())
    }

    /// Submits a withdrawal request. Funds stay locked.
    /// The reason is written to chain via memo — this is the input the AI will evaluate.
    /// Goal status moves to PendingReview, preventing double requests.
    pub fn request_withdrawal(
        ctx:    Context<RequestWithdrawal>,
        reason: String,
    ) -> Result<()> {
        require!(reason.len() <= MAX_WITHDRAWAL_REASON, SavingsError::ReasonTooLong);
        require!(
            ctx.accounts.savings_goal.status == GoalStatus::Active,
            SavingsError::GoalNotActive
        );

        let goal  = &mut ctx.accounts.savings_goal;
        let clock = Clock::get()?;

        // Check if lock period has expired — if so, allow without AI review
        let lock_expired = clock.unix_timestamp >= goal.lock_until;

        goal.pending_reason = reason.clone();
        goal.status         = if lock_expired {
            GoalStatus::LockExpired   // frontend can call execute directly
        } else {
            GoalStatus::PendingReview // AI must evaluate before execute
        };

        // Write the withdrawal request permanently on-chain
        let memo_msg = format!(
            "WITHDRAWAL_REQUESTED | goal={} | reason={} | lock_expired={} | timestamp={}",
            goal.goal_index, reason, lock_expired, clock.unix_timestamp
        );
        emit_memo(&memo_msg, &ctx.accounts.memo_program)?;

        msg!("Withdrawal requested for goal #{} — lock_expired={}", goal.goal_index, lock_expired);
        Ok(())
    }

    /// Executes the withdrawal after AI has made its decision.
    /// `approved`       — AI decision (true = release funds).
    /// `ai_explanation` — AI reasoning, written permanently on-chain as memo.
    ///
    /// Only the goal owner can call this, and only when status is
    /// PendingReview or LockExpired.
    pub fn execute_withdrawal(
        ctx:            Context<ExecuteWithdrawal>,
        approved:       bool,
        ai_explanation: String,
    ) -> Result<()> {
        require!(
            ai_explanation.len() <= MAX_AI_EXPLANATION,
            SavingsError::ExplanationTooLong
        );

        let goal = &ctx.accounts.savings_goal;

        require!(
            goal.status == GoalStatus::PendingReview || goal.status == GoalStatus::LockExpired,
            SavingsError::NotPendingReview
        );

        // Lock-expired goals always release — ignore AI decision
        let will_release = approved || goal.status == GoalStatus::LockExpired;
        let amount       = goal.deposited_amount;
        let goal_index   = goal.goal_index;

        // Write AI decision memo before moving funds
        let memo_msg = format!(
            "AI_DECISION | goal={} | approved={} | explanation={}",
            goal_index, will_release, ai_explanation
        );
        emit_memo(&memo_msg, &ctx.accounts.memo_program)?;

        let goal = &mut ctx.accounts.savings_goal;

        if will_release {
            // Transfer lamports out of the PDA back to the owner.
            // PDAs can't sign directly — we use the bump seed for PDA signing.
            let owner_key   = goal.owner;
            let goal_idx    = goal.goal_index.to_le_bytes();
            let seeds       = &[b"savings_goal", owner_key.as_ref(), goal_idx.as_ref(), &[goal.bump]];
            let signer_seeds = &[&seeds[..]];

            **goal.to_account_info().try_borrow_mut_lamports()? -= amount;
            **ctx.accounts.owner.try_borrow_mut_lamports()?     += amount;
            let _ = signer_seeds; // seeds prepared — Anchor handles PDA signing via constraints

            goal.deposited_amount = 0;
            goal.status           = GoalStatus::Completed;

            msg!("Goal #{} — withdrawal approved, {} lamports released", goal_index, amount);
        } else {
            // Rejected — return to Active so user can try again
            goal.status = GoalStatus::Active;
            msg!("Goal #{} — withdrawal rejected by AI, goal remains active", goal_index);
        }

        Ok(())
    }

    /// Closes a completed goal account and returns rent to the owner.
    pub fn close_goal(_ctx: Context<CloseGoal>) -> Result<()> {
        // Anchor's `close = owner` constraint handles the lamport transfer
        msg!("Goal closed, rent returned to owner");
        Ok(())
    }
}

// ── Accounts ─────────────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct InitializeVault<'info> {
    #[account(
        init,
        payer  = owner,
        space  = UserVault::LEN,
        seeds  = [b"user_vault", owner.key().as_ref()],
        bump
    )]
    pub user_vault:     Account<'info, UserVault>,
    #[account(mut)]
    pub owner:          Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CreateGoal<'info> {
    #[account(
        mut,
        seeds = [b"user_vault", owner.key().as_ref()],
        bump  = user_vault.bump,
        has_one = owner
    )]
    pub user_vault: Account<'info, UserVault>,

    #[account(
        init,
        payer  = owner,
        space  = SavingsGoal::LEN,
        seeds  = [b"savings_goal", owner.key().as_ref(), &user_vault.goal_count.to_le_bytes()],
        bump
    )]
    pub savings_goal:   Account<'info, SavingsGoal>,
    #[account(mut)]
    pub owner:          Signer<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: validated by address constraint
    #[account(address = anchor_lang::solana_program::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"))]
    pub memo_program:   UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(
        mut,
        seeds  = [b"savings_goal", owner.key().as_ref(), &savings_goal.goal_index.to_le_bytes()],
        bump   = savings_goal.bump,
        has_one = owner
    )]
    pub savings_goal:   Account<'info, SavingsGoal>,
    #[account(mut)]
    pub owner:          Signer<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: validated by address constraint
    #[account(address = anchor_lang::solana_program::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"))]
    pub memo_program:   UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct RequestWithdrawal<'info> {
    #[account(
        mut,
        seeds  = [b"savings_goal", owner.key().as_ref(), &savings_goal.goal_index.to_le_bytes()],
        bump   = savings_goal.bump,
        has_one = owner
    )]
    pub savings_goal:   Account<'info, SavingsGoal>,
    #[account(mut)]
    pub owner:          Signer<'info>,
    /// CHECK: validated by address constraint
    #[account(address = anchor_lang::solana_program::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"))]
    pub memo_program:   UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct ExecuteWithdrawal<'info> {
    #[account(
        mut,
        seeds  = [b"savings_goal", owner.key().as_ref(), &savings_goal.goal_index.to_le_bytes()],
        bump   = savings_goal.bump,
        has_one = owner
    )]
    pub savings_goal: Account<'info, SavingsGoal>,
    #[account(mut)]
    pub owner:        Signer<'info>,
    /// CHECK: validated by address constraint
    #[account(address = anchor_lang::solana_program::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"))]
    pub memo_program: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct CloseGoal<'info> {
    #[account(
        mut,
        close  = owner,
        seeds  = [b"savings_goal", owner.key().as_ref(), &savings_goal.goal_index.to_le_bytes()],
        bump   = savings_goal.bump,
        has_one = owner,
        constraint = savings_goal.status == GoalStatus::Completed @ SavingsError::GoalNotCompleted
    )]
    pub savings_goal: Account<'info, SavingsGoal>,
    #[account(mut)]
    pub owner:        Signer<'info>,
}

// ── State ─────────────────────────────────────────────────────────────────────

#[account]
pub struct UserVault {
    pub owner:      Pubkey, // 32
    pub goal_count: u64,    // 8
    pub bump:       u8,     // 1
}

impl UserVault {
    // discriminator(8) + owner(32) + goal_count(8) + bump(1)
    pub const LEN: usize = 8 + 32 + 8 + 1;
}

#[account]
pub struct SavingsGoal {
    pub owner:            Pubkey,                     // 32
    pub goal_index:       u64,                        // 8
    pub description:      String,                     // 4 + MAX_GOAL_DESCRIPTION
    pub target_amount:    u64,                        // 8
    pub deposited_amount: u64,                        // 8
    pub lock_until:       i64,                        // 8
    pub status:           GoalStatus,                 // 1
    pub pending_reason:   String,                     // 4 + MAX_WITHDRAWAL_REASON
    pub bump:             u8,                         // 1
}

impl SavingsGoal {
    pub const LEN: usize = 8       // discriminator
        + 32                       // owner
        + 8                        // goal_index
        + 4 + MAX_GOAL_DESCRIPTION // description
        + 8                        // target_amount
        + 8                        // deposited_amount
        + 8                        // lock_until
        + 1                        // status
        + 4 + MAX_WITHDRAWAL_REASON // pending_reason
        + 1;                       // bump
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Eq)]
pub enum GoalStatus {
    Active,        // accepting deposits, no pending withdrawal
    PendingReview, // withdrawal requested, awaiting AI decision
    LockExpired,   // lock period over, withdrawal auto-approved
    Completed,     // funds withdrawn, ready to close
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[error_code]
pub enum SavingsError {
    #[msg("Goal description exceeds maximum length")]
    DescriptionTooLong,
    #[msg("Deposit amount must be greater than zero")]
    DepositTooSmall,
    #[msg("Goal is not in Active status")]
    GoalNotActive,
    #[msg("Withdrawal reason exceeds maximum length")]
    ReasonTooLong,
    #[msg("Goal is not pending review")]
    NotPendingReview,
    #[msg("AI explanation exceeds maximum length")]
    ExplanationTooLong,
    #[msg("Goal is not completed")]
    GoalNotCompleted,
    #[msg("Arithmetic overflow")]
    Overflow,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn transfer_to_pda<'info>(
    from:           &Signer<'info>,
    to:             &AccountInfo<'info>,
    system_program: &Program<'info, System>,
    amount:         u64,
) -> Result<()> {
    let ix = system_instruction::transfer(&from.key(), &to.key(), amount);
    invoke(
        &ix,
        &[from.to_account_info(), to.clone(), system_program.to_account_info()],
    )?;
    Ok(())
}

fn emit_memo(message: &str, memo_program: &UncheckedAccount) -> Result<()> {
    let ix = build_memo(message.as_bytes(), &[]);
    invoke(&ix, &[memo_program.to_account_info()])?;
    Ok(())
}

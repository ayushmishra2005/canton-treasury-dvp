use anchor_lang::prelude::*;

pub mod canonical;
pub mod confidential;
pub mod ed25519;
pub mod errors;
pub mod handlers;
pub mod state;

use handlers::*;

#[cfg(feature = "devnet-id")]
declare_id!("BkDwMbtMVhDWeQ1nHwvCKmTT2XZhP2RMYGw18c6imnPf");
#[cfg(not(feature = "devnet-id"))]
declare_id!("9Yuvt4HxfbGCL9gPk3ygMLV3UdrMFgAJsyhdoJvbKcUD");

#[program]
pub mod confidential_escrow {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>, args: InitializeArgs) -> Result<()> {
        handlers::initialize(ctx, args)
    }

    pub fn lock_confidential(ctx: Context<LockConfidential>, args: LockArgs) -> Result<()> {
        handlers::lock_confidential(ctx, args)
    }

    pub fn approve_operation(ctx: Context<ApproveOperation>, args: ApproveArgs) -> Result<()> {
        handlers::approve_operation(ctx, args)
    }

    pub fn cancel_confidential(ctx: Context<MoveConfidential>, args: MovementArgs) -> Result<()> {
        handlers::cancel_confidential(ctx, args)
    }

    pub fn release_confidential(ctx: Context<MoveConfidential>, args: MovementArgs) -> Result<()> {
        handlers::release_confidential(ctx, args)
    }
}

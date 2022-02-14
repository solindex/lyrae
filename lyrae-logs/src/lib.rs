use anchor_lang::prelude::*;
declare_id!("Fg6PaFpoGXkYsidMpWTK6W2BeZ7FEfcYkg476zPFsLnS");

// Log to Program Log with a prologue so transaction scraper knows following line is valid lyrae log
#[macro_export]
macro_rules! lyrae_emit {
    ($e:expr) => {
        msg!("lyrae-log");
        emit!($e);
    };
}

// This is a dummy program to take advantage of Anchor events
#[program]
pub mod lyrae_logs {}

#[event]
pub struct FillLog {
    pub lyrae_group: Pubkey,
    pub market_index: u64,
    pub taker_side: u8, // side from the taker's POV
    pub maker_slot: u8,
    pub maker_out: bool, // true if maker order quantity == 0
    pub timestamp: u64,
    pub seq_num: u64, // note: usize same as u64

    pub maker: Pubkey,
    pub maker_order_id: i128,
    pub maker_client_order_id: u64,
    pub maker_fee: i128,

    // The best bid/ask at the time the maker order was placed. Used for liquidity incentives
    pub best_initial: i64,

    // Timestamp of when the maker order was placed; copied over from the LeafNode
    pub maker_timestamp: u64,

    pub taker: Pubkey,
    pub taker_order_id: i128,
    pub taker_client_order_id: u64,
    pub taker_fee: i128,

    pub price: i64,
    pub quantity: i64, // number of base lots
}

#[event]
pub struct TokenBalanceLog {
    pub lyrae_group: Pubkey,
    pub lyrae_account: Pubkey,
    pub token_index: u64, // IDL doesn't support usize
    pub deposit: i128, // on client convert i128 to I80F48 easily by passing in the BN to I80F48 ctor
    pub borrow: i128,
}

#[event]
pub struct CachePricesLog {
    pub lyrae_group: Pubkey,
    pub oracle_indexes: Vec<u64>,
    pub oracle_prices: Vec<i128>, // I80F48 format
}
#[event]
pub struct CacheRootBanksLog {
    pub lyrae_group: Pubkey,
    pub token_indexes: Vec<u64>,    // usize
    pub deposit_indexes: Vec<i128>, // I80F48
    pub borrow_indexes: Vec<i128>,  // I80F48
}

#[event]
pub struct CachePerpMarketsLog {
    pub lyrae_group: Pubkey,
    pub market_indexes: Vec<u64>,
    pub long_fundings: Vec<i128>,  // I80F48
    pub short_fundings: Vec<i128>, // I80F48
}

#[event]
pub struct SettlePnlLog {
    pub lyrae_group: Pubkey,
    pub lyrae_account_a: Pubkey,
    pub lyrae_account_b: Pubkey,
    pub market_index: u64,
    pub settlement: i128, // I80F48
}

#[event]
pub struct SettleFeesLog {
    pub lyrae_group: Pubkey,
    pub lyrae_account: Pubkey,
    pub market_index: u64,
    pub settlement: i128, // I80F48
}

#[event]
pub struct LiquidateTokenAndTokenLog {
    pub lyrae_group: Pubkey,
    pub liqee: Pubkey,
    pub liqor: Pubkey,
    pub asset_index: u64,
    pub liab_index: u64,
    pub asset_transfer: i128, // I80F48
    pub liab_transfer: i128,  // I80F48
    pub asset_price: i128,    // I80F48
    pub liab_price: i128,     // I80F48
    pub bankruptcy: bool,
}

#[event]
pub struct LiquidateTokenAndPerpLog {
    pub lyrae_group: Pubkey,
    pub liqee: Pubkey,
    pub liqor: Pubkey,
    pub asset_index: u64,
    pub liab_index: u64,
    pub asset_type: u8,
    pub liab_type: u8,
    pub asset_price: i128,    // I80F48
    pub liab_price: i128,     // I80F48
    pub asset_transfer: i128, // I80F48
    pub liab_transfer: i128,  // I80F48
    pub bankruptcy: bool,
}

#[event]
pub struct LiquidatePerpMarketLog {
    pub lyrae_group: Pubkey,
    pub liqee: Pubkey,
    pub liqor: Pubkey,
    pub market_index: u64,
    pub price: i128, // I80F48
    pub base_transfer: i64,
    pub quote_transfer: i128, // I80F48
    pub bankruptcy: bool,
}

#[event]
pub struct PerpBankruptcyLog {
    pub lyrae_group: Pubkey,
    pub liqee: Pubkey,
    pub liqor: Pubkey,
    pub liab_index: u64,
    pub insurance_transfer: u64,
    pub socialized_loss: i128,     // I80F48
    pub cache_long_funding: i128,  // I80F48
    pub cache_short_funding: i128, // I80F48
}

#[event]
pub struct TokenBankruptcyLog {
    pub lyrae_group: Pubkey,
    pub liqee: Pubkey,
    pub liqor: Pubkey,
    pub liab_index: u64,
    pub insurance_transfer: u64,
    /// This is in native units for the liab token NOT static units
    pub socialized_loss: i128, // I80F48
    pub percentage_loss: i128,     // I80F48
    pub cache_deposit_index: i128, // I80F48
}

#[event]
pub struct UpdateRootBankLog {
    pub lyrae_group: Pubkey,
    pub token_index: u64,
    pub deposit_index: i128, // I80F48
    pub borrow_index: i128,  // I80F48
}

#[event]
pub struct UpdateFundingLog {
    pub lyrae_group: Pubkey,
    pub market_index: u64,
    pub long_funding: i128,  // I80F48
    pub short_funding: i128, // I80F48
}

#[event]
pub struct OpenOrdersBalanceLog {
    pub lyrae_group: Pubkey,
    pub lyrae_account: Pubkey,
    pub market_index: u64,
    pub base_total: u64,
    pub base_free: u64,
    /// this field does not include the referrer_rebates; need to add that in to get true total
    pub quote_total: u64,
    pub quote_free: u64,
    pub referrer_rebates_accrued: u64,
}

#[event]
pub struct LyrAccrualLog {
    pub lyrae_group: Pubkey,
    pub lyrae_account: Pubkey,
    pub market_index: u64,
    /// incremental lyr accrual from canceling/filling this order or set of orders
    pub lyr_accrual: u64,
}

#[event]
pub struct WithdrawLog {
    pub lyrae_group: Pubkey,
    pub lyrae_account: Pubkey,
    pub owner: Pubkey,
    pub token_index: u64,
    pub quantity: u64,
}

#[event]
pub struct DepositLog {
    pub lyrae_group: Pubkey,
    pub lyrae_account: Pubkey,
    pub owner: Pubkey,
    pub token_index: u64,
    pub quantity: u64,
}

#[event]
pub struct RedeemLyrLog {
    pub lyrae_group: Pubkey,
    pub lyrae_account: Pubkey,
    pub market_index: u64,
    pub redeemed_lyr: u64,
}

#[event]
pub struct CancelAllPerpOrdersLog {
    pub lyrae_group: Pubkey,
    pub lyrae_account: Pubkey,
    pub market_index: u64,
    pub all_order_ids: Vec<i128>,
    pub canceled_order_ids: Vec<i128>,
}

#[event]
pub struct PerpBalanceLog {
    pub lyrae_group: Pubkey,
    pub lyrae_account: Pubkey,
    pub market_index: u64, // IDL doesn't support usize
    pub base_position: i64,
    pub quote_position: i128,        // I80F48
    pub long_settled_funding: i128,  // I80F48
    pub short_settled_funding: i128, // I80F48

    pub long_funding: i128,  // I80F48
    pub short_funding: i128, // I80F48
}

#[event]
pub struct ReferralFeeAccrualLog {
    pub lyrae_group: Pubkey,
    pub referrer_lyrae_account: Pubkey,
    pub referree_lyrae_account: Pubkey,
    pub market_index: u64,
    pub referral_fee_accrual: i128, // I80F48
}

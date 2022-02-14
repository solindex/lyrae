use crate::matching::{OrderType, Side};
use crate::state::{AssetType, INFO_LEN};
use crate::state::{TriggerCondition, MAX_PAIRS};
use arrayref::{array_ref, array_refs};
use fixed::types::I80F48;
use num_enum::TryFromPrimitive;
use serde::{Deserialize, Serialize};
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::program_error::ProgramError;
use solana_program::pubkey::Pubkey;
use std::convert::{TryFrom, TryInto};
use std::num::NonZeroU64;

#[repr(C)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LyraeInstruction {
    /// Initialize a group of lending pools that can be cross margined
    ///
    /// Accounts expected by this instruction (12):
    ///
    /// 0. `[writable]` lyrae_group_ai
    /// 1. `[]` signer_ai
    /// 2. `[]` admin_ai
    /// 3. `[]` quote_mint_ai
    /// 4. `[]` quote_vault_ai
    /// 5. `[writable]` quote_node_bank_ai
    /// 6. `[writable]` quote_root_bank_ai
    /// 7. `[]` dao_vault_ai - aka insurance fund
    /// 8. `[]` msrm_vault_ai - msrm deposits for fee discounts; can be Pubkey::default()
    /// 9. `[]` fees_vault_ai - vault owned by Lyrae DAO token governance to receive fees
    /// 10. `[writable]` lyrae_cache_ai - Account to cache prices, root banks, and perp markets
    /// 11. `[]` dex_prog_ai
    InitLyraeGroup {
        signer_nonce: u64,
        valid_interval: u64,
        quote_optimal_util: I80F48,
        quote_optimal_rate: I80F48,
        quote_max_rate: I80F48,
    },

    /// DEPRECATED Initialize a lyrae account for a user
    /// Accounts created with this function cannot be closed without upgrading with UpgradeLyraeAccountV0V1
    ///
    /// Accounts expected by this instruction (3):
    ///
    /// 0. `[]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[writable]` lyrae_account_ai - the lyrae account data
    /// 2. `[signer]` owner_ai - Solana account of owner of the lyrae account
    InitLyraeAccount,

    /// Deposit funds into lyrae account
    ///
    /// Accounts expected by this instruction (9):
    ///
    /// 0. `[]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[writable]` lyrae_account_ai - the lyrae account for this user
    /// 2. `[signer]` owner_ai - Solana account of owner of the lyrae account
    /// 3. `[]` lyrae_cache_ai - LyraeCache
    /// 4. `[]` root_bank_ai - RootBank owned by LyraeGroup
    /// 5. `[writable]` node_bank_ai - NodeBank owned by RootBank
    /// 6. `[writable]` vault_ai - TokenAccount owned by LyraeGroup
    /// 7. `[]` token_prog_ai - acc pointed to by SPL token program id
    /// 8. `[writable]` owner_token_account_ai - TokenAccount owned by user which will be sending the funds
    Deposit {
        quantity: u64,
    },

    /// Withdraw funds that were deposited earlier.
    ///
    /// Accounts expected by this instruction (10):
    ///
    /// 0. `[read]` lyrae_group_ai,   -
    /// 1. `[write]` lyrae_account_ai, -
    /// 2. `[read]` owner_ai,         -
    /// 3. `[read]` lyrae_cache_ai,   -
    /// 4. `[read]` root_bank_ai,     -
    /// 5. `[write]` node_bank_ai,     -
    /// 6. `[write]` vault_ai,         -
    /// 7. `[write]` token_account_ai, -
    /// 8. `[read]` signer_ai,        -
    /// 9. `[read]` token_prog_ai,    -
    /// 10..+ `[]` open_orders_accs - open orders for each of the spot market
    Withdraw {
        quantity: u64,
        allow_borrow: bool,
    },

    /// Add a token to a lyrae group
    ///
    /// Accounts expected by this instruction (8):
    ///
    /// 0. `[writable]` lyrae_group_ai
    /// 1  `[]` oracle_ai
    /// 2. `[]` spot_market_ai
    /// 3. `[]` dex_program_ai
    /// 4. `[]` mint_ai
    /// 5. `[writable]` node_bank_ai
    /// 6. `[]` vault_ai
    /// 7. `[writable]` root_bank_ai
    /// 8. `[signer]` admin_ai
    AddSpotMarket {
        maint_leverage: I80F48,
        init_leverage: I80F48,
        liquidation_fee: I80F48,
        optimal_util: I80F48,
        optimal_rate: I80F48,
        max_rate: I80F48,
    },

    /// DEPRECATED
    AddToBasket {
        market_index: usize,
    },

    /// DEPRECATED - use Withdraw with allow_borrow = true
    Borrow {
        quantity: u64,
    },

    /// Cache prices
    ///
    /// Accounts expected: 3 + Oracles
    /// 0. `[]` lyrae_group_ai -
    /// 1. `[writable]` lyrae_cache_ai -
    /// 2+... `[]` oracle_ais - flux aggregator feed accounts
    CachePrices,

    /// DEPRECATED - caching of root banks now happens in update index
    /// Cache root banks
    ///
    /// Accounts expected: 2 + Root Banks
    /// 0. `[]` lyrae_group_ai
    /// 1. `[writable]` lyrae_cache_ai
    CacheRootBanks,

    /// Place an order on the Serum Dex using Lyrae account
    ///
    /// Accounts expected by this instruction (23 + MAX_PAIRS):
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[writable]` lyrae_account_ai - the LyraeAccount of owner
    /// 2. `[signer]` owner_ai - owner of LyraeAccount
    /// 3. `[]` lyrae_cache_ai - LyraeCache for this LyraeGroup
    /// 4. `[]` dex_prog_ai - serum dex program id
    /// 5. `[writable]` spot_market_ai - serum dex MarketState account
    /// 6. `[writable]` bids_ai - bids account for serum dex market
    /// 7. `[writable]` asks_ai - asks account for serum dex market
    /// 8. `[writable]` dex_request_queue_ai - request queue for serum dex market
    /// 9. `[writable]` dex_event_queue_ai - event queue for serum dex market
    /// 10. `[writable]` dex_base_ai - base currency serum dex market vault
    /// 11. `[writable]` dex_quote_ai - quote currency serum dex market vault
    /// 12. `[]` base_root_bank_ai - root bank of base currency
    /// 13. `[writable]` base_node_bank_ai - node bank of base currency
    /// 14. `[writable]` base_vault_ai - vault of the basenode bank
    /// 15. `[]` quote_root_bank_ai - root bank of quote currency
    /// 16. `[writable]` quote_node_bank_ai - node bank of quote currency
    /// 17. `[writable]` quote_vault_ai - vault of the quote node bank
    /// 18. `[]` token_prog_ai - SPL token program id
    /// 19. `[]` signer_ai - signer key for this LyraeGroup
    /// 20. `[]` rent_ai - rent sysvar var
    /// 21. `[]` dex_signer_key - signer for serum dex
    /// 22. `[]` msrm_or_srm_vault_ai - the msrm or srm vault in this LyraeGroup. Can be zero key
    /// 23+ `[writable]` open_orders_ais - An array of MAX_PAIRS. Only OpenOrders of current market
    ///         index needs to be writable. Only OpenOrders in_margin_basket needs to be correct;
    ///         remaining open orders can just be Pubkey::default() (the zero key)
    PlaceSpotOrder {
        order: serum_dex::instruction::NewOrderInstructionV3,
    },

    /// Add oracle
    ///
    /// Accounts expected: 3
    /// 0. `[writable]` lyrae_group_ai - LyraeGroup
    /// 1. `[writable]` oracle_ai - oracle
    /// 2. `[signer]` admin_ai - admin
    AddOracle, // = 10

    /// Add a perp market to a lyrae group
    ///
    /// Accounts expected by this instruction (7):
    ///
    /// 0. `[writable]` lyrae_group_ai
    /// 1. `[]` oracle_ai
    /// 2. `[writable]` perp_market_ai
    /// 3. `[writable]` event_queue_ai
    /// 4. `[writable]` bids_ai
    /// 5. `[writable]` asks_ai
    /// 6. `[]` Lyr_vault_ai - the vault from which liquidity incentives will be paid out for this market
    /// 7. `[signer]` admin_ai
    AddPerpMarket {
        maint_leverage: I80F48,
        init_leverage: I80F48,
        liquidation_fee: I80F48,
        maker_fee: I80F48,
        taker_fee: I80F48,
        base_lot_size: i64,
        quote_lot_size: i64,
        /// Starting rate for liquidity mining
        rate: I80F48,
        /// depth liquidity mining works for
        max_depth_bps: I80F48,
        /// target length in seconds of one period
        target_period_length: u64,
        /// amount LYR rewarded per period
        lyr_per_period: u64,
        /// Optional: Exponent in the liquidity mining formula; default 2
        exp: u8,
    },

    /// Place an order on a perp market
    ///
    /// In case this order is matched, the corresponding order structs on both
    /// PerpAccounts (taker & maker) will be adjusted, and the position size
    /// will be adjusted w/o accounting for fees.
    /// In addition a FillEvent will be placed on the event queue.
    /// Through a subsequent invocation of ConsumeEvents the FillEvent can be
    /// executed and the perp account balances (base/quote) and fees will be
    /// paid from the quote position. Only at this point the position balance
    /// is 100% refelecting the trade.
    ///
    /// Accounts expected by this instruction (8 + `MAX_PAIRS` + (optional 1)):
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[writable]` lyrae_account_ai - the LyraeAccount of owner
    /// 2. `[signer]` owner_ai - owner of LyraeAccount
    /// 3. `[]` lyrae_cache_ai - LyraeCache for this LyraeGroup
    /// 4. `[writable]` perp_market_ai
    /// 5. `[writable]` bids_ai - bids account for this PerpMarket
    /// 6. `[writable]` asks_ai - asks account for this PerpMarket
    /// 7. `[writable]` event_queue_ai - EventQueue for this PerpMarket
    /// 8..23 `[]` open_orders_ais - array of open orders accounts on this LyraeAccount
    /// 23. `[writable]` referrer_lyrae_account_ai - optional, lyrae account of referrer
    PlacePerpOrder {
        price: i64,
        quantity: i64,
        client_order_id: u64,
        side: Side,
        /// Can be 0 -> LIMIT, 1 -> IOC, 2 -> PostOnly, 3 -> Market, 4 -> PostOnlySlide
        order_type: OrderType,
        /// Optional to be backward compatible; default false
        reduce_only: bool,
    },

    CancelPerpOrderByClientId {
        client_order_id: u64,
        invalid_id_ok: bool,
    },

    CancelPerpOrder {
        order_id: i128,
        invalid_id_ok: bool,
    },

    ConsumeEvents {
        limit: usize,
    },

    /// Cache perp markets
    ///
    /// Accounts expected: 2 + Perp Markets
    /// 0. `[]` lyrae_group_ai
    /// 1. `[writable]` lyrae_cache_ai
    CachePerpMarkets,

    /// Update funding related variables
    UpdateFunding,

    /// Can only be used on a stub oracle in devnet
    SetOracle {
        price: I80F48,
    },

    /// Settle all funds from serum dex open orders
    ///
    /// Accounts expected by this instruction (18):
    ///
    /// 0. `[]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[]` lyrae_cache_ai - LyraeCache for this LyraeGroup
    /// 2. `[signer]` owner_ai - LyraeAccount owner
    /// 3. `[writable]` lyrae_account_ai - LyraeAccount
    /// 4. `[]` dex_prog_ai - program id of serum dex
    /// 5.  `[writable]` spot_market_ai - dex MarketState account
    /// 6.  `[writable]` open_orders_ai - open orders for this market for this LyraeAccount
    /// 7. `[]` signer_ai - LyraeGroup signer key
    /// 8. `[writable]` dex_base_ai - base vault for dex MarketState
    /// 9. `[writable]` dex_quote_ai - quote vault for dex MarketState
    /// 10. `[]` base_root_bank_ai - LyraeGroup base vault acc
    /// 11. `[writable]` base_node_bank_ai - LyraeGroup quote vault acc
    /// 12. `[]` quote_root_bank_ai - LyraeGroup quote vault acc
    /// 13. `[writable]` quote_node_bank_ai - LyraeGroup quote vault acc
    /// 14. `[writable]` base_vault_ai - LyraeGroup base vault acc
    /// 15. `[writable]` quote_vault_ai - LyraeGroup quote vault acc
    /// 16. `[]` dex_signer_ai - dex Market signer account
    /// 17. `[]` spl token program
    SettleFunds,

    /// Cancel an order using dex instruction
    ///
    /// Accounts expected by this instruction ():
    ///
    CancelSpotOrder {
        // 20
        order: serum_dex::instruction::CancelOrderInstructionV2,
    },

    /// Update a root bank's indexes by providing all it's node banks
    ///
    /// Accounts expected: 2 + Node Banks
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[]` root_bank_ai - RootBank
    /// 2+... `[]` node_bank_ais - NodeBanks
    UpdateRootBank,

    /// Take two LyraeAccounts and settle profits and losses between them for a perp market
    ///
    /// Accounts expected (6):
    SettlePnl {
        market_index: usize,
    },

    /// DEPRECATED - no longer makes sense
    /// Use this token's position and deposit to reduce borrows
    ///
    /// Accounts expected by this instruction (5):
    SettleBorrow {
        token_index: usize,
        quantity: u64,
    },

    /// Force cancellation of open orders for a user being liquidated
    ///
    /// Accounts expected: 19 + Liqee open orders accounts (MAX_PAIRS)
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[]` lyrae_cache_ai - LyraeCache
    /// 2. `[writable]` liqee_lyrae_account_ai - LyraeAccount
    /// 3. `[]` base_root_bank_ai - RootBank
    /// 4. `[writable]` base_node_bank_ai - NodeBank
    /// 5. `[writable]` base_vault_ai - LyraeGroup base vault acc
    /// 6. `[]` quote_root_bank_ai - RootBank
    /// 7. `[writable]` quote_node_bank_ai - NodeBank
    /// 8. `[writable]` quote_vault_ai - LyraeGroup quote vault acc
    /// 9. `[writable]` spot_market_ai - SpotMarket
    /// 10. `[writable]` bids_ai - SpotMarket bids acc
    /// 11. `[writable]` asks_ai - SpotMarket asks acc
    /// 12. `[signer]` signer_ai - Signer
    /// 13. `[writable]` dex_event_queue_ai - Market event queue acc
    /// 14. `[writable]` dex_base_ai -
    /// 15. `[writable]` dex_quote_ai -
    /// 16. `[]` dex_signer_ai -
    /// 17. `[]` dex_prog_ai - Dex Program acc
    /// 18. `[]` token_prog_ai - Token Program acc
    /// 19+... `[]` liqee_open_orders_ais - Liqee open orders accs
    ForceCancelSpotOrders {
        limit: u8,
    },

    /// Force cancellation of open orders for a user being liquidated
    ///
    /// Accounts expected: 6 + Liqee open orders accounts (MAX_PAIRS)
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[]` lyrae_cache_ai - LyraeCache
    /// 2. `[]` perp_market_ai - PerpMarket
    /// 3. `[writable]` bids_ai - Bids acc
    /// 4. `[writable]` asks_ai - Asks acc
    /// 5. `[writable]` liqee_lyrae_account_ai - Liqee LyraeAccount
    /// 6+... `[]` liqor_open_orders_ais - Liqee open orders accs
    ForceCancelPerpOrders {
        limit: u8,
    },

    /// Liquidator takes some of borrows at token at `liab_index` and receives some deposits from
    /// the token at `asset_index`
    ///
    /// Accounts expected: 9 + Liqee open orders accounts (MAX_PAIRS) + Liqor open orders accounts (MAX_PAIRS)
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[]` lyrae_cache_ai - LyraeCache
    /// 2. `[writable]` liqee_lyrae_account_ai - LyraeAccount
    /// 3. `[writable]` liqor_lyrae_account_ai - LyraeAccount
    /// 4. `[signer]` liqor_ai - Liqor Account
    /// 5. `[]` asset_root_bank_ai - RootBank
    /// 6. `[writable]` asset_node_bank_ai - NodeBank
    /// 7. `[]` liab_root_bank_ai - RootBank
    /// 8. `[writable]` liab_node_bank_ai - NodeBank
    /// 9+... `[]` liqee_open_orders_ais - Liqee open orders accs
    /// 9+MAX_PAIRS... `[]` liqor_open_orders_ais - Liqor open orders accs
    LiquidateTokenAndToken {
        max_liab_transfer: I80F48,
    },

    /// Swap tokens for perp quote position if only and only if the base position in that market is 0
    ///
    /// Accounts expected: 7 + Liqee open orders accounts (MAX_PAIRS) + Liqor open orders accounts (MAX_PAIRS)
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[]` lyrae_cache_ai - LyraeCache
    /// 2. `[writable]` liqee_lyrae_account_ai - LyraeAccount
    /// 3. `[writable]` liqor_lyrae_account_ai - LyraeAccount
    /// 4. `[signer]` liqor_ai - Liqor Account
    /// 5. `[]` root_bank_ai - RootBank
    /// 6. `[writable]` node_bank_ai - NodeBank
    /// 7+... `[]` liqee_open_orders_ais - Liqee open orders accs
    /// 7+MAX_PAIRS... `[]` liqor_open_orders_ais - Liqor open orders accs
    LiquidateTokenAndPerp {
        asset_type: AssetType,
        asset_index: usize,
        liab_type: AssetType,
        liab_index: usize,
        max_liab_transfer: I80F48,
    },

    /// Reduce some of the base position in exchange for quote position in this market
    ///
    /// Accounts expected: 7 + Liqee open orders accounts (MAX_PAIRS) + Liqor open orders accounts (MAX_PAIRS)
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[]` lyrae_cache_ai - LyraeCache
    /// 2. `[writable]` perp_market_ai - PerpMarket
    /// 3. `[writable]` event_queue_ai - EventQueue
    /// 4. `[writable]` liqee_lyrae_account_ai - LyraeAccount
    /// 5. `[writable]` liqor_lyrae_account_ai - LyraeAccount
    /// 6. `[signer]` liqor_ai - Liqor Account
    /// 7+... `[]` liqee_open_orders_ais - Liqee open orders accs
    /// 7+MAX_PAIRS... `[]` liqor_open_orders_ais - Liqor open orders accs
    LiquidatePerpMarket {
        base_transfer_request: i64,
    },

    /// Take an account that has losses in the selected perp market to account for fees_accrued
    ///
    /// Accounts expected: 10
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[]` lyrae_cache_ai - LyraeCache
    /// 2. `[writable]` perp_market_ai - PerpMarket
    /// 3. `[writable]` lyrae_account_ai - LyraeAccount
    /// 4. `[]` root_bank_ai - RootBank
    /// 5. `[writable]` node_bank_ai - NodeBank
    /// 6. `[writable]` bank_vault_ai - ?
    /// 7. `[writable]` fees_vault_ai - fee vault owned by lyrae DAO token governance
    /// 8. `[]` signer_ai - Group Signer Account
    /// 9. `[]` token_prog_ai - Token Program Account
    SettleFees,

    /// Claim insurance fund and then socialize loss
    ///
    /// Accounts expected: 12 + Liqor open orders accounts (MAX_PAIRS)
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[writable]` lyrae_cache_ai - LyraeCache
    /// 2. `[writable]` liqee_lyrae_account_ai - Liqee LyraeAccount
    /// 3. `[writable]` liqor_lyrae_account_ai - Liqor LyraeAccount
    /// 4. `[signer]` liqor_ai - Liqor Account
    /// 5. `[]` root_bank_ai - RootBank
    /// 6. `[writable]` node_bank_ai - NodeBank
    /// 7. `[writable]` vault_ai - ?
    /// 8. `[writable]` dao_vault_ai - DAO Vault
    /// 9. `[]` signer_ai - Group Signer Account
    /// 10. `[]` perp_market_ai - PerpMarket
    /// 11. `[]` token_prog_ai - Token Program Account
    /// 12+... `[]` liqor_open_orders_ais - Liqor open orders accs
    ResolvePerpBankruptcy {
        // 30
        liab_index: usize,
        max_liab_transfer: I80F48,
    },

    /// Claim insurance fund and then socialize loss
    ///
    /// Accounts expected: 13 + Liqor open orders accounts (MAX_PAIRS) + Liab node banks (MAX_NODE_BANKS)
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[writable]` lyrae_cache_ai - LyraeCache
    /// 2. `[writable]` liqee_lyrae_account_ai - Liqee LyraeAccount
    /// 3. `[writable]` liqor_lyrae_account_ai - Liqor LyraeAccount
    /// 4. `[signer]` liqor_ai - Liqor Account
    /// 5. `[]` quote_root_bank_ai - RootBank
    /// 6. `[writable]` quote_node_bank_ai - NodeBank
    /// 7. `[writable]` quote_vault_ai - ?
    /// 8. `[writable]` dao_vault_ai - DAO Vault
    /// 9. `[]` signer_ai - Group Signer Account
    /// 10. `[]` liab_root_bank_ai - RootBank
    /// 11. `[writable]` liab_node_bank_ai - NodeBank
    /// 12. `[]` token_prog_ai - Token Program Account
    /// 13+... `[]` liqor_open_orders_ais - Liqor open orders accs
    /// 14+MAX_PAIRS... `[]` liab_node_bank_ais - Lib token node banks
    ResolveTokenBankruptcy {
        max_liab_transfer: I80F48,
    },

    /// Initialize open orders
    ///
    /// Accounts expected by this instruction (8):
    ///
    /// 0. `[]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[writable]` lyrae_account_ai - LyraeAccount
    /// 2. `[signer]` owner_ai - LyraeAccount owner
    /// 3. `[]` dex_prog_ai - program id of serum dex
    /// 4. `[writable]` open_orders_ai - open orders for this market for this LyraeAccount
    /// 5. `[]` spot_market_ai - dex MarketState account
    /// 6. `[]` signer_ai - Group Signer Account
    /// 7. `[]` rent_ai - Rent sysvar account
    InitSpotOpenOrders,

    /// Redeem the Lyr_accrued in a PerpAccount for LYR in LyraeAccount deposits
    ///
    /// Accounts expected by this instruction (11):
    /// 0. `[]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[]` lyrae_cache_ai - LyraeCache
    /// 2. `[writable]` lyrae_account_ai - LyraeAccount
    /// 3. `[signer]` owner_ai - LyraeAccount owner
    /// 4. `[]` perp_market_ai - PerpMarket
    /// 5. `[writable]` Lyr_perp_vault_ai
    /// 6. `[]` Lyr_root_bank_ai
    /// 7. `[writable]` Lyr_node_bank_ai
    /// 8. `[writable]` Lyr_bank_vault_ai
    /// 9. `[]` signer_ai - Group Signer Account
    /// 10. `[]` token_prog_ai - SPL Token program id
    RedeemLyr,

    /// Add account info; useful for naming accounts
    ///
    /// Accounts expected by this instruction (3):
    /// 0. `[]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[writable]` lyrae_account_ai - LyraeAccount
    /// 2. `[signer]` owner_ai - LyraeAccount owner
    AddLyraeAccountInfo {
        info: [u8; INFO_LEN],
    },

    /// Deposit MSRM to reduce fees. This MSRM is not at risk and is not used for any health calculations
    ///
    /// Accounts expected by this instruction (6):
    ///
    /// 0. `[]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[writable]` lyrae_account_ai - LyraeAccount
    /// 2. `[signer]` owner_ai - LyraeAccount owner
    /// 3. `[writable]` msrm_account_ai - MSRM token account
    /// 4. `[writable]` msrm_vault_ai - MSRM vault owned by lyrae program
    /// 5. `[]` token_prog_ai - SPL Token program id
    DepositMsrm {
        quantity: u64,
    },

    /// Withdraw the MSRM deposited
    ///
    /// Accounts expected by this instruction (7):
    ///
    /// 0. `[]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[writable]` lyrae_account_ai - LyraeAccount
    /// 2. `[signer]` owner_ai - LyraeAccount owner
    /// 3. `[writable]` msrm_account_ai - MSRM token account
    /// 4. `[writable]` msrm_vault_ai - MSRM vault owned by lyrae program
    /// 5. `[]` signer_ai - signer key of the LyraeGroup
    /// 6. `[]` token_prog_ai - SPL Token program id
    WithdrawMsrm {
        quantity: u64,
    },

    /// Change the params for perp market.
    ///
    /// Accounts expected by this instruction (3):
    /// 0. `[writable]` lyrae_group_ai - LyraeGroup
    /// 1. `[writable]` perp_market_ai - PerpMarket
    /// 2. `[signer]` admin_ai - LyraeGroup admin
    ChangePerpMarketParams {
        #[serde(serialize_with = "serialize_option_fixed_width")]
        maint_leverage: Option<I80F48>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        init_leverage: Option<I80F48>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        liquidation_fee: Option<I80F48>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        maker_fee: Option<I80F48>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        taker_fee: Option<I80F48>,

        /// Starting rate for liquidity mining
        #[serde(serialize_with = "serialize_option_fixed_width")]
        rate: Option<I80F48>,

        /// depth liquidity mining works for
        #[serde(serialize_with = "serialize_option_fixed_width")]
        max_depth_bps: Option<I80F48>,

        /// target length in seconds of one period
        #[serde(serialize_with = "serialize_option_fixed_width")]
        target_period_length: Option<u64>,

        /// amount LYR rewarded per period
        #[serde(serialize_with = "serialize_option_fixed_width")]
        lyr_per_period: Option<u64>,

        /// Optional: Exponent in the liquidity mining formula
        #[serde(serialize_with = "serialize_option_fixed_width")]
        exp: Option<u8>,
    },

    /// Transfer admin permissions over group to another account
    ///
    /// Accounts expected by this instruction (3):
    /// 0. `[writable]` lyrae_group_ai - LyraeGroup
    /// 1. `[]` new_admin_ai - New LyraeGroup admin
    /// 2. `[signer]` admin_ai - LyraeGroup admin
    SetGroupAdmin,

    /// Cancel all perp open orders (batch cancel)
    ///
    /// Accounts expected: 6
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[writable]` lyrae_account_ai - LyraeAccount
    /// 2. `[signer]` owner_ai - Owner of Lyrae Account
    /// 3. `[writable]` perp_market_ai - PerpMarket
    /// 4. `[writable]` bids_ai - Bids acc
    /// 5. `[writable]` asks_ai - Asks acc
    CancelAllPerpOrders {
        limit: u8,
    },

    /// DEPRECATED - No longer valid instruction as of release 3.0.5
    /// Liqor takes on all the quote positions where base_position == 0
    /// Equivalent amount of quote currency is credited/debited in deposits/borrows.
    /// This is very similar to the settle_pnl function, but is forced for Sick accounts
    ///
    /// Accounts expected: 7 + MAX_PAIRS
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[]` lyrae_cache_ai - LyraeCache
    /// 2. `[writable]` liqee_lyrae_account_ai - LyraeAccount
    /// 3. `[writable]` liqor_lyrae_account_ai - LyraeAccount
    /// 4. `[signer]` liqor_ai - Liqor Account
    /// 5. `[]` root_bank_ai - RootBank
    /// 6. `[writable]` node_bank_ai - NodeBank
    /// 7+... `[]` liqee_open_orders_ais - Liqee open orders accs
    ForceSettleQuotePositions, // instruction 40

    /// Place an order on the Serum Dex using Lyrae account. Improved over PlaceSpotOrder
    /// by reducing the tx size
    PlaceSpotOrder2 {
        order: serum_dex::instruction::NewOrderInstructionV3,
    },

    /// Initialize the advanced open orders account for a LyraeAccount and set
    InitAdvancedOrders,

    /// Add a trigger order which executes if the trigger condition is met.
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[]` lyrae_account_ai - the LyraeAccount of owner
    /// 2. `[writable, signer]` owner_ai - owner of LyraeAccount
    /// 3  `[writable]` advanced_orders_ai - the AdvanceOrdersAccount of owner
    /// 4. `[]` lyrae_cache_ai - LyraeCache for this LyraeGroup
    /// 5. `[]` perp_market_ai
    /// 6. `[]` system_prog_ai
    /// 7.. `[]` open_orders_ais - OpenOrders account for each serum dex market in margin basket
    AddPerpTriggerOrder {
        order_type: OrderType,
        side: Side,
        trigger_condition: TriggerCondition,
        reduce_only: bool, // only valid on perp order
        client_order_id: u64,
        price: i64,
        quantity: i64,
        trigger_price: I80F48,
    },
    /// Remove the order at the order_index
    RemoveAdvancedOrder {
        order_index: u8,
    },

    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[writable]` lyrae_account_ai - the LyraeAccount of owner
    /// 2  `[writable]` advanced_orders_ai - the AdvanceOrdersAccount of owner
    /// 3. `[writable,signer]` agent_ai - operator of the execution service (receives lamports)
    /// 4. `[]` lyrae_cache_ai - LyraeCache for this LyraeGroup
    /// 5. `[writable]` perp_market_ai
    /// 6. `[writable]` bids_ai - bids account for this PerpMarket
    /// 7. `[writable]` asks_ai - asks account for this PerpMarket
    /// 8. `[writable]` event_queue_ai - EventQueue for this PerpMarket
    /// 9. `[] system_prog_ai
    ExecutePerpTriggerOrder {
        order_index: u8,
    },

    /// Create the necessary PDAs for the perp market and initialize them and add to LyraeGroup
    ///
    /// Accounts expected by this instruction (13):
    ///
    /// 0. `[writable]` lyrae_group_ai
    /// 1. `[]` oracle_ai
    /// 2. `[writable]` perp_market_ai
    /// 3. `[writable]` event_queue_ai
    /// 4. `[writable]` bids_ai
    /// 5. `[writable]` asks_ai
    /// 6. `[]` Lyr_mint_ai - Lyr token mint
    /// 7. `[writable]` Lyr_vault_ai - the vault from which liquidity incentives will be paid out for this market
    /// 8. `[signer, writable]` admin_ai - writable if admin_ai is also funder
    /// 9. `[writable]` signer_ai - optionally writable if funder is signer_ai
    /// 10. `[]` system_prog_ai - system program
    /// 11. `[]` token_prog_ai - SPL token program
    /// 12. `[]` rent_ai - rent sysvar because SPL token program requires it
    CreatePerpMarket {
        maint_leverage: I80F48,
        init_leverage: I80F48,
        liquidation_fee: I80F48,
        maker_fee: I80F48,
        taker_fee: I80F48,
        base_lot_size: i64,
        quote_lot_size: i64,
        /// Starting rate for liquidity mining
        rate: I80F48,
        /// v0: depth in bps for liquidity mining; v1: depth in contract size
        max_depth_bps: I80F48,
        /// target length in seconds of one period
        target_period_length: u64,
        /// amount LYR rewarded per period
        lyr_per_period: u64,
        exp: u8,
        version: u8,
        /// Helps with integer overflow
        lm_size_shift: u8,
        /// define base decimals in case spot market has not yet been listed
        base_decimals: u8,
    },

    /// Change the params for perp market.
    ///
    /// Accounts expected by this instruction (3):
    /// 0. `[writable]` lyrae_group_ai - LyraeGroup
    /// 1. `[writable]` perp_market_ai - PerpMarket
    /// 2. `[signer]` admin_ai - LyraeGroup admin
    ChangePerpMarketParams2 {
        #[serde(serialize_with = "serialize_option_fixed_width")]
        maint_leverage: Option<I80F48>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        init_leverage: Option<I80F48>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        liquidation_fee: Option<I80F48>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        maker_fee: Option<I80F48>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        taker_fee: Option<I80F48>,

        /// Starting rate for liquidity mining
        #[serde(serialize_with = "serialize_option_fixed_width")]
        rate: Option<I80F48>,

        /// depth liquidity mining works for
        #[serde(serialize_with = "serialize_option_fixed_width")]
        max_depth_bps: Option<I80F48>,

        /// target length in seconds of one period
        #[serde(serialize_with = "serialize_option_fixed_width")]
        target_period_length: Option<u64>,

        /// amount LYR rewarded per period
        #[serde(serialize_with = "serialize_option_fixed_width")]
        lyr_per_period: Option<u64>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        exp: Option<u8>,
        #[serde(serialize_with = "serialize_option_fixed_width")]
        version: Option<u8>,
        #[serde(serialize_with = "serialize_option_fixed_width")]
        lm_size_shift: Option<u8>,
    },

    /// Change the params for perp market.
    ///
    /// Accounts expected by this instruction (2 + MAX_PAIRS):
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[writable]` lyrae_account_ai - LyraeAccount
    /// 2+ `[]` open_orders_ais - An array of MAX_PAIRS. Only OpenOrders of current market
    ///         index needs to be writable. Only OpenOrders in_margin_basket needs to be correct;
    ///         remaining open orders can just be Pubkey::default() (the zero key)
    UpdateMarginBasket,

    /// Change the maximum number of closeable LyraeAccounts.v1 allowed
    ///
    /// Accounts expected by this instruction (2):
    ///
    /// 0. `[writable]` lyrae_group_ai - LyraeGroup
    /// 1. `[signer]` admin_ai - Admin
    ChangeMaxLyraeAccounts {
        max_lyrae_accounts: u32,
    },
    /// Delete a lyrae account and return lamports
    ///
    /// Accounts expected by this instruction (3):
    ///
    /// 0. `[writable]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[writable]` lyrae_account_ai - the lyrae account data
    /// 2. `[signer]` owner_ai - Solana account of owner of the lyrae account
    CloseLyraeAccount, // instruction 50

    /// Delete a spot open orders account and return lamports
    ///
    /// Accounts expected by this instruction (7):
    ///
    /// 0. `[]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[writable]` lyrae_account_ai - the lyrae account data
    /// 2. `[signer, writable]` owner_ai - Solana account of owner of the lyrae account
    /// 3. `[]` dex_prog_ai - The serum dex program id
    /// 4. `[writable]` open_orders_ai - The open orders account to close
    /// 5. `[]` spot_market_ai - The spot market for the account
    /// 6. `[]` signer_ai - Lyrae group signer key
    CloseSpotOpenOrders,

    /// Delete an advanced orders account and return lamports
    ///
    /// Accounts expected by this instruction (4):
    ///
    /// 0. `[]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[writable]` lyrae_account_ai - the lyrae account data
    /// 2. `[signer, writable]` owner_ai - Solana account of owner of the lyrae account
    /// 3. `[writable]` advanced_orders_ai - the advanced orders account
    CloseAdvancedOrders,

    /// Create a PDA Lyrae Account for collecting dust owned by a group
    ///
    /// Accounts expected by this instruction (4)
    /// 0. `[]` lyrae_group_ai - LyraeGroup to create the dust account for
    /// 1. `[writable]` lyrae_account_ai - the lyrae account data
    /// 2. `[signer, writable]` signer_ai - Signer and fee payer account
    /// 3. `[writable]` system_prog_ai - System program
    CreateDustAccount,

    /// Transfer dust (< 1 native SPL token) assets and liabilities for a single token to the group's dust account
    ///
    /// Accounts expected by this instruction (7)
    ///
    /// 0. `[]` lyrae_group_ai - LyraeGroup of the lyrae account
    /// 1. `[writable]` lyrae_account_ai - the lyrae account data
    /// 2. `[signer, writable]` owner_ai - Solana account of owner of the lyrae account
    /// 3. `[writable]` dust_account_ai - Dust Account for the group
    /// 4. `[]` root_bank_ai - The root bank for the token
    /// 5. `[writable]` node_bank_ai - A node bank for the token
    /// 6. `[]` lyrae_cache_ai - The cache for the group
    ResolveDust,

    /// Create a PDA lyrae account for a user
    ///
    /// Accounts expected by this instruction (5):
    ///
    /// 0. `[writable]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[writable]` lyrae_account_ai - the lyrae account data
    /// 2. `[signer]` owner_ai - Solana account of owner of the lyrae account
    /// 3. `[]` system_prog_ai - System program
    /// 4. `[signer, writable]` payer_ai - pays for the PDA creation
    CreateLyraeAccount {
        account_num: u64,
    },

    /// Upgrade a V0 Lyrae Account to V1 allowing it to be closed
    ///
    /// Accounts expected by this instruction (3):
    ///
    /// 0. `[writable]` lyrae_group_ai - LyraeGroup
    /// 1. `[writable]` lyrae_account_ai - LyraeAccount
    /// 2. `[signer]` owner_ai - Solana account of owner of the lyrae account
    UpgradeLyraeAccountV0V1,

    /// Cancel all perp open orders for one side of the book
    ///
    /// Accounts expected: 6
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[writable]` lyrae_account_ai - LyraeAccount
    /// 2. `[signer]` owner_ai - Owner of Lyrae Account
    /// 3. `[writable]` perp_market_ai - PerpMarket
    /// 4. `[writable]` bids_ai - Bids acc
    /// 5. `[writable]` asks_ai - Asks acc
    CancelPerpOrdersSide {
        side: Side,
        limit: u8,
    },

    /// https://github.com/blockworks-foundation/lyrae-v3/pull/97/
    /// Set delegate authority to lyrae account which can do everything regular account can do
    /// except Withdraw and CloseLyraeAccount. Set to Pubkey::default() to revoke delegate
    ///
    /// Accounts expected: 4
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[writable]` lyrae_account_ai - LyraeAccount
    /// 2. `[signer]` owner_ai - Owner of Lyrae Account
    /// 3. `[]` delegate_ai - delegate
    SetDelegate,

    /// Change the params for a spot market.
    ///
    /// Accounts expected by this instruction (4):
    /// 0. `[writable]` lyrae_group_ai - LyraeGroup
    /// 1. `[writable]` spot_market_ai - Market
    /// 2. `[writable]` root_bank_ai - RootBank
    /// 3. `[signer]` admin_ai - LyraeGroup admin
    ChangeSpotMarketParams {
        #[serde(serialize_with = "serialize_option_fixed_width")]
        maint_leverage: Option<I80F48>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        init_leverage: Option<I80F48>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        liquidation_fee: Option<I80F48>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        optimal_util: Option<I80F48>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        optimal_rate: Option<I80F48>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        max_rate: Option<I80F48>,

        #[serde(serialize_with = "serialize_option_fixed_width")]
        version: Option<u8>,
    },

    /// Create an OpenOrders PDA and initialize it with InitOpenOrders call to serum dex
    ///
    /// Accounts expected by this instruction (9):
    ///
    /// 0. `[]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[writable]` lyrae_account_ai - LyraeAccount
    /// 2. `[signer]` owner_ai - LyraeAccount owner
    /// 3. `[]` dex_prog_ai - program id of serum dex
    /// 4. `[writable]` open_orders_ai - open orders PDA
    /// 5. `[]` spot_market_ai - dex MarketState account
    /// 6. `[]` signer_ai - Group Signer Account
    /// 7. `[]` system_prog_ai - System program
    /// 8. `[signer, writable]` payer_ai - pays for the PDA creation
    CreateSpotOpenOrders, // instruction 60

    /// Set the `ref_surcharge_centibps`, `ref_share_centibps` and `ref_Lyr_required` on `LyraeGroup`
    ///
    /// Accounts expected by this instruction (2):
    /// 0. `[writable]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[signer]` admin_ai - lyrae_group.admin
    ChangeReferralFeeParams {
        ref_surcharge_centibps: u32,
        ref_share_centibps: u32,
        ref_lyr_required: u64,
    },
    /// Store the referrer's LyraeAccount pubkey on the Referrer account
    /// It will create the Referrer account as a PDA of user's LyraeAccount if it doesn't exist
    /// This is primarily useful for the UI; the referrer address stored here is not necessarily
    /// who earns the ref fees.
    ///
    /// Accounts expected by this instruction (7):
    ///
    /// 0. `[]` lyrae_group_ai - LyraeGroup that this lyrae account is for
    /// 1. `[]` lyrae_account_ai - LyraeAccount of the referred
    /// 2. `[signer]` owner_ai - LyraeAccount owner or delegate
    /// 3. `[writable]` referrer_memory_ai - ReferrerMemory struct; will be initialized if required
    /// 4. `[]` referrer_lyrae_account_ai - referrer's LyraeAccount
    /// 5. `[signer, writable]` payer_ai - payer for PDA; can be same as owner
    /// 6. `[]` system_prog_ai - System program
    SetReferrerMemory,

    /// Associate the referrer's LyraeAccount with a human readable `referrer_id` which can be used
    /// in a ref link. This is primarily useful for the UI.
    /// Create the `ReferrerIdRecord` PDA; if it already exists throw error
    ///
    /// Accounts expected by this instruction (5):
    /// 0. `[]` lyrae_group_ai - LyraeGroup
    /// 1. `[]` referrer_lyrae_account_ai - LyraeAccount
    /// 2. `[writable]` referrer_id_record_ai - The PDA to store the record on
    /// 3. `[signer, writable]` payer_ai - payer for PDA; can be same as owner
    /// 4. `[]` system_prog_ai - System program
    RegisterReferrerId {
        referrer_id: [u8; INFO_LEN],
    },
}

impl LyraeInstruction {
    pub fn unpack(input: &[u8]) -> Option<Self> {
        let (&discrim, data) = array_refs![input, 4; ..;];
        let discrim = u32::from_le_bytes(discrim);
        Some(match discrim {
            0 => {
                let data = array_ref![data, 0, 64];
                let (
                    signer_nonce,
                    valid_interval,
                    quote_optimal_util,
                    quote_optimal_rate,
                    quote_max_rate,
                ) = array_refs![data, 8, 8, 16, 16, 16];

                LyraeInstruction::InitLyraeGroup {
                    signer_nonce: u64::from_le_bytes(*signer_nonce),
                    valid_interval: u64::from_le_bytes(*valid_interval),
                    quote_optimal_util: I80F48::from_le_bytes(*quote_optimal_util),
                    quote_optimal_rate: I80F48::from_le_bytes(*quote_optimal_rate),
                    quote_max_rate: I80F48::from_le_bytes(*quote_max_rate),
                }
            }
            1 => LyraeInstruction::InitLyraeAccount,
            2 => {
                let quantity = array_ref![data, 0, 8];
                LyraeInstruction::Deposit {
                    quantity: u64::from_le_bytes(*quantity),
                }
            }
            3 => {
                let data = array_ref![data, 0, 9];
                let (quantity, allow_borrow) = array_refs![data, 8, 1];

                let allow_borrow = match allow_borrow {
                    [0] => false,
                    [1] => true,
                    _ => return None,
                };
                LyraeInstruction::Withdraw {
                    quantity: u64::from_le_bytes(*quantity),
                    allow_borrow,
                }
            }
            4 => {
                let data = array_ref![data, 0, 96];
                let (
                    maint_leverage,
                    init_leverage,
                    liquidation_fee,
                    optimal_util,
                    optimal_rate,
                    max_rate,
                ) = array_refs![data, 16, 16, 16, 16, 16, 16];
                LyraeInstruction::AddSpotMarket {
                    maint_leverage: I80F48::from_le_bytes(*maint_leverage),
                    init_leverage: I80F48::from_le_bytes(*init_leverage),
                    liquidation_fee: I80F48::from_le_bytes(*liquidation_fee),
                    optimal_util: I80F48::from_le_bytes(*optimal_util),
                    optimal_rate: I80F48::from_le_bytes(*optimal_rate),
                    max_rate: I80F48::from_le_bytes(*max_rate),
                }
            }
            5 => {
                let market_index = array_ref![data, 0, 8];
                LyraeInstruction::AddToBasket {
                    market_index: usize::from_le_bytes(*market_index),
                }
            }
            6 => {
                let quantity = array_ref![data, 0, 8];
                LyraeInstruction::Borrow {
                    quantity: u64::from_le_bytes(*quantity),
                }
            }
            7 => LyraeInstruction::CachePrices,
            8 => LyraeInstruction::CacheRootBanks,
            9 => {
                let data_arr = array_ref![data, 0, 46];
                let order = unpack_dex_new_order_v3(data_arr)?;
                LyraeInstruction::PlaceSpotOrder { order }
            }
            10 => LyraeInstruction::AddOracle,
            11 => {
                let exp = if data.len() > 144 { data[144] } else { 2 };
                let data_arr = array_ref![data, 0, 144];
                let (
                    maint_leverage,
                    init_leverage,
                    liquidation_fee,
                    maker_fee,
                    taker_fee,
                    base_lot_size,
                    quote_lot_size,
                    rate,
                    max_depth_bps,
                    target_period_length,
                    lyr_per_period,
                ) = array_refs![data_arr, 16, 16, 16, 16, 16, 8, 8, 16, 16, 8, 8];
                LyraeInstruction::AddPerpMarket {
                    maint_leverage: I80F48::from_le_bytes(*maint_leverage),
                    init_leverage: I80F48::from_le_bytes(*init_leverage),
                    liquidation_fee: I80F48::from_le_bytes(*liquidation_fee),
                    maker_fee: I80F48::from_le_bytes(*maker_fee),
                    taker_fee: I80F48::from_le_bytes(*taker_fee),
                    base_lot_size: i64::from_le_bytes(*base_lot_size),
                    quote_lot_size: i64::from_le_bytes(*quote_lot_size),
                    rate: I80F48::from_le_bytes(*rate),
                    max_depth_bps: I80F48::from_le_bytes(*max_depth_bps),
                    target_period_length: u64::from_le_bytes(*target_period_length),
                    lyr_per_period: u64::from_le_bytes(*lyr_per_period),
                    exp,
                }
            }
            12 => {
                let reduce_only = if data.len() > 26 {
                    data[26] != 0
                } else {
                    false
                };
                let data_arr = array_ref![data, 0, 26];
                let (price, quantity, client_order_id, side, order_type) =
                    array_refs![data_arr, 8, 8, 8, 1, 1];
                LyraeInstruction::PlacePerpOrder {
                    price: i64::from_le_bytes(*price),
                    quantity: i64::from_le_bytes(*quantity),
                    client_order_id: u64::from_le_bytes(*client_order_id),
                    side: Side::try_from_primitive(side[0]).ok()?,
                    order_type: OrderType::try_from_primitive(order_type[0]).ok()?,
                    reduce_only,
                }
            }
            13 => {
                let data_arr = array_ref![data, 0, 9];
                let (client_order_id, invalid_id_ok) = array_refs![data_arr, 8, 1];

                LyraeInstruction::CancelPerpOrderByClientId {
                    client_order_id: u64::from_le_bytes(*client_order_id),
                    invalid_id_ok: invalid_id_ok[0] != 0,
                }
            }
            14 => {
                let data_arr = array_ref![data, 0, 17];
                let (order_id, invalid_id_ok) = array_refs![data_arr, 16, 1];
                LyraeInstruction::CancelPerpOrder {
                    order_id: i128::from_le_bytes(*order_id),
                    invalid_id_ok: invalid_id_ok[0] != 0,
                }
            }
            15 => {
                let data_arr = array_ref![data, 0, 8];
                LyraeInstruction::ConsumeEvents {
                    limit: usize::from_le_bytes(*data_arr),
                }
            }
            16 => LyraeInstruction::CachePerpMarkets,
            17 => LyraeInstruction::UpdateFunding,
            18 => {
                let data_arr = array_ref![data, 0, 16];
                LyraeInstruction::SetOracle {
                    price: I80F48::from_le_bytes(*data_arr),
                }
            }
            19 => LyraeInstruction::SettleFunds,
            20 => {
                let data_array = array_ref![data, 0, 20];
                let fields = array_refs![data_array, 4, 16];
                let side = match u32::from_le_bytes(*fields.0) {
                    0 => serum_dex::matching::Side::Bid,
                    1 => serum_dex::matching::Side::Ask,
                    _ => return None,
                };
                let order_id = u128::from_le_bytes(*fields.1);
                let order = serum_dex::instruction::CancelOrderInstructionV2 { side, order_id };
                LyraeInstruction::CancelSpotOrder { order }
            }
            21 => LyraeInstruction::UpdateRootBank,

            22 => {
                let data_arr = array_ref![data, 0, 8];

                LyraeInstruction::SettlePnl {
                    market_index: usize::from_le_bytes(*data_arr),
                }
            }
            23 => {
                let data = array_ref![data, 0, 16];
                let (token_index, quantity) = array_refs![data, 8, 8];

                LyraeInstruction::SettleBorrow {
                    token_index: usize::from_le_bytes(*token_index),
                    quantity: u64::from_le_bytes(*quantity),
                }
            }
            24 => {
                let data_arr = array_ref![data, 0, 1];

                LyraeInstruction::ForceCancelSpotOrders {
                    limit: u8::from_le_bytes(*data_arr),
                }
            }
            25 => {
                let data_arr = array_ref![data, 0, 1];

                LyraeInstruction::ForceCancelPerpOrders {
                    limit: u8::from_le_bytes(*data_arr),
                }
            }
            26 => {
                let data_arr = array_ref![data, 0, 16];

                LyraeInstruction::LiquidateTokenAndToken {
                    max_liab_transfer: I80F48::from_le_bytes(*data_arr),
                }
            }
            27 => {
                let data = array_ref![data, 0, 34];
                let (asset_type, asset_index, liab_type, liab_index, max_liab_transfer) =
                    array_refs![data, 1, 8, 1, 8, 16];

                LyraeInstruction::LiquidateTokenAndPerp {
                    asset_type: AssetType::try_from(u8::from_le_bytes(*asset_type)).unwrap(),
                    asset_index: usize::from_le_bytes(*asset_index),
                    liab_type: AssetType::try_from(u8::from_le_bytes(*liab_type)).unwrap(),
                    liab_index: usize::from_le_bytes(*liab_index),
                    max_liab_transfer: I80F48::from_le_bytes(*max_liab_transfer),
                }
            }
            28 => {
                let data_arr = array_ref![data, 0, 8];

                LyraeInstruction::LiquidatePerpMarket {
                    base_transfer_request: i64::from_le_bytes(*data_arr),
                }
            }
            29 => LyraeInstruction::SettleFees,
            30 => {
                let data = array_ref![data, 0, 24];
                let (liab_index, max_liab_transfer) = array_refs![data, 8, 16];

                LyraeInstruction::ResolvePerpBankruptcy {
                    liab_index: usize::from_le_bytes(*liab_index),
                    max_liab_transfer: I80F48::from_le_bytes(*max_liab_transfer),
                }
            }
            31 => {
                let data_arr = array_ref![data, 0, 16];

                LyraeInstruction::ResolveTokenBankruptcy {
                    max_liab_transfer: I80F48::from_le_bytes(*data_arr),
                }
            }
            32 => LyraeInstruction::InitSpotOpenOrders,
            33 => LyraeInstruction::RedeemLyr,
            34 => {
                let info = array_ref![data, 0, INFO_LEN];
                LyraeInstruction::AddLyraeAccountInfo { info: *info }
            }
            35 => {
                let quantity = array_ref![data, 0, 8];
                LyraeInstruction::DepositMsrm {
                    quantity: u64::from_le_bytes(*quantity),
                }
            }
            36 => {
                let quantity = array_ref![data, 0, 8];
                LyraeInstruction::WithdrawMsrm {
                    quantity: u64::from_le_bytes(*quantity),
                }
            }

            37 => {
                let exp = if data.len() > 137 {
                    unpack_u8_opt(&[data[137], data[138]])
                } else {
                    None
                };
                let data_arr = array_ref![data, 0, 137];
                let (
                    maint_leverage,
                    init_leverage,
                    liquidation_fee,
                    maker_fee,
                    taker_fee,
                    rate,
                    max_depth_bps,
                    target_period_length,
                    lyr_per_period,
                ) = array_refs![data_arr, 17, 17, 17, 17, 17, 17, 17, 9, 9];

                LyraeInstruction::ChangePerpMarketParams {
                    maint_leverage: unpack_i80f48_opt(maint_leverage),
                    init_leverage: unpack_i80f48_opt(init_leverage),
                    liquidation_fee: unpack_i80f48_opt(liquidation_fee),
                    maker_fee: unpack_i80f48_opt(maker_fee),
                    taker_fee: unpack_i80f48_opt(taker_fee),
                    rate: unpack_i80f48_opt(rate),
                    max_depth_bps: unpack_i80f48_opt(max_depth_bps),
                    target_period_length: unpack_u64_opt(target_period_length),
                    lyr_per_period: unpack_u64_opt(lyr_per_period),
                    exp,
                }
            }

            38 => LyraeInstruction::SetGroupAdmin,

            39 => {
                let data_arr = array_ref![data, 0, 1];
                LyraeInstruction::CancelAllPerpOrders {
                    limit: u8::from_le_bytes(*data_arr),
                }
            }

            40 => LyraeInstruction::ForceSettleQuotePositions,
            41 => {
                let data_arr = array_ref![data, 0, 46];
                let order = unpack_dex_new_order_v3(data_arr)?;
                LyraeInstruction::PlaceSpotOrder2 { order }
            }

            42 => LyraeInstruction::InitAdvancedOrders,

            43 => {
                let data_arr = array_ref![data, 0, 44];
                let (
                    order_type,
                    side,
                    trigger_condition,
                    reduce_only,
                    client_order_id,
                    price,
                    quantity,
                    trigger_price,
                ) = array_refs![data_arr, 1, 1, 1, 1, 8, 8, 8, 16];
                LyraeInstruction::AddPerpTriggerOrder {
                    order_type: OrderType::try_from_primitive(order_type[0]).ok()?,
                    side: Side::try_from_primitive(side[0]).ok()?,
                    trigger_condition: TriggerCondition::try_from(u8::from_le_bytes(
                        *trigger_condition,
                    ))
                    .unwrap(),
                    reduce_only: reduce_only[0] != 0,
                    client_order_id: u64::from_le_bytes(*client_order_id),
                    price: i64::from_le_bytes(*price),
                    quantity: i64::from_le_bytes(*quantity),
                    trigger_price: I80F48::from_le_bytes(*trigger_price),
                }
            }

            44 => {
                let order_index = array_ref![data, 0, 1][0];
                LyraeInstruction::RemoveAdvancedOrder { order_index }
            }
            45 => {
                let order_index = array_ref![data, 0, 1][0];
                LyraeInstruction::ExecutePerpTriggerOrder { order_index }
            }
            46 => {
                let data_arr = array_ref![data, 0, 148];
                let (
                    maint_leverage,
                    init_leverage,
                    liquidation_fee,
                    maker_fee,
                    taker_fee,
                    base_lot_size,
                    quote_lot_size,
                    rate,
                    max_depth_bps,
                    target_period_length,
                    lyr_per_period,
                    exp,
                    version,
                    lm_size_shift,
                    base_decimals,
                ) = array_refs![data_arr, 16, 16, 16, 16, 16, 8, 8, 16, 16, 8, 8, 1, 1, 1, 1];
                LyraeInstruction::CreatePerpMarket {
                    maint_leverage: I80F48::from_le_bytes(*maint_leverage),
                    init_leverage: I80F48::from_le_bytes(*init_leverage),
                    liquidation_fee: I80F48::from_le_bytes(*liquidation_fee),
                    maker_fee: I80F48::from_le_bytes(*maker_fee),
                    taker_fee: I80F48::from_le_bytes(*taker_fee),
                    base_lot_size: i64::from_le_bytes(*base_lot_size),
                    quote_lot_size: i64::from_le_bytes(*quote_lot_size),
                    rate: I80F48::from_le_bytes(*rate),
                    max_depth_bps: I80F48::from_le_bytes(*max_depth_bps),
                    target_period_length: u64::from_le_bytes(*target_period_length),
                    lyr_per_period: u64::from_le_bytes(*lyr_per_period),
                    exp: exp[0],
                    version: version[0],
                    lm_size_shift: lm_size_shift[0],
                    base_decimals: base_decimals[0],
                }
            }
            47 => {
                let data_arr = array_ref![data, 0, 143];
                let (
                    maint_leverage,
                    init_leverage,
                    liquidation_fee,
                    maker_fee,
                    taker_fee,
                    rate,
                    max_depth_bps,
                    target_period_length,
                    lyr_per_period,
                    exp,
                    version,
                    lm_size_shift,
                ) = array_refs![data_arr, 17, 17, 17, 17, 17, 17, 17, 9, 9, 2, 2, 2];

                LyraeInstruction::ChangePerpMarketParams2 {
                    maint_leverage: unpack_i80f48_opt(maint_leverage),
                    init_leverage: unpack_i80f48_opt(init_leverage),
                    liquidation_fee: unpack_i80f48_opt(liquidation_fee),
                    maker_fee: unpack_i80f48_opt(maker_fee),
                    taker_fee: unpack_i80f48_opt(taker_fee),
                    rate: unpack_i80f48_opt(rate),
                    max_depth_bps: unpack_i80f48_opt(max_depth_bps),
                    target_period_length: unpack_u64_opt(target_period_length),
                    lyr_per_period: unpack_u64_opt(lyr_per_period),
                    exp: unpack_u8_opt(exp),
                    version: unpack_u8_opt(version),
                    lm_size_shift: unpack_u8_opt(lm_size_shift),
                }
            }
            48 => LyraeInstruction::UpdateMarginBasket,
            49 => {
                let data_arr = array_ref![data, 0, 4];
                LyraeInstruction::ChangeMaxLyraeAccounts {
                    max_lyrae_accounts: u32::from_le_bytes(*data_arr),
                }
            }
            50 => LyraeInstruction::CloseLyraeAccount,
            51 => LyraeInstruction::CloseSpotOpenOrders,
            52 => LyraeInstruction::CloseAdvancedOrders,
            53 => LyraeInstruction::CreateDustAccount,
            54 => LyraeInstruction::ResolveDust,
            55 => {
                let account_num = array_ref![data, 0, 8];
                LyraeInstruction::CreateLyraeAccount {
                    account_num: u64::from_le_bytes(*account_num),
                }
            }
            56 => LyraeInstruction::UpgradeLyraeAccountV0V1,
            57 => {
                let data_arr = array_ref![data, 0, 2];
                let (side, limit) = array_refs![data_arr, 1, 1];

                LyraeInstruction::CancelPerpOrdersSide {
                    side: Side::try_from_primitive(side[0]).ok()?,
                    limit: u8::from_le_bytes(*limit),
                }
            }
            58 => LyraeInstruction::SetDelegate,
            59 => {
                let data_arr = array_ref![data, 0, 104];
                let (
                    maint_leverage,
                    init_leverage,
                    liquidation_fee,
                    optimal_util,
                    optimal_rate,
                    max_rate,
                    version,
                ) = array_refs![data_arr, 17, 17, 17, 17, 17, 17, 2];

                LyraeInstruction::ChangeSpotMarketParams {
                    maint_leverage: unpack_i80f48_opt(maint_leverage),
                    init_leverage: unpack_i80f48_opt(init_leverage),
                    liquidation_fee: unpack_i80f48_opt(liquidation_fee),
                    optimal_util: unpack_i80f48_opt(optimal_util),
                    optimal_rate: unpack_i80f48_opt(optimal_rate),
                    max_rate: unpack_i80f48_opt(max_rate),
                    version: unpack_u8_opt(version),
                }
            }
            60 => LyraeInstruction::CreateSpotOpenOrders,
            61 => {
                let data = array_ref![data, 0, 16];
                let (ref_surcharge_centibps, ref_share_centibps, ref_lyr_required) =
                    array_refs![data, 4, 4, 8];
                LyraeInstruction::ChangeReferralFeeParams {
                    ref_surcharge_centibps: u32::from_le_bytes(*ref_surcharge_centibps),
                    ref_share_centibps: u32::from_le_bytes(*ref_share_centibps),
                    ref_lyr_required: u64::from_le_bytes(*ref_lyr_required),
                }
            }
            62 => LyraeInstruction::SetReferrerMemory,
            63 => {
                let referrer_id = array_ref![data, 0, INFO_LEN];
                LyraeInstruction::RegisterReferrerId {
                    referrer_id: *referrer_id,
                }
            }
            _ => {
                return None;
            }
        })
    }
    pub fn pack(&self) -> Vec<u8> {
        bincode::serialize(self).unwrap()
    }
}

fn unpack_u8_opt(data: &[u8; 2]) -> Option<u8> {
    if data[0] == 0 {
        None
    } else {
        Some(data[1])
    }
}

fn unpack_i80f48_opt(data: &[u8; 17]) -> Option<I80F48> {
    let (opt, val) = array_refs![data, 1, 16];
    if opt[0] == 0 {
        None
    } else {
        Some(I80F48::from_le_bytes(*val))
    }
}
fn unpack_u64_opt(data: &[u8; 9]) -> Option<u64> {
    let (opt, val) = array_refs![data, 1, 8];
    if opt[0] == 0 {
        None
    } else {
        Some(u64::from_le_bytes(*val))
    }
}

fn unpack_dex_new_order_v3(
    data: &[u8; 46],
) -> Option<serum_dex::instruction::NewOrderInstructionV3> {
    let (
        &side_arr,
        &price_arr,
        &max_coin_qty_arr,
        &max_native_pc_qty_arr,
        &self_trade_behavior_arr,
        &otype_arr,
        &client_order_id_bytes,
        &limit_arr,
    ) = array_refs![data, 4, 8, 8, 8, 4, 4, 8, 2];

    let side = serum_dex::matching::Side::try_from_primitive(
        u32::from_le_bytes(side_arr).try_into().ok()?,
    )
    .ok()?;
    let limit_price = NonZeroU64::new(u64::from_le_bytes(price_arr))?;
    let max_coin_qty = NonZeroU64::new(u64::from_le_bytes(max_coin_qty_arr))?;
    let max_native_pc_qty_including_fees =
        NonZeroU64::new(u64::from_le_bytes(max_native_pc_qty_arr))?;
    let self_trade_behavior = serum_dex::instruction::SelfTradeBehavior::try_from_primitive(
        u32::from_le_bytes(self_trade_behavior_arr)
            .try_into()
            .ok()?,
    )
    .ok()?;
    let order_type = serum_dex::matching::OrderType::try_from_primitive(
        u32::from_le_bytes(otype_arr).try_into().ok()?,
    )
    .ok()?;
    let client_order_id = u64::from_le_bytes(client_order_id_bytes);
    let limit = u16::from_le_bytes(limit_arr);

    Some(serum_dex::instruction::NewOrderInstructionV3 {
        side,
        limit_price,
        max_coin_qty,
        max_native_pc_qty_including_fees,
        self_trade_behavior,
        order_type,
        client_order_id,
        limit,
    })
}

pub fn init_lyrae_group(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    signer_pk: &Pubkey,
    admin_pk: &Pubkey,
    quote_mint_pk: &Pubkey,
    quote_vault_pk: &Pubkey,
    quote_node_bank_pk: &Pubkey,
    quote_root_bank_pk: &Pubkey,
    insurance_vault_pk: &Pubkey,
    msrm_vault_pk: &Pubkey, // send in Pubkey:default() if not using this feature
    fees_vault_pk: &Pubkey,
    lyrae_cache_ai: &Pubkey,
    dex_program_pk: &Pubkey,

    signer_nonce: u64,
    valid_interval: u64,
    quote_optimal_util: I80F48,
    quote_optimal_rate: I80F48,
    quote_max_rate: I80F48,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new(*lyrae_group_pk, false),
        AccountMeta::new_readonly(*signer_pk, false),
        AccountMeta::new_readonly(*admin_pk, true),
        AccountMeta::new_readonly(*quote_mint_pk, false),
        AccountMeta::new_readonly(*quote_vault_pk, false),
        AccountMeta::new(*quote_node_bank_pk, false),
        AccountMeta::new(*quote_root_bank_pk, false),
        AccountMeta::new_readonly(*insurance_vault_pk, false),
        AccountMeta::new_readonly(*msrm_vault_pk, false),
        AccountMeta::new_readonly(*fees_vault_pk, false),
        AccountMeta::new(*lyrae_cache_ai, false),
        AccountMeta::new_readonly(*dex_program_pk, false),
    ];

    let instr = LyraeInstruction::InitLyraeGroup {
        signer_nonce,
        valid_interval,
        quote_optimal_util,
        quote_optimal_rate,
        quote_max_rate,
    };

    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn init_lyrae_account(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    owner_pk: &Pubkey,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
    ];

    let instr = LyraeInstruction::InitLyraeAccount;
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn close_lyrae_account(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    owner_pk: &Pubkey,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
    ];

    let instr = LyraeInstruction::CloseLyraeAccount;
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn create_lyrae_account(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    owner_pk: &Pubkey,
    system_prog_pk: &Pubkey,
    payer_pk: &Pubkey,
    account_num: u64,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
        AccountMeta::new_readonly(*system_prog_pk, false),
        AccountMeta::new(*payer_pk, true),
    ];

    let instr = LyraeInstruction::CreateLyraeAccount { account_num };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn set_delegate(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    owner_pk: &Pubkey,
    delegate_pk: &Pubkey,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
        AccountMeta::new_readonly(*delegate_pk, false),
    ];

    let instr = LyraeInstruction::SetDelegate {};
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn upgrade_lyrae_account_v0_v1(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    owner_pk: &Pubkey,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
    ];

    let instr = LyraeInstruction::UpgradeLyraeAccountV0V1;
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn deposit(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    owner_pk: &Pubkey,
    lyrae_cache_pk: &Pubkey,
    root_bank_pk: &Pubkey,
    node_bank_pk: &Pubkey,
    vault_pk: &Pubkey,
    owner_token_account_pk: &Pubkey,

    quantity: u64,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
        AccountMeta::new_readonly(*lyrae_cache_pk, false),
        AccountMeta::new_readonly(*root_bank_pk, false),
        AccountMeta::new(*node_bank_pk, false),
        AccountMeta::new(*vault_pk, false),
        AccountMeta::new_readonly(spl_token::ID, false),
        AccountMeta::new(*owner_token_account_pk, false),
    ];

    let instr = LyraeInstruction::Deposit { quantity };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn add_spot_market(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    oracle_pk: &Pubkey,
    spot_market_pk: &Pubkey,
    dex_program_pk: &Pubkey,
    token_mint_pk: &Pubkey,
    node_bank_pk: &Pubkey,
    vault_pk: &Pubkey,
    root_bank_pk: &Pubkey,
    admin_pk: &Pubkey,

    maint_leverage: I80F48,
    init_leverage: I80F48,
    liquidation_fee: I80F48,
    optimal_util: I80F48,
    optimal_rate: I80F48,
    max_rate: I80F48,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new(*lyrae_group_pk, false),
        AccountMeta::new_readonly(*oracle_pk, false),
        AccountMeta::new_readonly(*spot_market_pk, false),
        AccountMeta::new_readonly(*dex_program_pk, false),
        AccountMeta::new_readonly(*token_mint_pk, false),
        AccountMeta::new(*node_bank_pk, false),
        AccountMeta::new_readonly(*vault_pk, false),
        AccountMeta::new(*root_bank_pk, false),
        AccountMeta::new_readonly(*admin_pk, true),
    ];

    let instr = LyraeInstruction::AddSpotMarket {
        maint_leverage,
        init_leverage,
        liquidation_fee,
        optimal_util,
        optimal_rate,
        max_rate,
    };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn add_perp_market(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    oracle_pk: &Pubkey,
    perp_market_pk: &Pubkey,
    event_queue_pk: &Pubkey,
    bids_pk: &Pubkey,
    asks_pk: &Pubkey,
    lyr_vault_pk: &Pubkey,
    admin_pk: &Pubkey,

    maint_leverage: I80F48,
    init_leverage: I80F48,
    liquidation_fee: I80F48,
    maker_fee: I80F48,
    taker_fee: I80F48,
    base_lot_size: i64,
    quote_lot_size: i64,
    rate: I80F48,
    max_depth_bps: I80F48,
    target_period_length: u64,
    lyr_per_period: u64,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new(*lyrae_group_pk, false),
        AccountMeta::new(*oracle_pk, false),
        AccountMeta::new(*perp_market_pk, false),
        AccountMeta::new(*event_queue_pk, false),
        AccountMeta::new(*bids_pk, false),
        AccountMeta::new(*asks_pk, false),
        AccountMeta::new_readonly(*lyr_vault_pk, false),
        AccountMeta::new_readonly(*admin_pk, true),
    ];

    let instr = LyraeInstruction::AddPerpMarket {
        maint_leverage,
        init_leverage,
        liquidation_fee,
        maker_fee,
        taker_fee,
        base_lot_size,
        quote_lot_size,
        rate,
        max_depth_bps,
        target_period_length,
        lyr_per_period,
        exp: 2, // TODO add this to function signature
    };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn place_perp_order(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    owner_pk: &Pubkey,
    lyrae_cache_pk: &Pubkey,
    perp_market_pk: &Pubkey,
    bids_pk: &Pubkey,
    asks_pk: &Pubkey,
    event_queue_pk: &Pubkey,
    referrer_lyrae_account_pk: Option<&Pubkey>,
    open_orders_pks: &[Pubkey; MAX_PAIRS],
    side: Side,
    price: i64,
    quantity: i64,
    client_order_id: u64,
    order_type: OrderType,
    reduce_only: bool,
) -> Result<Instruction, ProgramError> {
    let mut accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
        AccountMeta::new_readonly(*lyrae_cache_pk, false),
        AccountMeta::new(*perp_market_pk, false),
        AccountMeta::new(*bids_pk, false),
        AccountMeta::new(*asks_pk, false),
        AccountMeta::new(*event_queue_pk, false),
    ];
    accounts.extend(
        open_orders_pks
            .iter()
            .map(|pk| AccountMeta::new_readonly(*pk, false)),
    );
    if let Some(referrer_lyrae_account_pk) = referrer_lyrae_account_pk {
        accounts.push(AccountMeta::new(*referrer_lyrae_account_pk, false));
    }

    let instr = LyraeInstruction::PlacePerpOrder {
        side,
        price,
        quantity,
        client_order_id,
        order_type,
        reduce_only,
    };
    let data = instr.pack();

    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn cancel_perp_order_by_client_id(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,   // read
    lyrae_account_pk: &Pubkey, // write
    owner_pk: &Pubkey,         // read, signer
    perp_market_pk: &Pubkey,   // write
    bids_pk: &Pubkey,          // write
    asks_pk: &Pubkey,          // write
    client_order_id: u64,
    invalid_id_ok: bool,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
        AccountMeta::new(*perp_market_pk, false),
        AccountMeta::new(*bids_pk, false),
        AccountMeta::new(*asks_pk, false),
    ];
    let instr = LyraeInstruction::CancelPerpOrderByClientId {
        client_order_id,
        invalid_id_ok,
    };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn cancel_perp_order(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,   // read
    lyrae_account_pk: &Pubkey, // write
    owner_pk: &Pubkey,         // read, signer
    perp_market_pk: &Pubkey,   // write
    bids_pk: &Pubkey,          // write
    asks_pk: &Pubkey,          // write
    order_id: i128,
    invalid_id_ok: bool,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
        AccountMeta::new(*perp_market_pk, false),
        AccountMeta::new(*bids_pk, false),
        AccountMeta::new(*asks_pk, false),
    ];
    let instr = LyraeInstruction::CancelPerpOrder {
        order_id,
        invalid_id_ok,
    };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn cancel_all_perp_orders(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,   // read
    lyrae_account_pk: &Pubkey, // write
    owner_pk: &Pubkey,         // read, signer
    perp_market_pk: &Pubkey,   // write
    bids_pk: &Pubkey,          // write
    asks_pk: &Pubkey,          // write
    limit: u8,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
        AccountMeta::new(*perp_market_pk, false),
        AccountMeta::new(*bids_pk, false),
        AccountMeta::new(*asks_pk, false),
    ];
    let instr = LyraeInstruction::CancelAllPerpOrders { limit };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn cancel_perp_orders_side(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,   // read
    lyrae_account_pk: &Pubkey, // write
    owner_pk: &Pubkey,         // read, signer
    perp_market_pk: &Pubkey,   // write
    bids_pk: &Pubkey,          // write
    asks_pk: &Pubkey,          // write
    side: Side,
    limit: u8,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
        AccountMeta::new(*perp_market_pk, false),
        AccountMeta::new(*bids_pk, false),
        AccountMeta::new(*asks_pk, false),
    ];
    let instr = LyraeInstruction::CancelPerpOrdersSide { side, limit };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn force_cancel_perp_orders(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,         // read
    lyrae_cache_pk: &Pubkey,         // read
    perp_market_pk: &Pubkey,         // read
    bids_pk: &Pubkey,                // write
    asks_pk: &Pubkey,                // write
    liqee_lyrae_account_pk: &Pubkey, // write
    open_orders_pks: &[Pubkey],      // read
    limit: u8,
) -> Result<Instruction, ProgramError> {
    let mut accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new_readonly(*lyrae_cache_pk, false),
        AccountMeta::new_readonly(*perp_market_pk, false),
        AccountMeta::new(*bids_pk, false),
        AccountMeta::new(*asks_pk, false),
        AccountMeta::new(*liqee_lyrae_account_pk, false),
    ];
    accounts.extend(
        open_orders_pks
            .iter()
            .map(|pk| AccountMeta::new_readonly(*pk, false)),
    );
    let instr = LyraeInstruction::ForceCancelPerpOrders { limit };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn init_advanced_orders(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,     // read
    lyrae_account_pk: &Pubkey,   // write
    owner_pk: &Pubkey,           // write & signer
    advanced_orders_pk: &Pubkey, // write
    system_prog_pk: &Pubkey,     // read
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new(*owner_pk, true),
        AccountMeta::new(*advanced_orders_pk, false),
        AccountMeta::new_readonly(*system_prog_pk, false),
    ];
    let instr = LyraeInstruction::InitAdvancedOrders {};
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn close_advanced_orders(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    advanced_orders_pk: &Pubkey,
    owner_pk: &Pubkey,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new(*owner_pk, true),
        AccountMeta::new(*advanced_orders_pk, false),
    ];

    let instr = LyraeInstruction::CloseAdvancedOrders;
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn add_perp_trigger_order(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,     // read
    lyrae_account_pk: &Pubkey,   // read
    owner_pk: &Pubkey,           // write & signer
    advanced_orders_pk: &Pubkey, // write
    lyrae_cache_pk: &Pubkey,     // read
    perp_market_pk: &Pubkey,     // read
    system_prog_pk: &Pubkey,     // read
    order_type: OrderType,
    side: Side,
    trigger_condition: TriggerCondition,
    reduce_only: bool,
    client_order_id: u64,
    price: i64,
    quantity: i64,
    trigger_price: I80F48,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new_readonly(*lyrae_account_pk, false),
        AccountMeta::new(*owner_pk, true),
        AccountMeta::new(*advanced_orders_pk, false),
        AccountMeta::new_readonly(*lyrae_cache_pk, false),
        AccountMeta::new_readonly(*perp_market_pk, false),
        AccountMeta::new_readonly(*system_prog_pk, false),
    ];
    let instr = LyraeInstruction::AddPerpTriggerOrder {
        order_type,
        side,
        trigger_condition,
        reduce_only,
        client_order_id,
        price,
        quantity,
        trigger_price,
    };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn remove_advanced_order(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,     // read
    lyrae_account_pk: &Pubkey,   // read
    owner_pk: &Pubkey,           // write & signer
    advanced_orders_pk: &Pubkey, // write
    system_prog_pk: &Pubkey,     // read
    order_index: u8,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new_readonly(*lyrae_account_pk, false),
        AccountMeta::new(*owner_pk, true),
        AccountMeta::new(*advanced_orders_pk, false),
        AccountMeta::new_readonly(*system_prog_pk, false),
    ];
    let instr = LyraeInstruction::RemoveAdvancedOrder { order_index };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn execute_perp_trigger_order(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,     // read
    lyrae_account_pk: &Pubkey,   // write
    advanced_orders_pk: &Pubkey, // write
    agent_pk: &Pubkey,           // write & signer
    lyrae_cache_pk: &Pubkey,     // read
    perp_market_pk: &Pubkey,     // write
    bids_pk: &Pubkey,            // write
    asks_pk: &Pubkey,            // write
    event_queue_pk: &Pubkey,     // write
    order_index: u8,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new(*advanced_orders_pk, false),
        AccountMeta::new(*agent_pk, true),
        AccountMeta::new_readonly(*lyrae_cache_pk, false),
        AccountMeta::new(*perp_market_pk, false),
        AccountMeta::new(*bids_pk, false),
        AccountMeta::new(*asks_pk, false),
        AccountMeta::new(*event_queue_pk, false),
    ];
    let instr = LyraeInstruction::ExecutePerpTriggerOrder { order_index };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn consume_events(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,      // read
    lyrae_cache_pk: &Pubkey,      // read
    perp_market_pk: &Pubkey,      // read
    event_queue_pk: &Pubkey,      // write
    lyrae_acc_pks: &mut [Pubkey], // write
    limit: usize,
) -> Result<Instruction, ProgramError> {
    let fixed_accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new_readonly(*lyrae_cache_pk, false),
        AccountMeta::new(*perp_market_pk, false),
        AccountMeta::new(*event_queue_pk, false),
    ];
    lyrae_acc_pks.sort();
    let lyrae_accounts = lyrae_acc_pks
        .into_iter()
        .map(|pk| AccountMeta::new(*pk, false));
    let accounts = fixed_accounts.into_iter().chain(lyrae_accounts).collect();
    let instr = LyraeInstruction::ConsumeEvents { limit };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn settle_pnl(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,     // read
    lyrae_account_a_pk: &Pubkey, // write
    lyrae_account_b_pk: &Pubkey, // write
    lyrae_cache_pk: &Pubkey,     // read
    root_bank_pk: &Pubkey,       // read
    node_bank_pk: &Pubkey,       // write
    market_index: usize,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_a_pk, false),
        AccountMeta::new(*lyrae_account_b_pk, false),
        AccountMeta::new_readonly(*lyrae_cache_pk, false),
        AccountMeta::new_readonly(*root_bank_pk, false),
        AccountMeta::new(*node_bank_pk, false),
    ];
    let instr = LyraeInstruction::SettlePnl { market_index };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn update_funding(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey, // read
    lyrae_cache_pk: &Pubkey, // write
    perp_market_pk: &Pubkey, // write
    bids_pk: &Pubkey,        // read
    asks_pk: &Pubkey,        // read
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_cache_pk, false),
        AccountMeta::new(*perp_market_pk, false),
        AccountMeta::new_readonly(*bids_pk, false),
        AccountMeta::new_readonly(*asks_pk, false),
    ];
    let instr = LyraeInstruction::UpdateFunding {};
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn withdraw(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    owner_pk: &Pubkey,
    lyrae_cache_pk: &Pubkey,
    root_bank_pk: &Pubkey,
    node_bank_pk: &Pubkey,
    vault_pk: &Pubkey,
    token_account_pk: &Pubkey,
    signer_pk: &Pubkey,
    open_orders_pks: &[Pubkey],

    quantity: u64,
    allow_borrow: bool,
) -> Result<Instruction, ProgramError> {
    let mut accounts = vec![
        AccountMeta::new(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
        AccountMeta::new_readonly(*lyrae_cache_pk, false),
        AccountMeta::new_readonly(*root_bank_pk, false),
        AccountMeta::new(*node_bank_pk, false),
        AccountMeta::new(*vault_pk, false),
        AccountMeta::new(*token_account_pk, false),
        AccountMeta::new_readonly(*signer_pk, false),
        AccountMeta::new_readonly(spl_token::ID, false),
    ];

    accounts.extend(
        open_orders_pks
            .iter()
            .map(|pk| AccountMeta::new_readonly(*pk, false)),
    );

    let instr = LyraeInstruction::Withdraw {
        quantity,
        allow_borrow,
    };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn borrow(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    lyrae_cache_pk: &Pubkey,
    owner_pk: &Pubkey,
    root_bank_pk: &Pubkey,
    node_bank_pk: &Pubkey,
    open_orders_pks: &[Pubkey],

    quantity: u64,
) -> Result<Instruction, ProgramError> {
    let mut accounts = vec![
        AccountMeta::new(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
        AccountMeta::new_readonly(*lyrae_cache_pk, false),
        AccountMeta::new_readonly(*root_bank_pk, false),
        AccountMeta::new(*node_bank_pk, false),
    ];

    accounts.extend(
        open_orders_pks
            .iter()
            .map(|pk| AccountMeta::new(*pk, false)),
    );

    let instr = LyraeInstruction::Borrow { quantity };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn cache_prices(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_cache_pk: &Pubkey,
    oracle_pks: &[Pubkey],
) -> Result<Instruction, ProgramError> {
    let mut accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_cache_pk, false),
    ];
    accounts.extend(
        oracle_pks
            .iter()
            .map(|pk| AccountMeta::new_readonly(*pk, false)),
    );
    let instr = LyraeInstruction::CachePrices;
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn cache_root_banks(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_cache_pk: &Pubkey,
    root_bank_pks: &[Pubkey],
) -> Result<Instruction, ProgramError> {
    let mut accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_cache_pk, false),
    ];
    accounts.extend(
        root_bank_pks
            .iter()
            .map(|pk| AccountMeta::new_readonly(*pk, false)),
    );
    let instr = LyraeInstruction::CacheRootBanks;
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn cache_perp_markets(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_cache_pk: &Pubkey,
    perp_market_pks: &[Pubkey],
) -> Result<Instruction, ProgramError> {
    let mut accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_cache_pk, false),
    ];
    accounts.extend(
        perp_market_pks
            .iter()
            .map(|pk| AccountMeta::new_readonly(*pk, false)),
    );
    let instr = LyraeInstruction::CachePerpMarkets;
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn init_spot_open_orders(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    owner_pk: &Pubkey,
    dex_prog_pk: &Pubkey,
    open_orders_pk: &Pubkey,
    spot_market_pk: &Pubkey,
    signer_pk: &Pubkey,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
        AccountMeta::new_readonly(*dex_prog_pk, false),
        AccountMeta::new(*open_orders_pk, false),
        AccountMeta::new_readonly(*spot_market_pk, false),
        AccountMeta::new_readonly(*signer_pk, false),
        AccountMeta::new_readonly(solana_program::sysvar::rent::ID, false),
    ];

    let instr = LyraeInstruction::InitSpotOpenOrders;
    let data = instr.pack();

    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn create_spot_open_orders(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    owner_pk: &Pubkey,
    dex_prog_pk: &Pubkey,
    open_orders_pk: &Pubkey,
    spot_market_pk: &Pubkey,
    signer_pk: &Pubkey,
    payer_pk: &Pubkey,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
        AccountMeta::new_readonly(*dex_prog_pk, false),
        AccountMeta::new(*open_orders_pk, false),
        AccountMeta::new_readonly(*spot_market_pk, false),
        AccountMeta::new_readonly(*signer_pk, false),
        AccountMeta::new_readonly(solana_program::system_program::ID, false),
        AccountMeta::new(*payer_pk, true),
    ];

    let instr = LyraeInstruction::CreateSpotOpenOrders;
    let data = instr.pack();

    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn close_spot_open_orders(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    owner_pk: &Pubkey,
    dex_prog_pk: &Pubkey,
    open_orders_pk: &Pubkey,
    spot_market_pk: &Pubkey,
    signer_pk: &Pubkey,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new(*owner_pk, true),
        AccountMeta::new_readonly(*dex_prog_pk, false),
        AccountMeta::new(*open_orders_pk, false),
        AccountMeta::new_readonly(*spot_market_pk, false),
        AccountMeta::new_readonly(*signer_pk, false),
    ];

    let instr = LyraeInstruction::CloseSpotOpenOrders;
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn place_spot_order(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    owner_pk: &Pubkey,
    lyrae_cache_pk: &Pubkey,
    dex_prog_pk: &Pubkey,
    spot_market_pk: &Pubkey,
    bids_pk: &Pubkey,
    asks_pk: &Pubkey,
    dex_request_queue_pk: &Pubkey,
    dex_event_queue_pk: &Pubkey,
    dex_base_pk: &Pubkey,
    dex_quote_pk: &Pubkey,
    base_root_bank_pk: &Pubkey,
    base_node_bank_pk: &Pubkey,
    base_vault_pk: &Pubkey,
    quote_root_bank_pk: &Pubkey,
    quote_node_bank_pk: &Pubkey,
    quote_vault_pk: &Pubkey,
    signer_pk: &Pubkey,
    dex_signer_pk: &Pubkey,
    msrm_or_srm_vault_pk: &Pubkey,
    open_orders_pks: &[Pubkey],

    market_index: usize, // used to determine which of the open orders accounts should be passed in write
    order: serum_dex::instruction::NewOrderInstructionV3,
) -> Result<Instruction, ProgramError> {
    let mut accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
        AccountMeta::new_readonly(*lyrae_cache_pk, false),
        AccountMeta::new_readonly(*dex_prog_pk, false),
        AccountMeta::new(*spot_market_pk, false),
        AccountMeta::new(*bids_pk, false),
        AccountMeta::new(*asks_pk, false),
        AccountMeta::new(*dex_request_queue_pk, false),
        AccountMeta::new(*dex_event_queue_pk, false),
        AccountMeta::new(*dex_base_pk, false),
        AccountMeta::new(*dex_quote_pk, false),
        AccountMeta::new_readonly(*base_root_bank_pk, false),
        AccountMeta::new(*base_node_bank_pk, false),
        AccountMeta::new(*base_vault_pk, false),
        AccountMeta::new_readonly(*quote_root_bank_pk, false),
        AccountMeta::new(*quote_node_bank_pk, false),
        AccountMeta::new(*quote_vault_pk, false),
        AccountMeta::new_readonly(spl_token::ID, false),
        AccountMeta::new_readonly(*signer_pk, false),
        AccountMeta::new_readonly(solana_program::sysvar::rent::ID, false),
        AccountMeta::new_readonly(*dex_signer_pk, false),
        AccountMeta::new_readonly(*msrm_or_srm_vault_pk, false),
    ];

    accounts.extend(open_orders_pks.iter().enumerate().map(|(i, pk)| {
        if i == market_index {
            AccountMeta::new(*pk, false)
        } else {
            AccountMeta::new_readonly(*pk, false)
        }
    }));

    let instr = LyraeInstruction::PlaceSpotOrder { order };
    let data = instr.pack();

    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn settle_funds(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_cache_pk: &Pubkey,
    owner_pk: &Pubkey,
    lyrae_account_pk: &Pubkey,
    dex_prog_pk: &Pubkey,
    spot_market_pk: &Pubkey,
    open_orders_pk: &Pubkey,
    signer_pk: &Pubkey,
    dex_base_pk: &Pubkey,
    dex_quote_pk: &Pubkey,
    base_root_bank_pk: &Pubkey,
    base_node_bank_pk: &Pubkey,
    quote_root_bank_pk: &Pubkey,
    quote_node_bank_pk: &Pubkey,
    base_vault_pk: &Pubkey,
    quote_vault_pk: &Pubkey,
    dex_signer_pk: &Pubkey,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new_readonly(*lyrae_cache_pk, false),
        AccountMeta::new_readonly(*owner_pk, true),
        AccountMeta::new(*lyrae_account_pk, false),
        AccountMeta::new_readonly(*dex_prog_pk, false),
        AccountMeta::new(*spot_market_pk, false),
        AccountMeta::new(*open_orders_pk, false),
        AccountMeta::new_readonly(*signer_pk, false),
        AccountMeta::new(*dex_base_pk, false),
        AccountMeta::new(*dex_quote_pk, false),
        AccountMeta::new_readonly(*base_root_bank_pk, false),
        AccountMeta::new(*base_node_bank_pk, false),
        AccountMeta::new_readonly(*quote_root_bank_pk, false),
        AccountMeta::new(*quote_node_bank_pk, false),
        AccountMeta::new(*base_vault_pk, false),
        AccountMeta::new(*quote_vault_pk, false),
        AccountMeta::new_readonly(*dex_signer_pk, false),
        AccountMeta::new_readonly(spl_token::ID, false),
    ];

    let instr = LyraeInstruction::SettleFunds;
    let data = instr.pack();

    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn add_oracle(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    oracle_pk: &Pubkey,
    admin_pk: &Pubkey,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new(*lyrae_group_pk, false),
        AccountMeta::new(*oracle_pk, false),
        AccountMeta::new_readonly(*admin_pk, true),
    ];

    let instr = LyraeInstruction::AddOracle;
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn update_root_bank(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_cache_pk: &Pubkey,
    root_bank_pk: &Pubkey,
    node_bank_pks: &[Pubkey],
) -> Result<Instruction, ProgramError> {
    let mut accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*lyrae_cache_pk, false),
        AccountMeta::new(*root_bank_pk, false),
    ];

    accounts.extend(
        node_bank_pks
            .iter()
            .map(|pk| AccountMeta::new_readonly(*pk, false)),
    );

    let instr = LyraeInstruction::UpdateRootBank;
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn set_oracle(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    oracle_pk: &Pubkey,
    admin_pk: &Pubkey,
    price: I80F48,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new(*oracle_pk, false),
        AccountMeta::new_readonly(*admin_pk, true),
    ];

    let instr = LyraeInstruction::SetOracle { price };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn liquidate_token_and_token(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    lyrae_cache_pk: &Pubkey,
    liqee_lyrae_account_pk: &Pubkey,
    liqor_lyrae_account_pk: &Pubkey,
    liqor_pk: &Pubkey,
    asset_root_bank_pk: &Pubkey,
    asset_node_bank_pk: &Pubkey,
    liab_root_bank_pk: &Pubkey,
    liab_node_bank_pk: &Pubkey,
    liqee_open_orders_pks: &[Pubkey],
    liqor_open_orders_pks: &[Pubkey],
    max_liab_transfer: I80F48,
) -> Result<Instruction, ProgramError> {
    let mut accounts = vec![
        AccountMeta::new_readonly(*lyrae_group_pk, false),
        AccountMeta::new_readonly(*lyrae_cache_pk, false),
        AccountMeta::new(*liqee_lyrae_account_pk, false),
        AccountMeta::new(*liqor_lyrae_account_pk, false),
        AccountMeta::new_readonly(*liqor_pk, true),
        AccountMeta::new_readonly(*asset_root_bank_pk, false),
        AccountMeta::new(*asset_node_bank_pk, false),
        AccountMeta::new_readonly(*liab_root_bank_pk, false),
        AccountMeta::new(*liab_node_bank_pk, false),
    ];

    accounts.extend(
        liqee_open_orders_pks
            .iter()
            .map(|pk| AccountMeta::new_readonly(*pk, false)),
    );
    accounts.extend(
        liqor_open_orders_pks
            .iter()
            .map(|pk| AccountMeta::new_readonly(*pk, false)),
    );

    let instr = LyraeInstruction::LiquidateTokenAndToken { max_liab_transfer };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

pub fn change_spot_market_params(
    program_id: &Pubkey,
    lyrae_group_pk: &Pubkey,
    spot_market_pk: &Pubkey,
    root_bank_pk: &Pubkey,
    admin_pk: &Pubkey,
    maint_leverage: Option<I80F48>,
    init_leverage: Option<I80F48>,
    liquidation_fee: Option<I80F48>,
    optimal_util: Option<I80F48>,
    optimal_rate: Option<I80F48>,
    max_rate: Option<I80F48>,
    version: Option<u8>,
) -> Result<Instruction, ProgramError> {
    let accounts = vec![
        AccountMeta::new(*lyrae_group_pk, false),
        AccountMeta::new(*spot_market_pk, false),
        AccountMeta::new(*root_bank_pk, false),
        AccountMeta::new_readonly(*admin_pk, true),
    ];

    let instr = LyraeInstruction::ChangeSpotMarketParams {
        maint_leverage,
        init_leverage,
        liquidation_fee,
        optimal_util,
        optimal_rate,
        max_rate,
        version,
    };
    let data = instr.pack();
    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

/// Serialize Option<T> as (bool, T). This gives the binary representation
/// a fixed width, instead of it becoming one byte for None.
fn serialize_option_fixed_width<S: serde::Serializer, T: Sized + Default + Serialize>(
    opt: &Option<T>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeTuple;
    let mut tup = serializer.serialize_tuple(2)?;
    match opt {
        Some(value) => {
            tup.serialize_element(&true)?;
            tup.serialize_element(&value)?;
        }
        None => {
            tup.serialize_element(&false)?;
            tup.serialize_element(&T::default())?;
        }
    };
    tup.end()
}

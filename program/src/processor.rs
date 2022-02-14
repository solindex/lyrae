use std::cell::RefMut;
use std::cmp::min;
use std::convert::{identity, TryFrom};
use std::mem::size_of;
use std::vec;

use anchor_lang::prelude::emit;
use arrayref::{array_ref, array_refs};
use bytemuck::{cast, cast_mut, cast_ref};
use fixed::types::I80F48;
use serum_dex::instruction::NewOrderInstructionV3;
use serum_dex::state::ToAlignedBytes;
use solana_program::account_info::AccountInfo;
use solana_program::clock::Clock;
use solana_program::entrypoint::ProgramResult;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::msg;
use solana_program::program_error::ProgramError;
use solana_program::program_pack::{IsInitialized, Pack};
use solana_program::pubkey::Pubkey;
use solana_program::rent::Rent;
use solana_program::sysvar::Sysvar;
use spl_token::state::{Account, Mint};
use switchboard_program::FastRoundResultAccountData;

use lyrae_common::Loadable;
use lyrae_logs::{
    lyrae_emit, CachePerpMarketsLog, CachePricesLog, CacheRootBanksLog, CancelAllPerpOrdersLog,
    DepositLog, LiquidatePerpMarketLog, LiquidateTokenAndPerpLog, LiquidateTokenAndTokenLog,
    LyrAccrualLog, OpenOrdersBalanceLog, PerpBankruptcyLog, RedeemLyrLog, SettleFeesLog,
    SettlePnlLog, TokenBalanceLog, TokenBankruptcyLog, UpdateFundingLog, UpdateRootBankLog,
    WithdrawLog,
};

use crate::error::{check_assert, LyraeError, LyraeErrorCode, LyraeResult, SourceFileId};
use crate::ids::{msrm_token, srm_token};
use crate::instruction::LyraeInstruction;
use crate::matching::{Book, BookSide, OrderType, Side};
#[cfg(not(feature = "devnet"))]
use crate::oracle::PriceStatus;
use crate::oracle::{determine_oracle_type, OracleType, Price, StubOracle};
use crate::queue::{EventQueue, EventType, FillEvent, LiquidateEvent, OutEvent};
#[cfg(not(feature = "devnet"))]
use crate::state::PYTH_CONF_FILTER;
use crate::state::{
    check_open_orders, load_asks_mut, load_bids_mut, load_market_state, load_open_orders,
    load_open_orders_accounts, AdvancedOrderType, AdvancedOrders, AssetType, DataType, HealthCache,
    HealthType, LyraeAccount, LyraeCache, LyraeGroup, MetaData, NodeBank, PerpMarket,
    PerpMarketCache, PerpMarketInfo, PerpTriggerOrder, PriceCache, ReferrerIdRecord,
    ReferrerMemory, RootBank, RootBankCache, SpotMarketInfo, TokenInfo, TriggerCondition,
    UserActiveAssets, ADVANCED_ORDER_FEE, FREE_ORDER_SLOT, INFO_LEN, MAX_ADVANCED_ORDERS,
    MAX_NODE_BANKS, MAX_PAIRS, MAX_PERP_OPEN_ORDERS, MAX_TOKENS, NEG_ONE_I80F48, ONE_I80F48,
    QUOTE_INDEX, ZERO_I80F48,
};
use crate::utils::{emit_perp_balances, gen_signer_key, gen_signer_seeds};

declare_check_assert_macros!(SourceFileId::Processor);

pub struct Processor {}

impl Processor {
    #[inline(never)]
    fn init_lyrae_group(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        signer_nonce: u64,
        valid_interval: u64,
        quote_optimal_util: I80F48,
        quote_optimal_rate: I80F48,
        quote_max_rate: I80F48,
    ) -> LyraeResult<()> {
        const NUM_FIXED: usize = 12;
        let accounts = array_ref![accounts, 0, NUM_FIXED];

        let [
        lyrae_group_ai,     // write
            signer_ai,          // read
            admin_ai,           // read
            quote_mint_ai,      // read
            quote_vault_ai,     // read
            quote_node_bank_ai, // write
            quote_root_bank_ai, // write
            insurance_vault_ai, // read
            msrm_vault_ai,      // read
            fees_vault_ai,      // read
        lyrae_cache_ai,     // write
            dex_prog_ai         // read
        ] = accounts;
        check_eq!(lyrae_group_ai.owner, program_id, LyraeErrorCode::InvalidGroupOwner)?;
        let rent = Rent::get()?;
        check!(
            rent.is_exempt(lyrae_group_ai.lamports(), size_of::<LyraeGroup>()),
            LyraeErrorCode::GroupNotRentExempt
        )?;
        let mut lyrae_group: RefMut<LyraeGroup> = LyraeGroup::load_mut(lyrae_group_ai)?;
        check!(!lyrae_group.meta_data.is_initialized, LyraeErrorCode::Default)?;

        check!(
            gen_signer_key(signer_nonce, lyrae_group_ai.key, program_id)? == *signer_ai.key,
            LyraeErrorCode::InvalidSignerKey
        )?;
        lyrae_group.signer_nonce = signer_nonce;
        lyrae_group.signer_key = *signer_ai.key;
        lyrae_group.valid_interval = valid_interval;
        lyrae_group.dex_program_id = *dex_prog_ai.key;

        // TODO OPT make PDA
        let insurance_vault = Account::unpack(&insurance_vault_ai.try_borrow_data()?)?;
        check!(insurance_vault.is_initialized(), LyraeErrorCode::InvalidVault)?;
        check!(insurance_vault.delegate.is_none(), LyraeErrorCode::InvalidVault)?;
        check!(insurance_vault.close_authority.is_none(), LyraeErrorCode::InvalidVault)?;
        check_eq!(insurance_vault.owner, lyrae_group.signer_key, LyraeErrorCode::InvalidVault)?;
        check_eq!(&insurance_vault.mint, quote_mint_ai.key, LyraeErrorCode::InvalidVault)?;
        check_eq!(insurance_vault_ai.owner, &spl_token::ID, LyraeErrorCode::InvalidVault)?;
        lyrae_group.insurance_vault = *insurance_vault_ai.key;

        let fees_vault = Account::unpack(&fees_vault_ai.try_borrow_data()?)?;
        check!(fees_vault.is_initialized(), LyraeErrorCode::Default)?;
        check!(fees_vault.delegate.is_none(), LyraeErrorCode::InvalidVault)?;
        check!(fees_vault.close_authority.is_none(), LyraeErrorCode::InvalidVault)?;
        check_eq!(&fees_vault.mint, quote_mint_ai.key, LyraeErrorCode::InvalidVault)?;
        check_eq!(fees_vault_ai.owner, &spl_token::ID, LyraeErrorCode::InvalidVault)?;
        lyrae_group.fees_vault = *fees_vault_ai.key;

        // TODO OPT make this a PDA
        if msrm_vault_ai.key != &Pubkey::default() {
            let msrm_vault = Account::unpack(&msrm_vault_ai.try_borrow_data()?)?;
            check!(msrm_vault.is_initialized(), LyraeErrorCode::InvalidVault)?;
            check!(msrm_vault.delegate.is_none(), LyraeErrorCode::InvalidVault)?;
            check!(msrm_vault.close_authority.is_none(), LyraeErrorCode::InvalidVault)?;
            check_eq!(msrm_vault.owner, lyrae_group.signer_key, LyraeErrorCode::InvalidVault)?;
            check_eq!(&msrm_vault.mint, &msrm_token::ID, LyraeErrorCode::InvalidVault)?;
            check_eq!(msrm_vault_ai.owner, &spl_token::ID, LyraeErrorCode::InvalidVault)?;
            lyrae_group.msrm_vault = *msrm_vault_ai.key;
        }

        let _root_bank = init_root_bank(
            program_id,
            &lyrae_group,
            quote_mint_ai,
            quote_vault_ai,
            quote_root_bank_ai,
            quote_node_bank_ai,
            &rent,
            quote_optimal_util,
            quote_optimal_rate,
            quote_max_rate,
        )?;
        let mint = Mint::unpack(&quote_mint_ai.try_borrow_data()?)?;
        lyrae_group.tokens[QUOTE_INDEX] = TokenInfo {
            mint: *quote_mint_ai.key,
            root_bank: *quote_root_bank_ai.key,
            decimals: mint.decimals,
            padding: [0u8; 7],
        };

        check!(admin_ai.is_signer, LyraeErrorCode::Default)?;
        lyrae_group.admin = *admin_ai.key;

        lyrae_group.meta_data = MetaData::new(DataType::LyraeGroup, 0, true);

        // init LyraeCache
        check!(
            rent.is_exempt(lyrae_cache_ai.lamports(), size_of::<LyraeCache>()),
            LyraeErrorCode::AccountNotRentExempt
        )?;
        let mut lyrae_cache = LyraeCache::load_mut(&lyrae_cache_ai)?;
        check!(!lyrae_cache.meta_data.is_initialized, LyraeErrorCode::Default)?;
        lyrae_cache.meta_data = MetaData::new(DataType::LyraeCache, 0, true);
        lyrae_group.lyrae_cache = *lyrae_cache_ai.key;
        lyrae_group.max_lyrae_accounts = 100_000;

        // check size
        Ok(())
    }

    #[allow(unused)]
    fn remove_spot_market(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        todo!()
    }
    #[inline(never)]
    /// DEPRECATED - if you use this instruction after v3.3.0 you will not be able to close your LyraeAccount
    fn init_lyrae_account(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 3;
        let accounts = array_ref![accounts, 0, NUM_FIXED];

        let [
        lyrae_group_ai,     // read
        lyrae_account_ai,   // write
            owner_ai            // read, signer
        ] = accounts;

        let rent = Rent::get()?;
        check!(
            rent.is_exempt(lyrae_account_ai.lamports(), size_of::<LyraeAccount>()),
            LyraeErrorCode::AccountNotRentExempt
        )?;
        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check!(
            lyrae_group.num_lyrae_accounts < lyrae_group.max_lyrae_accounts,
            LyraeErrorCode::MaxAccountsReached
        )?;

        let mut lyrae_account: RefMut<LyraeAccount> = LyraeAccount::load_mut(lyrae_account_ai)?;
        check_eq!(&lyrae_account_ai.owner, &program_id, LyraeErrorCode::InvalidOwner)?;
        check!(!lyrae_account.meta_data.is_initialized, LyraeErrorCode::InvalidAccountState)?;

        lyrae_account.lyrae_group = *lyrae_group_ai.key;
        lyrae_account.owner = *owner_ai.key;
        lyrae_account.order_market = [FREE_ORDER_SLOT; MAX_PERP_OPEN_ORDERS];
        lyrae_account.meta_data = MetaData::new(DataType::LyraeAccount, 0, true);
        lyrae_account.not_upgradable = true;
        Ok(())
    }

    #[inline(never)]
    fn close_lyrae_account(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 3;
        let accounts = array_ref![accounts, 0, NUM_FIXED];

        let [
        lyrae_group_ai,     // write
        lyrae_account_ai,   // write
            owner_ai,           // write, signer
        ] = accounts;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, &lyrae_group_ai.key)?;
        check_eq!(&lyrae_account.owner, owner_ai.key, LyraeErrorCode::InvalidOwner)?;
        check!(owner_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        check!(lyrae_account.meta_data.version > 0, LyraeErrorCode::InvalidAccountState)?;

        // Check deposits and borrows are zero
        for i in 0..MAX_TOKENS {
            check_eq!(lyrae_account.deposits[i], ZERO_I80F48, LyraeErrorCode::InvalidAccountState)?;
            check_eq!(lyrae_account.borrows[i], ZERO_I80F48, LyraeErrorCode::InvalidAccountState)?;
        }
        // Check no perp positions or orders
        for perp_account in lyrae_account.perp_accounts.iter() {
            check_eq!(perp_account.base_position, 0, LyraeErrorCode::InvalidAccountState)?;
            check_eq!(
                perp_account.quote_position,
                ZERO_I80F48,
                LyraeErrorCode::InvalidAccountState
            )?;
            check!(perp_account.lyr_accrued == 0, LyraeErrorCode::InvalidAccountState)?;
            check!(perp_account.has_no_open_orders(), LyraeErrorCode::InvalidAccountState)?;
        }
        // Check no msrm
        check_eq!(lyrae_account.msrm_amount, 0, LyraeErrorCode::InvalidAccountState)?;
        // Check not being liquidated/bankrupt
        check!(!lyrae_account.being_liquidated, LyraeErrorCode::BeingLiquidated)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;

        // Check open orders accounts closed
        for open_orders_key in lyrae_account.spot_open_orders.iter() {
            check_eq!(open_orders_key, &Pubkey::default(), LyraeErrorCode::InvalidAccountState)?;
        }
        // Check advanced orders account closed
        check_eq!(
            &lyrae_account.advanced_orders_key,
            &Pubkey::default(),
            LyraeErrorCode::InvalidAccountState
        )?;

        // Transfer lamports to owner
        program_transfer_lamports(lyrae_account_ai, owner_ai, lyrae_account_ai.lamports())?;

        let mut lyrae_group = LyraeGroup::load_mut_checked(lyrae_group_ai, program_id)?;
        lyrae_group.num_lyrae_accounts = lyrae_group.num_lyrae_accounts.checked_sub(1).unwrap();

        // Prevent account being loaded by program and zero all unchecked data
        lyrae_account.meta_data.is_initialized = false;
        lyrae_account.lyrae_group = Pubkey::default();
        lyrae_account.owner = Pubkey::default();
        lyrae_account.delegate = Pubkey::default();
        lyrae_account.in_margin_basket = [false; MAX_PAIRS];
        lyrae_account.info = [0; INFO_LEN];

        Ok(())
    }

    #[inline(never)]
    fn resolve_dust(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult {
        const NUM_FIXED: usize = 7;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
        lyrae_group_ai,     // read
        lyrae_account_ai,   // write
            owner_ai,           // read, signer
            dust_account_ai,    // write
            root_bank_ai,       // read
            node_bank_ai,       // write
        lyrae_cache_ai      // read
        ] = accounts;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, &lyrae_group_ai.key)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(owner_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        check!(!lyrae_account.being_liquidated, LyraeErrorCode::BeingLiquidated)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;

        let token_index = lyrae_group
            .find_root_bank_index(root_bank_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidRootBank))?;

        if lyrae_account.deposits[token_index].is_zero()
            && lyrae_account.borrows[token_index].is_zero()
        {
            // Nothing to settle. Just return
            return Ok(());
        }

        let mut dust_account =
            LyraeAccount::load_mut_checked(dust_account_ai, program_id, &lyrae_group_ai.key)?;

        // Check dust account
        let (pda_address, _bump_seed) = Pubkey::find_program_address(
            &[&lyrae_group_ai.key.as_ref(), b"DustAccount"],
            program_id,
        );
        check!(&pda_address == dust_account_ai.key, LyraeErrorCode::InvalidAccount)?;

        // Find the node_bank pubkey in root_bank, if not found error
        let root_bank = RootBank::load_checked(root_bank_ai, program_id)?;
        check!(root_bank.node_banks.contains(node_bank_ai.key), LyraeErrorCode::InvalidNodeBank)?;
        let mut node_bank = NodeBank::load_mut_checked(node_bank_ai, program_id)?;

        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        let active_assets = UserActiveAssets::new(
            &lyrae_group,
            &dust_account,
            vec![(AssetType::Token, token_index)],
        );
        let clock = Clock::get()?;
        let now_ts = clock.unix_timestamp as u64;
        lyrae_cache.check_valid(&lyrae_group, &active_assets, now_ts)?;

        // No need to check validity here because it's part of active_assets
        let root_bank_cache = &lyrae_cache.root_bank_cache[token_index];

        let borrow_amount = lyrae_account.get_native_borrow(root_bank_cache, token_index)?;
        let deposit_amount = lyrae_account.get_native_deposit(root_bank_cache, token_index)?;

        // Amount must be dust aka < 1 native spl token
        if borrow_amount > ZERO_I80F48 && borrow_amount < ONE_I80F48 {
            transfer_token_internal(
                root_bank_cache,
                &mut node_bank,
                &mut dust_account,
                &mut lyrae_account,
                dust_account_ai.key,
                lyrae_account_ai.key,
                token_index,
                borrow_amount,
            )?;

            // We know DustAccount doesn't have any open orders; but check it just in case
            check!(dust_account.num_in_margin_basket == 0, LyraeErrorCode::InvalidAccountState)?;

            // Make sure DustAccount satisfies health check only when it has taken on more borrows
            let mut health_cache = HealthCache::new(active_assets);
            let open_orders_accounts: Vec<Option<&serum_dex::state::OpenOrders>> =
                vec![None; MAX_PAIRS];
            health_cache.init_vals_with_orders_vec(
                &lyrae_group,
                &lyrae_cache,
                &dust_account,
                &open_orders_accounts,
            )?;
            let health = health_cache.get_health(&lyrae_group, HealthType::Init);
            check!(health >= ZERO_I80F48, LyraeErrorCode::InsufficientFunds)?;
        } else if deposit_amount > ZERO_I80F48 && deposit_amount < ONE_I80F48 {
            transfer_token_internal(
                root_bank_cache,
                &mut node_bank,
                &mut lyrae_account,
                &mut dust_account,
                lyrae_account_ai.key,
                dust_account_ai.key,
                token_index,
                deposit_amount,
            )?;
        }

        Ok(())
    }

    #[inline(never)]
    /// Add asset and spot market to lyrae group
    /// Initialize a root bank and add it to the lyrae group
    /// Requires a price oracle for this asset priced in quote currency
    /// Only allow admin to add to LyraeGroup
    // TODO - implement remove asset
    fn add_spot_market(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        maint_leverage: I80F48,
        init_leverage: I80F48,
        liquidation_fee: I80F48,
        optimal_util: I80F48,
        optimal_rate: I80F48,
        max_rate: I80F48,
    ) -> LyraeResult {
        check!(
            init_leverage >= ONE_I80F48 && maint_leverage > init_leverage,
            LyraeErrorCode::InvalidParam
        )?;

        const NUM_FIXED: usize = 9;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
        lyrae_group_ai, // write
            oracle_ai,      // read
            spot_market_ai, // read
            dex_program_ai, // read
            mint_ai,        // read
            node_bank_ai,   // write
            vault_ai,       // read
            root_bank_ai,   // write
            admin_ai        // read, signer
        ] = accounts;

        let mut lyrae_group = LyraeGroup::load_mut_checked(lyrae_group_ai, program_id)?;
        check!(admin_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check_eq!(admin_ai.key, &lyrae_group.admin, LyraeErrorCode::InvalidAdminKey)?;

        let market_index = lyrae_group.find_oracle_index(oracle_ai.key).ok_or(throw!())?;

        // This will catch the issue if oracle_ai.key == Pubkey::Default
        check!(market_index < lyrae_group.num_oracles, LyraeErrorCode::InvalidParam)?;

        // Make sure spot market at this index not already initialized
        check!(
            lyrae_group.spot_markets[market_index].is_empty(),
            LyraeErrorCode::InvalidAccountState
        )?;

        // Make sure token at this index not already initialized
        check!(lyrae_group.tokens[market_index].is_empty(), LyraeErrorCode::InvalidAccountState)?;

        let _root_bank = init_root_bank(
            program_id,
            &lyrae_group,
            mint_ai,
            vault_ai,
            root_bank_ai,
            node_bank_ai,
            &Rent::get()?,
            optimal_util,
            optimal_rate,
            max_rate,
        )?;

        let mint = Mint::unpack(&mint_ai.try_borrow_data()?)?;

        // If PerpMarket was added first, then decimals was set by the create_perp_market instruction.
        // Make sure the decimals is not changed
        if !lyrae_group.perp_markets[market_index].is_empty() {
            let token_info = &lyrae_group.tokens[market_index];
            check!(mint.decimals == token_info.decimals, LyraeErrorCode::InvalidParam)?;
        }

        lyrae_group.tokens[market_index] = TokenInfo {
            mint: *mint_ai.key,
            root_bank: *root_bank_ai.key,
            decimals: mint.decimals,
            padding: [0u8; 7],
        };

        let (maint_asset_weight, maint_liab_weight) = get_leverage_weights(maint_leverage);
        let (init_asset_weight, init_liab_weight) = get_leverage_weights(init_leverage);

        lyrae_group.spot_markets[market_index] = SpotMarketInfo {
            spot_market: *spot_market_ai.key,
            maint_asset_weight,
            init_asset_weight,
            maint_liab_weight,
            init_liab_weight,
            liquidation_fee,
        };

        let spot_market = load_market_state(spot_market_ai, dex_program_ai.key)?;

        check_eq!(
            identity(spot_market.coin_mint),
            mint_ai.key.to_aligned_bytes(),
            LyraeErrorCode::Default
        )?;
        check_eq!(
            identity(spot_market.pc_mint),
            lyrae_group.tokens[QUOTE_INDEX].mint.to_aligned_bytes(),
            LyraeErrorCode::Default
        )?;

        // TODO - what if quote currency is lyr, srm or msrm
        // if mint is SRM set srm_vault

        if mint_ai.key == &srm_token::ID {
            check!(lyrae_group.srm_vault == Pubkey::default(), LyraeErrorCode::Default)?;
            lyrae_group.srm_vault = *vault_ai.key;
        }
        Ok(())
    }

    #[inline(never)]
    /// Add an oracle to the LyraeGroup
    /// This must be called first before `add_spot_market` or `add_perp_market`
    /// There will never be a gap in the lyrae_group.oracles array
    fn add_oracle(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 3;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
        lyrae_group_ai, // write
            oracle_ai,      // write
            admin_ai        // read
        ] = accounts;

        let mut lyrae_group = LyraeGroup::load_mut_checked(lyrae_group_ai, program_id)?;
        check!(admin_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check_eq!(admin_ai.key, &lyrae_group.admin, LyraeErrorCode::InvalidAdminKey)?;

        let oracle_type = determine_oracle_type(oracle_ai);
        match oracle_type {
            OracleType::Pyth => {
                msg!("OracleType:Pyth"); // Do nothing really cause all that's needed is storing the pkey
            }
            OracleType::Switchboard => {
                msg!("OracleType::Switchboard");
            }
            OracleType::Stub | OracleType::Unknown => {
                msg!("OracleType: got unknown or stub");
                let rent = Rent::get()?;
                let mut oracle = StubOracle::load_and_init(oracle_ai, program_id, &rent)?;
                oracle.magic = 0x6F676E4D;
            }
        }

        let oracle_index = lyrae_group.num_oracles;
        lyrae_group.oracles[oracle_index] = *oracle_ai.key;
        lyrae_group.num_oracles += 1;

        Ok(())
    }

    #[inline(never)]
    fn set_oracle(program_id: &Pubkey, accounts: &[AccountInfo], price: I80F48) -> LyraeResult<()> {
        const NUM_FIXED: usize = 3;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
        lyrae_group_ai, // read
            oracle_ai,      // write
            admin_ai        // read
        ] = accounts;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check!(admin_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check_eq!(admin_ai.key, &lyrae_group.admin, LyraeErrorCode::InvalidAdminKey)?;
        check!(lyrae_group.find_oracle_index(oracle_ai.key).is_some(), LyraeErrorCode::Default)?;

        let oracle_type = determine_oracle_type(oracle_ai);
        check_eq!(oracle_type, OracleType::Stub, LyraeErrorCode::Default)?;

        let mut oracle = StubOracle::load_mut_checked(oracle_ai, program_id)?;
        oracle.price = price;
        let clock = Clock::get()?;
        oracle.last_update = clock.unix_timestamp as u64;
        Ok(())
    }

    #[inline(never)]
    /// DEPRECATED Initialize perp market including orderbooks and queues
    fn add_perp_market(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        maint_leverage: I80F48,
        init_leverage: I80F48,
        liquidation_fee: I80F48,
        maker_fee: I80F48,
        taker_fee: I80F48,
        base_lot_size: i64,
        quote_lot_size: i64,
        rate: I80F48, // starting rate for liquidity mining
        max_depth_bps: I80F48,
        target_period_length: u64,
        lyr_per_period: u64,
        exp: u8,
    ) -> LyraeResult<()> {
        // params check
        check!(init_leverage >= ONE_I80F48, LyraeErrorCode::InvalidParam)?;
        check!(maint_leverage > init_leverage, LyraeErrorCode::InvalidParam)?;
        check!(maker_fee + taker_fee >= ZERO_I80F48, LyraeErrorCode::InvalidParam)?;
        check!(base_lot_size.is_positive(), LyraeErrorCode::InvalidParam)?;
        check!(quote_lot_size.is_positive(), LyraeErrorCode::InvalidParam)?;
        check!(!max_depth_bps.is_negative(), LyraeErrorCode::InvalidParam)?;
        check!(!rate.is_negative(), LyraeErrorCode::InvalidParam)?;
        check!(target_period_length > 0, LyraeErrorCode::InvalidParam)?;
        check!(exp <= 8 && exp > 0, LyraeErrorCode::InvalidParam)?;

        const NUM_FIXED: usize = 8;
        let accounts = array_ref![accounts, 0, NUM_FIXED];

        let [
        lyrae_group_ai, // write
        oracle_ai,      // read
        perp_market_ai, // write
        event_queue_ai, // write
        bids_ai,        // write
        asks_ai,        // write
        lyr_vault_ai,  // read
        admin_ai        // read, signer
        ] = accounts;

        let rent = Rent::get()?; // dynamically load rent sysvar

        let mut lyrae_group = LyraeGroup::load_mut_checked(lyrae_group_ai, program_id)?;

        check!(admin_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check_eq!(admin_ai.key, &lyrae_group.admin, LyraeErrorCode::InvalidAdminKey)?;

        let market_index = lyrae_group.find_oracle_index(oracle_ai.key).ok_or(throw!())?;

        // This will catch the issue if oracle_ai.key == Pubkey::Default
        check!(market_index < lyrae_group.num_oracles, LyraeErrorCode::InvalidParam)?;

        // Make sure perp market at this index not already initialized
        check!(lyrae_group.perp_markets[market_index].is_empty(), LyraeErrorCode::InvalidParam)?;

        let (maint_asset_weight, maint_liab_weight) = get_leverage_weights(maint_leverage);
        let (init_asset_weight, init_liab_weight) = get_leverage_weights(init_leverage);

        // This means there isn't already a token and spot market in Lyrae
        // Default the decimals to 6 and only allow AddSpotMarket if it has 6 decimals
        if lyrae_group.tokens[market_index].is_empty() {
            lyrae_group.tokens[market_index].decimals = 6;
        }

        lyrae_group.perp_markets[market_index] = PerpMarketInfo {
            perp_market: *perp_market_ai.key,
            maint_asset_weight,
            init_asset_weight,
            maint_liab_weight,
            init_liab_weight,
            liquidation_fee,
            maker_fee,
            taker_fee,
            base_lot_size,
            quote_lot_size,
        };

        // Initialize the Bids
        let _bids = BookSide::load_and_init(bids_ai, program_id, DataType::Bids, &rent)?;

        // Initialize the Asks
        let _asks = BookSide::load_and_init(asks_ai, program_id, DataType::Asks, &rent)?;

        // Initialize the EventQueue
        // TODO: check that the event queue is reasonably large
        let _event_queue = EventQueue::load_and_init(event_queue_ai, program_id, &rent)?;

        // Now initialize the PerpMarket itself
        let _perp_market = PerpMarket::load_and_init(
            perp_market_ai,
            program_id,
            lyrae_group_ai,
            bids_ai,
            asks_ai,
            event_queue_ai,
            lyr_vault_ai,
            &lyrae_group,
            &rent,
            base_lot_size,
            quote_lot_size,
            rate,
            max_depth_bps,
            target_period_length,
            lyr_per_period,
            exp,
            0,
            0,
        )?;

        Ok(())
    }

    /// Create the PerpMarket and associated PDAs and initialize them.
    /// Bids, Asks and EventQueue are not PDAs. They must be created beforehand and owner assigned
    /// to Lyrae program id
    #[inline(never)]
    fn create_perp_market(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        maint_leverage: I80F48,
        init_leverage: I80F48,
        liquidation_fee: I80F48,
        maker_fee: I80F48,
        taker_fee: I80F48,
        base_lot_size: i64,
        quote_lot_size: i64,
        rate: I80F48, // starting rate for liquidity mining
        max_depth_bps: I80F48,
        target_period_length: u64,
        lyr_per_period: u64,
        exp: u8,
        version: u8,
        lm_size_shift: u8,
        base_decimals: u8,
    ) -> LyraeResult {
        // params check
        check!(init_leverage >= ONE_I80F48, LyraeErrorCode::InvalidParam)?;
        check!(maint_leverage > init_leverage, LyraeErrorCode::InvalidParam)?;
        check!(maker_fee + taker_fee >= ZERO_I80F48, LyraeErrorCode::InvalidParam)?;
        check!(base_lot_size.is_positive(), LyraeErrorCode::InvalidParam)?;
        check!(quote_lot_size.is_positive(), LyraeErrorCode::InvalidParam)?;
        check!(!max_depth_bps.is_negative(), LyraeErrorCode::InvalidParam)?;
        if version == 1 {
            check!(max_depth_bps.int() == max_depth_bps, LyraeErrorCode::InvalidParam)?;
        }
        check!(!rate.is_negative(), LyraeErrorCode::InvalidParam)?;
        check!(target_period_length > 0, LyraeErrorCode::InvalidParam)?;
        check!(exp <= 8 && exp > 0, LyraeErrorCode::InvalidParam)?;

        const NUM_FIXED: usize = 13;
        let accounts = array_ref![accounts, 0, NUM_FIXED];

        let [
        lyrae_group_ai, // write
            oracle_ai,      // read
            perp_market_ai, // write
            event_queue_ai, // write
            bids_ai,        // write
            asks_ai,        // write
        lyr_mint_ai,   // read
        lyr_vault_ai,  // write
            admin_ai,       // signer (write if admin has SOL and no data)
            signer_ai,      // write  (if admin has data and is owned by governance)
            system_prog_ai, // read
            token_prog_ai,  // read
            rent_ai         // read
        ] = accounts;
        check!(token_prog_ai.key == &spl_token::ID, LyraeErrorCode::InvalidProgramId)?;
        check!(
            system_prog_ai.key == &solana_program::system_program::id(),
            LyraeErrorCode::InvalidProgramId
        )?;
        check!(rent_ai.key == &solana_program::sysvar::rent::ID, LyraeErrorCode::InvalidAccount)?;

        let rent = Rent::get()?; // dynamically load rent sysvar

        let mut lyrae_group = LyraeGroup::load_mut_checked(lyrae_group_ai, program_id)?;
        check!(&lyrae_group.signer_key == signer_ai.key, LyraeErrorCode::InvalidSignerKey)?;
        check!(admin_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check_eq!(admin_ai.key, &lyrae_group.admin, LyraeErrorCode::InvalidAdminKey)?;

        let market_index = lyrae_group.find_oracle_index(oracle_ai.key).ok_or(throw!())?;

        // This will catch the issue if oracle_ai.key == Pubkey::Default
        check!(market_index < lyrae_group.num_oracles, LyraeErrorCode::InvalidParam)?;

        // Make sure perp market at this index not already initialized
        check!(lyrae_group.perp_markets[market_index].is_empty(), LyraeErrorCode::InvalidParam)?;

        // This means there isn't already a token and spot market in Lyrae
        // Set the base decimals; if token not empty, ignore user input base_decimals
        if lyrae_group.tokens[market_index].is_empty() {
            lyrae_group.tokens[market_index].decimals = base_decimals;
        }
        // Initialize the Bids
        let _bids = BookSide::load_and_init(bids_ai, program_id, DataType::Bids, &rent)?;

        // Initialize the Asks
        let _asks = BookSide::load_and_init(asks_ai, program_id, DataType::Asks, &rent)?;

        // Initialize the EventQueue
        // TODO: check that the event queue is reasonably large
        let _event_queue = EventQueue::load_and_init(event_queue_ai, program_id, &rent)?;

        let lyrae_signer_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
        let (funder_ai, funder_seeds): (&AccountInfo, &[&[u8]]) = if admin_ai.data_is_empty() {
            (admin_ai, &[])
        } else {
            (signer_ai, &lyrae_signer_seeds)
        };

        // Create PDA and Initialize LYR vault
        let lyr_vault_seeds =
            &[perp_market_ai.key.as_ref(), token_prog_ai.key.as_ref(), lyr_mint_ai.key.as_ref()];
        seed_and_create_pda(
            program_id,
            funder_ai,
            &rent,
            spl_token::state::Account::LEN,
            &spl_token::id(),
            system_prog_ai,
            lyr_vault_ai,
            lyr_vault_seeds,
            funder_seeds,
        )?;

        solana_program::program::invoke_unchecked(
            &spl_token::instruction::initialize_account2(
                token_prog_ai.key,
                lyr_vault_ai.key,
                lyr_mint_ai.key,
                signer_ai.key,
            )?,
            &[
                lyr_vault_ai.clone(),
                lyr_mint_ai.clone(),
                signer_ai.clone(),
                rent_ai.clone(),
                token_prog_ai.clone(),
            ],
        )?;

        // Create PerpMarket PDA and Initialize the PerpMarket
        let perp_market_seeds =
            &[lyrae_group_ai.key.as_ref(), b"PerpMarket", oracle_ai.key.as_ref()];
        seed_and_create_pda(
            program_id,
            funder_ai,
            &rent,
            size_of::<PerpMarket>(),
            program_id,
            system_prog_ai,
            perp_market_ai,
            perp_market_seeds,
            funder_seeds,
        )?;

        let _perp_market = PerpMarket::load_and_init(
            perp_market_ai,
            program_id,
            lyrae_group_ai,
            bids_ai,
            asks_ai,
            event_queue_ai,
            lyr_vault_ai,
            &lyrae_group,
            &rent,
            base_lot_size,
            quote_lot_size,
            rate,
            max_depth_bps,
            target_period_length,
            lyr_per_period,
            exp,
            version,
            lm_size_shift,
        )?;

        let (maint_asset_weight, maint_liab_weight) = get_leverage_weights(maint_leverage);
        let (init_asset_weight, init_liab_weight) = get_leverage_weights(init_leverage);
        lyrae_group.perp_markets[market_index] = PerpMarketInfo {
            perp_market: *perp_market_ai.key,
            maint_asset_weight,
            init_asset_weight,
            maint_liab_weight,
            init_liab_weight,
            liquidation_fee,
            maker_fee,
            taker_fee,
            base_lot_size,
            quote_lot_size,
        };

        Ok(())
    }

    #[inline(never)]
    /// Deposit instruction
    fn deposit(program_id: &Pubkey, accounts: &[AccountInfo], quantity: u64) -> LyraeResult<()> {
        // TODO - consider putting update crank here
        const NUM_FIXED: usize = 9;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
        lyrae_group_ai,         // read
        lyrae_account_ai,       // write
            owner_ai,               // read
        lyrae_cache_ai,         // read
            root_bank_ai,           // read
            node_bank_ai,           // write
            vault_ai,               // write
            token_prog_ai,          // read
            owner_token_account_ai, // write
        ] = accounts;
        check_eq!(token_prog_ai.key, &spl_token::ID, LyraeErrorCode::InvalidProgramId)?;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;

        // Note: a check for &lyrae_account.owner == owner_ai.key doesn't exist on purpose
        // this is how lyrae currently reimburses users

        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;

        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;

        let token_index = lyrae_group
            .find_root_bank_index(root_bank_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidRootBank))?;

        // Find the node_bank pubkey in root_bank, if not found error
        let root_bank = RootBank::load_checked(root_bank_ai, program_id)?;
        check!(root_bank.node_banks.contains(node_bank_ai.key), LyraeErrorCode::InvalidNodeBank)?;
        let mut node_bank = NodeBank::load_mut_checked(node_bank_ai, program_id)?;
        check_eq!(&node_bank.vault, vault_ai.key, LyraeErrorCode::InvalidVault)?;

        // deposit into node bank token vault using invoke_transfer
        invoke_transfer(token_prog_ai, owner_token_account_ai, vault_ai, owner_ai, &[], quantity)?;

        // Check validity of root bank cache
        let now_ts = Clock::get()?.unix_timestamp as u64;
        let root_bank_cache = &lyrae_cache.root_bank_cache[token_index];
        let deposit = I80F48::from_num(quantity);
        root_bank_cache.check_valid(&lyrae_group, now_ts)?;

        checked_change_net(
            root_bank_cache,
            &mut node_bank,
            &mut lyrae_account,
            lyrae_account_ai.key,
            token_index,
            deposit,
        )?;

        lyrae_emit!(DepositLog {
            lyrae_group: *lyrae_group_ai.key,
            lyrae_account: *lyrae_account_ai.key,
            owner: *owner_ai.key,
            token_index: token_index as u64,
            quantity,
        });

        Ok(())
    }
    // TODO create client functions and instruction.rs
    #[inline(never)]
    #[allow(unused)]
    /// Change the shape of the interest rate function
    fn change_rate_params(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        optimal_util: I80F48,
        optimal_rate: I80F48,
        max_rate: I80F48,
    ) -> LyraeResult<()> {
        const NUM_FIXED: usize = 3;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
        lyrae_group_ai, // read
            root_bank_ai,   // read
            admin_ai        // read, signer
        ] = accounts;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check!(admin_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check_eq!(admin_ai.key, &lyrae_group.admin, LyraeErrorCode::InvalidAdminKey)?;
        check!(
            lyrae_group.find_root_bank_index(root_bank_ai.key).is_some(),
            LyraeErrorCode::InvalidRootBank
        )?;
        let mut root_bank = RootBank::load_mut_checked(root_bank_ai, program_id)?;
        root_bank.set_rate_params(optimal_util, optimal_rate, max_rate)?;

        Ok(())
    }

    #[inline(never)]
    #[allow(dead_code)]
    /// Change leverage, fees and liquidity mining params
    fn change_perp_market_params2(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        maint_leverage: Option<I80F48>,
        init_leverage: Option<I80F48>,
        liquidation_fee: Option<I80F48>,
        maker_fee: Option<I80F48>,
        taker_fee: Option<I80F48>,
        rate: Option<I80F48>,
        max_depth_bps: Option<I80F48>,
        target_period_length: Option<u64>,
        lyr_per_period: Option<u64>,
        exp: Option<u8>,
        version: Option<u8>,
        lm_size_shift: Option<u8>,
    ) -> LyraeResult<()> {
        const NUM_FIXED: usize = 3;
        let accounts = array_ref![accounts, 0, NUM_FIXED];

        let [
        lyrae_group_ai, // write
            perp_market_ai, // write
            admin_ai        // read, signer
        ] = accounts;

        let mut lyrae_group = LyraeGroup::load_mut_checked(lyrae_group_ai, program_id)?;
        check_eq!(admin_ai.key, &lyrae_group.admin, LyraeErrorCode::InvalidAdminKey)?;
        check!(admin_ai.is_signer, LyraeErrorCode::SignerNecessary)?;

        let market_index = lyrae_group
            .find_perp_market_index(perp_market_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidMarket))?;

        let mut perp_market =
            PerpMarket::load_mut_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;
        let mut info = &mut lyrae_group.perp_markets[market_index];

        // Unwrap params. Default to current state if Option is None
        let (maint_asset_weight, maint_liab_weight) = if let Some(x) = maint_leverage {
            get_leverage_weights(x)
        } else {
            (info.maint_asset_weight, info.maint_liab_weight)
        };
        let (init_asset_weight, init_liab_weight) = if let Some(x) = init_leverage {
            get_leverage_weights(x)
        } else {
            (info.init_asset_weight, info.init_liab_weight)
        };

        let liquidation_fee = liquidation_fee.unwrap_or(info.liquidation_fee);
        let maker_fee = maker_fee.unwrap_or(info.maker_fee);
        let taker_fee = taker_fee.unwrap_or(info.taker_fee);

        // params check
        check!(init_asset_weight > ZERO_I80F48, LyraeErrorCode::InvalidParam)?;
        check!(maint_asset_weight > init_asset_weight, LyraeErrorCode::InvalidParam)?;
        // maint leverage may only increase to prevent unforeseen liquidations
        check!(maint_asset_weight >= info.maint_asset_weight, LyraeErrorCode::InvalidParam)?;

        check!(maker_fee + taker_fee >= ZERO_I80F48, LyraeErrorCode::InvalidParam)?;

        // Set the params on LyraeGroup PerpMarketInfo
        info.maker_fee = maker_fee;
        info.taker_fee = taker_fee;
        info.liquidation_fee = liquidation_fee;
        info.maint_asset_weight = maint_asset_weight;
        info.init_asset_weight = init_asset_weight;
        info.maint_liab_weight = maint_liab_weight;
        info.init_liab_weight = init_liab_weight;

        let version = version.unwrap_or(perp_market.meta_data.version);
        check!(version == 0 || version == 1, LyraeErrorCode::InvalidParam)?;

        // If any of the LM params are changed, reset LM then change.
        if rate.is_some()
            || max_depth_bps.is_some()
            || target_period_length.is_some()
            || lyr_per_period.is_some()
            || exp.is_some()
            || lm_size_shift.is_some()
        {
            if version == 0 {
                let exp = exp.unwrap_or(perp_market.meta_data.extra_info[0]);
                check!(exp > 0 && exp <= 8, LyraeErrorCode::InvalidParam)?;
                let lm_size_shift = lm_size_shift.unwrap_or(perp_market.meta_data.extra_info[1]);

                perp_market.meta_data.extra_info[0] = exp;
                perp_market.meta_data.extra_info[1] = lm_size_shift;

                let mut lmi = &mut perp_market.liquidity_mining_info;
                let rate = rate.unwrap_or(lmi.rate);
                let max_depth_bps = max_depth_bps.unwrap_or(lmi.max_depth_bps);
                let target_period_length = target_period_length.unwrap_or(lmi.target_period_length);
                let lyr_per_period = lyr_per_period.unwrap_or(lmi.lyr_per_period);

                // Check params are valid
                check!(!max_depth_bps.is_negative(), LyraeErrorCode::InvalidParam)?;
                check!(!rate.is_negative(), LyraeErrorCode::InvalidParam)?;
                check!(target_period_length > 0, LyraeErrorCode::InvalidParam)?;

                // Reset liquidity incentives
                lmi.lyr_left = lyr_per_period;
                lmi.period_start = Clock::get()?.unix_timestamp as u64;

                // Set new params
                lmi.rate = rate;
                lmi.max_depth_bps = max_depth_bps;
                lmi.target_period_length = target_period_length;
                lmi.lyr_per_period = lyr_per_period;
            } else {
                let exp = exp.unwrap_or(perp_market.meta_data.extra_info[0]);
                let lm_size_shift = lm_size_shift.unwrap_or(perp_market.meta_data.extra_info[1]);
                check!(exp > 0 && exp <= 4, LyraeErrorCode::InvalidParam)?;
                perp_market.meta_data.extra_info[0] = exp;
                perp_market.meta_data.extra_info[1] = lm_size_shift;
                let mut lmi = &mut perp_market.liquidity_mining_info;
                let rate = rate.unwrap_or(lmi.rate);
                let max_depth_bps = max_depth_bps.unwrap_or(lmi.max_depth_bps);
                let target_period_length = target_period_length.unwrap_or(lmi.target_period_length);
                let lyr_per_period = lyr_per_period.unwrap_or(lmi.lyr_per_period);

                // Check params are valid
                check!(!max_depth_bps.is_negative(), LyraeErrorCode::InvalidParam)?;
                check!(max_depth_bps.int() == max_depth_bps, LyraeErrorCode::InvalidParam)?;
                check!(!rate.is_negative(), LyraeErrorCode::InvalidParam)?;
                check!(target_period_length > 0, LyraeErrorCode::InvalidParam)?;

                // Reset liquidity incentives
                lmi.lyr_left = lyr_per_period;
                lmi.period_start = Clock::get()?.unix_timestamp as u64;

                // Set new params
                lmi.rate = rate;
                lmi.max_depth_bps = max_depth_bps;
                lmi.target_period_length = target_period_length;
                lmi.lyr_per_period = lyr_per_period;
            }
        } else {
            // If version was changed and LM params stay same, that's an error probably
            check!(version == perp_market.meta_data.version, LyraeErrorCode::InvalidParam)?;
        }

        perp_market.meta_data.version = version;
        Ok(())
    }

    #[inline(never)]
    /// Write oracle prices onto LyraeAccount before calling a value-dep instruction (e.g. Withdraw)
    fn cache_prices(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 2;
        let (fixed_ais, oracle_ais) = array_refs![accounts, NUM_FIXED; ..;];
        let [
        lyrae_group_ai,     // read
        lyrae_cache_ai,     // write
        ] = fixed_ais;
        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let mut lyrae_cache =
            LyraeCache::load_mut_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        let clock = Clock::get()?;
        let last_update = clock.unix_timestamp as u64;

        let mut oracle_indexes = Vec::new();
        let mut oracle_prices = Vec::new();
        for oracle_ai in oracle_ais.iter() {
            let oracle_index = lyrae_group.find_oracle_index(oracle_ai.key).ok_or(throw!())?;

            if let Ok(price) = read_oracle(&lyrae_group, oracle_index, oracle_ai) {
                lyrae_cache.price_cache[oracle_index] = PriceCache { price, last_update };

                oracle_indexes.push(oracle_index as u64);
                oracle_prices.push(price.to_bits());
            } else {
                msg!("Failed CachePrice for oracle_index: {}", oracle_index);
            }
        }

        lyrae_emit!(CachePricesLog {
            lyrae_group: *lyrae_group_ai.key,
            oracle_indexes,
            oracle_prices
        });

        Ok(())
    }

    #[inline(never)]
    fn cache_root_banks(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 2;
        let (fixed_ais, root_bank_ais) = array_refs![accounts, NUM_FIXED; ..;];
        let [
        lyrae_group_ai,     // read
        lyrae_cache_ai,     // write
        ] = fixed_ais;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let mut lyrae_cache =
            LyraeCache::load_mut_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        let clock = Clock::get()?;
        let now_ts = clock.unix_timestamp as u64;

        let mut token_indexes = Vec::new();
        let mut deposit_indexes = Vec::new();
        let mut borrow_indexes = Vec::new();

        for root_bank_ai in root_bank_ais.iter() {
            let index = lyrae_group.find_root_bank_index(root_bank_ai.key).unwrap();
            let root_bank = RootBank::load_checked(root_bank_ai, program_id)?;
            lyrae_cache.root_bank_cache[index] = RootBankCache {
                deposit_index: root_bank.deposit_index,
                borrow_index: root_bank.borrow_index,
                last_update: now_ts,
            };

            token_indexes.push(index as u64);
            deposit_indexes.push(root_bank.deposit_index.to_bits());
            borrow_indexes.push(root_bank.borrow_index.to_bits())
        }
        lyrae_emit!(CacheRootBanksLog {
            lyrae_group: *lyrae_group_ai.key,
            token_indexes,
            deposit_indexes,
            borrow_indexes
        });

        Ok(())
    }

    #[inline(never)]
    fn cache_perp_markets(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 2;
        let (fixed_ais, perp_market_ais) = array_refs![accounts, NUM_FIXED; ..;];
        let [
        lyrae_group_ai,     // read
        lyrae_cache_ai,     // write
        ] = fixed_ais;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let mut lyrae_cache =
            LyraeCache::load_mut_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        let clock = Clock::get()?;
        let now_ts = clock.unix_timestamp as u64;

        let mut market_indexes = Vec::new();
        let mut long_fundings = Vec::new();
        let mut short_fundings = Vec::new();

        for perp_market_ai in perp_market_ais.iter() {
            let index = lyrae_group.find_perp_market_index(perp_market_ai.key).unwrap();
            let perp_market =
                PerpMarket::load_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;
            lyrae_cache.perp_market_cache[index] = PerpMarketCache {
                long_funding: perp_market.long_funding,
                short_funding: perp_market.short_funding,
                last_update: now_ts,
            };

            market_indexes.push(index as u64);
            long_fundings.push(perp_market.long_funding.to_bits());
            short_fundings.push(perp_market.short_funding.to_bits());
        }
        lyrae_emit!(CachePerpMarketsLog {
            lyrae_group: *lyrae_group_ai.key,
            market_indexes,
            long_fundings,
            short_fundings
        });

        Ok(())
    }

    #[inline(never)]
    /// Withdraw a token from the bank if collateral ratio permits
    fn withdraw(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        quantity: u64,
        allow_borrow: bool,
    ) -> LyraeResult<()> {
        const NUM_FIXED: usize = 10;
        let accounts = array_ref![accounts, 0, NUM_FIXED + MAX_PAIRS];
        let (fixed_ais, open_orders_ais) = array_refs![accounts, NUM_FIXED, MAX_PAIRS];
        let [
        lyrae_group_ai,     // read
        lyrae_account_ai,   // write
            owner_ai,           // read
        lyrae_cache_ai,     // read
            root_bank_ai,       // read
            node_bank_ai,       // write
            vault_ai,           // write
            token_account_ai,   // write
            signer_ai,          // read
            token_prog_ai,      // read
        ] = fixed_ais;
        check_eq!(&spl_token::ID, token_prog_ai.key, LyraeErrorCode::InvalidProgramId)?;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check!(signer_ai.key == &lyrae_group.signer_key, LyraeErrorCode::InvalidSignerKey)?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(&lyrae_account.owner == owner_ai.key, LyraeErrorCode::InvalidOwner)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        lyrae_account.check_open_orders(&lyrae_group, open_orders_ais)?;

        let root_bank = RootBank::load_checked(root_bank_ai, program_id)?;
        let token_index = lyrae_group
            .find_root_bank_index(root_bank_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidToken))?;

        let mut node_bank = NodeBank::load_mut_checked(node_bank_ai, program_id)?;
        check!(root_bank.node_banks.contains(node_bank_ai.key), LyraeErrorCode::InvalidNodeBank)?;
        let clock = Clock::get()?;
        let now_ts = clock.unix_timestamp as u64;

        // Safety checks
        check_eq!(&node_bank.vault, vault_ai.key, LyraeErrorCode::InvalidVault)?;

        let active_assets = UserActiveAssets::new(
            &lyrae_group,
            &lyrae_account,
            vec![(AssetType::Token, token_index)],
        );
        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        lyrae_cache.check_valid(&lyrae_group, &active_assets, now_ts)?;

        let root_bank_cache = &lyrae_cache.root_bank_cache[token_index];

        let native_deposit = lyrae_account.get_native_deposit(root_bank_cache, token_index)?;
        // if quantity is u64 max, interpret as a request to get all
        let (withdraw, quantity) = if quantity == u64::MAX && !allow_borrow {
            let floored = native_deposit.checked_floor().unwrap();
            (floored, floored.to_num::<u64>())
        } else {
            (I80F48::from_num(quantity), quantity)
        };

        // Borrow if withdrawing more than deposits
        check!(native_deposit >= withdraw || allow_borrow, LyraeErrorCode::InsufficientFunds)?;
        checked_change_net(
            root_bank_cache,
            &mut node_bank,
            &mut lyrae_account,
            lyrae_account_ai.key,
            token_index,
            -withdraw,
        )?;

        let signers_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
        invoke_transfer(
            token_prog_ai,
            vault_ai,
            token_account_ai,
            signer_ai,
            &[&signers_seeds],
            quantity,
        )?;

        let mut health_cache = HealthCache::new(active_assets);
        health_cache.init_vals(&lyrae_group, &lyrae_cache, &lyrae_account, open_orders_ais)?;
        let health = health_cache.get_health(&lyrae_group, HealthType::Init);

        check!(health >= ZERO_I80F48, LyraeErrorCode::InsufficientFunds)?;

        // If health is above Init then being liquidated should be false anyway
        lyrae_account.being_liquidated = false;

        lyrae_emit!(WithdrawLog {
            lyrae_group: *lyrae_group_ai.key,
            lyrae_account: *lyrae_account_ai.key,
            owner: *owner_ai.key,
            token_index: token_index as u64,
            quantity,
        });

        Ok(())
    }

    #[inline(never)]
    /// Call the init_open_orders instruction in serum dex and add this OpenOrders account to margin account
    fn init_spot_open_orders(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 8;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
        lyrae_group_ai,         // read
        lyrae_account_ai,       // write
            owner_ai,               // read
            dex_prog_ai,            // read
            open_orders_ai,         // write
            spot_market_ai,         // read
            signer_ai,              // read
            rent_ai,                // read
        ] = accounts;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check_eq!(dex_prog_ai.key, &lyrae_group.dex_program_id, LyraeErrorCode::InvalidProgramId)?;
        check!(&lyrae_group.signer_key == signer_ai.key, LyraeErrorCode::InvalidSignerKey)?;

        let market_index = lyrae_group
            .find_spot_market_index(spot_market_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidMarket))?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(owner_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;

        {
            let open_orders = load_open_orders(open_orders_ai)?;

            // Make sure this open orders account has not been initialized already
            check_eq!(open_orders.account_flags, 0, LyraeErrorCode::Default)?;
        }

        // Make sure there isn't already an open orders account for this market
        check!(
            lyrae_account.spot_open_orders[market_index] == Pubkey::default(),
            LyraeErrorCode::Default
        )?;

        let signers_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
        invoke_init_open_orders(
            dex_prog_ai,
            open_orders_ai,
            signer_ai,
            spot_market_ai,
            rent_ai,
            &[&signers_seeds],
        )?;

        lyrae_account.spot_open_orders[market_index] = *open_orders_ai.key;

        Ok(())
    }

    #[inline(never)]
    /// Create a new OpenOrders PDA then
    /// Call the init_open_orders instruction in serum dex and add this OpenOrders account to margin account
    fn create_spot_open_orders(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult {
        const NUM_FIXED: usize = 8;
        let fixed_accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
        lyrae_group_ai,         // read
        lyrae_account_ai,       // write
            owner_ai,               // read (write if no payer passed) & signer
            dex_prog_ai,            // read
            open_orders_ai,         // write
            spot_market_ai,         // read
            signer_ai,              // read
            system_prog_ai,         // read
        ] = fixed_accounts;
        let payer_ai = if accounts.len() > NUM_FIXED {
            &accounts[NUM_FIXED] // write & signer
        } else {
            owner_ai
        };
        check!(
            system_prog_ai.key == &solana_program::system_program::id(),
            LyraeErrorCode::InvalidProgramId
        )?;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check_eq!(dex_prog_ai.key, &lyrae_group.dex_program_id, LyraeErrorCode::InvalidProgramId)?;
        check!(&lyrae_group.signer_key == signer_ai.key, LyraeErrorCode::InvalidSignerKey)?;

        let market_index = lyrae_group
            .find_spot_market_index(spot_market_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidMarket))?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(owner_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        check!(payer_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;

        let open_orders_seeds: &[&[u8]] =
            &[&lyrae_account_ai.key.as_ref(), &market_index.to_le_bytes(), b"OpenOrders"];
        seed_and_create_pda(
            program_id,
            payer_ai,
            &Rent::get()?,
            size_of::<serum_dex::state::OpenOrders>() + 12,
            dex_prog_ai.key,
            system_prog_ai,
            open_orders_ai,
            open_orders_seeds,
            &[],
        )?;

        {
            let open_orders = load_open_orders(open_orders_ai)?;

            // Make sure this open orders account has not been initialized already
            check_eq!(open_orders.account_flags, 0, LyraeErrorCode::Default)?;
        }

        // Make sure there isn't already an open orders account for this market
        check!(
            lyrae_account.spot_open_orders[market_index] == Pubkey::default(),
            LyraeErrorCode::Default
        )?;

        let signers_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
        invoke_init_open_orders(
            dex_prog_ai,
            open_orders_ai,
            signer_ai,
            spot_market_ai,
            system_prog_ai, // no need to send in rent ai
            &[&signers_seeds],
        )?;

        lyrae_account.spot_open_orders[market_index] = *open_orders_ai.key;

        Ok(())
    }

    #[inline(never)]
    fn close_spot_open_orders(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 7;
        let accounts = array_ref![accounts, 0, NUM_FIXED];

        let [
        lyrae_group_ai,     // read
        lyrae_account_ai,   // write
            owner_ai,           // write, signer
            dex_prog_ai,        // read
            open_orders_ai,     // write
            spot_market_ai,     // read
            signer_ai,          // read
        ] = accounts;

        check!(owner_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check_eq!(dex_prog_ai.key, &lyrae_group.dex_program_id, LyraeErrorCode::InvalidProgramId)?;
        check_eq!(signer_ai.key, &lyrae_group.signer_key, LyraeErrorCode::InvalidParam)?;

        let market_index = lyrae_group
            .find_spot_market_index(spot_market_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidMarket))?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, &lyrae_group_ai.key)?;
        check_eq!(&lyrae_account.owner, owner_ai.key, LyraeErrorCode::InvalidOwner)?;
        check!(!lyrae_account.being_liquidated, LyraeErrorCode::BeingLiquidated)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        check_eq!(
            &lyrae_account.spot_open_orders[market_index],
            open_orders_ai.key,
            LyraeErrorCode::InvalidOpenOrdersAccount
        )?;

        if lyrae_account.in_margin_basket[market_index] {
            let open_orders = load_open_orders(open_orders_ai)?;
            lyrae_account.update_basket(market_index, &open_orders)?;
            check!(
                !lyrae_account.in_margin_basket[market_index],
                LyraeErrorCode::InvalidAccountState
            )?;
        }

        let signers_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
        invoke_close_open_orders(
            dex_prog_ai,
            open_orders_ai,
            signer_ai,
            owner_ai,
            spot_market_ai,
            &[&signers_seeds],
        )?;

        lyrae_account.spot_open_orders[market_index] = Pubkey::default();

        Ok(())
    }

    #[inline(never)]
    /// DEPRECATED
    fn place_spot_order(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        order: serum_dex::instruction::NewOrderInstructionV3,
    ) -> LyraeResult<()> {
        const NUM_FIXED: usize = 23;
        let accounts = array_ref![accounts, 0, NUM_FIXED + MAX_PAIRS];
        let (fixed_ais, open_orders_ais) = array_refs![accounts, NUM_FIXED, MAX_PAIRS];

        let [
        lyrae_group_ai,         // read
        lyrae_account_ai,       // write
            owner_ai,               // read & signer
        lyrae_cache_ai,         // read
            dex_prog_ai,            // read
            spot_market_ai,         // write
            bids_ai,                // write
            asks_ai,                // write
            dex_request_queue_ai,   // write
            dex_event_queue_ai,     // write
            dex_base_ai,            // write
            dex_quote_ai,           // write
            base_root_bank_ai,      // read
            base_node_bank_ai,      // write
            base_vault_ai,          // write
            quote_root_bank_ai,     // read
            quote_node_bank_ai,     // write
            quote_vault_ai,         // write
            token_prog_ai,          // read
            signer_ai,              // read
            _rent_ai,               // read
            dex_signer_ai,          // read
            msrm_or_srm_vault_ai,   // read
        ] = fixed_ais;

        // TODO OPT - reduce size of this transaction
        // put bank info into group +64 bytes (can't do this now)
        // remove settle_funds +64 bytes (can't do this for UX reasons)
        // ask serum dex to use dynamic sysvars +32 bytes
        // only send in open orders pubkeys we need +38 bytes
        // shrink size of order instruction +10 bytes

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check_eq!(token_prog_ai.key, &spl_token::ID, LyraeErrorCode::InvalidProgramId)?;
        check_eq!(dex_prog_ai.key, &lyrae_group.dex_program_id, LyraeErrorCode::InvalidProgramId)?;
        check!(signer_ai.key == &lyrae_group.signer_key, LyraeErrorCode::InvalidSignerKey)?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(owner_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;

        let clock = Clock::get()?;
        let now_ts = clock.unix_timestamp as u64;

        let market_index = lyrae_group
            .find_spot_market_index(spot_market_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidMarket))?;

        check!(
            &lyrae_group.tokens[market_index].root_bank == base_root_bank_ai.key,
            LyraeErrorCode::InvalidRootBank
        )?;
        let base_root_bank = RootBank::load_checked(base_root_bank_ai, program_id)?;

        check!(
            base_root_bank.node_banks.contains(base_node_bank_ai.key),
            LyraeErrorCode::InvalidNodeBank
        )?;
        let mut base_node_bank = NodeBank::load_mut_checked(base_node_bank_ai, program_id)?;

        check_eq!(&base_node_bank.vault, base_vault_ai.key, LyraeErrorCode::InvalidVault)?;

        check!(
            &lyrae_group.tokens[QUOTE_INDEX].root_bank == quote_root_bank_ai.key,
            LyraeErrorCode::InvalidRootBank
        )?;
        let quote_root_bank = RootBank::load_checked(quote_root_bank_ai, program_id)?;

        check!(
            quote_root_bank.node_banks.contains(quote_node_bank_ai.key),
            LyraeErrorCode::InvalidNodeBank
        )?;
        let mut quote_node_bank = NodeBank::load_mut_checked(quote_node_bank_ai, program_id)?;
        check_eq!(&quote_node_bank.vault, quote_vault_ai.key, LyraeErrorCode::InvalidVault)?;

        // Fix the margin basket incase there are empty ones; main benefit is freeing up basket space
        for i in 0..lyrae_group.num_oracles {
            if lyrae_account.in_margin_basket[i] {
                let open_orders = load_open_orders(&open_orders_ais[i])?;
                lyrae_account.update_basket(i, &open_orders)?;
            }
        }

        // Adjust margin basket; this also makes this market an active asset
        lyrae_account.add_to_basket(market_index)?;
        lyrae_account.check_open_orders(&lyrae_group, open_orders_ais)?;

        let active_assets = UserActiveAssets::new(&lyrae_group, &lyrae_account, vec![]);
        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        lyrae_cache.check_valid(&lyrae_group, &active_assets, now_ts)?;

        let mut health_cache = HealthCache::new(active_assets);
        health_cache.init_vals(&lyrae_group, &lyrae_cache, &lyrae_account, open_orders_ais)?;
        let pre_health = health_cache.get_health(&lyrae_group, HealthType::Init);

        // update the being_liquidated flag
        if lyrae_account.being_liquidated {
            if pre_health >= ZERO_I80F48 {
                lyrae_account.being_liquidated = false;
            } else {
                return Err(throw_err!(LyraeErrorCode::BeingLiquidated));
            }
        }

        // This means health must only go up
        let reduce_only = pre_health < ZERO_I80F48;

        // TODO maybe check that root bank was updated recently
        // TODO maybe check oracle was updated recently

        // TODO OPT - write a zero copy way to deserialize Account to reduce compute

        // this is to keep track of the amount of funds transferred
        let (pre_base, pre_quote) = {
            (
                Account::unpack(&base_vault_ai.try_borrow_data()?)?.amount,
                Account::unpack(&quote_vault_ai.try_borrow_data()?)?.amount,
            )
        };

        let order_side = order.side;
        let vault_ai = match order_side {
            serum_dex::matching::Side::Bid => quote_vault_ai,
            serum_dex::matching::Side::Ask => base_vault_ai,
        };

        // Enforce order price limits if the order is a limit order that goes on the book
        let native_price = {
            let market = load_market_state(spot_market_ai, dex_prog_ai.key)?;

            I80F48::from_num(order.limit_price.get())
                .checked_mul(I80F48::from_num(market.pc_lot_size))
                .unwrap()
                .checked_div(I80F48::from_num(market.coin_lot_size))
                .unwrap()
        };
        let oracle_price = lyrae_cache.get_price(market_index);
        let info = &lyrae_group.spot_markets[market_index];

        // If not post_allowed, then pre_locked may not increase
        let (post_allowed, pre_locked) = {
            let open_orders = load_open_orders(&open_orders_ais[market_index])?;
            match order_side {
                serum_dex::matching::Side::Bid => (
                    native_price.checked_div(oracle_price).unwrap() <= info.maint_liab_weight,
                    open_orders.native_pc_total - open_orders.native_pc_free,
                ),
                serum_dex::matching::Side::Ask => (
                    native_price.checked_div(oracle_price).unwrap() >= info.maint_asset_weight,
                    open_orders.native_coin_total - open_orders.native_coin_free,
                ),
            }
        };

        // Send order to serum dex
        let signers_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
        invoke_new_order(
            dex_prog_ai,
            spot_market_ai,
            &open_orders_ais[market_index],
            dex_request_queue_ai,
            dex_event_queue_ai,
            bids_ai,
            asks_ai,
            vault_ai,
            signer_ai,
            dex_base_ai,
            dex_quote_ai,
            token_prog_ai,
            msrm_or_srm_vault_ai,
            &[&signers_seeds],
            order,
        )?;

        // Settle funds for this market
        invoke_settle_funds(
            dex_prog_ai,
            spot_market_ai,
            &open_orders_ais[market_index],
            signer_ai,
            dex_base_ai,
            dex_quote_ai,
            base_vault_ai,
            quote_vault_ai,
            dex_signer_ai,
            token_prog_ai,
            &[&signers_seeds],
        )?;

        // See if we can remove this market from margin
        let open_orders = load_open_orders(&open_orders_ais[market_index])?;
        lyrae_account.update_basket(market_index, &open_orders)?;

        let post_locked = match order_side {
            serum_dex::matching::Side::Bid => {
                open_orders.native_pc_total - open_orders.native_pc_free
            }
            serum_dex::matching::Side::Ask => {
                open_orders.native_coin_total - open_orders.native_coin_free
            }
        };
        check!(post_allowed || post_locked <= pre_locked, LyraeErrorCode::InvalidParam)?;
        let (post_base, post_quote) = {
            (
                Account::unpack(&base_vault_ai.try_borrow_data()?)?.amount,
                Account::unpack(&quote_vault_ai.try_borrow_data()?)?.amount,
            )
        };

        let quote_change = I80F48::from_num(post_quote) - I80F48::from_num(pre_quote);
        let base_change = I80F48::from_num(post_base) - I80F48::from_num(pre_base);

        checked_change_net(
            &lyrae_cache.root_bank_cache[QUOTE_INDEX],
            &mut quote_node_bank,
            &mut lyrae_account,
            lyrae_account_ai.key,
            QUOTE_INDEX,
            quote_change,
        )?;

        checked_change_net(
            &lyrae_cache.root_bank_cache[market_index],
            &mut base_node_bank,
            &mut lyrae_account,
            lyrae_account_ai.key,
            market_index,
            base_change,
        )?;

        // Update health for tokens that may have changed
        health_cache.update_quote(&lyrae_cache, &lyrae_account);
        health_cache.update_spot_val(
            &lyrae_group,
            &lyrae_cache,
            &lyrae_account,
            &open_orders_ais[market_index],
            market_index,
        )?;
        let post_health = health_cache.get_health(&lyrae_group, HealthType::Init);

        // If an account is in reduce_only mode, health must only go up
        check!(
            post_health >= ZERO_I80F48 || (reduce_only && post_health >= pre_health),
            LyraeErrorCode::InsufficientFunds
        )?;

        lyrae_emit!(OpenOrdersBalanceLog {
            lyrae_group: *lyrae_group_ai.key,
            lyrae_account: *lyrae_account_ai.key,
            market_index: market_index as u64,
            base_total: open_orders.native_coin_total,
            base_free: open_orders.native_coin_free,
            quote_total: open_orders.native_pc_total,
            quote_free: open_orders.native_pc_free,
            referrer_rebates_accrued: open_orders.referrer_rebates_accrued
        });

        Ok(())
    }

    #[inline(never)]
    fn place_spot_order2(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        order: serum_dex::instruction::NewOrderInstructionV3,
    ) -> LyraeResult<()> {
        const NUM_FIXED: usize = 22;
        let (fixed_ais, packed_open_orders_ais) = array_refs![accounts, NUM_FIXED; ..;];

        let [
        lyrae_group_ai,         // read
        lyrae_account_ai,       // write
            owner_ai,               // read & signer
        lyrae_cache_ai,         // read
            dex_prog_ai,            // read
            spot_market_ai,         // write
            bids_ai,                // write
            asks_ai,                // write
            dex_request_queue_ai,   // write
            dex_event_queue_ai,     // write
            dex_base_ai,            // write
            dex_quote_ai,           // write
            base_root_bank_ai,      // read
            base_node_bank_ai,      // write
            base_vault_ai,          // write
            quote_root_bank_ai,     // read
            quote_node_bank_ai,     // write
            quote_vault_ai,         // write
            token_prog_ai,          // read
            signer_ai,              // read
            dex_signer_ai,          // read
            msrm_or_srm_vault_ai,   // read
        ] = fixed_ais;

        // put bank info into group +64 bytes (can't do this now)
        // remove settle_funds +64 bytes (can't do this for UX reasons)
        // ask serum dex to use dynamic sysvars +31 bytes (done)
        // only send in open orders pubkeys we need +38 bytes (done)
        // shrink size of order instruction +10 bytes

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check_eq!(token_prog_ai.key, &spl_token::ID, LyraeErrorCode::InvalidProgramId)?;
        check_eq!(dex_prog_ai.key, &lyrae_group.dex_program_id, LyraeErrorCode::InvalidProgramId)?;
        check!(signer_ai.key == &lyrae_group.signer_key, LyraeErrorCode::InvalidSignerKey)?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(owner_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;

        let clock = Clock::get()?;
        let now_ts = clock.unix_timestamp as u64;

        let market_index = lyrae_group
            .find_spot_market_index(spot_market_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidMarket))?;

        check!(
            &lyrae_group.tokens[market_index].root_bank == base_root_bank_ai.key,
            LyraeErrorCode::InvalidRootBank
        )?;
        let base_root_bank = RootBank::load_checked(base_root_bank_ai, program_id)?;

        check!(
            base_root_bank.node_banks.contains(base_node_bank_ai.key),
            LyraeErrorCode::InvalidNodeBank
        )?;
        let mut base_node_bank = NodeBank::load_mut_checked(base_node_bank_ai, program_id)?;

        check_eq!(&base_node_bank.vault, base_vault_ai.key, LyraeErrorCode::InvalidVault)?;

        check!(
            &lyrae_group.tokens[QUOTE_INDEX].root_bank == quote_root_bank_ai.key,
            LyraeErrorCode::InvalidRootBank
        )?;
        let quote_root_bank = RootBank::load_checked(quote_root_bank_ai, program_id)?;

        check!(
            quote_root_bank.node_banks.contains(quote_node_bank_ai.key),
            LyraeErrorCode::InvalidNodeBank
        )?;
        let mut quote_node_bank = NodeBank::load_mut_checked(quote_node_bank_ai, program_id)?;
        check_eq!(&quote_node_bank.vault, quote_vault_ai.key, LyraeErrorCode::InvalidVault)?;

        let mut open_orders_ais =
            lyrae_account.checked_unpack_open_orders(&lyrae_group, packed_open_orders_ais)?;
        let open_orders_accounts = load_open_orders_accounts(&open_orders_ais)?;

        // Fix the margin basket incase there are empty ones; main benefit is freeing up basket space
        for i in 0..lyrae_group.num_oracles {
            if lyrae_account.in_margin_basket[i] {
                let open_orders = load_open_orders(open_orders_ais[i].unwrap())?;
                lyrae_account.update_basket(i, &open_orders)?;
            }
        }

        // Adjust margin basket; this also makes this market an active asset
        lyrae_account.add_to_basket(market_index)?;
        if open_orders_ais[market_index].is_none() {
            open_orders_ais[market_index] = Some(lyrae_account.checked_unpack_open_orders_single(
                &lyrae_group,
                packed_open_orders_ais,
                market_index,
            )?);
        }

        let active_assets = UserActiveAssets::new(&lyrae_group, &lyrae_account, vec![]);
        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        lyrae_cache.check_valid(&lyrae_group, &active_assets, now_ts)?;

        let mut health_cache = HealthCache::new(active_assets);
        health_cache.init_vals_with_orders_vec(
            &lyrae_group,
            &lyrae_cache,
            &lyrae_account,
            &open_orders_accounts,
        )?;
        let pre_health = health_cache.get_health(&lyrae_group, HealthType::Init);

        // update the being_liquidated flag
        if lyrae_account.being_liquidated {
            if pre_health >= ZERO_I80F48 {
                lyrae_account.being_liquidated = false;
            } else {
                return Err(throw_err!(LyraeErrorCode::BeingLiquidated));
            }
        }

        // This means health must only go up
        let reduce_only = pre_health < ZERO_I80F48;

        // TODO maybe check that root bank was updated recently
        // TODO maybe check oracle was updated recently

        // TODO OPT - write a zero copy way to deserialize Account to reduce compute
        // this is to keep track of the amount of funds transferred
        let (pre_base, pre_quote) = {
            (
                Account::unpack(&base_vault_ai.try_borrow_data()?)?.amount,
                Account::unpack(&quote_vault_ai.try_borrow_data()?)?.amount,
            )
        };
        let order_side = order.side;
        let vault_ai = match order_side {
            serum_dex::matching::Side::Bid => quote_vault_ai,
            serum_dex::matching::Side::Ask => base_vault_ai,
        };

        // Enforce order price limits if the order is a limit order that goes on the book
        let native_price = {
            // Conver the price in
            let market = load_market_state(spot_market_ai, dex_prog_ai.key)?;
            I80F48::from_num(order.limit_price.get())
                .checked_mul(I80F48::from_num(market.pc_lot_size))
                .unwrap()
                .checked_div(I80F48::from_num(market.coin_lot_size))
                .unwrap()
        };
        let oracle_price = lyrae_cache.get_price(market_index);
        let info = &lyrae_group.spot_markets[market_index];
        let market_open_orders_ai = open_orders_ais[market_index].unwrap();

        // If not post_allowed, then pre_locked may not increase
        let (post_allowed, pre_locked) = {
            let open_orders = load_open_orders(market_open_orders_ai)?;
            match order_side {
                serum_dex::matching::Side::Bid => (
                    native_price.checked_div(oracle_price).unwrap() <= info.maint_liab_weight,
                    open_orders.native_pc_total - open_orders.native_pc_free,
                ),
                serum_dex::matching::Side::Ask => (
                    native_price.checked_div(oracle_price).unwrap() >= info.maint_asset_weight,
                    open_orders.native_coin_total - open_orders.native_coin_free,
                ),
            }
        };

        // Send order to serum dex
        let signers_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
        invoke_new_order(
            dex_prog_ai,
            spot_market_ai,
            market_open_orders_ai,
            dex_request_queue_ai,
            dex_event_queue_ai,
            bids_ai,
            asks_ai,
            vault_ai,
            signer_ai,
            dex_base_ai,
            dex_quote_ai,
            token_prog_ai,
            msrm_or_srm_vault_ai,
            &[&signers_seeds],
            order,
        )?;

        // Settle funds for this market
        invoke_settle_funds(
            dex_prog_ai,
            spot_market_ai,
            market_open_orders_ai,
            signer_ai,
            dex_base_ai,
            dex_quote_ai,
            base_vault_ai,
            quote_vault_ai,
            dex_signer_ai,
            token_prog_ai,
            &[&signers_seeds],
        )?;

        // See if we can remove this market from margin
        let open_orders = load_open_orders(market_open_orders_ai)?;
        lyrae_account.update_basket(market_index, &open_orders)?;

        let post_locked = match order_side {
            serum_dex::matching::Side::Bid => {
                open_orders.native_pc_total - open_orders.native_pc_free
            }
            serum_dex::matching::Side::Ask => {
                open_orders.native_coin_total - open_orders.native_coin_free
            }
        };

        // If not post allowed, locked amount (i.e. amount on the order book) should not increase
        check!(post_allowed || post_locked <= pre_locked, LyraeErrorCode::InvalidParam)?;

        let (post_base, post_quote) = {
            (
                Account::unpack(&base_vault_ai.try_borrow_data()?)?.amount,
                Account::unpack(&quote_vault_ai.try_borrow_data()?)?.amount,
            )
        };

        let quote_change = I80F48::from_num(post_quote) - I80F48::from_num(pre_quote);
        let base_change = I80F48::from_num(post_base) - I80F48::from_num(pre_base);

        checked_change_net(
            &lyrae_cache.root_bank_cache[QUOTE_INDEX],
            &mut quote_node_bank,
            &mut lyrae_account,
            lyrae_account_ai.key,
            QUOTE_INDEX,
            quote_change,
        )?;

        checked_change_net(
            &lyrae_cache.root_bank_cache[market_index],
            &mut base_node_bank,
            &mut lyrae_account,
            lyrae_account_ai.key,
            market_index,
            base_change,
        )?;

        // Update health for tokens that may have changed
        health_cache.update_quote(&lyrae_cache, &lyrae_account);
        health_cache.update_spot_val(
            &lyrae_group,
            &lyrae_cache,
            &lyrae_account,
            market_open_orders_ai,
            market_index,
        )?;
        let post_health = health_cache.get_health(&lyrae_group, HealthType::Init);

        // If an account is in reduce_only mode, health must only go up
        check!(
            post_health >= ZERO_I80F48 || (reduce_only && post_health >= pre_health),
            LyraeErrorCode::InsufficientFunds
        )?;

        lyrae_emit!(OpenOrdersBalanceLog {
            lyrae_group: *lyrae_group_ai.key,
            lyrae_account: *lyrae_account_ai.key,
            market_index: market_index as u64,
            base_total: open_orders.native_coin_total,
            base_free: open_orders.native_coin_free,
            quote_total: open_orders.native_pc_total,
            quote_free: open_orders.native_pc_free,
            referrer_rebates_accrued: open_orders.referrer_rebates_accrued
        });

        Ok(())
    }

    #[inline(never)]
    fn cancel_spot_order(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        data: Vec<u8>,
    ) -> LyraeResult<()> {
        // TODO add param `ok_invalid_id` to return Ok() instead of Err if order id or client id invalid

        const NUM_FIXED: usize = 10;
        let accounts = array_ref![accounts, 0, NUM_FIXED];

        let [
        lyrae_group_ai,     // read
            owner_ai,           // signer
        lyrae_account_ai,   // read
            dex_prog_ai,        // read
            spot_market_ai,     // write
            bids_ai,            // write
            asks_ai,            // write
            open_orders_ai,     // write
            signer_ai,          // read
            dex_event_queue_ai, // write
        ] = accounts;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check_eq!(dex_prog_ai.key, &lyrae_group.dex_program_id, LyraeErrorCode::InvalidProgramId)?;
        check!(signer_ai.key == &lyrae_group.signer_key, LyraeErrorCode::InvalidSignerKey)?;

        let lyrae_account =
            LyraeAccount::load_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(owner_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;

        let market_index = lyrae_group.find_spot_market_index(spot_market_ai.key).unwrap();
        check_eq!(
            &lyrae_account.spot_open_orders[market_index],
            open_orders_ai.key,
            LyraeErrorCode::InvalidOpenOrdersAccount
        )?;

        let signer_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
        invoke_cancel_order(
            dex_prog_ai,
            spot_market_ai,
            bids_ai,
            asks_ai,
            open_orders_ai,
            signer_ai,
            dex_event_queue_ai,
            data,
            &[&signer_seeds],
        )?;

        let open_orders = load_open_orders(open_orders_ai)?;
        lyrae_emit!(OpenOrdersBalanceLog {
            lyrae_group: *lyrae_group_ai.key,
            lyrae_account: *lyrae_account_ai.key,
            market_index: market_index as u64,
            base_total: open_orders.native_coin_total,
            base_free: open_orders.native_coin_free,
            quote_total: open_orders.native_pc_total,
            quote_free: open_orders.native_pc_free,
            referrer_rebates_accrued: open_orders.referrer_rebates_accrued
        });

        Ok(())
    }

    #[inline(never)]
    fn settle_funds(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult {
        const NUM_FIXED: usize = 18;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,         // read
            lyrae_cache_ai,         // read
            owner_ai,               // signer
            lyrae_account_ai,       // write
            dex_prog_ai,            // read
            spot_market_ai,         // write
            open_orders_ai,         // write
            signer_ai,              // read
            dex_base_ai,            // write
            dex_quote_ai,           // write
            base_root_bank_ai,      // read
            base_node_bank_ai,      // write
            quote_root_bank_ai,     // read
            quote_node_bank_ai,     // write
            base_vault_ai,          // write
            quote_vault_ai,         // write
            dex_signer_ai,          // read
            token_prog_ai,          // read
        ] = accounts;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check_eq!(token_prog_ai.key, &spl_token::id(), LyraeErrorCode::InvalidProgramId)?;
        check_eq!(dex_prog_ai.key, &lyrae_group.dex_program_id, LyraeErrorCode::InvalidProgramId)?;
        check!(signer_ai.key == &lyrae_group.signer_key, LyraeErrorCode::InvalidSignerKey)?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;

        // Make sure the spot market is valid
        let market_index = lyrae_group
            .find_spot_market_index(spot_market_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidMarket))?;

        let base_root_bank = RootBank::load_checked(base_root_bank_ai, program_id)?;
        check!(
            base_root_bank_ai.key == &lyrae_group.tokens[market_index].root_bank,
            LyraeErrorCode::InvalidRootBank
        )?;

        let mut base_node_bank = NodeBank::load_mut_checked(base_node_bank_ai, program_id)?;
        check!(
            base_root_bank.node_banks.contains(base_node_bank_ai.key),
            LyraeErrorCode::InvalidNodeBank
        )?;
        check_eq!(&base_node_bank.vault, base_vault_ai.key, LyraeErrorCode::InvalidVault)?;

        let quote_root_bank = RootBank::load_checked(quote_root_bank_ai, program_id)?;
        check!(
            quote_root_bank_ai.key == &lyrae_group.tokens[QUOTE_INDEX].root_bank,
            LyraeErrorCode::InvalidRootBank
        )?;
        let mut quote_node_bank = NodeBank::load_mut_checked(quote_node_bank_ai, program_id)?;
        check!(
            quote_root_bank.node_banks.contains(quote_node_bank_ai.key),
            LyraeErrorCode::InvalidNodeBank
        )?;
        check_eq!(&quote_node_bank.vault, quote_vault_ai.key, LyraeErrorCode::InvalidVault)?;

        check_eq!(
            &lyrae_account.spot_open_orders[market_index],
            open_orders_ai.key,
            LyraeErrorCode::Default
        )?;

        if *open_orders_ai.key == Pubkey::default() {
            return Ok(());
        }

        check_open_orders(open_orders_ai, &lyrae_group.signer_key, &lyrae_group.dex_program_id)?;

        let (pre_base, pre_quote) = {
            let open_orders = load_open_orders(open_orders_ai)?;
            (
                open_orders.native_coin_free,
                open_orders.native_pc_free + open_orders.referrer_rebates_accrued,
            )
        };

        let signer_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
        invoke_settle_funds(
            dex_prog_ai,
            spot_market_ai,
            open_orders_ai,
            signer_ai,
            dex_base_ai,
            dex_quote_ai,
            base_vault_ai,
            quote_vault_ai,
            dex_signer_ai,
            token_prog_ai,
            &[&signer_seeds],
        )?;

        let (post_base, post_quote) = {
            let open_orders = load_open_orders(open_orders_ai)?;
            // remove from margin basket if it's empty
            lyrae_account.update_basket(market_index, &open_orders)?;
            lyrae_emit!(OpenOrdersBalanceLog {
                lyrae_group: *lyrae_group_ai.key,
                lyrae_account: *lyrae_account_ai.key,
                market_index: market_index as u64,
                base_total: open_orders.native_coin_total,
                base_free: open_orders.native_coin_free,
                quote_total: open_orders.native_pc_total,
                quote_free: open_orders.native_pc_free,
                referrer_rebates_accrued: open_orders.referrer_rebates_accrued
            });

            (
                open_orders.native_coin_free,
                open_orders.native_pc_free + open_orders.referrer_rebates_accrued,
            )
        };

        // TODO OPT - remove sanity check if confident
        check!(post_base <= pre_base, LyraeErrorCode::MathError)?;
        check!(post_quote <= pre_quote, LyraeErrorCode::MathError)?;

        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;

        let now_ts = Clock::get()?.unix_timestamp as u64;
        let base_root_bank_cache = &lyrae_cache.root_bank_cache[market_index];
        let quote_root_bank_cache = &lyrae_cache.root_bank_cache[QUOTE_INDEX];

        base_root_bank_cache.check_valid(&lyrae_group, now_ts)?;
        quote_root_bank_cache.check_valid(&lyrae_group, now_ts)?;

        checked_change_net(
            base_root_bank_cache,
            &mut base_node_bank,
            &mut lyrae_account,
            lyrae_account_ai.key,
            market_index,
            I80F48::from_num(pre_base - post_base),
        )?;
        checked_change_net(
            quote_root_bank_cache,
            &mut quote_node_bank,
            &mut lyrae_account,
            lyrae_account_ai.key,
            QUOTE_INDEX,
            I80F48::from_num(pre_quote - post_quote),
        )
    }

    #[inline(never)]
    fn place_perp_order(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        side: Side,
        price: i64,
        quantity: i64,
        client_order_id: u64,
        order_type: OrderType,
        reduce_only: bool,
    ) -> LyraeResult {
        check!(price > 0, LyraeErrorCode::InvalidParam)?;
        check!(quantity > 0, LyraeErrorCode::InvalidParam)?;

        const NUM_FIXED: usize = 8;
        let (fixed_ais, open_orders_ais, opt_ais) =
            array_refs![accounts, NUM_FIXED, MAX_PAIRS; ..;];
        let [
            lyrae_group_ai,     // read
            lyrae_account_ai,   // write
            owner_ai,           // read, signer
            lyrae_cache_ai,     // read
            perp_market_ai,     // write
            bids_ai,            // write
            asks_ai,            // write
            event_queue_ai,     // write
        ] = fixed_ais;

        let referrer_lyrae_account_ai = opt_ais.first();

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        lyrae_account.check_open_orders(&lyrae_group, open_orders_ais)?;

        let clock = Clock::get()?;
        let now_ts = clock.unix_timestamp as u64;

        let mut perp_market =
            PerpMarket::load_mut_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;
        let market_index = lyrae_group
            .find_perp_market_index(perp_market_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidMarket))?;

        let active_assets = UserActiveAssets::new(
            &lyrae_group,
            &lyrae_account,
            vec![(AssetType::Perp, market_index)],
        );

        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        lyrae_cache.check_valid(&lyrae_group, &active_assets, now_ts)?;

        let mut health_cache = HealthCache::new(active_assets);
        health_cache.init_vals(&lyrae_group, &lyrae_cache, &lyrae_account, open_orders_ais)?;
        let pre_health = health_cache.get_health(&lyrae_group, HealthType::Init);

        // update the being_liquidated flag
        if lyrae_account.being_liquidated {
            if pre_health >= ZERO_I80F48 {
                lyrae_account.being_liquidated = false;
            } else {
                return Err(throw_err!(LyraeErrorCode::BeingLiquidated));
            }
        }

        // This means health must only go up
        let health_up_only = pre_health < ZERO_I80F48;

        let mut book = Book::load_checked(program_id, bids_ai, asks_ai, &perp_market)?;
        let mut event_queue =
            EventQueue::load_mut_checked(event_queue_ai, program_id, &perp_market)?;

        // If reduce_only, position must only go down
        let quantity = if reduce_only {
            let base_pos = lyrae_account.get_complete_base_pos(
                market_index,
                &event_queue,
                lyrae_account_ai.key,
            )?;

            if (side == Side::Bid && base_pos > 0) || (side == Side::Ask && base_pos < 0) {
                0
            } else {
                base_pos.abs().min(quantity)
            }
        } else {
            quantity
        };

        if quantity == 0 {
            return Ok(());
        }

        book.new_order(
            program_id,
            &lyrae_group,
            lyrae_group_ai.key,
            &lyrae_cache,
            &mut event_queue,
            &mut perp_market,
            lyrae_cache.get_price(market_index),
            &mut lyrae_account,
            lyrae_account_ai.key,
            market_index,
            side,
            price,
            quantity,
            order_type,
            client_order_id,
            now_ts,
            referrer_lyrae_account_ai,
        )?;

        health_cache.update_perp_val(&lyrae_group, &lyrae_cache, &lyrae_account, market_index)?;
        let post_health = health_cache.get_health(&lyrae_group, HealthType::Init);
        check!(
            post_health >= ZERO_I80F48 || (health_up_only && post_health >= pre_health),
            LyraeErrorCode::InsufficientFunds
        )
    }

    #[inline(never)]
    fn cancel_perp_order_by_client_id(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        client_order_id: u64,
    ) -> LyraeResult<()> {
        const NUM_FIXED: usize = 6;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,     // read
            lyrae_account_ai,   // write
            owner_ai,           // read, signer
            perp_market_ai,     // write
            bids_ai,            // write
            asks_ai,            // write
        ] = accounts;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;

        let mut perp_market =
            PerpMarket::load_mut_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;

        let market_index = lyrae_group.find_perp_market_index(perp_market_ai.key).unwrap();

        let (order_id, side) = lyrae_account
            .find_order_with_client_id(market_index, client_order_id)
            .ok_or(throw_err!(LyraeErrorCode::ClientIdNotFound))?;

        let mut book = Book::load_checked(program_id, bids_ai, asks_ai, &perp_market)?;
        let best_final = if perp_market.meta_data.version == 0 {
            match side {
                Side::Bid => book.get_best_bid_price().unwrap(),
                Side::Ask => book.get_best_ask_price().unwrap(),
            }
        } else {
            let max_depth: i64 = perp_market.liquidity_mining_info.max_depth_bps.to_num();
            match side {
                Side::Bid => book.get_bids_size_above_order(order_id, max_depth),
                Side::Ask => book.get_asks_size_below_order(order_id, max_depth),
            }
        };

        let order = book.cancel_order(order_id, side)?;
        check_eq!(&order.owner, lyrae_account_ai.key, LyraeErrorCode::InvalidOrderId)?;
        lyrae_account.remove_order(order.owner_slot as usize, order.quantity)?;

        // If order version doesn't match the perp market version, no incentives
        if order.version != perp_market.meta_data.version {
            return Ok(());
        }

        let lyr_start = lyrae_account.perp_accounts[market_index].lyr_accrued;
        if perp_market.meta_data.version == 0 {
            lyrae_account.perp_accounts[market_index].apply_price_incentives(
                &mut perp_market,
                side,
                order.price(),
                order.best_initial,
                best_final,
                order.timestamp,
                Clock::get()?.unix_timestamp as u64,
                order.quantity,
            )?;
        } else {
            lyrae_account.perp_accounts[market_index].apply_size_incentives(
                &mut perp_market,
                order.best_initial,
                best_final,
                order.timestamp,
                Clock::get()?.unix_timestamp as u64,
                order.quantity,
            )?;
        }

        lyrae_emit!(LyrAccrualLog {
            lyrae_group: *lyrae_group_ai.key,
            lyrae_account: *lyrae_account_ai.key,
            market_index: market_index as u64,
            lyr_accrual: lyrae_account.perp_accounts[market_index].lyr_accrued - lyr_start
        });

        Ok(())
    }

    #[inline(never)]
    fn cancel_perp_order(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        order_id: i128,
    ) -> LyraeResult<()> {
        const NUM_FIXED: usize = 6;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,     // read
            lyrae_account_ai,   // write
            owner_ai,           // read, signer
            perp_market_ai,     // write
            bids_ai,            // write
            asks_ai,            // write
        ] = accounts;

        // TODO OPT put the liquidity incentive stuff in the bids and asks accounts so perp market
        //  doesn't have to be passed in as write

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;

        let mut perp_market =
            PerpMarket::load_mut_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;

        let market_index = lyrae_group.find_perp_market_index(perp_market_ai.key).unwrap();
        let side = lyrae_account
            .find_order_side(market_index, order_id)
            .ok_or(throw_err!(LyraeErrorCode::InvalidOrderId))?;
        let mut book = Book::load_checked(program_id, bids_ai, asks_ai, &perp_market)?;

        let best_final = if perp_market.meta_data.version == 0 {
            match side {
                Side::Bid => book.get_best_bid_price().unwrap(),
                Side::Ask => book.get_best_ask_price().unwrap(),
            }
        } else {
            let max_depth: i64 = perp_market.liquidity_mining_info.max_depth_bps.to_num();
            match side {
                Side::Bid => book.get_bids_size_above_order(order_id, max_depth),
                Side::Ask => book.get_asks_size_below_order(order_id, max_depth),
            }
        };

        let order = book.cancel_order(order_id, side)?;
        check_eq!(&order.owner, lyrae_account_ai.key, LyraeErrorCode::InvalidOrderId)?;
        lyrae_account.remove_order(order.owner_slot as usize, order.quantity)?;

        // If order version doesn't match the perp market version, no incentives
        if order.version != perp_market.meta_data.version {
            return Ok(());
        }

        let lyr_start = lyrae_account.perp_accounts[market_index].lyr_accrued;
        if perp_market.meta_data.version == 0 {
            lyrae_account.perp_accounts[market_index].apply_price_incentives(
                &mut perp_market,
                side,
                order.price(),
                order.best_initial,
                best_final,
                order.timestamp,
                Clock::get()?.unix_timestamp as u64,
                order.quantity,
            )?;
        } else {
            lyrae_account.perp_accounts[market_index].apply_size_incentives(
                &mut perp_market,
                order.best_initial,
                best_final,
                order.timestamp,
                Clock::get()?.unix_timestamp as u64,
                order.quantity,
            )?;
        }

        lyrae_emit!(LyrAccrualLog {
            lyrae_group: *lyrae_group_ai.key,
            lyrae_account: *lyrae_account_ai.key,
            market_index: market_index as u64,
            lyr_accrual: lyrae_account.perp_accounts[market_index].lyr_accrued - lyr_start
        });

        Ok(())
    }

    #[inline(never)]
    fn cancel_all_perp_orders(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        limit: u8,
    ) -> LyraeResult {
        const NUM_FIXED: usize = 6;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,     // read
            lyrae_account_ai,   // write
            owner_ai,           // read, signer
            perp_market_ai,     // write
            bids_ai,            // write
            asks_ai,            // write
        ] = accounts;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;

        let mut perp_market =
            PerpMarket::load_mut_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;

        let market_index = lyrae_group.find_perp_market_index(perp_market_ai.key).unwrap();

        let mut book = Book::load_checked(program_id, bids_ai, asks_ai, &perp_market)?;
        let lyr_start = lyrae_account.perp_accounts[market_index].lyr_accrued;

        if perp_market.meta_data.version == 0 {
            book.cancel_all_with_price_incentives(
                &mut lyrae_account,
                &mut perp_market,
                market_index,
                limit,
            )?;
        } else {
            let (all_order_ids, canceled_order_ids) = book.cancel_all_with_size_incentives(
                &mut lyrae_account,
                &mut perp_market,
                market_index,
                limit,
            )?;
            lyrae_emit!(CancelAllPerpOrdersLog {
                lyrae_group: *lyrae_group_ai.key,
                lyrae_account: *lyrae_account_ai.key,
                market_index: market_index as u64,
                all_order_ids,
                canceled_order_ids
            });
        }

        lyrae_emit!(LyrAccrualLog {
            lyrae_group: *lyrae_group_ai.key,
            lyrae_account: *lyrae_account_ai.key,
            market_index: market_index as u64,
            lyr_accrual: lyrae_account.perp_accounts[market_index].lyr_accrued - lyr_start
        });
        Ok(())
    }

    #[inline(never)]
    /// Take two LyraeAccount and settle quote currency pnl between them
    fn settle_pnl(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        market_index: usize,
    ) -> LyraeResult<()> {
        // TODO - what if someone has no collateral except other perps contracts
        //  maybe you don't allow people to withdraw if they don't have enough
        //  when liquidating, make sure you settle their pnl first?
        // TODO consider doing this in batches of 32 accounts that are close to zero sum
        // TODO write unit tests for this function

        const NUM_FIXED: usize = 6;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,     // read
            lyrae_account_a_ai, // write
            lyrae_account_b_ai, // write
            lyrae_cache_ai,     // read
            root_bank_ai,       // read
            node_bank_ai,       // write
        ] = accounts;
        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;

        let mut lyrae_account_a =
            LyraeAccount::load_mut_checked(lyrae_account_a_ai, program_id, lyrae_group_ai.key)?;
        check!(!lyrae_account_a.is_bankrupt, LyraeErrorCode::Bankrupt)?;

        let mut lyrae_account_b =
            LyraeAccount::load_mut_checked(lyrae_account_b_ai, program_id, lyrae_group_ai.key)?;
        check!(!lyrae_account_b.is_bankrupt, LyraeErrorCode::Bankrupt)?;

        match lyrae_group.find_root_bank_index(root_bank_ai.key) {
            None => return Err(throw_err!(LyraeErrorCode::Default)),
            Some(i) => check!(i == QUOTE_INDEX, LyraeErrorCode::Default)?,
        }
        let root_bank = RootBank::load_checked(root_bank_ai, program_id)?;
        let mut node_bank = NodeBank::load_mut_checked(node_bank_ai, program_id)?;
        check!(root_bank.node_banks.contains(node_bank_ai.key), LyraeErrorCode::Default)?;

        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        let now_ts = Clock::get()?.unix_timestamp as u64;

        let root_bank_cache = &lyrae_cache.root_bank_cache[QUOTE_INDEX];
        let price_cache = &lyrae_cache.price_cache[market_index];
        let perp_market_cache = &lyrae_cache.perp_market_cache[market_index];

        root_bank_cache.check_valid(&lyrae_group, now_ts)?;
        price_cache.check_valid(&lyrae_group, now_ts)?;
        perp_market_cache.check_valid(&lyrae_group, now_ts)?;

        let price = price_cache.price;

        let a = &mut lyrae_account_a.perp_accounts[market_index];
        let b = &mut lyrae_account_b.perp_accounts[market_index];

        // Account for unrealized funding payments before settling
        a.settle_funding(perp_market_cache);
        b.settle_funding(perp_market_cache);

        let contract_size = lyrae_group.perp_markets[market_index].base_lot_size;
        let new_quote_pos_a = I80F48::from_num(-a.base_position * contract_size) * price;
        let new_quote_pos_b = I80F48::from_num(-b.base_position * contract_size) * price;
        let a_pnl: I80F48 = a.quote_position - new_quote_pos_a;
        let b_pnl: I80F48 = b.quote_position - new_quote_pos_b;

        // pnl must be opposite signs for there to be a settlement
        if a_pnl * b_pnl > 0 {
            return Ok(());
        }

        let settlement = a_pnl.abs().min(b_pnl.abs());
        let a_settle = if a_pnl > 0 { settlement } else { -settlement };
        a.transfer_quote_position(b, a_settle);

        transfer_token_internal(
            &root_bank_cache,
            &mut node_bank,
            &mut lyrae_account_b,
            &mut lyrae_account_a,
            lyrae_account_b_ai.key,
            lyrae_account_a_ai.key,
            QUOTE_INDEX,
            a_settle,
        )?;

        lyrae_emit!(SettlePnlLog {
            lyrae_group: *lyrae_group_ai.key,
            lyrae_account_a: *lyrae_account_a_ai.key,
            lyrae_account_b: *lyrae_account_b_ai.key,
            market_index: market_index as u64,
            settlement: a_settle.to_bits(), // Will be positive if a has positive pnl and settling with b
        });
        emit_perp_balances(
            *lyrae_group_ai.key,
            *lyrae_account_a_ai.key,
            market_index as u64,
            &lyrae_account_a.perp_accounts[market_index],
            perp_market_cache,
        );
        emit_perp_balances(
            *lyrae_group_ai.key,
            *lyrae_account_b_ai.key,
            market_index as u64,
            &lyrae_account_b.perp_accounts[market_index],
            perp_market_cache,
        );

        Ok(())
    }

    #[inline(never)]
    /// Take an account that has losses in the selected perp market to account for fees_accrued
    fn settle_fees(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 10;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,     // read
            lyrae_cache_ai,     // read
            perp_market_ai,     // write
            lyrae_account_ai,   // write
            root_bank_ai,       // read
            node_bank_ai,       // write
            bank_vault_ai,      // write
            fees_vault_ai,      // write
            signer_ai,          // read
            token_prog_ai,      // read
        ] = accounts;
        check_eq!(token_prog_ai.key, &spl_token::ID, LyraeErrorCode::InvalidProgramId)?;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check!(fees_vault_ai.key == &lyrae_group.fees_vault, LyraeErrorCode::InvalidVault)?;
        check!(signer_ai.key == &lyrae_group.signer_key, LyraeErrorCode::InvalidSignerKey)?;

        let mut perp_market =
            PerpMarket::load_mut_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;
        let market_index = lyrae_group.find_perp_market_index(perp_market_ai.key).unwrap();

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;

        check!(
            &lyrae_group.tokens[QUOTE_INDEX].root_bank == root_bank_ai.key,
            LyraeErrorCode::InvalidRootBank
        )?;
        let root_bank = RootBank::load_checked(root_bank_ai, program_id)?;
        let mut node_bank = NodeBank::load_mut_checked(node_bank_ai, program_id)?;
        check!(root_bank.node_banks.contains(node_bank_ai.key), LyraeErrorCode::InvalidNodeBank)?;
        check!(bank_vault_ai.key == &node_bank.vault, LyraeErrorCode::InvalidVault)?;

        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        let now_ts = Clock::get()?.unix_timestamp as u64;

        let root_bank_cache = &lyrae_cache.root_bank_cache[QUOTE_INDEX];
        let price_cache = &lyrae_cache.price_cache[market_index];
        let perp_market_cache = &lyrae_cache.perp_market_cache[market_index];

        root_bank_cache.check_valid(&lyrae_group, now_ts)?;
        price_cache.check_valid(&lyrae_group, now_ts)?;
        perp_market_cache.check_valid(&lyrae_group, now_ts)?;

        let price = price_cache.price;

        let pa = &mut lyrae_account.perp_accounts[market_index];
        pa.settle_funding(&perp_market_cache);
        let contract_size = lyrae_group.perp_markets[market_index].base_lot_size;
        let new_quote_pos = I80F48::from_num(-pa.base_position * contract_size) * price;
        let pnl: I80F48 = pa.quote_position - new_quote_pos;
        check!(pnl.is_negative(), LyraeErrorCode::Default)?;
        check!(perp_market.fees_accrued.is_positive(), LyraeErrorCode::Default)?;

        let settlement = pnl.abs().min(perp_market.fees_accrued).checked_floor().unwrap();

        perp_market.fees_accrued -= settlement;
        pa.quote_position += settlement;

        // Transfer quote token from bank vault to fees vault owned by Lyrae DAO
        let signers_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
        invoke_transfer(
            token_prog_ai,
            bank_vault_ai,
            fees_vault_ai,
            signer_ai,
            &[&signers_seeds],
            settlement.to_num(),
        )?;

        // Decrement deposits on lyrae account
        checked_change_net(
            root_bank_cache,
            &mut node_bank,
            &mut lyrae_account,
            lyrae_account_ai.key,
            QUOTE_INDEX,
            -settlement,
        )?;

        lyrae_emit!(SettleFeesLog {
            lyrae_group: *lyrae_group_ai.key,
            lyrae_account: *lyrae_account_ai.key,
            market_index: market_index as u64,
            settlement: settlement.to_bits()
        });

        emit_perp_balances(
            *lyrae_group_ai.key,
            *lyrae_account_ai.key,
            market_index as u64,
            &lyrae_account.perp_accounts[market_index],
            perp_market_cache,
        );

        Ok(())
    }

    #[inline(never)]
    fn force_cancel_spot_orders(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        limit: u8,
    ) -> LyraeResult<()> {
        const NUM_FIXED: usize = 19;
        let accounts = array_ref![accounts, 0, NUM_FIXED + MAX_PAIRS];
        let (fixed_ais, liqee_open_orders_ais) = array_refs![accounts, NUM_FIXED, MAX_PAIRS];

        let [
            lyrae_group_ai,         // read
            lyrae_cache_ai,         // read
            liqee_lyrae_account_ai, // write
            base_root_bank_ai,      // read
            base_node_bank_ai,      // write
            base_vault_ai,          // write
            quote_root_bank_ai,     // read
            quote_node_bank_ai,     // write
            quote_vault_ai,         // write

            spot_market_ai,         // write
            bids_ai,                // write
            asks_ai,                // write
            signer_ai,              // read
            dex_event_queue_ai,     // write
            dex_base_ai,            // write
            dex_quote_ai,           // write
            dex_signer_ai,          // read
            dex_prog_ai,            // read
            token_prog_ai,          // read
        ] = fixed_ais;

        // Check token program id
        check_eq!(token_prog_ai.key, &spl_token::ID, LyraeErrorCode::InvalidProgramId)?;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check_eq!(dex_prog_ai.key, &lyrae_group.dex_program_id, LyraeErrorCode::InvalidProgramId)?;
        check!(signer_ai.key == &lyrae_group.signer_key, LyraeErrorCode::InvalidSignerKey)?;

        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        let mut liqee_ma =
            LyraeAccount::load_mut_checked(liqee_lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(!liqee_ma.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        liqee_ma.check_open_orders(&lyrae_group, liqee_open_orders_ais)?;

        let market_index = lyrae_group.find_spot_market_index(spot_market_ai.key).unwrap();
        check!(liqee_ma.in_margin_basket[market_index], LyraeErrorCode::Default)?;

        check_eq!(
            &lyrae_group.tokens[market_index].root_bank,
            base_root_bank_ai.key,
            LyraeErrorCode::InvalidRootBank
        )?;
        let base_root_bank = RootBank::load_checked(base_root_bank_ai, program_id)?;

        check!(
            base_root_bank.node_banks.contains(base_node_bank_ai.key),
            LyraeErrorCode::InvalidNodeBank
        )?;
        let mut base_node_bank = NodeBank::load_mut_checked(base_node_bank_ai, program_id)?;
        check_eq!(&base_node_bank.vault, base_vault_ai.key, LyraeErrorCode::InvalidVault)?;

        check_eq!(
            &lyrae_group.tokens[QUOTE_INDEX].root_bank,
            quote_root_bank_ai.key,
            LyraeErrorCode::InvalidRootBank
        )?;
        let quote_root_bank = RootBank::load_checked(quote_root_bank_ai, program_id)?;

        check!(
            quote_root_bank.node_banks.contains(quote_node_bank_ai.key),
            LyraeErrorCode::InvalidNodeBank
        )?;
        let mut quote_node_bank = NodeBank::load_mut_checked(quote_node_bank_ai, program_id)?;
        check_eq!(&quote_node_bank.vault, quote_vault_ai.key, LyraeErrorCode::InvalidVault)?;

        let now_ts = Clock::get()?.unix_timestamp as u64;

        let liqee_active_assets = UserActiveAssets::new(&lyrae_group, &liqee_ma, vec![]);

        lyrae_cache.check_valid(&lyrae_group, &liqee_active_assets, now_ts)?;

        let mut health_cache = HealthCache::new(liqee_active_assets);
        health_cache.init_vals(&lyrae_group, &lyrae_cache, &liqee_ma, liqee_open_orders_ais)?;
        let init_health = health_cache.get_health(&lyrae_group, HealthType::Init);
        let maint_health = health_cache.get_health(&lyrae_group, HealthType::Maint);

        // Can only force cancel on an account already being liquidated
        if liqee_ma.being_liquidated {
            if init_health > ZERO_I80F48 {
                liqee_ma.being_liquidated = false;
                msg!("Account init_health above zero.");
                return Ok(());
            }
        } else if maint_health >= ZERO_I80F48 {
            return Err(throw_err!(LyraeErrorCode::NotLiquidatable));
        } else {
            liqee_ma.being_liquidated = true;
        }

        // Cancel orders up to the limit
        let open_orders_ai = &liqee_open_orders_ais[market_index];
        let signers_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
        invoke_cancel_orders(
            open_orders_ai,
            dex_prog_ai,
            spot_market_ai,
            bids_ai,
            asks_ai,
            signer_ai,
            dex_event_queue_ai,
            &[&signers_seeds],
            limit,
        )?;

        let (pre_base, pre_quote) = {
            let open_orders = load_open_orders(open_orders_ai)?;
            (
                open_orders.native_coin_free,
                open_orders.native_pc_free + open_orders.referrer_rebates_accrued,
            )
        };

        if pre_base == 0 && pre_quote == 0 {
            // margin basket may be in an invalid state; correct it before returning
            let open_orders = load_open_orders(open_orders_ai)?;
            liqee_ma.update_basket(market_index, &open_orders)?;
            return Ok(());
        }

        // Settle funds released by canceling open orders
        // TODO OPT add a new ForceSettleFunds to save compute in this instruction
        invoke_settle_funds(
            dex_prog_ai,
            spot_market_ai,
            open_orders_ai,
            signer_ai,
            dex_base_ai,
            dex_quote_ai,
            base_vault_ai,
            quote_vault_ai,
            dex_signer_ai,
            token_prog_ai,
            &[&signers_seeds],
        )?;

        let (post_base, post_quote) = {
            let open_orders = load_open_orders(open_orders_ai)?;
            liqee_ma.update_basket(market_index, &open_orders)?;
            lyrae_emit!(OpenOrdersBalanceLog {
                lyrae_group: *lyrae_group_ai.key,
                lyrae_account: *liqee_lyrae_account_ai.key,
                market_index: market_index as u64,
                base_total: open_orders.native_coin_total,
                base_free: open_orders.native_coin_free,
                quote_total: open_orders.native_pc_total,
                quote_free: open_orders.native_pc_free,
                referrer_rebates_accrued: open_orders.referrer_rebates_accrued
            });

            (
                open_orders.native_coin_free,
                open_orders.native_pc_free + open_orders.referrer_rebates_accrued,
            )
        };

        check!(post_base <= pre_base, LyraeErrorCode::Default)?;
        check!(post_quote <= pre_quote, LyraeErrorCode::Default)?;

        // Update balances from settling funds
        let base_change = I80F48::from_num(pre_base - post_base);
        let quote_change = I80F48::from_num(pre_quote - post_quote);

        checked_change_net(
            &lyrae_cache.root_bank_cache[market_index],
            &mut base_node_bank,
            &mut liqee_ma,
            liqee_lyrae_account_ai.key,
            market_index,
            base_change,
        )?;
        checked_change_net(
            &lyrae_cache.root_bank_cache[QUOTE_INDEX],
            &mut quote_node_bank,
            &mut liqee_ma,
            liqee_lyrae_account_ai.key,
            QUOTE_INDEX,
            quote_change,
        )
    }

    #[inline(never)]
    fn force_cancel_perp_orders(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        limit: u8,
    ) -> LyraeResult<()> {
        const NUM_FIXED: usize = 6;
        let accounts = array_ref![accounts, 0, NUM_FIXED + MAX_PAIRS];
        let (fixed_ais, liqee_open_orders_ais) = array_refs![accounts, NUM_FIXED, MAX_PAIRS];

        let [
            lyrae_group_ai,         // read
            lyrae_cache_ai,         // read
            perp_market_ai,         // read
            bids_ai,                // write
            asks_ai,                // write
            liqee_lyrae_account_ai, // write
        ] = fixed_ais;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;

        let mut liqee_ma =
            LyraeAccount::load_mut_checked(liqee_lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(!liqee_ma.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        liqee_ma.check_open_orders(&lyrae_group, liqee_open_orders_ais)?;

        let perp_market = PerpMarket::load_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;
        let market_index = lyrae_group.find_perp_market_index(perp_market_ai.key).unwrap();
        let perp_market_info = &lyrae_group.perp_markets[market_index];
        check!(!perp_market_info.is_empty(), LyraeErrorCode::InvalidMarket)?;

        let now_ts = Clock::get()?.unix_timestamp as u64;

        let liqee_active_assets = UserActiveAssets::new(&lyrae_group, &liqee_ma, vec![]);

        lyrae_cache.check_valid(&lyrae_group, &liqee_active_assets, now_ts)?;

        let mut health_cache = HealthCache::new(liqee_active_assets);
        health_cache.init_vals(&lyrae_group, &lyrae_cache, &liqee_ma, liqee_open_orders_ais)?;
        let init_health = health_cache.get_health(&lyrae_group, HealthType::Init);
        let maint_health = health_cache.get_health(&lyrae_group, HealthType::Maint);

        if liqee_ma.being_liquidated {
            if init_health > ZERO_I80F48 {
                liqee_ma.being_liquidated = false;
                msg!("Account init_health above zero.");
                return Ok(());
            }
        } else if maint_health >= ZERO_I80F48 {
            msg!(
                "maint health {} init health {}",
                maint_health.to_num::<f64>(),
                init_health.to_num::<f64>()
            );
            return Err(throw_err!(LyraeErrorCode::NotLiquidatable));
        } else {
            liqee_ma.being_liquidated = true;
        }

        let mut book = Book::load_checked(program_id, bids_ai, asks_ai, &perp_market)?;
        book.cancel_all(&mut liqee_ma, market_index, limit)
    }

    #[inline(never)]
    /// Liquidator takes some of borrows at token at `liab_index` and receives some deposits from
    /// the token at `asset_index`
    /// Requires: `liab_index != asset_index`
    fn liquidate_token_and_token(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        max_liab_transfer: I80F48,
    ) -> LyraeResult<()> {
        // parameter checks
        check!(max_liab_transfer.is_positive(), LyraeErrorCode::InvalidParam)?;

        const NUM_FIXED: usize = 9;
        let accounts = array_ref![accounts, 0, NUM_FIXED + 2 * MAX_PAIRS];
        let (fixed_ais, liqee_open_orders_ais, liqor_open_orders_ais) =
            array_refs![accounts, NUM_FIXED, MAX_PAIRS, MAX_PAIRS];

        let [
            lyrae_group_ai,         // read
            lyrae_cache_ai,         // read
            liqee_lyrae_account_ai, // write
            liqor_lyrae_account_ai, // write
            liqor_ai,               // read, signer
            asset_root_bank_ai,     // read
            asset_node_bank_ai,     // write
            liab_root_bank_ai,      // read
            liab_node_bank_ai,      // write
        ] = fixed_ais;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        let mut liqee_ma =
            LyraeAccount::load_mut_checked(liqee_lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(!liqee_ma.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        liqee_ma.check_open_orders(&lyrae_group, liqee_open_orders_ais)?;

        let mut liqor_ma =
            LyraeAccount::load_mut_checked(liqor_lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(
            &liqor_ma.owner == liqor_ai.key || &liqor_ma.delegate == liqor_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(liqor_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        check!(!liqor_ma.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        liqor_ma.check_open_orders(&lyrae_group, liqor_open_orders_ais)?;

        let asset_root_bank = RootBank::load_checked(asset_root_bank_ai, program_id)?;
        let asset_index = lyrae_group.find_root_bank_index(asset_root_bank_ai.key).unwrap();
        let mut asset_node_bank = NodeBank::load_mut_checked(asset_node_bank_ai, program_id)?;
        check!(
            asset_root_bank.node_banks.contains(asset_node_bank_ai.key),
            LyraeErrorCode::InvalidNodeBank
        )?;

        let liab_root_bank = RootBank::load_checked(liab_root_bank_ai, program_id)?;
        let liab_index = lyrae_group.find_root_bank_index(liab_root_bank_ai.key).unwrap();
        let mut liab_node_bank = NodeBank::load_mut_checked(liab_node_bank_ai, program_id)?;
        check!(liab_root_bank.node_banks.contains(liab_node_bank_ai.key), LyraeErrorCode::Default)?;
        check!(asset_index != liab_index, LyraeErrorCode::InvalidParam)?;

        let now_ts = Clock::get()?.unix_timestamp as u64;
        let liqee_active_assets = UserActiveAssets::new(&lyrae_group, &liqee_ma, vec![]);
        let liqor_active_assets = UserActiveAssets::new(
            &lyrae_group,
            &liqor_ma,
            vec![(AssetType::Token, asset_index), (AssetType::Token, liab_index)],
        );

        lyrae_cache.check_valid(
            &lyrae_group,
            &UserActiveAssets::merge(&liqee_active_assets, &liqor_active_assets),
            now_ts,
        )?;

        // Make sure orders are cancelled for perps and check orders
        for i in 0..lyrae_group.num_oracles {
            if liqee_active_assets.perps[i] {
                check!(liqee_ma.perp_accounts[i].has_no_open_orders(), LyraeErrorCode::Default)?;
            }
        }

        let mut health_cache = HealthCache::new(liqee_active_assets);
        health_cache.init_vals(&lyrae_group, &lyrae_cache, &liqee_ma, liqee_open_orders_ais)?;
        let init_health = health_cache.get_health(&lyrae_group, HealthType::Init);
        let maint_health = health_cache.get_health(&lyrae_group, HealthType::Maint);

        if liqee_ma.being_liquidated {
            if init_health > ZERO_I80F48 {
                liqee_ma.being_liquidated = false;
                msg!("Account init_health above zero.");
                return Ok(());
            }
        } else if maint_health >= ZERO_I80F48 {
            return Err(throw_err!(LyraeErrorCode::NotLiquidatable));
        } else {
            liqee_ma.being_liquidated = true;
        }

        check!(liqee_ma.deposits[asset_index].is_positive(), LyraeErrorCode::Default)?;
        check!(liqee_ma.borrows[liab_index].is_positive(), LyraeErrorCode::Default)?;

        let asset_bank = &lyrae_cache.root_bank_cache[asset_index];
        let liab_bank = &lyrae_cache.root_bank_cache[liab_index];

        let asset_price = lyrae_cache.get_price(asset_index);
        let liab_price = lyrae_cache.get_price(liab_index);

        let (asset_fee, init_asset_weight) = if asset_index == QUOTE_INDEX {
            (ONE_I80F48, ONE_I80F48)
        } else {
            let asset_info = &lyrae_group.spot_markets[asset_index];
            check!(!asset_info.is_empty(), LyraeErrorCode::InvalidMarket)?;
            (ONE_I80F48 + asset_info.liquidation_fee, asset_info.init_asset_weight)
        };

        let (liab_fee, init_liab_weight) = if liab_index == QUOTE_INDEX {
            (ONE_I80F48, ONE_I80F48)
        } else {
            let liab_info = &lyrae_group.spot_markets[liab_index];
            check!(!liab_info.is_empty(), LyraeErrorCode::InvalidMarket)?;
            (ONE_I80F48 - liab_info.liquidation_fee, liab_info.init_liab_weight)
        };

        // Max liab transferred to reach init_health == 0
        let deficit_max_liab: I80F48 = -init_health
            / (liab_price * (init_liab_weight - init_asset_weight * asset_fee / liab_fee));

        let native_deposits = liqee_ma.get_native_deposit(asset_bank, asset_index)?;
        let native_borrows = liqee_ma.get_native_borrow(liab_bank, liab_index)?;

        // Max liab transferred to reach asset_i == 0
        let asset_implied_liab_transfer =
            native_deposits * asset_price * liab_fee / (liab_price * asset_fee);
        let actual_liab_transfer = min(
            min(min(deficit_max_liab, native_borrows), max_liab_transfer),
            asset_implied_liab_transfer,
        );

        // Transfer into liqee to reduce liabilities
        checked_change_net(
            &liab_bank,
            &mut liab_node_bank,
            &mut liqee_ma,
            liqee_lyrae_account_ai.key,
            liab_index,
            actual_liab_transfer,
        )?; // TODO make sure deposits for this index is == 0

        // Transfer from liqor
        checked_change_net(
            &liab_bank,
            &mut liab_node_bank,
            &mut liqor_ma,
            liqor_lyrae_account_ai.key,
            liab_index,
            -actual_liab_transfer,
        )?;

        let asset_transfer =
            actual_liab_transfer * liab_price * asset_fee / (liab_fee * asset_price);

        // Transfer collater into liqor
        checked_change_net(
            &asset_bank,
            &mut asset_node_bank,
            &mut liqor_ma,
            liqor_lyrae_account_ai.key,
            asset_index,
            asset_transfer,
        )?;

        // Transfer collateral out of liqee
        checked_change_net(
            &asset_bank,
            &mut asset_node_bank,
            &mut liqee_ma,
            liqee_lyrae_account_ai.key,
            asset_index,
            -asset_transfer,
        )?;

        let mut liqor_health_cache = HealthCache::new(liqor_active_assets);
        liqor_health_cache.init_vals(
            &lyrae_group,
            &lyrae_cache,
            &liqor_ma,
            liqor_open_orders_ais,
        )?;
        let liqor_health = liqor_health_cache.get_health(&lyrae_group, HealthType::Init);
        check!(liqor_health >= ZERO_I80F48, LyraeErrorCode::InsufficientFunds)?;

        // Update liqee's health where it may have changed
        for &i in &[asset_index, liab_index] {
            health_cache.update_token_val(
                &lyrae_group,
                &lyrae_cache,
                &liqee_ma,
                liqee_open_orders_ais,
                i,
            )?;
        }
        let liqee_maint_health = health_cache.get_health(&lyrae_group, HealthType::Maint);
        if liqee_maint_health < ZERO_I80F48 {
            liqee_ma.is_bankrupt =
                liqee_ma.check_enter_bankruptcy(&lyrae_group, liqee_open_orders_ais);
        } else {
            let liqee_init_health = health_cache.get_health(&lyrae_group, HealthType::Init);

            // this is equivalent to one native USDC or 1e-6 USDC
            // This is used as threshold to flip flag instead of 0 because of dust issues
            liqee_ma.being_liquidated = liqee_init_health < NEG_ONE_I80F48;
        }

        lyrae_emit!(LiquidateTokenAndTokenLog {
            lyrae_group: *lyrae_group_ai.key,
            liqee: *liqee_lyrae_account_ai.key,
            liqor: *liqor_lyrae_account_ai.key,
            asset_index: asset_index as u64,
            liab_index: liab_index as u64,
            asset_transfer: asset_transfer.to_bits(),
            liab_transfer: actual_liab_transfer.to_bits(),
            asset_price: asset_price.to_bits(),
            liab_price: liab_price.to_bits(),
            bankruptcy: liqee_ma.is_bankrupt
        });

        Ok(())
    }

    #[inline(never)]
    /// swap tokens for perp quote position only and only if the base position in that market is 0
    fn liquidate_token_and_perp(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        asset_type: AssetType,
        asset_index: usize,
        liab_type: AssetType,
        liab_index: usize,
        max_liab_transfer: I80F48,
    ) -> LyraeResult<()> {
        check!(max_liab_transfer.is_positive(), LyraeErrorCode::InvalidParam)?;
        check!(asset_type != liab_type, LyraeErrorCode::InvalidParam)?;

        const NUM_FIXED: usize = 7;
        let accounts = array_ref![accounts, 0, NUM_FIXED + 2 * MAX_PAIRS];
        let (fixed_ais, liqee_open_orders_ais, liqor_open_orders_ais) =
            array_refs![accounts, NUM_FIXED, MAX_PAIRS, MAX_PAIRS];

        let [
            lyrae_group_ai,         // read
            lyrae_cache_ai,         // read
            liqee_lyrae_account_ai, // write
            liqor_lyrae_account_ai, // write
            liqor_ai,               // read, signer
            root_bank_ai,           // read
            node_bank_ai,           // write
        ] = fixed_ais;
        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        let mut liqee_ma =
            LyraeAccount::load_mut_checked(liqee_lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(!liqee_ma.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        liqee_ma.check_open_orders(&lyrae_group, liqee_open_orders_ais)?;

        let mut liqor_ma =
            LyraeAccount::load_mut_checked(liqor_lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(
            &liqor_ma.owner == liqor_ai.key || &liqor_ma.delegate == liqor_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(liqor_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        check!(!liqor_ma.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        liqor_ma.check_open_orders(&lyrae_group, liqor_open_orders_ais)?;

        let root_bank = RootBank::load_checked(root_bank_ai, program_id)?;
        let mut node_bank = NodeBank::load_mut_checked(node_bank_ai, program_id)?;
        check!(root_bank.node_banks.contains(node_bank_ai.key), LyraeErrorCode::InvalidNodeBank)?;

        let now_ts = Clock::get()?.unix_timestamp as u64;
        let liqee_active_assets = UserActiveAssets::new(&lyrae_group, &liqee_ma, vec![]);
        let liqor_active_assets = UserActiveAssets::new(
            &lyrae_group,
            &liqor_ma,
            vec![(asset_type, asset_index), (liab_type, liab_index)],
        );

        lyrae_cache.check_valid(
            &lyrae_group,
            &UserActiveAssets::merge(&liqee_active_assets, &liqor_active_assets),
            now_ts,
        )?;

        // Make sure orders are cancelled for perps and check orders
        for i in 0..lyrae_group.num_oracles {
            if liqee_active_assets.perps[i] {
                check!(liqee_ma.perp_accounts[i].has_no_open_orders(), LyraeErrorCode::Default)?;
            }
        }

        let mut health_cache = HealthCache::new(liqee_active_assets);
        health_cache.init_vals(&lyrae_group, &lyrae_cache, &liqee_ma, liqee_open_orders_ais)?;
        let init_health = health_cache.get_health(&lyrae_group, HealthType::Init);
        let maint_health = health_cache.get_health(&lyrae_group, HealthType::Maint);

        if liqee_ma.being_liquidated {
            if init_health > ZERO_I80F48 {
                liqee_ma.being_liquidated = false;
                msg!("Account init_health above zero.");
                return Ok(());
            }
        } else if maint_health >= ZERO_I80F48 {
            return Err(throw_err!(LyraeErrorCode::NotLiquidatable));
        } else {
            liqee_ma.being_liquidated = true;
        }

        let asset_price: I80F48;
        let liab_price: I80F48;
        let asset_transfer: I80F48;
        let actual_liab_transfer: I80F48;
        if asset_type == AssetType::Token {
            // we know asset_type != liab_type
            asset_price = lyrae_cache.get_price(asset_index);
            liab_price = ONE_I80F48;
            let bank_cache = &lyrae_cache.root_bank_cache[asset_index];
            check!(liqee_ma.deposits[asset_index].is_positive(), LyraeErrorCode::Default)?;
            check!(liab_index != QUOTE_INDEX, LyraeErrorCode::Default)?;
            check!(
                lyrae_group.find_root_bank_index(root_bank_ai.key).unwrap() == asset_index,
                LyraeErrorCode::InvalidRootBank
            )?;
            let native_borrows = -liqee_ma.perp_accounts[liab_index].quote_position;
            check!(liqee_ma.perp_accounts[liab_index].base_position == 0, LyraeErrorCode::Default)?;
            check!(native_borrows.is_positive(), LyraeErrorCode::Default)?;

            let (asset_fee, init_asset_weight) = if asset_index == QUOTE_INDEX {
                (ONE_I80F48, ONE_I80F48)
            } else {
                let asset_info = &lyrae_group.spot_markets[asset_index];
                check!(!asset_info.is_empty(), LyraeErrorCode::InvalidMarket)?;
                (ONE_I80F48 + asset_info.liquidation_fee, asset_info.init_asset_weight)
            };

            let liab_info = &lyrae_group.perp_markets[liab_index];
            check!(!liab_info.is_empty(), LyraeErrorCode::InvalidMarket)?;

            let (liab_fee, init_liab_weight) = (ONE_I80F48, ONE_I80F48);

            let native_deposits = liqee_ma.get_native_deposit(bank_cache, asset_index)?;

            // Max liab transferred to reach init_health == 0
            let deficit_max_liab = if asset_index == QUOTE_INDEX {
                native_deposits
            } else {
                -init_health
                    / (liab_price * (init_liab_weight - init_asset_weight * asset_fee / liab_fee))
            };

            // Max liab transferred to reach asset_i == 0
            let asset_implied_liab_transfer =
                native_deposits * asset_price * liab_fee / (liab_price * asset_fee);
            actual_liab_transfer = deficit_max_liab
                .min(native_borrows)
                .min(max_liab_transfer)
                .min(asset_implied_liab_transfer);

            // Transfer the negative quote position from liqee to liqor
            liqee_ma.perp_accounts[liab_index].transfer_quote_position(
                &mut liqor_ma.perp_accounts[liab_index],
                -actual_liab_transfer,
            );

            asset_transfer =
                actual_liab_transfer * liab_price * asset_fee / (liab_fee * asset_price);

            // Transfer collateral from liqee to liqor
            transfer_token_internal(
                bank_cache,
                &mut node_bank,
                &mut liqee_ma,
                &mut liqor_ma,
                liqee_lyrae_account_ai.key,
                liqor_lyrae_account_ai.key,
                asset_index,
                asset_transfer,
            )?;

            health_cache.update_token_val(
                &lyrae_group,
                &lyrae_cache,
                &liqee_ma,
                liqee_open_orders_ais,
                asset_index,
            )?;

            health_cache.update_perp_val(&lyrae_group, &lyrae_cache, &liqee_ma, liab_index)?;
        } else {
            asset_price = ONE_I80F48;
            liab_price = lyrae_cache.get_price(liab_index);
            check!(
                lyrae_group.find_root_bank_index(root_bank_ai.key).unwrap() == liab_index,
                LyraeErrorCode::InvalidRootBank
            )?;

            check!(liqee_ma.borrows[liab_index].is_positive(), LyraeErrorCode::Default)?;
            check!(asset_index != QUOTE_INDEX, LyraeErrorCode::Default)?;

            check!(
                liqee_ma.perp_accounts[asset_index].base_position == 0,
                LyraeErrorCode::Default
            )?;
            let native_deposits = liqee_ma.perp_accounts[asset_index].quote_position;
            check!(native_deposits.is_positive(), LyraeErrorCode::Default)?;

            let bank_cache = &lyrae_cache.root_bank_cache[liab_index];
            let (asset_fee, init_asset_weight) = (ONE_I80F48, ONE_I80F48);
            let (liab_fee, init_liab_weight) = if liab_index == QUOTE_INDEX {
                (ONE_I80F48, ONE_I80F48)
            } else {
                let liab_info = &lyrae_group.spot_markets[liab_index];
                check!(!liab_info.is_empty(), LyraeErrorCode::InvalidMarket)?;
                (ONE_I80F48 - liab_info.liquidation_fee, liab_info.init_liab_weight)
            };

            let native_borrows = liqee_ma.get_native_borrow(bank_cache, liab_index)?;
            let deficit_max_liab = if liab_index == QUOTE_INDEX {
                native_borrows
            } else {
                -init_health
                    / (liab_price * (init_liab_weight - init_asset_weight * asset_fee / liab_fee))
            };

            // Max liab transferred to reach asset_i == 0
            let asset_implied_liab_transfer =
                native_deposits * asset_price * liab_fee / (liab_price * asset_fee);
            actual_liab_transfer = deficit_max_liab
                .min(native_borrows)
                .min(max_liab_transfer)
                .min(asset_implied_liab_transfer);

            asset_transfer =
                actual_liab_transfer * liab_price * asset_fee / (liab_fee * asset_price);

            // Transfer liabilities from liqee to liqor (i.e. increase liqee and decrease liqor)
            transfer_token_internal(
                bank_cache,
                &mut node_bank,
                &mut liqor_ma,
                &mut liqee_ma,
                liqor_lyrae_account_ai.key,
                liqee_lyrae_account_ai.key,
                liab_index,
                actual_liab_transfer,
            )?;

            // Transfer positive quote position from liqee to liqor
            liqee_ma.perp_accounts[asset_index]
                .transfer_quote_position(&mut liqor_ma.perp_accounts[asset_index], asset_transfer);

            health_cache.update_token_val(
                &lyrae_group,
                &lyrae_cache,
                &liqee_ma,
                liqee_open_orders_ais,
                liab_index,
            )?;

            health_cache.update_perp_val(&lyrae_group, &lyrae_cache, &liqee_ma, asset_index)?;
        }

        let mut liqor_health_cache = HealthCache::new(liqor_active_assets);
        liqor_health_cache.init_vals(
            &lyrae_group,
            &lyrae_cache,
            &liqor_ma,
            liqor_open_orders_ais,
        )?;
        let liqor_health = liqor_health_cache.get_health(&lyrae_group, HealthType::Init);
        check!(liqor_health >= ZERO_I80F48, LyraeErrorCode::InsufficientFunds)?;

        let liqee_maint_health = health_cache.get_health(&lyrae_group, HealthType::Maint);
        if liqee_maint_health < ZERO_I80F48 {
            liqee_ma.is_bankrupt =
                liqee_ma.check_enter_bankruptcy(&lyrae_group, liqee_open_orders_ais);
        } else {
            let liqee_init_health = health_cache.get_health(&lyrae_group, HealthType::Init);
            // this is equivalent to one native USDC or 1e-6 USDC
            // This is used as threshold to flip flag instead of 0 because of dust issues
            liqee_ma.being_liquidated = liqee_init_health < NEG_ONE_I80F48;
        }

        lyrae_emit!(LiquidateTokenAndPerpLog {
            lyrae_group: *lyrae_group_ai.key,
            liqee: *liqee_lyrae_account_ai.key,
            liqor: *liqor_lyrae_account_ai.key,
            asset_index: asset_index as u64,
            liab_index: liab_index as u64,
            asset_type: asset_type as u8,
            liab_type: liab_type as u8,
            asset_transfer: asset_transfer.to_bits(),
            liab_transfer: actual_liab_transfer.to_bits(),
            asset_price: asset_price.to_bits(),
            liab_price: liab_price.to_bits(),
            bankruptcy: liqee_ma.is_bankrupt,
        });

        let perp_market_index: usize;
        if asset_type == AssetType::Token {
            perp_market_index = liab_index;
        } else {
            perp_market_index = asset_index;
        }
        emit_perp_balances(
            *lyrae_group_ai.key,
            *liqee_lyrae_account_ai.key,
            perp_market_index as u64,
            &liqee_ma.perp_accounts[perp_market_index],
            &lyrae_cache.perp_market_cache[perp_market_index],
        );
        emit_perp_balances(
            *lyrae_group_ai.key,
            *liqor_lyrae_account_ai.key,
            perp_market_index as u64,
            &liqor_ma.perp_accounts[perp_market_index],
            &lyrae_cache.perp_market_cache[perp_market_index],
        );

        Ok(())
    }

    #[inline(never)]
    /// Reduce some of the base position in exchange for quote position in this market
    /// Transfer will not exceed abs(base_position)
    /// Example:
    ///     BTC/USD price 9.4k
    ///     liquidation_fee = 0.025
    ///     liqee initial
    ///         USDC deposit 10k
    ///         BTC-PERP base_position = 10
    ///         BTC-PERP quote_position = -100k
    ///         maint_health = -700
    ///         init_health = -5400
    ///     liqee after liquidate_perp_market
    ///         USDC deposit 10k
    ///         BTC-PERP base_position = 2.3404
    ///         BTC-PERP quote_position = -29799.766
    ///         init_health = 0.018
    ///     liqor after liquidate_perp_market
    ///         BTC-PERP base_position = 7.6596
    ///         BTC-PERP quote_position = -70200.234
    fn liquidate_perp_market(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        base_transfer_request: i64,
    ) -> LyraeResult<()> {
        // TODO OPT find a way to send in open orders accounts without zero keys
        // liqor passes in his own account and the liqee lyrae account
        // position is transfered to the liqor at favorable rate
        check!(base_transfer_request != 0, LyraeErrorCode::InvalidParam)?;
        const NUM_FIXED: usize = 7;
        let accounts = array_ref![accounts, 0, NUM_FIXED + 2 * MAX_PAIRS];
        let (fixed_ais, liqee_open_orders_ais, liqor_open_orders_ais) =
            array_refs![accounts, NUM_FIXED, MAX_PAIRS, MAX_PAIRS];

        let [
            lyrae_group_ai,         // read
            lyrae_cache_ai,         // read
            perp_market_ai,         // write
            event_queue_ai,         // write
            liqee_lyrae_account_ai, // write
            liqor_lyrae_account_ai, // write
            liqor_ai,               // read, signer
        ] = fixed_ais;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;

        let mut liqee_ma =
            LyraeAccount::load_mut_checked(liqee_lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(!liqee_ma.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        liqee_ma.check_open_orders(&lyrae_group, liqee_open_orders_ais)?;

        let mut liqor_ma =
            LyraeAccount::load_mut_checked(liqor_lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(!liqor_ma.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        check!(
            &liqor_ma.owner == liqor_ai.key || &liqor_ma.delegate == liqor_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(liqor_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        liqor_ma.check_open_orders(&lyrae_group, liqor_open_orders_ais)?;

        let mut perp_market =
            PerpMarket::load_mut_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;
        let market_index = lyrae_group.find_perp_market_index(perp_market_ai.key).unwrap();
        let pmi = &lyrae_group.perp_markets[market_index];
        check!(!pmi.is_empty(), LyraeErrorCode::InvalidMarket)?;
        let mut event_queue: EventQueue =
            EventQueue::load_mut_checked(event_queue_ai, program_id, &perp_market)?;

        // Move funding into quote position. Not necessary to adjust funding settled after funding is moved
        let cache = &lyrae_cache.perp_market_cache[market_index];

        let now_ts = Clock::get()?.unix_timestamp as u64;
        let liqee_active_assets = UserActiveAssets::new(&lyrae_group, &liqee_ma, vec![]);
        let liqor_active_assets =
            UserActiveAssets::new(&lyrae_group, &liqor_ma, vec![(AssetType::Perp, market_index)]);

        lyrae_cache.check_valid(
            &lyrae_group,
            &UserActiveAssets::merge(&liqee_active_assets, &liqor_active_assets),
            now_ts,
        )?;
        liqee_ma.perp_accounts[market_index].settle_funding(cache);
        liqor_ma.perp_accounts[market_index].settle_funding(cache);

        // Make sure orders are cancelled for perps before liquidation
        for i in 0..lyrae_group.num_oracles {
            if liqee_active_assets.perps[i] {
                check!(liqee_ma.perp_accounts[i].has_no_open_orders(), LyraeErrorCode::Default)?;
            }
        }

        let mut health_cache = HealthCache::new(liqee_active_assets);
        health_cache.init_vals(&lyrae_group, &lyrae_cache, &liqee_ma, liqee_open_orders_ais)?;
        let init_health = health_cache.get_health(&lyrae_group, HealthType::Init);
        let maint_health = health_cache.get_health(&lyrae_group, HealthType::Maint);

        if liqee_ma.being_liquidated {
            if init_health > ZERO_I80F48 {
                liqee_ma.being_liquidated = false;
                msg!("Account init_health above zero.");
                return Ok(());
            }
        } else if maint_health >= ZERO_I80F48 {
            return Err(throw_err!(LyraeErrorCode::NotLiquidatable));
        } else {
            liqee_ma.being_liquidated = true;
        }

        // TODO - what happens if base position and quote position have same sign?
        // TODO - what if base position is 0 but quote is negative. Perhaps settle that pnl first?

        let liqee_perp_account = &mut liqee_ma.perp_accounts[market_index];
        let liqor_perp_account = &mut liqor_ma.perp_accounts[market_index];

        let price = lyrae_cache.price_cache[market_index].price;
        let lot_price = price * I80F48::from_num(pmi.base_lot_size);
        let (base_transfer, quote_transfer) = if liqee_perp_account.base_position > 0 {
            check!(base_transfer_request > 0, LyraeErrorCode::InvalidParam)?;

            let health_per_lot =
                lot_price * (ONE_I80F48 - pmi.init_asset_weight - pmi.liquidation_fee);
            let max_transfer = -init_health / health_per_lot;
            let max_transfer: i64 = max_transfer.checked_ceil().unwrap().checked_to_num().unwrap();

            let base_transfer =
                max_transfer.min(base_transfer_request).min(liqee_perp_account.base_position);

            let quote_transfer = I80F48::from_num(-base_transfer * pmi.base_lot_size)
                * price
                * (ONE_I80F48 - pmi.liquidation_fee);

            (base_transfer, quote_transfer)
        } else {
            // We know it liqee_perp_account.base_position < 0
            check!(base_transfer_request < 0, LyraeErrorCode::InvalidParam)?;

            let health_per_lot =
                lot_price * (ONE_I80F48 - pmi.init_liab_weight + pmi.liquidation_fee);
            let max_transfer = -init_health / health_per_lot;
            let max_transfer: i64 = max_transfer.checked_floor().unwrap().checked_to_num().unwrap();

            let base_transfer =
                max_transfer.max(base_transfer_request).max(liqee_perp_account.base_position);
            let quote_transfer = I80F48::from_num(-base_transfer * pmi.base_lot_size)
                * price
                * (ONE_I80F48 + pmi.liquidation_fee);

            (base_transfer, quote_transfer)
        };

        liqee_perp_account.change_base_position(&mut perp_market, -base_transfer);
        liqor_perp_account.change_base_position(&mut perp_market, base_transfer);

        liqee_perp_account.transfer_quote_position(liqor_perp_account, quote_transfer);

        // Log this to EventQueue
        let liquidate_event = LiquidateEvent::new(
            now_ts,
            event_queue.header.seq_num,
            *liqee_lyrae_account_ai.key,
            *liqor_lyrae_account_ai.key,
            price,
            base_transfer,
            pmi.liquidation_fee,
        );
        event_queue.push_back(cast(liquidate_event)).unwrap();

        // Calculate the health of liqor and see if liqor is still valid
        let mut liqor_health_cache = HealthCache::new(liqor_active_assets);
        liqor_health_cache.init_vals(
            &lyrae_group,
            &lyrae_cache,
            &liqor_ma,
            liqor_open_orders_ais,
        )?;
        let liqor_health = liqor_health_cache.get_health(&lyrae_group, HealthType::Init);
        check!(liqor_health >= ZERO_I80F48, LyraeErrorCode::InsufficientFunds)?;

        health_cache.update_perp_val(&lyrae_group, &lyrae_cache, &liqee_ma, market_index)?;
        let liqee_maint_health = health_cache.get_health(&lyrae_group, HealthType::Maint);
        if liqee_maint_health < ZERO_I80F48 {
            liqee_ma.is_bankrupt =
                liqee_ma.check_enter_bankruptcy(&lyrae_group, liqee_open_orders_ais);
        } else {
            let liqee_init_health = health_cache.get_health(&lyrae_group, HealthType::Init);
            // this is equivalent to one native USDC or 1e-6 USDC
            // This is used as threshold to flip flag instead of 0 because of dust issues
            liqee_ma.being_liquidated = liqee_init_health < NEG_ONE_I80F48;
        }

        lyrae_emit!(LiquidatePerpMarketLog {
            lyrae_group: *lyrae_group_ai.key,
            liqee: *liqee_lyrae_account_ai.key,
            liqor: *liqor_lyrae_account_ai.key,
            market_index: market_index as u64,
            price: price.to_bits(),
            base_transfer,
            quote_transfer: quote_transfer.to_bits(),
            bankruptcy: liqee_ma.is_bankrupt
        });
        emit_perp_balances(
            *lyrae_group_ai.key,
            *liqee_lyrae_account_ai.key,
            market_index as u64,
            &liqee_ma.perp_accounts[market_index],
            &lyrae_cache.perp_market_cache[market_index],
        );
        emit_perp_balances(
            *lyrae_group_ai.key,
            *liqor_lyrae_account_ai.key,
            market_index as u64,
            &liqor_ma.perp_accounts[market_index],
            &lyrae_cache.perp_market_cache[market_index],
        );

        Ok(())
    }

    #[inline(never)]
    /// Claim insurance fund and then socialize loss
    fn resolve_perp_bankruptcy(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        liab_index: usize,
        max_liab_transfer: I80F48,
    ) -> LyraeResult<()> {
        // First check the account is bankrupt
        // Determine the value of the liab transfer
        // Check if insurance fund has enough (given the fees)
        // If insurance fund does not have enough, start the socialize loss function

        // TODO - since liquidation fee is 0 for USDC, what's the incentive for someone to call this?
        //  just add 1bp fee

        // Do parameter checks
        check!(liab_index < QUOTE_INDEX, LyraeErrorCode::InvalidParam)?;
        check!(max_liab_transfer.is_positive(), LyraeErrorCode::InvalidParam)?;

        const NUM_FIXED: usize = 12;
        let accounts = array_ref![accounts, 0, NUM_FIXED + MAX_PAIRS];
        let (fixed_ais, liqor_open_orders_ais) = array_refs![accounts, NUM_FIXED, MAX_PAIRS];

        let [
            lyrae_group_ai,         // read
            lyrae_cache_ai,         // write
            liqee_lyrae_account_ai, // write
            liqor_lyrae_account_ai, // write
            liqor_ai,               // read, signer
            root_bank_ai,           // read
            node_bank_ai,           // write
            vault_ai,               // write
            insurance_vault_ai,     // write
            signer_ai,              // read
            perp_market_ai,         // write
            token_prog_ai,          // read
        ] = fixed_ais;
        check_eq!(token_prog_ai.key, &spl_token::ID, LyraeErrorCode::InvalidProgramId)?;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check!(signer_ai.key == &lyrae_group.signer_key, LyraeErrorCode::InvalidSignerKey)?;

        let mut lyrae_cache =
            LyraeCache::load_mut_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        let mut liqee_ma =
            LyraeAccount::load_mut_checked(liqee_lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(liqee_ma.is_bankrupt, LyraeErrorCode::Default)?;

        let mut liqor_ma =
            LyraeAccount::load_mut_checked(liqor_lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(
            &liqor_ma.owner == liqor_ai.key || &liqor_ma.delegate == liqor_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(liqor_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        check!(!liqor_ma.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        liqor_ma.check_open_orders(&lyrae_group, liqor_open_orders_ais)?;

        let root_bank = RootBank::load_checked(root_bank_ai, program_id)?;
        check!(
            &lyrae_group.tokens[QUOTE_INDEX].root_bank == root_bank_ai.key,
            LyraeErrorCode::InvalidRootBank
        )?;

        check!(root_bank.node_banks.contains(node_bank_ai.key), LyraeErrorCode::InvalidNodeBank)?;
        let mut node_bank = NodeBank::load_mut_checked(node_bank_ai, program_id)?;
        check!(vault_ai.key == &node_bank.vault, LyraeErrorCode::InvalidVault)?;

        let now_ts = Clock::get()?.unix_timestamp as u64;
        let liqor_active_assets =
            UserActiveAssets::new(&lyrae_group, &liqor_ma, vec![(AssetType::Perp, liab_index)]);

        lyrae_cache.check_valid(&lyrae_group, &liqor_active_assets, now_ts)?;

        check!(
            insurance_vault_ai.key == &lyrae_group.insurance_vault,
            LyraeErrorCode::InvalidVault
        )?;
        let insurance_vault = Account::unpack(&insurance_vault_ai.try_borrow_data()?)?;

        let bank_cache = &lyrae_cache.root_bank_cache[QUOTE_INDEX];
        let quote_pos = liqee_ma.perp_accounts[liab_index].quote_position;
        check!(quote_pos.is_negative(), LyraeErrorCode::Default)?;

        let liab_transfer_u64 = max_liab_transfer
            .min(-quote_pos) // minimum of what liqor wants and what liqee has
            .checked_ceil() // round up and convert to native quote token
            .unwrap()
            .checked_to_num::<u64>()
            .unwrap()
            .min(insurance_vault.amount); // take min of what ins. fund has

        if liab_transfer_u64 != 0 {
            let signers_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
            invoke_transfer(
                token_prog_ai,
                insurance_vault_ai,
                vault_ai,
                signer_ai,
                &[&signers_seeds],
                liab_transfer_u64,
            )?;
            let liab_transfer = I80F48::from_num(liab_transfer_u64);
            liqee_ma.perp_accounts[liab_index]
                .transfer_quote_position(&mut liqor_ma.perp_accounts[liab_index], -liab_transfer);

            checked_change_net(
                bank_cache,
                &mut node_bank,
                &mut liqor_ma,
                liqor_lyrae_account_ai.key,
                QUOTE_INDEX,
                liab_transfer,
            )?;

            // Make sure liqor is above init cond.
            let mut liqor_health_cache = HealthCache::new(liqor_active_assets);
            liqor_health_cache.init_vals(
                &lyrae_group,
                &lyrae_cache,
                &liqor_ma,
                liqor_open_orders_ais,
            )?;

            let liqor_health = liqor_health_cache.get_health(&lyrae_group, HealthType::Init);
            check!(liqor_health >= ZERO_I80F48, LyraeErrorCode::InsufficientFunds)?;
        }

        let quote_position = liqee_ma.perp_accounts[liab_index].quote_position;
        // If we transferred everything out of insurance_vault, insurance vault is empty
        // and if quote position is still negative
        let socialized_loss =
            if liab_transfer_u64 == insurance_vault.amount && quote_position.is_negative() {
                // insurance fund empty so socialize loss
                check!(
                    &lyrae_group.perp_markets[liab_index].perp_market == perp_market_ai.key,
                    LyraeErrorCode::InvalidMarket
                )?;
                let mut perp_market =
                    PerpMarket::load_mut_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;

                perp_market.socialize_loss(
                    &mut liqee_ma.perp_accounts[liab_index],
                    &mut lyrae_cache.perp_market_cache[liab_index],
                )?
            } else {
                ZERO_I80F48
            };

        liqee_ma.is_bankrupt = !liqee_ma.check_exit_bankruptcy(&lyrae_group);

        lyrae_emit!(PerpBankruptcyLog {
            lyrae_group: *lyrae_group_ai.key,
            liqee: *liqee_lyrae_account_ai.key,
            liqor: *liqor_lyrae_account_ai.key,
            liab_index: liab_index as u64,
            insurance_transfer: liab_transfer_u64,
            socialized_loss: socialized_loss.to_bits(),
            cache_long_funding: lyrae_cache.perp_market_cache[liab_index].long_funding.to_bits(),
            cache_short_funding: lyrae_cache.perp_market_cache[liab_index].short_funding.to_bits()
        });
        emit_perp_balances(
            *lyrae_group_ai.key,
            *liqee_lyrae_account_ai.key,
            liab_index as u64,
            &liqee_ma.perp_accounts[liab_index],
            &lyrae_cache.perp_market_cache[liab_index],
        );
        emit_perp_balances(
            *lyrae_group_ai.key,
            *liqor_lyrae_account_ai.key,
            liab_index as u64,
            &liqor_ma.perp_accounts[liab_index],
            &lyrae_cache.perp_market_cache[liab_index],
        );

        Ok(())
    }

    #[inline(never)]
    /// Claim insurance fund and then socialize loss
    fn resolve_token_bankruptcy(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        max_liab_transfer: I80F48, // in native token terms
    ) -> LyraeResult<()> {
        // First check the account is bankrupt
        // Determine the value of the liab transfer
        // Check if insurance fund has enough (given the fees)
        // If insurance fund does not have enough, start the socialize loss function
        check!(max_liab_transfer.is_positive(), LyraeErrorCode::InvalidParam)?;

        const NUM_FIXED: usize = 13;
        let accounts = array_ref![accounts, 0, NUM_FIXED + MAX_PAIRS + MAX_NODE_BANKS];
        let (
            fixed_ais,
            liqor_open_orders_ais, // read
            liab_node_bank_ais,    // write
        ) = array_refs![accounts, NUM_FIXED, MAX_PAIRS, MAX_NODE_BANKS];

        let [
            lyrae_group_ai,         // read
            lyrae_cache_ai,         // write
            liqee_lyrae_account_ai, // write
            liqor_lyrae_account_ai, // write
            liqor_ai,               // read, signer
            quote_root_bank_ai,     // read
            quote_node_bank_ai,     // write
            quote_vault_ai,         // write
            insurance_vault_ai,     // write
            signer_ai,              // read
            liab_root_bank_ai,      // write
            liab_node_bank_ai,      // write
            token_prog_ai,          // read
        ] = fixed_ais;
        check_eq!(token_prog_ai.key, &spl_token::ID, LyraeErrorCode::InvalidProgramId)?;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check!(signer_ai.key == &lyrae_group.signer_key, LyraeErrorCode::InvalidSignerKey)?;

        let mut lyrae_cache =
            LyraeCache::load_mut_checked(lyrae_cache_ai, program_id, &lyrae_group)?;

        // Load the liqee's lyrae account
        let mut liqee_ma =
            LyraeAccount::load_mut_checked(liqee_lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(liqee_ma.is_bankrupt, LyraeErrorCode::Default)?;

        // Load the liqor's lyrae account
        let mut liqor_ma =
            LyraeAccount::load_mut_checked(liqor_lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(
            &liqor_ma.owner == liqor_ai.key || &liqor_ma.delegate == liqor_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(liqor_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        check!(!liqor_ma.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        liqor_ma.check_open_orders(&lyrae_group, liqor_open_orders_ais)?;

        // Load the bank for liab token
        let liab_index = lyrae_group
            .find_root_bank_index(liab_root_bank_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidRootBank))?;
        let mut liab_root_bank = RootBank::load_mut_checked(liab_root_bank_ai, program_id)?;

        let now_ts = Clock::get()?.unix_timestamp as u64;
        let liqor_active_assets =
            UserActiveAssets::new(&lyrae_group, &liqor_ma, vec![(AssetType::Token, liab_index)]);

        lyrae_cache.check_valid(&lyrae_group, &liqor_active_assets, now_ts)?;

        // Load the insurance vault (insurance fund)
        check!(
            insurance_vault_ai.key == &lyrae_group.insurance_vault,
            LyraeErrorCode::InvalidVault
        )?;
        let insurance_vault = Account::unpack(&insurance_vault_ai.try_borrow_data()?)?;

        // Make sure there actually exist liabs here
        check!(liqee_ma.borrows[liab_index].is_positive(), LyraeErrorCode::Default)?;
        let liab_price = lyrae_cache.get_price(liab_index);
        let liab_fee = if liab_index == QUOTE_INDEX {
            ONE_I80F48
        } else {
            let liab_info = &lyrae_group.spot_markets[liab_index];
            ONE_I80F48 - liab_info.liquidation_fee
        };

        let liab_bank_cache = &lyrae_cache.root_bank_cache[liab_index];
        let native_borrows = liqee_ma.get_native_borrow(liab_bank_cache, liab_index)?;

        let insured_liabs = I80F48::from_num(insurance_vault.amount) * liab_fee / liab_price;
        let liab_transfer = max_liab_transfer.min(native_borrows).min(insured_liabs);

        let insurance_transfer = (liab_transfer * liab_price / liab_fee)
            .checked_ceil()
            .unwrap()
            .checked_to_num::<u64>()
            .unwrap()
            .min(insurance_vault.amount);

        if insurance_transfer != 0 {
            // First transfer quote currency into Lyrae quote vault from insurance fund
            check!(signer_ai.key == &lyrae_group.signer_key, LyraeErrorCode::InvalidSignerKey)?;
            let signers_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
            invoke_transfer(
                token_prog_ai,
                insurance_vault_ai,
                quote_vault_ai, // this vault is checked in conditional branch below
                signer_ai,
                &[&signers_seeds],
                insurance_transfer,
            )?;

            // Transfer equivalent amount of liabilities adjusted for fees
            let liab_transfer = I80F48::from_num(insurance_transfer) * liab_fee / liab_price;

            check!(
                liab_root_bank.node_banks.contains(liab_node_bank_ai.key),
                LyraeErrorCode::InvalidNodeBank
            )?;
            let mut liab_node_bank = NodeBank::load_mut_checked(liab_node_bank_ai, program_id)?;

            // Only load quote banks if they are different from liab banks to prevent double mut borrow
            if liab_index == QUOTE_INDEX {
                check!(quote_vault_ai.key == &liab_node_bank.vault, LyraeErrorCode::InvalidVault)?;

                // Increase the quote balance on the liqor equivalent to insurance transfer
                checked_change_net(
                    &lyrae_cache.root_bank_cache[QUOTE_INDEX],
                    &mut liab_node_bank,
                    &mut liqor_ma,
                    liqor_lyrae_account_ai.key,
                    QUOTE_INDEX,
                    I80F48::from_num(insurance_transfer),
                )?;
            } else {
                // Load the bank for quote token which we now know is different from liab banks
                let quote_root_bank = RootBank::load_checked(quote_root_bank_ai, program_id)?;
                check!(
                    &lyrae_group.tokens[QUOTE_INDEX].root_bank == quote_root_bank_ai.key,
                    LyraeErrorCode::InvalidRootBank
                )?;
                let mut quote_node_bank =
                    NodeBank::load_mut_checked(quote_node_bank_ai, program_id)?;
                check!(
                    quote_root_bank.node_banks.contains(quote_node_bank_ai.key),
                    LyraeErrorCode::InvalidNodeBank
                )?;
                check!(quote_vault_ai.key == &quote_node_bank.vault, LyraeErrorCode::InvalidVault)?;

                checked_change_net(
                    &lyrae_cache.root_bank_cache[QUOTE_INDEX],
                    &mut quote_node_bank,
                    &mut liqor_ma,
                    liqor_lyrae_account_ai.key,
                    QUOTE_INDEX,
                    I80F48::from_num(insurance_transfer),
                )?;
            }

            // Liqor transfers to cancel out liability on liqee
            transfer_token_internal(
                liab_bank_cache,
                &mut liab_node_bank,
                &mut liqor_ma,
                &mut liqee_ma,
                liqor_lyrae_account_ai.key,
                liqee_lyrae_account_ai.key,
                liab_index,
                liab_transfer,
            )?;

            // Make sure liqor is above init health
            let mut liqor_health_cache = HealthCache::new(liqor_active_assets);
            liqor_health_cache.init_vals(
                &lyrae_group,
                &lyrae_cache,
                &liqor_ma,
                liqor_open_orders_ais,
            )?;
            let liqor_health = liqor_health_cache.get_health(&lyrae_group, HealthType::Init);
            check!(liqor_health >= ZERO_I80F48, LyraeErrorCode::InsufficientFunds)?;
        }

        let (socialized_loss, percentage_loss) = if insurance_transfer == insurance_vault.amount
            && liqee_ma.borrows[liab_index].is_positive()
        {
            // insurance fund empty so socialize loss
            liab_root_bank.socialize_loss(
                program_id,
                liab_index,
                &mut lyrae_cache,
                &mut liqee_ma,
                liab_node_bank_ais,
            )?
        } else {
            (ZERO_I80F48, ZERO_I80F48)
        };

        liqee_ma.is_bankrupt = !liqee_ma.check_exit_bankruptcy(&lyrae_group);

        lyrae_emit!(TokenBankruptcyLog {
            lyrae_group: *lyrae_group_ai.key,
            liqee: *liqee_lyrae_account_ai.key,
            liqor: *liqor_lyrae_account_ai.key,
            liab_index: liab_index as u64,
            insurance_transfer,
            socialized_loss: socialized_loss.to_bits(),
            percentage_loss: percentage_loss.to_bits(),
            cache_deposit_index: lyrae_cache.root_bank_cache[liab_index].deposit_index.to_bits()
        });

        Ok(())
    }

    #[inline(never)]
    /// *** Keeper Related Instructions ***
    /// Update the deposit and borrow index on a passed in RootBank
    fn update_root_bank(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 3;
        let (fixed_accounts, node_bank_ais) = array_refs![accounts, NUM_FIXED; ..;];
        let [
            lyrae_group_ai, // read
            lyrae_cache_ai, // write
            root_bank_ai,   // write
        ] = fixed_accounts;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let mut lyrae_cache =
            LyraeCache::load_mut_checked(lyrae_cache_ai, program_id, &lyrae_group)?;

        let index = lyrae_group
            .find_root_bank_index(root_bank_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidRootBank))?;

        // TODO check root bank belongs to group in load functions
        let mut root_bank = RootBank::load_mut_checked(&root_bank_ai, program_id)?;
        check_eq!(root_bank.num_node_banks, node_bank_ais.len(), LyraeErrorCode::Default)?;
        for i in 0..root_bank.num_node_banks {
            check!(
                node_bank_ais.iter().any(|ai| ai.key == &root_bank.node_banks[i]),
                LyraeErrorCode::InvalidNodeBank
            )?;
        }
        let clock = Clock::get()?;
        let now_ts = clock.unix_timestamp as u64;
        root_bank.update_index(node_bank_ais, program_id, now_ts)?;

        lyrae_cache.root_bank_cache[index] = RootBankCache {
            deposit_index: root_bank.deposit_index,
            borrow_index: root_bank.borrow_index,
            last_update: now_ts,
        };

        lyrae_emit!(UpdateRootBankLog {
            lyrae_group: *lyrae_group_ai.key,
            token_index: index as u64,
            deposit_index: lyrae_cache.root_bank_cache[index].deposit_index.to_bits(),
            borrow_index: lyrae_cache.root_bank_cache[index].borrow_index.to_bits()
        });

        Ok(())
    }

    #[inline(never)]
    /// similar to serum dex, but also need to do some extra magic with funding
    fn consume_events(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        limit: usize,
    ) -> LyraeResult<()> {
        // Limit may be max 4 because of compute and memory limits from logging. Increase if compute/mem goes up
        let limit = min(limit, 4);

        const NUM_FIXED: usize = 4;
        let (fixed_ais, lyrae_account_ais) = array_refs![accounts, NUM_FIXED; ..;];
        let [
            lyrae_group_ai,     // read
            lyrae_cache_ai,     // read
            perp_market_ai,     // write
            event_queue_ai,     // write
        ] = fixed_ais;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        let mut perp_market =
            PerpMarket::load_mut_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;
        let mut event_queue: EventQueue =
            EventQueue::load_mut_checked(event_queue_ai, program_id, &perp_market)?;

        let now_ts = Clock::get()?.unix_timestamp as u64;
        let market_index = lyrae_group.find_perp_market_index(perp_market_ai.key).unwrap();
        let perp_market_cache = &lyrae_cache.perp_market_cache[market_index];

        perp_market_cache.check_valid(&lyrae_group, now_ts)?;

        for _ in 0..limit {
            let event = match event_queue.peek_front() {
                None => break,
                Some(e) => e,
            };

            match EventType::try_from(event.event_type).map_err(|_| throw!())? {
                EventType::Fill => {
                    let fill: &FillEvent = cast_ref(event);

                    // handle self trade separately because of rust borrow checker
                    if fill.maker == fill.taker {
                        let mut ma = match lyrae_account_ais.iter().find(|ai| ai.key == &fill.maker)
                        {
                            None => {
                                msg!("Unable to find account {}", fill.maker.to_string());
                                return Ok(());
                            }
                            Some(account_info) => LyraeAccount::load_mut_checked(
                                account_info,
                                program_id,
                                lyrae_group_ai.key,
                            )?,
                        };
                        let pre_lyr = ma.perp_accounts[market_index].lyr_accrued;
                        ma.execute_maker(market_index, &mut perp_market, perp_market_cache, fill)?;
                        ma.execute_taker(market_index, &mut perp_market, perp_market_cache, fill)?;
                        lyrae_emit!(LyrAccrualLog {
                            lyrae_group: *lyrae_group_ai.key,
                            lyrae_account: fill.maker,
                            market_index: market_index as u64,
                            lyr_accrual: ma.perp_accounts[market_index].lyr_accrued - pre_lyr
                        });
                        emit_perp_balances(
                            *lyrae_group_ai.key,
                            fill.maker,
                            market_index as u64,
                            &ma.perp_accounts[market_index],
                            &lyrae_cache.perp_market_cache[market_index],
                        );
                    } else {
                        let mut maker =
                            match lyrae_account_ais.iter().find(|ai| ai.key == &fill.maker) {
                                None => {
                                    msg!("Unable to find maker account {}", fill.maker.to_string());
                                    return Ok(());
                                }
                                Some(account_info) => LyraeAccount::load_mut_checked(
                                    account_info,
                                    program_id,
                                    lyrae_group_ai.key,
                                )?,
                            };
                        let mut taker =
                            match lyrae_account_ais.iter().find(|ai| ai.key == &fill.taker) {
                                None => {
                                    msg!("Unable to find taker account {}", fill.taker.to_string());
                                    return Ok(());
                                }
                                Some(account_info) => LyraeAccount::load_mut_checked(
                                    account_info,
                                    program_id,
                                    lyrae_group_ai.key,
                                )?,
                            };
                        let pre_lyr = maker.perp_accounts[market_index].lyr_accrued;

                        maker.execute_maker(
                            market_index,
                            &mut perp_market,
                            perp_market_cache,
                            fill,
                        )?;
                        taker.execute_taker(
                            market_index,
                            &mut perp_market,
                            perp_market_cache,
                            fill,
                        )?;
                        lyrae_emit!(LyrAccrualLog {
                            lyrae_group: *lyrae_group_ai.key,
                            lyrae_account: fill.maker,
                            market_index: market_index as u64,
                            lyr_accrual: maker.perp_accounts[market_index].lyr_accrued - pre_lyr
                        });
                        emit_perp_balances(
                            *lyrae_group_ai.key,
                            fill.maker,
                            market_index as u64,
                            &maker.perp_accounts[market_index],
                            &lyrae_cache.perp_market_cache[market_index],
                        );
                        emit_perp_balances(
                            *lyrae_group_ai.key,
                            fill.taker,
                            market_index as u64,
                            &taker.perp_accounts[market_index],
                            &lyrae_cache.perp_market_cache[market_index],
                        );
                    }
                    lyrae_emit!(fill.to_fill_log(*lyrae_group_ai.key, market_index));
                }
                EventType::Out => {
                    let out: &OutEvent = cast_ref(event);

                    let mut ma = match lyrae_account_ais.iter().find(|ai| ai.key == &out.owner) {
                        None => {
                            msg!("Unable to find account {}", out.owner.to_string());
                            return Ok(());
                        }
                        Some(account_info) => LyraeAccount::load_mut_checked(
                            account_info,
                            program_id,
                            lyrae_group_ai.key,
                        )?,
                    };

                    ma.remove_order(out.slot as usize, out.quantity)?;
                }
                EventType::Liquidate => {
                    // This is purely for record keeping. Can be removed if program logs are superior
                }
            }

            // consume this event
            event_queue.pop_front().map_err(|_| throw!())?;
        }
        Ok(())
    }

    #[inline(never)]
    /// Update the `funding_earned` of a `PerpMarket` using the current book price, spot index price
    /// and time since last update
    fn update_funding(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 5;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,     // read
            lyrae_cache_ai,     // write
            perp_market_ai,     // write
            bids_ai,            // read
            asks_ai,            // read
        ] = accounts;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;

        let mut lyrae_cache =
            LyraeCache::load_mut_checked(lyrae_cache_ai, program_id, &lyrae_group)?;

        let mut perp_market =
            PerpMarket::load_mut_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;

        let book = Book::load_checked(program_id, bids_ai, asks_ai, &perp_market)?;

        let market_index = lyrae_group.find_perp_market_index(perp_market_ai.key).unwrap();

        let clock = Clock::get()?;
        let now_ts = clock.unix_timestamp as u64;

        perp_market.update_funding(&lyrae_group, &book, &lyrae_cache, market_index, now_ts)?;
        lyrae_cache.perp_market_cache[market_index] = PerpMarketCache {
            long_funding: perp_market.long_funding,
            short_funding: perp_market.short_funding,
            last_update: now_ts,
        };

        // only need to use UpdateFundingLog; don't worry about CachePerpMarket log
        lyrae_emit!(UpdateFundingLog {
            lyrae_group: *lyrae_group_ai.key,
            market_index: market_index as u64,
            long_funding: perp_market.long_funding.to_bits(),
            short_funding: perp_market.short_funding.to_bits(),
        });

        Ok(())
    }

    #[inline(never)]
    /// Settle the lyr_accrued in a PerpAccount for LYR tokens
    fn redeem_lyr(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 11;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,     // read
            lyrae_cache_ai,     // read
            lyrae_account_ai,   // write
            owner_ai,           // read, signer
            perp_market_ai,     // read
            lyr_perp_vault_ai, // write
        lyr_root_bank_ai,  // read
        lyr_node_bank_ai,  // write
        lyr_bank_vault_ai, // write
            signer_ai,          // read
            token_prog_ai,      // read
        ] = accounts;
        check!(token_prog_ai.key == &spl_token::ID, LyraeErrorCode::InvalidProgramId)?;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check!(signer_ai.key == &lyrae_group.signer_key, LyraeErrorCode::InvalidSignerKey)?;

        let market_index = lyrae_group
            .find_perp_market_index(perp_market_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidMarket))?;
        let lyr_index = lyrae_group
            .find_root_bank_index(lyr_root_bank_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidRootBank))?;

        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        let lyr_bank_cache = &lyrae_cache.root_bank_cache[lyr_index];

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;

        let perp_account = &mut lyrae_account.perp_accounts[market_index];

        // Load the lyr banks
        let root_bank = RootBank::load_checked(lyr_root_bank_ai, program_id)?;
        check!(
            root_bank.node_banks.contains(lyr_node_bank_ai.key),
            LyraeErrorCode::InvalidNodeBank
        )?;
        let mut lyr_node_bank = NodeBank::load_mut_checked(lyr_node_bank_ai, program_id)?;
        check_eq!(&lyr_node_bank.vault, lyr_bank_vault_ai.key, LyraeErrorCode::InvalidVault)?;

        let perp_market = PerpMarket::load_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;
        check!(lyr_perp_vault_ai.key == &perp_market.lyr_vault, LyraeErrorCode::InvalidVault)?;

        let lyr_perp_vault = Account::unpack(&lyr_perp_vault_ai.try_borrow_data()?)?;

        let lyr = min(perp_account.lyr_accrued, lyr_perp_vault.amount);
        perp_account.lyr_accrued -= lyr;

        let signers_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
        invoke_transfer(
            token_prog_ai,
            lyr_perp_vault_ai,
            lyr_bank_vault_ai,
            signer_ai,
            &[&signers_seeds],
            lyr,
        )?;

        let now_ts = Clock::get()?.unix_timestamp as u64;
        lyr_bank_cache.check_valid(&lyrae_group, now_ts)?;

        let redeemed_lyr = I80F48::from_num(lyr);
        checked_change_net(
            lyr_bank_cache,
            &mut lyr_node_bank,
            &mut lyrae_account,
            lyrae_account_ai.key,
            lyr_index,
            redeemed_lyr,
        )?;

        lyrae_emit!(RedeemLyrLog {
            lyrae_group: *lyrae_group_ai.key,
            lyrae_account: *lyrae_account_ai.key,
            market_index: market_index as u64,
            redeemed_lyr: lyr,
        });

        Ok(())
    }

    #[inline(never)]
    fn add_lyrae_account_info(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        info: [u8; INFO_LEN],
    ) -> LyraeResult<()> {
        const NUM_FIXED: usize = 3;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,     // read
            lyrae_account_ai,   // write
            owner_ai            // signer
        ] = accounts;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(owner_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;

        lyrae_account.info = info;
        Ok(())
    }

    #[inline(never)]
    fn deposit_msrm(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        quantity: u64,
    ) -> LyraeResult<()> {
        const NUM_FIXED: usize = 6;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,     // read
            lyrae_account_ai,   // write
            owner_ai,           // read, signer
            msrm_account_ai,    // write
            msrm_vault_ai,      // write
            token_prog_ai,      // read
        ] = accounts;
        check!(token_prog_ai.key == &spl_token::ID, LyraeErrorCode::InvalidProgramId)?;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check!(msrm_vault_ai.key == &lyrae_group.msrm_vault, LyraeErrorCode::InvalidVault)?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(&lyrae_account.owner == owner_ai.key, LyraeErrorCode::InvalidOwner)?;
        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;

        invoke_transfer(token_prog_ai, msrm_account_ai, msrm_vault_ai, owner_ai, &[], quantity)?;

        lyrae_account.msrm_amount += quantity;

        Ok(())
    }

    #[inline(never)]
    fn withdraw_msrm(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        quantity: u64,
    ) -> LyraeResult<()> {
        const NUM_FIXED: usize = 7;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,     // read
            lyrae_account_ai,   // write
            owner_ai,           // read, signer
            msrm_account_ai,    // write
            msrm_vault_ai,      // write
            signer_ai,          // read
            token_prog_ai,      // read
        ] = accounts;
        check!(token_prog_ai.key == &spl_token::ID, LyraeErrorCode::InvalidProgramId)?;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        check!(msrm_vault_ai.key == &lyrae_group.msrm_vault, LyraeErrorCode::InvalidVault)?;
        check!(signer_ai.key == &lyrae_group.signer_key, LyraeErrorCode::InvalidSignerKey)?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(&lyrae_account.owner == owner_ai.key, LyraeErrorCode::InvalidOwner)?;
        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;

        check!(lyrae_account.msrm_amount >= quantity, LyraeErrorCode::InsufficientFunds)?;

        let signer_seeds = gen_signer_seeds(&lyrae_group.signer_nonce, lyrae_group_ai.key);
        invoke_transfer(
            token_prog_ai,
            msrm_vault_ai,
            msrm_account_ai,
            signer_ai,
            &[&signer_seeds],
            quantity,
        )?;

        lyrae_account.msrm_amount -= quantity;

        Ok(())
    }

    #[inline(never)]
    fn set_group_admin(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 3;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,     // write
            new_admin_ai,       // read
            admin_ai,           // read, signer
        ] = accounts;

        let mut lyrae_group = LyraeGroup::load_mut_checked(lyrae_group_ai, program_id)?;

        check!(admin_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check_eq!(admin_ai.key, &lyrae_group.admin, LyraeErrorCode::InvalidAdminKey)?;

        lyrae_group.admin = *new_admin_ai.key;

        Ok(())
    }

    #[inline(never)]
    fn init_advanced_orders(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 5;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,         // read
            lyrae_account_ai,       // write
            owner_ai,               // write & signer
            advanced_orders_ai,     // write
            system_prog_ai,         // read
        ] = accounts;
        check!(
            system_prog_ai.key == &solana_program::system_program::id(),
            LyraeErrorCode::InvalidProgramId
        )?;

        let _lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(owner_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;

        // Make sure the LyraeAccount doesn't already have a AdvancedOrders set
        check!(
            lyrae_account.advanced_orders_key == Pubkey::default(),
            LyraeErrorCode::InvalidParam
        )?;

        let (pda_address, bump_seed) =
            Pubkey::find_program_address(&[&lyrae_account_ai.key.to_bytes()], program_id);
        check!(&pda_address == advanced_orders_ai.key, LyraeErrorCode::InvalidAccount)?;

        let pda_signer_seeds: &[&[u8]] = &[&lyrae_account_ai.key.to_bytes(), &[bump_seed]];
        let rent = Rent::get()?;
        create_pda_account(
            owner_ai,
            &rent,
            size_of::<AdvancedOrders>(),
            program_id,
            system_prog_ai,
            advanced_orders_ai,
            pda_signer_seeds,
            &[],
        )?;

        // initialize the AdvancedOrders account
        AdvancedOrders::init(advanced_orders_ai, program_id, &rent)?;

        // set the lyrae_account.advanced_orders field
        lyrae_account.advanced_orders_key = *advanced_orders_ai.key;
        Ok(())
    }

    #[inline(never)]
    fn close_advanced_orders(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult<()> {
        const NUM_FIXED: usize = 4;
        let accounts = array_ref![accounts, 0, NUM_FIXED];

        let [
            lyrae_group_ai,     // read
            lyrae_account_ai,   // write
            owner_ai,           // write, signer
            advanced_orders_ai, // write
        ] = accounts;

        check!(owner_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, &lyrae_group_ai.key)?;
        check!(&lyrae_account.owner == owner_ai.key, LyraeErrorCode::InvalidOwner)?;
        check!(!lyrae_account.being_liquidated, LyraeErrorCode::BeingLiquidated)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;

        let mut advanced_orders =
            AdvancedOrders::load_mut_checked(advanced_orders_ai, program_id, &lyrae_account)?;
        for i in 0..MAX_ADVANCED_ORDERS {
            advanced_orders.orders[i].is_active = false;
        }
        advanced_orders.meta_data.is_initialized = false;

        // Transfer lamports to owner
        program_transfer_lamports(advanced_orders_ai, owner_ai, advanced_orders_ai.lamports())?;

        lyrae_account.advanced_orders_key = Pubkey::default();

        Ok(())
    }

    /// Add a perp trigger order to the AdvancedOrders account
    /// The TriggerCondition specifies if trigger_price  must be above or below oracle price
    /// When the condition is met, the order is executed as a regular perp order
    #[inline(never)]
    fn add_perp_trigger_order(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        order_type: OrderType,
        side: Side,
        trigger_condition: TriggerCondition,
        reduce_only: bool,
        client_order_id: u64,
        price: i64,
        quantity: i64,
        trigger_price: I80F48,
    ) -> LyraeResult<()> {
        check!(price.is_positive(), LyraeErrorCode::InvalidParam)?;
        check!(quantity.is_positive(), LyraeErrorCode::InvalidParam)?;
        check!(trigger_price.is_positive(), LyraeErrorCode::InvalidParam)?; // Is this necessary?

        const NUM_FIXED: usize = 7;
        let (fixed_ais, open_orders_ais) = array_refs![accounts, NUM_FIXED; ..;];
        let [
            lyrae_group_ai,         // read
            lyrae_account_ai,       // read
            owner_ai,               // write & signer
            advanced_orders_ai,     // write
            lyrae_cache_ai,         // read
            perp_market_ai,         // read
            system_prog_ai,         // read
        ] = fixed_ais;
        check!(
            system_prog_ai.key == &solana_program::system_program::id(),
            LyraeErrorCode::InvalidProgramId
        )?;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        let open_orders_ais =
            lyrae_account.checked_unpack_open_orders(&lyrae_group, open_orders_ais)?;
        let open_orders_accounts = load_open_orders_accounts(&open_orders_ais)?;

        let market_index = lyrae_group
            .find_perp_market_index(perp_market_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidMarket))?;

        let active_assets = UserActiveAssets::new(
            &lyrae_group,
            &lyrae_account,
            vec![(AssetType::Perp, market_index)],
        );

        // load and validate the cache
        let clock = Clock::get()?;
        let now_ts = clock.unix_timestamp as u64;
        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        lyrae_cache.check_valid(&lyrae_group, &active_assets, now_ts)?;

        let mut health_cache = HealthCache::new(active_assets);
        health_cache.init_vals_with_orders_vec(
            &lyrae_group,
            &lyrae_cache,
            &lyrae_account,
            &open_orders_accounts,
        )?;
        let init_health = health_cache.get_health(&lyrae_group, HealthType::Init);
        let maint_health = health_cache.get_health(&lyrae_group, HealthType::Maint);

        // Only allow placing of trigger orders if account above Maint and not being liquidated
        check!(
            init_health >= ZERO_I80F48
                || (!lyrae_account.being_liquidated && maint_health >= ZERO_I80F48),
            LyraeErrorCode::InsufficientHealth
        )?;
        lyrae_account.being_liquidated = false;

        // Note: no need to check health here, needs to be checked on trigger
        // TODO: make sure liquidator cancels all advanced orders (why?)
        // Transfer lamports before so we don't hit rust borrow checker
        // If we don't succeed in adding the order, it will be reverted anyway
        invoke_transfer_lamports(
            owner_ai,
            advanced_orders_ai,
            system_prog_ai,
            ADVANCED_ORDER_FEE,
            &[],
        )?;

        let mut advanced_orders =
            AdvancedOrders::load_mut_checked(advanced_orders_ai, program_id, &lyrae_account)?;
        for i in 0..MAX_ADVANCED_ORDERS {
            if advanced_orders.orders[i].is_active {
                continue;
            }

            advanced_orders.orders[i] = cast(PerpTriggerOrder::new(
                market_index as u8,
                order_type,
                side,
                trigger_condition,
                reduce_only,
                client_order_id,
                price,
                quantity,
                trigger_price,
            ));

            return Ok(());
        }

        Err(throw_err!(LyraeErrorCode::OutOfSpace))
    }

    /// Remove the order and refund the fee
    #[inline(never)]
    fn remove_advanced_order(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        order_index: u8,
    ) -> LyraeResult<()> {
        let order_index = order_index as usize;
        check!(order_index < MAX_ADVANCED_ORDERS, LyraeErrorCode::InvalidParam)?;

        const NUM_FIXED: usize = 5;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,         // read
            lyrae_account_ai,       // read
            owner_ai,               // write & signer
            advanced_orders_ai,     // write
            system_prog_ai,         // read
        ] = accounts;
        check!(
            system_prog_ai.key == &solana_program::system_program::id(),
            LyraeErrorCode::InvalidProgramId
        )?;

        let lyrae_account =
            LyraeAccount::load_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(owner_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;
        // No bankruptcy check; removing order is fine

        let mut advanced_orders =
            AdvancedOrders::load_mut_checked(advanced_orders_ai, program_id, &lyrae_account)?;

        let order = &mut advanced_orders.orders[order_index];

        if order.is_active {
            order.is_active = false;
            program_transfer_lamports(advanced_orders_ai, owner_ai, ADVANCED_ORDER_FEE)
        } else {
            Ok(())
        }
    }

    #[inline(never)]
    fn execute_perp_trigger_order(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        order_index: u8,
    ) -> LyraeResult<()> {
        let order_index = order_index as usize;
        check!(order_index < MAX_ADVANCED_ORDERS, LyraeErrorCode::InvalidParam)?;
        const NUM_FIXED: usize = 9;
        let (fixed_ais, open_orders_ais) = array_refs![accounts, NUM_FIXED; ..;];
        let [
            lyrae_group_ai,         // read
            lyrae_account_ai,       // write
            advanced_orders_ai,     // write
            agent_ai,               // write
            lyrae_cache_ai,         // read
            perp_market_ai,         // write
            bids_ai,                // write
            asks_ai,                // write
            event_queue_ai,         // write
        ] = fixed_ais;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        let open_orders_ais =
            lyrae_account.checked_unpack_open_orders(&lyrae_group, open_orders_ais)?;
        let open_orders_accounts = load_open_orders_accounts(&open_orders_ais)?;

        let mut advanced_orders =
            AdvancedOrders::load_mut_checked(advanced_orders_ai, program_id, &lyrae_account)?;

        // deactivate all advanced orders if account is bankrupt
        if lyrae_account.is_bankrupt {
            msg!("Failed to trigger order; LyraeAccount is bankrupt.");
            return cancel_all_advanced_orders(advanced_orders_ai, &mut advanced_orders, agent_ai);
        }

        // Select the AdvancedOrder
        let order: &mut PerpTriggerOrder = cast_mut(&mut advanced_orders.orders[order_index]);
        check!(order.is_active, LyraeErrorCode::InvalidParam)?;
        check!(
            order.advanced_order_type == AdvancedOrderType::PerpTrigger,
            LyraeErrorCode::InvalidParam
        )?;
        let market_index = order.market_index as usize;

        // Check the caches are valid
        let active_assets = UserActiveAssets::new(
            &lyrae_group,
            &lyrae_account,
            vec![(AssetType::Perp, market_index)],
        );

        let clock = Clock::get()?;
        let now_ts = clock.unix_timestamp as u64;
        let lyrae_cache = LyraeCache::load_checked(lyrae_cache_ai, program_id, &lyrae_group)?;
        lyrae_cache.check_valid(&lyrae_group, &active_assets, now_ts)?;

        // Check trigger condition is met
        let price = lyrae_cache.get_price(market_index);
        match order.trigger_condition {
            TriggerCondition::Above => {
                check!(price >= order.trigger_price, LyraeErrorCode::TriggerConditionFalse)?;
            }
            TriggerCondition::Below => {
                check!(price <= order.trigger_price, LyraeErrorCode::TriggerConditionFalse)?;
            }
        }
        check!(
            &lyrae_group.perp_markets[market_index].perp_market == perp_market_ai.key,
            LyraeErrorCode::InvalidMarket
        )?;
        let mut perp_market =
            PerpMarket::load_mut_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;

        let mut health_cache = HealthCache::new(active_assets);
        health_cache.init_vals_with_orders_vec(
            &lyrae_group,
            &lyrae_cache,
            &lyrae_account,
            &open_orders_accounts,
        )?;
        let pre_health = health_cache.get_health(&lyrae_group, HealthType::Init);

        // update the being_liquidated flag
        if lyrae_account.being_liquidated {
            if pre_health >= ZERO_I80F48 {
                lyrae_account.being_liquidated = false;
            } else {
                msg!("Failed to trigger order; LyraeAccount is being liquidated.");
                return cancel_all_advanced_orders(
                    advanced_orders_ai,
                    &mut advanced_orders,
                    agent_ai,
                );
            }
        }

        // This means health must only go up
        let health_up_only = pre_health < ZERO_I80F48;

        let mut book = Book::load_checked(program_id, bids_ai, asks_ai, &perp_market)?;
        let mut event_queue =
            EventQueue::load_mut_checked(event_queue_ai, program_id, &perp_market)?;

        // If reduce_only, position must only go down
        let quantity = if order.reduce_only {
            let base_pos = lyrae_account.get_complete_base_pos(
                market_index,
                &event_queue,
                lyrae_account_ai.key,
            )?;

            if (order.side == Side::Bid && base_pos > 0)
                || (order.side == Side::Ask && base_pos < 0)
            {
                0
            } else {
                base_pos.abs().min(order.quantity)
            }
        } else {
            order.quantity
        };

        if quantity != 0 {
            let (taker_base, taker_quote, bids_quantity, asks_quantity) = match order.side {
                Side::Bid => book.sim_new_bid(
                    &perp_market,
                    &lyrae_group.perp_markets[market_index],
                    lyrae_cache.get_price(market_index),
                    order.price,
                    quantity,
                    order.order_type,
                )?,
                Side::Ask => book.sim_new_ask(
                    &perp_market,
                    &lyrae_group.perp_markets[market_index],
                    lyrae_cache.get_price(market_index),
                    order.price,
                    quantity,
                    order.order_type,
                )?,
            };

            // simulate the effect on health
            let sim_post_health = health_cache.get_health_after_sim_perp(
                &lyrae_group,
                &lyrae_cache,
                &lyrae_account,
                market_index,
                HealthType::Init,
                taker_base,
                taker_quote,
                bids_quantity,
                asks_quantity,
            )?;

            if sim_post_health >= ZERO_I80F48 || (health_up_only && sim_post_health >= pre_health) {
                let (taker_base, taker_quote, bids_quantity, asks_quantity) = {
                    let pa = &lyrae_account.perp_accounts[market_index];
                    (
                        pa.taker_base + taker_base,
                        pa.taker_quote + taker_quote,
                        pa.bids_quantity.checked_add(bids_quantity).unwrap(),
                        pa.asks_quantity.checked_add(asks_quantity).unwrap(),
                    )
                };

                book.new_order(
                    program_id,
                    &lyrae_group,
                    lyrae_group_ai.key,
                    &lyrae_cache,
                    &mut event_queue,
                    &mut perp_market,
                    lyrae_cache.get_price(market_index),
                    &mut lyrae_account,
                    lyrae_account_ai.key,
                    market_index,
                    order.side,
                    order.price,
                    quantity,
                    order.order_type,
                    order.client_order_id,
                    now_ts,
                    None,
                )?;

                // TODO OPT - unnecessary, remove after testing
                health_cache.update_perp_val(
                    &lyrae_group,
                    &lyrae_cache,
                    &lyrae_account,
                    market_index,
                )?;
                let post_health = health_cache.get_health(&lyrae_group, HealthType::Init);
                let pa = &lyrae_account.perp_accounts[market_index];
                check!(
                    sim_post_health == post_health
                        && taker_base == pa.taker_base
                        && taker_quote == pa.taker_quote
                        && bids_quantity == pa.bids_quantity
                        && asks_quantity == pa.asks_quantity,
                    LyraeErrorCode::MathError
                )?;
            } else {
                // normally this would be an InsufficientFunds error but we want to remove the AO and persist changes
                msg!("Failed to place perp order due to insufficient funds")
            }
        }

        order.is_active = false;
        program_transfer_lamports(advanced_orders_ai, agent_ai, ADVANCED_ORDER_FEE)
    }

    /// Create a LyraeAccount PDA and initialize it
    #[inline(never)]
    fn create_lyrae_account(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        account_num: u64,
    ) -> LyraeResult {
        const NUM_FIXED: usize = 4;
        let fixed_accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,         // write
            lyrae_account_ai,       // write
            owner_ai,               // read (write if no payer passed) & signer
            system_prog_ai,         // read
        ] = fixed_accounts;
        let payer_ai = if accounts.len() > NUM_FIXED {
            &accounts[NUM_FIXED] // write & signer
        } else {
            owner_ai
        };
        check!(
            system_prog_ai.key == &solana_program::system_program::id(),
            LyraeErrorCode::InvalidProgramId
        )?;
        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check!(payer_ai.is_signer, LyraeErrorCode::SignerNecessary)?;

        let mut lyrae_group = LyraeGroup::load_mut_checked(lyrae_group_ai, program_id)?;
        check!(
            lyrae_group.num_lyrae_accounts < lyrae_group.max_lyrae_accounts,
            LyraeErrorCode::MaxAccountsReached
        )?;
        let rent = Rent::get()?;

        let lyrae_account_seeds: &[&[u8]] =
            &[&lyrae_group_ai.key.as_ref(), &owner_ai.key.as_ref(), &account_num.to_le_bytes()];
        seed_and_create_pda(
            program_id,
            payer_ai,
            &rent,
            size_of::<LyraeAccount>(),
            program_id,
            system_prog_ai,
            lyrae_account_ai,
            lyrae_account_seeds,
            &[],
        )?;
        let mut lyrae_account: RefMut<LyraeAccount> = LyraeAccount::load_mut(lyrae_account_ai)?;
        check!(!lyrae_account.meta_data.is_initialized, LyraeErrorCode::InvalidAccountState)?;

        lyrae_account.lyrae_group = *lyrae_group_ai.key;
        lyrae_account.owner = *owner_ai.key;
        lyrae_account.order_market = [FREE_ORDER_SLOT; MAX_PERP_OPEN_ORDERS];
        lyrae_account.meta_data = MetaData::new(DataType::LyraeAccount, 1, true);

        lyrae_group.num_lyrae_accounts += 1;

        Ok(())
    }

    #[inline(never)]
    fn update_margin_basket(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult {
        const NUM_FIXED: usize = 2;
        let accounts = array_ref![accounts, 0, NUM_FIXED + MAX_PAIRS];
        let (fixed_ais, open_orders_ais) = array_refs![accounts, NUM_FIXED, MAX_PAIRS];

        let [
            lyrae_group_ai,         // read
            lyrae_account_ai,       // write
        ] = fixed_ais;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;

        for i in 0..lyrae_group.num_oracles {
            check_eq!(
                open_orders_ais[i].key,
                &lyrae_account.spot_open_orders[i],
                LyraeErrorCode::InvalidOpenOrdersAccount
            )?;

            if lyrae_account.spot_open_orders[i] != Pubkey::default() {
                check_open_orders(
                    &open_orders_ais[i],
                    &lyrae_group.signer_key,
                    &lyrae_group.dex_program_id,
                )?;
                let open_orders = load_open_orders(&open_orders_ais[i])?;
                lyrae_account.update_basket(i, &open_orders)?;
            }
        }

        Ok(())
    }

    #[inline(never)]
    /// Change the maximum number of LyraeAccounts.v1 allowed
    fn change_max_lyrae_accounts(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        max_lyrae_accounts: u32,
    ) -> LyraeResult {
        const NUM_FIXED: usize = 2;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai, // write
            admin_ai        // read, signer
        ] = accounts;

        let mut lyrae_group = LyraeGroup::load_mut_checked(lyrae_group_ai, program_id)?;
        check!(admin_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check_eq!(admin_ai.key, &lyrae_group.admin, LyraeErrorCode::InvalidAdminKey)?;

        lyrae_group.max_lyrae_accounts = max_lyrae_accounts;
        Ok(())
    }

    /// Create a DustAccount PDA and initialize it
    #[inline(never)]
    fn create_dust_account(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult {
        const NUM_FIXED: usize = 4;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,         // read
            lyrae_account_ai,       // write
            payer_ai,               // write & signer
            system_prog_ai,         // read
        ] = accounts;
        check!(
            system_prog_ai.key == &solana_program::system_program::id(),
            LyraeErrorCode::InvalidProgramId
        )?;
        check!(payer_ai.is_signer, LyraeErrorCode::SignerNecessary)?;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let rent = Rent::get()?;

        let lyrae_account_seeds: &[&[u8]] = &[&lyrae_group_ai.key.as_ref(), b"DustAccount"];
        seed_and_create_pda(
            program_id,
            payer_ai,
            &rent,
            size_of::<LyraeAccount>(),
            program_id,
            system_prog_ai,
            lyrae_account_ai,
            lyrae_account_seeds,
            &[],
        )?;
        let mut lyrae_account: RefMut<LyraeAccount> = LyraeAccount::load_mut(lyrae_account_ai)?;
        check!(!lyrae_account.meta_data.is_initialized, LyraeErrorCode::InvalidAccountState)?;

        lyrae_account.lyrae_group = *lyrae_group_ai.key;
        lyrae_account.owner = lyrae_group.admin;
        lyrae_account.order_market = [FREE_ORDER_SLOT; MAX_PERP_OPEN_ORDERS];
        lyrae_account.meta_data = MetaData::new(DataType::LyraeAccount, 0, true);
        lyrae_account.not_upgradable = true;

        Ok(())
    }

    #[inline(never)]
    fn upgrade_lyrae_account_v0_v1(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult {
        const NUM_FIXED: usize = 3;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,   // write
            lyrae_account_ai, // write
            owner_ai          // signer
        ] = accounts;

        let mut lyrae_group = LyraeGroup::load_mut_checked(lyrae_group_ai, program_id)?;
        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;

        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check_eq!(lyrae_account.meta_data.version, 0, LyraeErrorCode::InvalidAccountState)?;
        check!(!lyrae_account.not_upgradable, LyraeErrorCode::InvalidAccountState)?;
        check!(
            lyrae_group.num_lyrae_accounts < lyrae_group.max_lyrae_accounts,
            LyraeErrorCode::MaxAccountsReached
        )?;

        lyrae_group.num_lyrae_accounts += 1;
        lyrae_account.meta_data.version = 1;

        Ok(())
    }

    #[inline(never)]
    fn cancel_perp_orders_side(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        side: Side,
        limit: u8,
    ) -> LyraeResult {
        const NUM_FIXED: usize = 6;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,     // read
            lyrae_account_ai,   // write
            owner_ai,           // read, signer
            perp_market_ai,     // write
            bids_ai,            // write
            asks_ai,            // write
        ] = accounts;

        let lyrae_group = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;
        check!(!lyrae_account.is_bankrupt, LyraeErrorCode::Bankrupt)?;
        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;

        let mut perp_market =
            PerpMarket::load_mut_checked(perp_market_ai, program_id, lyrae_group_ai.key)?;

        let market_index = lyrae_group.find_perp_market_index(perp_market_ai.key).unwrap();

        let mut book = Book::load_checked(program_id, bids_ai, asks_ai, &perp_market)?;
        let lyr_start = lyrae_account.perp_accounts[market_index].lyr_accrued;

        if perp_market.meta_data.version == 0 {
            return Err(throw_err!(LyraeErrorCode::InvalidParam));
        } else {
            let (all_order_ids, canceled_order_ids) = book.cancel_all_side_with_size_incentives(
                &mut lyrae_account,
                &mut perp_market,
                market_index,
                side,
                limit,
            )?;
            lyrae_emit!(CancelAllPerpOrdersLog {
                lyrae_group: *lyrae_group_ai.key,
                lyrae_account: *lyrae_account_ai.key,
                market_index: market_index as u64,
                all_order_ids,
                canceled_order_ids
            });
        }

        lyrae_emit!(LyrAccrualLog {
            lyrae_group: *lyrae_group_ai.key,
            lyrae_account: *lyrae_account_ai.key,
            market_index: market_index as u64,
            lyr_accrual: lyrae_account.perp_accounts[market_index].lyr_accrued - lyr_start
        });
        Ok(())
    }

    #[inline(never)]
    fn set_delegate(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult {
        const NUM_FIXED: usize = 4;
        let accounts = array_ref![accounts, 0, NUM_FIXED];
        let [
            lyrae_group_ai,                   // read
            lyrae_account_ai,                 // write
            owner_ai,                         // read, signer
            delegate_ai,                      // read
        ] = accounts;

        let mut lyrae_account =
            LyraeAccount::load_mut_checked(lyrae_account_ai, program_id, lyrae_group_ai.key)?;

        check!(owner_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        check!(&lyrae_account.owner == owner_ai.key, LyraeErrorCode::InvalidOwner)?;
        check!(&lyrae_account.delegate != delegate_ai.key, LyraeErrorCode::InvalidAccount)?;

        lyrae_account.delegate = *delegate_ai.key;

        Ok(())
    }

    #[inline(never)]
    fn change_spot_market_params(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        maint_leverage: Option<I80F48>,
        init_leverage: Option<I80F48>,
        liquidation_fee: Option<I80F48>,
        optimal_util: Option<I80F48>,
        optimal_rate: Option<I80F48>,
        max_rate: Option<I80F48>,
        version: Option<u8>,
    ) -> LyraeResult {
        const NUM_FIXED: usize = 4;
        let accounts = array_ref![accounts, 0, NUM_FIXED];

        let [
        lyrae_group_ai, // write
        spot_market_ai, // write
        root_bank_ai,   // write
        admin_ai        // read, signer
        ] = accounts;

        let mut lyrae_group = LyraeGroup::load_mut_checked(lyrae_group_ai, program_id)?;
        check_eq!(admin_ai.key, &lyrae_group.admin, LyraeErrorCode::InvalidAdminKey)?;
        check!(admin_ai.is_signer, LyraeErrorCode::SignerNecessary)?;

        let market_index = lyrae_group
            .find_spot_market_index(spot_market_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidMarket))?;

        // checks rootbank is part of the group
        let _root_bank_index = lyrae_group
            .find_root_bank_index(root_bank_ai.key)
            .ok_or(throw_err!(LyraeErrorCode::InvalidRootBank))?;

        let mut root_bank = RootBank::load_mut_checked(&root_bank_ai, program_id)?;
        let mut info = &mut lyrae_group.spot_markets[market_index];

        // Unwrap params. Default to current state if Option is None
        let (init_asset_weight, init_liab_weight) = init_leverage
            .map_or((info.init_asset_weight, info.init_liab_weight), |x| get_leverage_weights(x));
        let (maint_asset_weight, maint_liab_weight) = maint_leverage
            .map_or((info.maint_asset_weight, info.maint_liab_weight), |x| get_leverage_weights(x));

        let liquidation_fee = liquidation_fee.unwrap_or(info.liquidation_fee);
        let optimal_util = optimal_util.unwrap_or(root_bank.optimal_util);
        let optimal_rate = optimal_rate.unwrap_or(root_bank.optimal_rate);
        let max_rate = max_rate.unwrap_or(root_bank.max_rate);
        let version = version.unwrap_or(root_bank.meta_data.version);

        // params check
        check!(init_asset_weight > ZERO_I80F48, LyraeErrorCode::InvalidParam)?;
        check!(maint_asset_weight > init_asset_weight, LyraeErrorCode::InvalidParam)?;
        // maint leverage may only increase to prevent unforeseen liquidations
        check!(maint_asset_weight >= info.maint_asset_weight, LyraeErrorCode::InvalidParam)?;

        // set the params on the RootBank
        root_bank.set_rate_params(optimal_util, optimal_rate, max_rate)?;

        // set the params on LyraeGroup SpotMarketInfo
        info.liquidation_fee = liquidation_fee;
        info.maint_asset_weight = maint_asset_weight;
        info.init_asset_weight = init_asset_weight;
        info.maint_liab_weight = maint_liab_weight;
        info.init_liab_weight = init_liab_weight;

        check!(version == 0, LyraeErrorCode::InvalidParam)?;

        root_bank.meta_data.version = version;
        Ok(())
    }

    /// Set the `ref_surcharge_centibps`, `ref_share_centibps` and `ref_lyr_required` on `LyraeGroup`
    #[inline(never)]
    fn change_referral_fee_params(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        ref_surcharge_centibps: u32,
        ref_share_centibps: u32,
        ref_lyr_required: u64,
    ) -> LyraeResult {
        check!(ref_surcharge_centibps >= ref_share_centibps, LyraeErrorCode::InvalidParam)?;

        const NUM_FIXED: usize = 2;
        let accounts = array_ref![accounts, 0, NUM_FIXED];

        let [
            lyrae_group_ai, // write
            admin_ai        // read, signer
        ] = accounts;

        let mut lyrae_group = LyraeGroup::load_mut_checked(lyrae_group_ai, program_id)?;
        check_eq!(admin_ai.key, &lyrae_group.admin, LyraeErrorCode::InvalidAdminKey)?;
        check!(admin_ai.is_signer, LyraeErrorCode::SignerNecessary)?;
        msg!("old referral fee params: ref_surcharge_centibps: {} ref_share_centibps: {} ref_lyr_required: {}", lyrae_group.ref_surcharge_centibps, lyrae_group.ref_share_centibps, lyrae_group.ref_lyr_required);

        // TODO - when this goes out, if there are any events on the EventQueue fee logging will be messed up

        lyrae_group.ref_surcharge_centibps = ref_surcharge_centibps;
        lyrae_group.ref_share_centibps = ref_share_centibps;
        lyrae_group.ref_lyr_required = ref_lyr_required;

        msg!("new referral fee params: ref_surcharge_centibps: {} ref_share_centibps: {} ref_lyr_required: {}", ref_surcharge_centibps, ref_share_centibps, ref_lyr_required);
        Ok(())
    }

    #[inline(never)]
    /// Store the referrer's LyraeAccount pubkey on the Referrer account
    /// It will create the Referrer account as a PDA of user's LyraeAccount if it doesn't exist
    /// This is primarily useful for the UI; the referrer address stored here is not necessarily
    /// who earns the ref fees.
    fn set_referrer_memory(program_id: &Pubkey, accounts: &[AccountInfo]) -> LyraeResult {
        const NUM_FIXED: usize = 7;
        let [
            lyrae_group_ai,             // read
            lyrae_account_ai,           // read
            owner_ai,                   // signer
            referrer_memory_ai,         // write
            referrer_lyrae_account_ai,  // read
            payer_ai,                   // write, signer
            system_prog_ai,             // read
        ] = array_ref![accounts, 0, NUM_FIXED];
        check!(
            system_prog_ai.key == &solana_program::system_program::id(),
            LyraeErrorCode::InvalidProgramId
        )?;

        let _ = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;
        let lyrae_account =
            LyraeAccount::load_checked(lyrae_account_ai, program_id, &lyrae_group_ai.key)?;
        check!(
            &lyrae_account.owner == owner_ai.key || &lyrae_account.delegate == owner_ai.key,
            LyraeErrorCode::InvalidOwner
        )?;
        check!(owner_ai.is_signer, LyraeErrorCode::InvalidSignerKey)?;

        let _ =
            LyraeAccount::load_checked(referrer_lyrae_account_ai, program_id, lyrae_group_ai.key)?;

        if referrer_memory_ai.data_is_empty() {
            // initialize it if it's not initialized yet
            let referrer_seeds: &[&[u8]] = &[&lyrae_account_ai.key.as_ref(), b"ReferrerMemory"];
            seed_and_create_pda(
                program_id,
                payer_ai,
                &Rent::get()?,
                size_of::<ReferrerMemory>(),
                program_id,
                system_prog_ai,
                referrer_memory_ai,
                referrer_seeds,
                &[],
            )?;
            ReferrerMemory::init(referrer_memory_ai, program_id, referrer_lyrae_account_ai)
        } else {
            // otherwise just set referrer pubkey
            let mut referrer_memory =
                ReferrerMemory::load_mut_checked(referrer_memory_ai, program_id)?;
            referrer_memory.referrer_lyrae_account = *referrer_lyrae_account_ai.key;
            Ok(())
        }
    }

    /// Associate the referrer's LyraeAccount with a human readable `referrer_id` which can be used
    /// in a ref link
    /// Create the `ReferrerIdRecord` PDA; if it already exists throw error
    #[inline(never)]
    fn register_referrer_id(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        referrer_id: [u8; INFO_LEN],
    ) -> LyraeResult {
        const NUM_FIXED: usize = 5;
        let [
            lyrae_group_ai,             // read
            referrer_lyrae_account_ai,  // read
            referrer_id_record_ai,      // write
            payer_ai,                   // write, signer
            system_prog_ai,             // read
        ] = array_ref![accounts, 0, NUM_FIXED];
        check!(
            system_prog_ai.key == &solana_program::system_program::id(),
            LyraeErrorCode::InvalidProgramId
        )?;

        let _ = LyraeGroup::load_checked(lyrae_group_ai, program_id)?;

        let _ =
            LyraeAccount::load_checked(referrer_lyrae_account_ai, program_id, lyrae_group_ai.key)?;

        // referrer_id_record must be empty; cannot be transferred
        check!(referrer_id_record_ai.data_is_empty(), LyraeErrorCode::InvalidAccount)?;
        let referrer_record_seeds: &[&[u8]] =
            &[&lyrae_group_ai.key.as_ref(), b"ReferrerIdRecord", &referrer_id];
        seed_and_create_pda(
            program_id,
            payer_ai,
            &Rent::get()?,
            size_of::<ReferrerIdRecord>(),
            program_id,
            system_prog_ai,
            referrer_id_record_ai,
            referrer_record_seeds,
            &[],
        )?;

        ReferrerIdRecord::init(
            referrer_id_record_ai,
            program_id,
            referrer_lyrae_account_ai,
            referrer_id,
        )
    }
    pub fn process(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> LyraeResult {
        let instruction =
            LyraeInstruction::unpack(data).ok_or(ProgramError::InvalidInstructionData)?;
        match instruction {
            LyraeInstruction::InitLyraeGroup {
                signer_nonce,
                valid_interval,
                quote_optimal_util,
                quote_optimal_rate,
                quote_max_rate,
            } => {
                msg!("Lyrae: InitLyraeGroup");
                Self::init_lyrae_group(
                    program_id,
                    accounts,
                    signer_nonce,
                    valid_interval,
                    quote_optimal_util,
                    quote_optimal_rate,
                    quote_max_rate,
                )
            }
            LyraeInstruction::InitLyraeAccount => {
                msg!("Lyrae: InitLyraeAccount DEPRECATED");
                Self::init_lyrae_account(program_id, accounts)
            }
            LyraeInstruction::CreateLyraeAccount { account_num } => {
                msg!("Lyrae: CreateLyraeAccount");
                Self::create_lyrae_account(program_id, accounts, account_num)
            }
            LyraeInstruction::CloseLyraeAccount => {
                msg!("Lyrae: CloseLyraeAccount");
                Self::close_lyrae_account(program_id, accounts)
            }
            LyraeInstruction::UpgradeLyraeAccountV0V1 => {
                msg!("Lyrae: UpgradeLyraeAccountV0V1");
                Self::upgrade_lyrae_account_v0_v1(program_id, accounts)
            }
            LyraeInstruction::Deposit { quantity } => {
                msg!("Lyrae: Deposit");
                Self::deposit(program_id, accounts, quantity)
            }
            LyraeInstruction::Withdraw { quantity, allow_borrow } => {
                msg!("Lyrae: Withdraw");
                Self::withdraw(program_id, accounts, quantity, allow_borrow)
            }
            LyraeInstruction::AddSpotMarket {
                maint_leverage,
                init_leverage,
                liquidation_fee,
                optimal_util,
                optimal_rate,
                max_rate,
            } => {
                msg!("Lyrae: AddSpotMarket");
                Self::add_spot_market(
                    program_id,
                    accounts,
                    maint_leverage,
                    init_leverage,
                    liquidation_fee,
                    optimal_util,
                    optimal_rate,
                    max_rate,
                )
            }
            LyraeInstruction::AddToBasket { .. } => {
                msg!("Lyrae: AddToBasket Deprecated");
                Ok(())
            }
            LyraeInstruction::Borrow { .. } => {
                msg!("Lyrae: Borrow DEPRECATED");
                Ok(())
            }
            LyraeInstruction::CachePrices => {
                msg!("Lyrae: CachePrices");
                Self::cache_prices(program_id, accounts)
            }
            LyraeInstruction::CacheRootBanks => {
                msg!("Lyrae: CacheRootBanks");
                Self::cache_root_banks(program_id, accounts)
            }
            LyraeInstruction::PlaceSpotOrder { order } => {
                msg!("Lyrae: PlaceSpotOrder");
                Self::place_spot_order(program_id, accounts, order)
            }
            LyraeInstruction::CancelSpotOrder { order, .. } => {
                msg!("Lyrae: CancelSpotOrder");
                let data = serum_dex::instruction::MarketInstruction::CancelOrderV2(order).pack();
                Self::cancel_spot_order(program_id, accounts, data)
            }
            LyraeInstruction::AddOracle => {
                msg!("Lyrae: AddOracle");
                Self::add_oracle(program_id, accounts)
            }
            LyraeInstruction::SettleFunds => {
                msg!("Lyrae: SettleFunds");
                Self::settle_funds(program_id, accounts)
            }
            LyraeInstruction::UpdateRootBank => {
                msg!("Lyrae: UpdateRootBank");
                Self::update_root_bank(program_id, accounts)
            }

            LyraeInstruction::AddPerpMarket {
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
            } => {
                msg!("Lyrae: AddPerpMarket DEPRECATED");
                Self::add_perp_market(
                    program_id,
                    accounts,
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
                )
            }
            LyraeInstruction::PlacePerpOrder {
                side,
                price,
                quantity,
                client_order_id,
                order_type,
                reduce_only,
            } => {
                msg!("Lyrae: PlacePerpOrder client_order_id={}", client_order_id);
                Self::place_perp_order(
                    program_id,
                    accounts,
                    side,
                    price,
                    quantity,
                    client_order_id,
                    order_type,
                    reduce_only,
                )
            }
            LyraeInstruction::CancelPerpOrderByClientId { client_order_id, invalid_id_ok } => {
                msg!("Lyrae: CancelPerpOrderByClientId client_order_id={}", client_order_id);
                let result =
                    Self::cancel_perp_order_by_client_id(program_id, accounts, client_order_id);
                if invalid_id_ok {
                    if let Err(LyraeError::LyraeErrorCode { lyrae_error_code, .. }) = result {
                        if lyrae_error_code == LyraeErrorCode::InvalidOrderId
                            || lyrae_error_code == LyraeErrorCode::ClientIdNotFound
                        {
                            return Ok(());
                        }
                    }
                }
                result
            }
            LyraeInstruction::CancelPerpOrder { order_id, invalid_id_ok } => {
                // TODO OPT this log may cost too much compute
                msg!("Lyrae: CancelPerpOrder order_id={}", order_id);
                let result = Self::cancel_perp_order(program_id, accounts, order_id);
                if invalid_id_ok {
                    if let Err(LyraeError::LyraeErrorCode { lyrae_error_code, .. }) = result {
                        if lyrae_error_code == LyraeErrorCode::InvalidOrderId {
                            return Ok(());
                        }
                    }
                }
                result
            }
            LyraeInstruction::ConsumeEvents { limit } => {
                msg!("Lyrae: ConsumeEvents limit={}", limit);
                Self::consume_events(program_id, accounts, limit)
            }
            LyraeInstruction::CachePerpMarkets => {
                msg!("Lyrae: CachePerpMarkets");
                Self::cache_perp_markets(program_id, accounts)
            }
            LyraeInstruction::UpdateFunding => {
                msg!("Lyrae: UpdateFunding");
                Self::update_funding(program_id, accounts)
            }
            LyraeInstruction::SetOracle { price } => {
                // msg!("Lyrae: SetOracle {:?}", price);
                msg!("Lyrae: SetOracle");
                Self::set_oracle(program_id, accounts, price)
            }
            LyraeInstruction::SettlePnl { market_index } => {
                msg!("Lyrae: SettlePnl");
                Self::settle_pnl(program_id, accounts, market_index)
            }
            LyraeInstruction::SettleBorrow { .. } => {
                msg!("Lyrae: SettleBorrow DEPRECATED");
                Ok(())
            }
            LyraeInstruction::ForceCancelSpotOrders { limit } => {
                msg!("Lyrae: ForceCancelSpotOrders");
                Self::force_cancel_spot_orders(program_id, accounts, limit)
            }
            LyraeInstruction::ForceCancelPerpOrders { limit } => {
                msg!("Lyrae: ForceCancelPerpOrders");
                Self::force_cancel_perp_orders(program_id, accounts, limit)
            }
            LyraeInstruction::LiquidateTokenAndToken { max_liab_transfer } => {
                msg!("Lyrae: LiquidateTokenAndToken");
                Self::liquidate_token_and_token(program_id, accounts, max_liab_transfer)
            }
            LyraeInstruction::LiquidateTokenAndPerp {
                asset_type,
                asset_index,
                liab_type,
                liab_index,
                max_liab_transfer,
            } => {
                msg!("Lyrae: LiquidateTokenAndPerp");
                Self::liquidate_token_and_perp(
                    program_id,
                    accounts,
                    asset_type,
                    asset_index,
                    liab_type,
                    liab_index,
                    max_liab_transfer,
                )
            }
            LyraeInstruction::LiquidatePerpMarket { base_transfer_request } => {
                msg!("Lyrae: LiquidatePerpMarket");
                Self::liquidate_perp_market(program_id, accounts, base_transfer_request)
            }
            LyraeInstruction::SettleFees => {
                msg!("Lyrae: SettleFees");
                Self::settle_fees(program_id, accounts)
            }
            LyraeInstruction::ResolvePerpBankruptcy { liab_index, max_liab_transfer } => {
                msg!("Lyrae: ResolvePerpBankruptcy");
                Self::resolve_perp_bankruptcy(program_id, accounts, liab_index, max_liab_transfer)
            }
            LyraeInstruction::ResolveTokenBankruptcy { max_liab_transfer } => {
                msg!("Lyrae: ResolveTokenBankruptcy");
                Self::resolve_token_bankruptcy(program_id, accounts, max_liab_transfer)
            }
            LyraeInstruction::InitSpotOpenOrders => {
                msg!("Lyrae: InitSpotOpenOrders");
                Self::init_spot_open_orders(program_id, accounts)
            }
            LyraeInstruction::CloseSpotOpenOrders => {
                msg!("Lyrae: CloseSpotOpenOrders");
                Self::close_spot_open_orders(program_id, accounts)
            }
            LyraeInstruction::RedeemLyr => {
                msg!("Lyrae: RedeemLyr");
                Self::redeem_lyr(program_id, accounts)
            }
            LyraeInstruction::AddLyraeAccountInfo { info } => {
                msg!("Lyrae: AddLyraeAccountInfo");
                Self::add_lyrae_account_info(program_id, accounts, info)
            }
            LyraeInstruction::DepositMsrm { quantity } => {
                msg!("Lyrae: DepositMsrm");
                Self::deposit_msrm(program_id, accounts, quantity)
            }
            LyraeInstruction::WithdrawMsrm { quantity } => {
                msg!("Lyrae: WithdrawMsrm");
                Self::withdraw_msrm(program_id, accounts, quantity)
            }
            LyraeInstruction::ChangePerpMarketParams { .. } => {
                msg!("Lyrae: ChangePerpMarketParams DEPRECATED - use ChangePerpMarketParams2 instead");
                Ok(())
            }
            LyraeInstruction::SetGroupAdmin => {
                msg!("Lyrae: SetGroupAdmin");
                Self::set_group_admin(program_id, accounts)
            }
            LyraeInstruction::CancelAllPerpOrders { limit } => {
                msg!("Lyrae: CancelAllPerpOrders | limit={}", limit);
                Self::cancel_all_perp_orders(program_id, accounts, limit)
            }
            LyraeInstruction::ForceSettleQuotePositions => {
                msg!("DEPRECATED Lyrae: ForceSettleQuotePositions");
                Ok(())
            }
            LyraeInstruction::PlaceSpotOrder2 { order } => {
                msg!("Lyrae: PlaceSpotOrder2");
                Self::place_spot_order2(program_id, accounts, order)
            }
            LyraeInstruction::InitAdvancedOrders => {
                msg!("Lyrae: InitAdvancedOrders");
                Self::init_advanced_orders(program_id, accounts)
            }
            LyraeInstruction::CloseAdvancedOrders => {
                msg!("Lyrae: CloseAdvancedOrders");
                Self::close_advanced_orders(program_id, accounts)
            }
            LyraeInstruction::AddPerpTriggerOrder {
                order_type,
                side,
                trigger_condition,
                reduce_only,
                client_order_id,
                price,
                quantity,
                trigger_price,
            } => {
                msg!(
                    "Lyrae: AddPerpTriggerOrder client_order_id={} type={:?} side={:?} trigger_condition={:?} price={} quantity={} trigger={}",
                    client_order_id,
                    order_type,
                    side,
                    trigger_condition,
                    price,
                    quantity,
                    trigger_price.to_num::<f64>()
                );
                Self::add_perp_trigger_order(
                    program_id,
                    accounts,
                    order_type,
                    side,
                    trigger_condition,
                    reduce_only,
                    client_order_id,
                    price,
                    quantity,
                    trigger_price,
                )
            }
            LyraeInstruction::RemoveAdvancedOrder { order_index } => {
                msg!("Lyrae: RemoveAdvancedOrder {}", order_index);
                Self::remove_advanced_order(program_id, accounts, order_index)
            }
            LyraeInstruction::ExecutePerpTriggerOrder { order_index } => {
                msg!("Lyrae: ExecutePerpTriggerOrder {}", order_index);
                Self::execute_perp_trigger_order(program_id, accounts, order_index)
            }
            LyraeInstruction::CreatePerpMarket {
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
            } => {
                msg!("Lyrae: CreatePerpMarket");
                Self::create_perp_market(
                    program_id,
                    accounts,
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
                )
            }
            LyraeInstruction::ChangePerpMarketParams2 {
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
            } => {
                msg!("Lyrae: ChangePerpMarketParams2");
                Self::change_perp_market_params2(
                    program_id,
                    accounts,
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
                )
            }
            LyraeInstruction::UpdateMarginBasket => {
                msg!("Lyrae: UpdateMarginBasket");
                Self::update_margin_basket(program_id, accounts)
            }
            LyraeInstruction::ChangeMaxLyraeAccounts { max_lyrae_accounts } => {
                msg!("Lyrae: ChangeMaxLyraeAccounts");
                Self::change_max_lyrae_accounts(program_id, accounts, max_lyrae_accounts)
            }
            LyraeInstruction::CreateDustAccount => {
                msg!("Lyrae: CreateDustAccount");
                Self::create_dust_account(program_id, accounts)
            }
            LyraeInstruction::ResolveDust => {
                msg!("Lyrae: ResolveDust");
                Self::resolve_dust(program_id, accounts)
            }
            LyraeInstruction::CancelPerpOrdersSide { side, limit } => {
                msg!("Lyrae: CancelSidePerpOrders");
                Self::cancel_perp_orders_side(program_id, accounts, side, limit)
            }
            LyraeInstruction::SetDelegate => {
                msg!("Lyrae: SetDelegate");
                Self::set_delegate(program_id, accounts)
            }
            LyraeInstruction::ChangeSpotMarketParams {
                maint_leverage,
                init_leverage,
                liquidation_fee,
                optimal_util,
                optimal_rate,
                max_rate,
                version,
            } => {
                msg!("Lyrae: ChangeSpotMarketParams");
                Self::change_spot_market_params(
                    program_id,
                    accounts,
                    maint_leverage,
                    init_leverage,
                    liquidation_fee,
                    optimal_util,
                    optimal_rate,
                    max_rate,
                    version,
                )
            }
            LyraeInstruction::CreateSpotOpenOrders => {
                msg!("Lyrae: CreateSpotOpenOrders");
                Self::create_spot_open_orders(program_id, accounts)
            }
            LyraeInstruction::ChangeReferralFeeParams {
                ref_surcharge_centibps,
                ref_share_centibps,
                ref_lyr_required,
            } => {
                msg!("Lyrae: ChangeReferralFeeParams");
                Self::change_referral_fee_params(
                    program_id,
                    accounts,
                    ref_surcharge_centibps,
                    ref_share_centibps,
                    ref_lyr_required,
                )
            }
            LyraeInstruction::SetReferrerMemory => {
                msg!("Lyrae: SetReferrerMemory");
                Self::set_referrer_memory(program_id, accounts)
            }
            LyraeInstruction::RegisterReferrerId { referrer_id } => {
                msg!("Lyrae: RegisterReferrerId");
                Self::register_referrer_id(program_id, accounts, referrer_id)
            }
        }
    }
}

fn init_root_bank(
    program_id: &Pubkey,
    lyrae_group: &LyraeGroup,
    mint_ai: &AccountInfo,
    vault_ai: &AccountInfo,
    root_bank_ai: &AccountInfo,
    node_bank_ai: &AccountInfo,
    rent: &Rent,

    optimal_util: I80F48,
    optimal_rate: I80F48,
    max_rate: I80F48,
) -> LyraeResult<RootBank> {
    let vault = Account::unpack(&vault_ai.try_borrow_data()?)?;
    check!(vault.is_initialized(), LyraeErrorCode::InvalidVault)?;
    check!(vault.delegate.is_none(), LyraeErrorCode::InvalidVault)?;
    check!(vault.close_authority.is_none(), LyraeErrorCode::InvalidVault)?;
    check_eq!(vault.owner, lyrae_group.signer_key, LyraeErrorCode::InvalidVault)?;
    check_eq!(&vault.mint, mint_ai.key, LyraeErrorCode::InvalidVault)?;
    check_eq!(vault_ai.owner, &spl_token::id(), LyraeErrorCode::InvalidVault)?;

    let _node_bank = NodeBank::load_and_init(&node_bank_ai, &program_id, &vault_ai, rent)?;
    let root_bank = RootBank::load_and_init(
        &root_bank_ai,
        &program_id,
        node_bank_ai,
        rent,
        optimal_util,
        optimal_rate,
        max_rate,
    )?;

    Ok(*root_bank)
}

fn invoke_settle_funds<'a>(
    dex_prog_ai: &AccountInfo<'a>,
    spot_market_ai: &AccountInfo<'a>,
    open_orders_ai: &AccountInfo<'a>,
    signer_ai: &AccountInfo<'a>,
    dex_base_ai: &AccountInfo<'a>,
    dex_quote_ai: &AccountInfo<'a>,
    base_vault_ai: &AccountInfo<'a>,
    quote_vault_ai: &AccountInfo<'a>,
    dex_signer_ai: &AccountInfo<'a>,
    token_prog_ai: &AccountInfo<'a>,
    signers_seeds: &[&[&[u8]]],
) -> ProgramResult {
    let data = serum_dex::instruction::MarketInstruction::SettleFunds.pack();
    let instruction = Instruction {
        program_id: *dex_prog_ai.key,
        data,
        accounts: vec![
            AccountMeta::new(*spot_market_ai.key, false),
            AccountMeta::new(*open_orders_ai.key, false),
            AccountMeta::new_readonly(*signer_ai.key, true),
            AccountMeta::new(*dex_base_ai.key, false),
            AccountMeta::new(*dex_quote_ai.key, false),
            AccountMeta::new(*base_vault_ai.key, false),
            AccountMeta::new(*quote_vault_ai.key, false),
            AccountMeta::new_readonly(*dex_signer_ai.key, false),
            AccountMeta::new_readonly(*token_prog_ai.key, false),
            AccountMeta::new(*quote_vault_ai.key, false),
        ],
    };

    let account_infos = [
        dex_prog_ai.clone(),
        spot_market_ai.clone(),
        open_orders_ai.clone(),
        signer_ai.clone(),
        dex_base_ai.clone(),
        dex_quote_ai.clone(),
        base_vault_ai.clone(),
        quote_vault_ai.clone(),
        dex_signer_ai.clone(),
        token_prog_ai.clone(),
        quote_vault_ai.clone(),
    ];
    solana_program::program::invoke_signed_unchecked(&instruction, &account_infos, signers_seeds)
}

fn invoke_cancel_order<'a>(
    dex_prog_ai: &AccountInfo<'a>,
    spot_market_ai: &AccountInfo<'a>,
    bids_ai: &AccountInfo<'a>,
    asks_ai: &AccountInfo<'a>,
    open_orders_ai: &AccountInfo<'a>,
    signer_ai: &AccountInfo<'a>,
    dex_event_queue_ai: &AccountInfo<'a>,
    data: Vec<u8>,
    signers_seeds: &[&[&[u8]]],
) -> ProgramResult {
    let instruction = Instruction {
        program_id: *dex_prog_ai.key,
        data,
        accounts: vec![
            AccountMeta::new(*spot_market_ai.key, false),
            AccountMeta::new(*bids_ai.key, false),
            AccountMeta::new(*asks_ai.key, false),
            AccountMeta::new(*open_orders_ai.key, false),
            AccountMeta::new_readonly(*signer_ai.key, true),
            AccountMeta::new(*dex_event_queue_ai.key, false),
        ],
    };

    let account_infos = [
        dex_prog_ai.clone(),
        spot_market_ai.clone(),
        bids_ai.clone(),
        asks_ai.clone(),
        open_orders_ai.clone(),
        signer_ai.clone(),
        dex_event_queue_ai.clone(),
    ];
    solana_program::program::invoke_signed(&instruction, &account_infos, signers_seeds)
}

fn invoke_transfer<'a>(
    token_prog_ai: &AccountInfo<'a>,
    source_ai: &AccountInfo<'a>,
    dest_ai: &AccountInfo<'a>,
    authority_ai: &AccountInfo<'a>,
    signers_seeds: &[&[&[u8]]],
    quantity: u64,
) -> ProgramResult {
    let transfer_instruction = spl_token::instruction::transfer(
        &spl_token::ID,
        source_ai.key,
        dest_ai.key,
        authority_ai.key,
        &[],
        quantity,
    )?;
    let accs = [
        token_prog_ai.clone(), // TODO check if passing in program_id is necessary
        source_ai.clone(),
        dest_ai.clone(),
        authority_ai.clone(),
    ];

    solana_program::program::invoke_signed(&transfer_instruction, &accs, signers_seeds)
}

#[inline(never)]
fn read_oracle(
    lyrae_group: &LyraeGroup,
    token_index: usize,
    oracle_ai: &AccountInfo,
) -> LyraeResult<I80F48> {
    let quote_decimals = lyrae_group.tokens[QUOTE_INDEX].decimals as i32;
    let base_decimals = lyrae_group.tokens[token_index].decimals as i32;

    let oracle_type = determine_oracle_type(oracle_ai);

    let price = match oracle_type {
        OracleType::Pyth => {
            let price_account = Price::get_price(oracle_ai)?;
            let value = I80F48::from_num(price_account.agg.price);

            // Filter out bad prices on mainnet
            #[cfg(not(feature = "devnet"))]
            let conf = I80F48::from_num(price_account.agg.conf).checked_div(value).unwrap();

            #[cfg(not(feature = "devnet"))]
            if price_account.agg.status != PriceStatus::Trading {
                msg!("Pyth status invalid: {}", price_account.agg.status as u8);
                return Err(throw_err!(LyraeErrorCode::InvalidOraclePrice));
            } else if conf > PYTH_CONF_FILTER {
                msg!(
                    "Pyth conf interval too high; oracle index: {} value: {} conf: {}",
                    token_index,
                    value.to_num::<f64>(),
                    conf.to_num::<f64>()
                );
                return Err(throw_err!(LyraeErrorCode::InvalidOraclePrice));
            }

            let decimals = quote_decimals
                .checked_add(price_account.expo)
                .unwrap()
                .checked_sub(base_decimals)
                .unwrap();

            let decimal_adj = I80F48::from_num(10u64.pow(decimals.abs() as u32));
            if decimals < 0 {
                value.checked_div(decimal_adj).unwrap()
            } else {
                value.checked_mul(decimal_adj).unwrap()
            }
        }
        OracleType::Stub => {
            let oracle = StubOracle::load(oracle_ai)?;
            I80F48::from_num(oracle.price)
        }
        OracleType::Switchboard => {
            let result =
                FastRoundResultAccountData::deserialize(&oracle_ai.try_borrow_data()?).unwrap();
            let value = I80F48::from_num(result.result.result);

            let decimals = quote_decimals.checked_sub(base_decimals).unwrap();
            if decimals < 0 {
                let decimal_adj = I80F48::from_num(10u64.pow(decimals.abs() as u32));
                value.checked_div(decimal_adj).unwrap()
            } else if decimals > 0 {
                let decimal_adj = I80F48::from_num(10u64.pow(decimals.abs() as u32));
                value.checked_mul(decimal_adj).unwrap()
            } else {
                value
            }
        }
        OracleType::Unknown => return Err(throw_err!(LyraeErrorCode::InvalidOracleType)),
    };
    Ok(price)
}

/// Transfer token deposits/borrows between two LyraeAccounts
/// `native_quantity` is subtracted from src and added to dst
/// Make sure to credit deposits first in case Node bank is fully utilized
fn transfer_token_internal(
    root_bank_cache: &RootBankCache,
    node_bank: &mut NodeBank,
    src: &mut LyraeAccount,
    dst: &mut LyraeAccount,
    src_pk: &Pubkey,
    dst_pk: &Pubkey,
    token_index: usize,
    native_quantity: I80F48,
) -> LyraeResult<()> {
    if native_quantity.is_positive() {
        // increase dst first before decreasing from src
        checked_change_net(root_bank_cache, node_bank, dst, dst_pk, token_index, native_quantity)?;
        checked_change_net(root_bank_cache, node_bank, src, src_pk, token_index, -native_quantity)?;
    } else if native_quantity.is_negative() {
        // increase src first before decreasing from dst
        checked_change_net(root_bank_cache, node_bank, src, src_pk, token_index, -native_quantity)?;
        checked_change_net(root_bank_cache, node_bank, dst, dst_pk, token_index, native_quantity)?;
    }
    Ok(())
}

fn checked_change_net(
    root_bank_cache: &RootBankCache,
    node_bank: &mut NodeBank,
    lyrae_account: &mut LyraeAccount,
    lyrae_account_pk: &Pubkey,
    token_index: usize,
    native_quantity: I80F48,
) -> LyraeResult<()> {
    if native_quantity.is_negative() {
        checked_sub_net(root_bank_cache, node_bank, lyrae_account, token_index, -native_quantity)?;
    } else if native_quantity.is_positive() {
        checked_add_net(root_bank_cache, node_bank, lyrae_account, token_index, native_quantity)?;
    }
    lyrae_emit!(TokenBalanceLog {
        lyrae_group: lyrae_account.lyrae_group,
        lyrae_account: *lyrae_account_pk,
        token_index: token_index as u64,
        deposit: lyrae_account.deposits[token_index].to_bits(),
        borrow: lyrae_account.borrows[token_index].to_bits()
    });

    Ok(()) // This is an optimization to prevent unnecessary I80F48 calculations
}

/// If there are borrows, pay down borrows first then increase deposits
/// WARNING: won't work if native_quantity is less than zero
fn checked_add_net(
    root_bank_cache: &RootBankCache,
    node_bank: &mut NodeBank,
    lyrae_account: &mut LyraeAccount,
    token_index: usize,
    mut native_quantity: I80F48,
) -> LyraeResult<()> {
    if lyrae_account.borrows[token_index].is_positive() {
        let native_borrows = lyrae_account.get_native_borrow(root_bank_cache, token_index)?;

        if native_quantity < native_borrows {
            return checked_sub_borrow(
                node_bank,
                lyrae_account,
                token_index,
                native_quantity / root_bank_cache.borrow_index,
            );
        } else {
            let borrows = lyrae_account.borrows[token_index];
            checked_sub_borrow(node_bank, lyrae_account, token_index, borrows)?;
            native_quantity -= native_borrows;
        }
    }

    checked_add_deposit(
        node_bank,
        lyrae_account,
        token_index,
        native_quantity / root_bank_cache.deposit_index,
    )
}

/// If there are deposits, draw down deposits first then increase borrows
/// WARNING: won't work if native_quantity is less than zero
fn checked_sub_net(
    root_bank_cache: &RootBankCache,
    node_bank: &mut NodeBank,
    lyrae_account: &mut LyraeAccount,
    token_index: usize,
    mut native_quantity: I80F48,
) -> LyraeResult<()> {
    if lyrae_account.deposits[token_index].is_positive() {
        let native_deposits = lyrae_account.get_native_deposit(root_bank_cache, token_index)?;

        if native_quantity < native_deposits {
            return checked_sub_deposit(
                node_bank,
                lyrae_account,
                token_index,
                native_quantity / root_bank_cache.deposit_index,
            );
        } else {
            let deposits = lyrae_account.deposits[token_index];
            checked_sub_deposit(node_bank, lyrae_account, token_index, deposits)?;
            native_quantity -= native_deposits;
        }
    }

    checked_add_borrow(
        node_bank,
        lyrae_account,
        token_index,
        native_quantity / root_bank_cache.borrow_index,
    )?;

    check!(
        node_bank.has_valid_deposits_borrows(root_bank_cache),
        LyraeErrorCode::InsufficientLiquidity
    )
}

/// TODO - although these values are I8048, they must never be less than zero
fn checked_add_deposit(
    node_bank: &mut NodeBank,
    lyrae_account: &mut LyraeAccount,
    token_index: usize,
    quantity: I80F48,
) -> LyraeResult<()> {
    lyrae_account.checked_add_deposit(token_index, quantity)?;
    node_bank.checked_add_deposit(quantity)
}

fn checked_sub_deposit(
    node_bank: &mut NodeBank,
    lyrae_account: &mut LyraeAccount,
    token_index: usize,
    quantity: I80F48,
) -> LyraeResult<()> {
    lyrae_account.checked_sub_deposit(token_index, quantity)?;
    node_bank.checked_sub_deposit(quantity)
}

fn checked_add_borrow(
    node_bank: &mut NodeBank,
    lyrae_account: &mut LyraeAccount,
    token_index: usize,
    quantity: I80F48,
) -> LyraeResult<()> {
    lyrae_account.checked_add_borrow(token_index, quantity)?;
    node_bank.checked_add_borrow(quantity)
}

fn checked_sub_borrow(
    node_bank: &mut NodeBank,
    lyrae_account: &mut LyraeAccount,
    token_index: usize,
    quantity: I80F48,
) -> LyraeResult<()> {
    lyrae_account.checked_sub_borrow(token_index, quantity)?;
    node_bank.checked_sub_borrow(quantity)
}

fn invoke_cancel_orders<'a>(
    open_orders_ai: &AccountInfo<'a>,
    dex_prog_ai: &AccountInfo<'a>,
    spot_market_ai: &AccountInfo<'a>,
    bids_ai: &AccountInfo<'a>,
    asks_ai: &AccountInfo<'a>,
    signer_ai: &AccountInfo<'a>,
    dex_event_queue_ai: &AccountInfo<'a>,
    signers_seeds: &[&[&[u8]]],

    mut limit: u8,
) -> LyraeResult<()> {
    let mut cancels = vec![];
    {
        let open_orders = load_open_orders(open_orders_ai)?;

        let market = load_market_state(spot_market_ai, dex_prog_ai.key)?;
        let bids = load_bids_mut(&market, bids_ai)?;
        let asks = load_asks_mut(&market, asks_ai)?;

        limit = min(limit, open_orders.free_slot_bits.count_zeros() as u8);
        if limit == 0 {
            return Ok(());
        }
        for j in 0..128 {
            let slot_mask = 1u128 << j;
            if open_orders.free_slot_bits & slot_mask != 0 {
                // means slot is free
                continue;
            }
            let order_id = open_orders.orders[j];

            let side = if open_orders.is_bid_bits & slot_mask != 0 {
                match bids.find_by_key(order_id) {
                    None => continue,
                    Some(_) => serum_dex::matching::Side::Bid,
                }
            } else {
                match asks.find_by_key(order_id) {
                    None => continue,
                    Some(_) => serum_dex::matching::Side::Ask,
                }
            };

            let cancel_instruction =
                serum_dex::instruction::CancelOrderInstructionV2 { side, order_id };

            cancels.push(cancel_instruction);

            limit -= 1;
            if limit == 0 {
                break;
            }
        }
    }

    let mut instruction = Instruction {
        program_id: *dex_prog_ai.key,
        data: vec![],
        accounts: vec![
            AccountMeta::new(*spot_market_ai.key, false),
            AccountMeta::new(*bids_ai.key, false),
            AccountMeta::new(*asks_ai.key, false),
            AccountMeta::new(*open_orders_ai.key, false),
            AccountMeta::new_readonly(*signer_ai.key, true),
            AccountMeta::new(*dex_event_queue_ai.key, false),
        ],
    };

    let account_infos = [
        dex_prog_ai.clone(),
        spot_market_ai.clone(),
        bids_ai.clone(),
        asks_ai.clone(),
        open_orders_ai.clone(),
        signer_ai.clone(),
        dex_event_queue_ai.clone(),
    ];

    for cancel in cancels.iter() {
        let cancel_instruction =
            serum_dex::instruction::MarketInstruction::CancelOrderV2(cancel.clone());
        instruction.data = cancel_instruction.pack();
        solana_program::program::invoke_signed(&instruction, &account_infos, signers_seeds)?;
    }

    Ok(())
}

fn invoke_new_order<'a>(
    dex_prog_ai: &AccountInfo<'a>, // Have to add account of the program id
    spot_market_ai: &AccountInfo<'a>,
    open_orders_ai: &AccountInfo<'a>,
    dex_request_queue_ai: &AccountInfo<'a>,
    dex_event_queue_ai: &AccountInfo<'a>,
    bids_ai: &AccountInfo<'a>,
    asks_ai: &AccountInfo<'a>,
    vault_ai: &AccountInfo<'a>,
    signer_ai: &AccountInfo<'a>,
    dex_base_ai: &AccountInfo<'a>,
    dex_quote_ai: &AccountInfo<'a>,
    token_prog_ai: &AccountInfo<'a>,
    msrm_or_srm_vault_ai: &AccountInfo<'a>,
    signers_seeds: &[&[&[u8]]],

    order: NewOrderInstructionV3,
) -> ProgramResult {
    let data = serum_dex::instruction::MarketInstruction::NewOrderV3(order).pack();
    let mut instruction = Instruction {
        program_id: *dex_prog_ai.key,
        data,
        accounts: vec![
            AccountMeta::new(*spot_market_ai.key, false),
            AccountMeta::new(*open_orders_ai.key, false),
            AccountMeta::new(*dex_request_queue_ai.key, false),
            AccountMeta::new(*dex_event_queue_ai.key, false),
            AccountMeta::new(*bids_ai.key, false),
            AccountMeta::new(*asks_ai.key, false),
            AccountMeta::new(*vault_ai.key, false),
            AccountMeta::new_readonly(*signer_ai.key, true),
            AccountMeta::new(*dex_base_ai.key, false),
            AccountMeta::new(*dex_quote_ai.key, false),
            AccountMeta::new_readonly(*token_prog_ai.key, false),
            AccountMeta::new_readonly(*signer_ai.key, false),
        ],
    };

    if msrm_or_srm_vault_ai.key != &Pubkey::default() {
        instruction.accounts.push(AccountMeta::new_readonly(*msrm_or_srm_vault_ai.key, false));
        let account_infos = [
            dex_prog_ai.clone(), // Have to add account of the program id
            spot_market_ai.clone(),
            open_orders_ai.clone(),
            dex_request_queue_ai.clone(),
            dex_event_queue_ai.clone(),
            bids_ai.clone(),
            asks_ai.clone(),
            vault_ai.clone(),
            signer_ai.clone(),
            dex_base_ai.clone(),
            dex_quote_ai.clone(),
            token_prog_ai.clone(),
            signer_ai.clone(),
            msrm_or_srm_vault_ai.clone(),
        ];
        solana_program::program::invoke_signed_unchecked(
            &instruction,
            &account_infos,
            signers_seeds,
        )
    } else {
        let account_infos = [
            dex_prog_ai.clone(), // Have to add account of the program id
            spot_market_ai.clone(),
            open_orders_ai.clone(),
            dex_request_queue_ai.clone(),
            dex_event_queue_ai.clone(),
            bids_ai.clone(),
            asks_ai.clone(),
            vault_ai.clone(),
            signer_ai.clone(),
            dex_base_ai.clone(),
            dex_quote_ai.clone(),
            token_prog_ai.clone(),
            signer_ai.clone(),
        ];
        solana_program::program::invoke_signed_unchecked(
            &instruction,
            &account_infos,
            signers_seeds,
        )
    }
}

fn invoke_init_open_orders<'a>(
    dex_prog_ai: &AccountInfo<'a>, // Have to add account of the program id
    open_orders_ai: &AccountInfo<'a>,
    signer_ai: &AccountInfo<'a>,
    spot_market_ai: &AccountInfo<'a>,
    rent_ai: &AccountInfo<'a>,
    signers_seeds: &[&[&[u8]]],
) -> ProgramResult {
    let data = serum_dex::instruction::MarketInstruction::InitOpenOrders.pack();
    let instruction = Instruction {
        program_id: *dex_prog_ai.key,
        data,
        accounts: vec![
            AccountMeta::new(*open_orders_ai.key, false),
            AccountMeta::new_readonly(*signer_ai.key, true),
            AccountMeta::new_readonly(*spot_market_ai.key, false),
            AccountMeta::new_readonly(*rent_ai.key, false),
        ],
    };

    let account_infos = [
        dex_prog_ai.clone(),
        open_orders_ai.clone(),
        signer_ai.clone(),
        spot_market_ai.clone(),
        rent_ai.clone(),
    ];
    solana_program::program::invoke_signed(&instruction, &account_infos, signers_seeds)
}

fn invoke_close_open_orders<'a>(
    dex_prog_ai: &AccountInfo<'a>, // Have to add account of the program id
    open_orders_ai: &AccountInfo<'a>,
    signer_ai: &AccountInfo<'a>,
    destination_ai: &AccountInfo<'a>,
    spot_market_ai: &AccountInfo<'a>,
    signers_seeds: &[&[&[u8]]],
) -> ProgramResult {
    let data = serum_dex::instruction::MarketInstruction::CloseOpenOrders.pack();

    let instruction = Instruction {
        program_id: *dex_prog_ai.key,
        data,
        accounts: vec![
            AccountMeta::new(*open_orders_ai.key, false),
            AccountMeta::new_readonly(*signer_ai.key, true),
            AccountMeta::new(*destination_ai.key, false),
            AccountMeta::new_readonly(*spot_market_ai.key, false),
        ],
    };

    let account_infos = [
        dex_prog_ai.clone(),
        open_orders_ai.clone(),
        signer_ai.clone(),
        destination_ai.clone(),
        spot_market_ai.clone(),
    ];
    solana_program::program::invoke_signed(&instruction, &account_infos, signers_seeds)
}

/*
TODO test order types
 */
fn invoke_transfer_lamports<'a>(
    src_ai: &AccountInfo<'a>,
    dst_ai: &AccountInfo<'a>,
    system_prog_ai: &AccountInfo<'a>,
    quantity: u64,
    signers_seeds: &[&[&[u8]]],
) -> ProgramResult {
    solana_program::program::invoke_signed(
        &solana_program::system_instruction::transfer(src_ai.key, dst_ai.key, quantity),
        &[src_ai.clone(), dst_ai.clone(), system_prog_ai.clone()],
        signers_seeds,
    )
}

fn seed_and_create_pda<'a>(
    program_id: &Pubkey,
    funder: &AccountInfo<'a>,
    rent: &Rent,
    space: usize,
    owner: &Pubkey,
    system_program: &AccountInfo<'a>,
    pda_account: &AccountInfo<'a>,
    seeds: &[&[u8]],
    funder_seeds: &[&[u8]],
) -> LyraeResult {
    let (pda_address, bump) = Pubkey::find_program_address(seeds, program_id);
    check!(&pda_address == pda_account.key, LyraeErrorCode::InvalidAccount)?;
    create_pda_account(
        funder,
        rent,
        space,
        owner,
        system_program,
        pda_account,
        &[seeds, &[&[bump]]].concat(),
        funder_seeds,
    )?;
    Ok(())
}
fn create_pda_account<'a>(
    funder: &AccountInfo<'a>,
    rent: &Rent,
    space: usize,
    owner: &Pubkey,
    system_program: &AccountInfo<'a>,
    new_pda_account: &AccountInfo<'a>,
    new_pda_signer_seeds: &[&[u8]],
    funder_seeds: &[&[u8]],
) -> ProgramResult {
    if new_pda_account.lamports() > 0 {
        let required_lamports =
            rent.minimum_balance(space).max(1).saturating_sub(new_pda_account.lamports());

        let transfer_seeds = if funder_seeds.is_empty() { vec![] } else { vec![funder_seeds] };
        if required_lamports > 0 {
            invoke_transfer_lamports(
                funder,
                new_pda_account,
                system_program,
                required_lamports,
                transfer_seeds.as_slice(),
            )?;
        }

        solana_program::program::invoke_signed(
            &solana_program::system_instruction::allocate(new_pda_account.key, space as u64),
            &[new_pda_account.clone(), system_program.clone()],
            &[new_pda_signer_seeds],
        )?;

        solana_program::program::invoke_signed(
            &solana_program::system_instruction::assign(new_pda_account.key, owner),
            &[new_pda_account.clone(), system_program.clone()],
            &[new_pda_signer_seeds],
        )
    } else {
        let all_signer_seeds = if funder_seeds.is_empty() {
            vec![new_pda_signer_seeds]
        } else {
            vec![funder_seeds, new_pda_signer_seeds]
        };
        solana_program::program::invoke_signed_unchecked(
            &solana_program::system_instruction::create_account(
                funder.key,
                new_pda_account.key,
                rent.minimum_balance(space).max(1),
                space as u64,
                owner,
            ),
            &[funder.clone(), new_pda_account.clone(), system_program.clone()],
            all_signer_seeds.as_slice(),
        )
    }
}

/// Transfer lamports from a src account owned by the currently executing program id
fn program_transfer_lamports(
    src_ai: &AccountInfo,
    dst_ai: &AccountInfo,
    quantity: u64,
) -> LyraeResult<()> {
    let src_lamports = src_ai.lamports().checked_sub(quantity).ok_or(math_err!())?;
    **src_ai.lamports.borrow_mut() = src_lamports;

    let dst_lamports = dst_ai.lamports().checked_add(quantity).ok_or(math_err!())?;
    **dst_ai.lamports.borrow_mut() = dst_lamports;
    Ok(())
}

fn cancel_all_advanced_orders<'a>(
    advanced_orders_ai: &AccountInfo<'a>,
    advanced_orders: &mut AdvancedOrders,
    agent_ai: &AccountInfo<'a>,
) -> LyraeResult<()> {
    let mut total_fee = 0u64;
    for i in 0..MAX_ADVANCED_ORDERS {
        if advanced_orders.orders[i].is_active {
            advanced_orders.orders[i].is_active = false;
            total_fee += ADVANCED_ORDER_FEE;
        }
    }
    program_transfer_lamports(advanced_orders_ai, agent_ai, total_fee)
}

// Returns asset_weight and liab_weight
pub fn get_leverage_weights(leverage: I80F48) -> (I80F48, I80F48) {
    (
        (leverage - ONE_I80F48).checked_div(leverage).unwrap(),
        (leverage + ONE_I80F48).checked_div(leverage).unwrap(),
    )
}
use bytemuck::Contiguous;
use solana_program::program_error::ProgramError;

use num_enum::IntoPrimitive;
use thiserror::Error;

pub type LyraeResult<T = ()> = Result<T, LyraeError>;

#[repr(u8)]
#[derive(Debug, Clone, Eq, PartialEq, Copy)]
pub enum SourceFileId {
    Processor = 0,
    State = 1,
    Critbit = 2,
    Queue = 3,
    Matching = 4,
    Oracle = 5,
}

impl std::fmt::Display for SourceFileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceFileId::Processor => write!(f, "src/processor.rs"),
            SourceFileId::State => write!(f, "src/state.rs"),
            SourceFileId::Critbit => write!(f, "src/critbit"),
            SourceFileId::Queue => write!(f, "src/queue.rs"),
            SourceFileId::Matching => write!(f, "src/matching.rs"),
            SourceFileId::Oracle => write!(f, "src/oracle.rs"),
        }
    }
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum LyraeError {
    #[error(transparent)]
    ProgramError(#[from] ProgramError),
    #[error("{lyrae_error_code}; {source_file_id}:{line}")]
    LyraeErrorCode { lyrae_error_code: LyraeErrorCode, line: u32, source_file_id: SourceFileId },
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq, IntoPrimitive)]
#[repr(u32)]
pub enum LyraeErrorCode {
    #[error("LyraeErrorCode::InvalidCache")] // 0
    InvalidCache,
    #[error("LyraeErrorCode::InvalidOwner")]
    InvalidOwner,
    #[error("LyraeErrorCode::InvalidGroupOwner")]
    InvalidGroupOwner,
    #[error("LyraeErrorCode::InvalidSignerKey")]
    InvalidSignerKey,
    #[error("LyraeErrorCode::InvalidAdminKey")]
    InvalidAdminKey,
    #[error("LyraeErrorCode::InvalidVault")]
    InvalidVault,
    #[error("LyraeErrorCode::MathError")]
    MathError,
    #[error("LyraeErrorCode::InsufficientFunds")]
    InsufficientFunds,
    #[error("LyraeErrorCode::InvalidToken")]
    InvalidToken,
    #[error("LyraeErrorCode::InvalidMarket")]
    InvalidMarket,
    #[error("LyraeErrorCode::InvalidProgramId")] // 10
    InvalidProgramId,
    #[error("LyraeErrorCode::GroupNotRentExempt")]
    GroupNotRentExempt,
    #[error("LyraeErrorCode::OutOfSpace")]
    OutOfSpace,
    #[error("LyraeErrorCode::TooManyOpenOrders Reached the maximum number of open orders for this market")]
    TooManyOpenOrders,

    #[error("LyraeErrorCode::AccountNotRentExempt")]
    AccountNotRentExempt,

    #[error("LyraeErrorCode::ClientIdNotFound")]
    ClientIdNotFound,
    #[error("LyraeErrorCode::InvalidNodeBank")]
    InvalidNodeBank,
    #[error("LyraeErrorCode::InvalidRootBank")]
    InvalidRootBank,
    #[error("LyraeErrorCode::MarginBasketFull")]
    MarginBasketFull,
    #[error("LyraeErrorCode::NotLiquidatable")]
    NotLiquidatable,
    #[error("LyraeErrorCode::Unimplemented")] // 20
    Unimplemented,
    #[error("LyraeErrorCode::PostOnly")]
    PostOnly,
    #[error("LyraeErrorCode::Bankrupt Invalid instruction for bankrupt account")]
    Bankrupt,
    #[error("LyraeErrorCode::InsufficientHealth")]
    InsufficientHealth,
    #[error("LyraeErrorCode::InvalidParam")]
    InvalidParam,
    #[error("LyraeErrorCode::InvalidAccount")]
    InvalidAccount,
    #[error("LyraeErrorCode::InvalidAccountState")]
    InvalidAccountState,
    #[error("LyraeErrorCode::SignerNecessary")]
    SignerNecessary,
    #[error("LyraeErrorCode::InsufficientLiquidity Not enough deposits in this node bank")]
    InsufficientLiquidity,
    #[error("LyraeErrorCode::InvalidOrderId")]
    InvalidOrderId,
    #[error("LyraeErrorCode::InvalidOpenOrdersAccount")] // 30
    InvalidOpenOrdersAccount,
    #[error("LyraeErrorCode::BeingLiquidated Invalid instruction while being liquidated")]
    BeingLiquidated,
    #[error("LyraeErrorCode::InvalidRootBankCache Cache the root bank to resolve")]
    InvalidRootBankCache,
    #[error("LyraeErrorCode::InvalidPriceCache Cache the oracle price to resolve")]
    InvalidPriceCache,
    #[error("LyraeErrorCode::InvalidPerpMarketCache Cache the perp market to resolve")]
    InvalidPerpMarketCache,
    #[error("LyraeErrorCode::TriggerConditionFalse The trigger condition for this TriggerOrder is not met")]
    TriggerConditionFalse,
    #[error("LyraeErrorCode::InvalidSeeds Invalid seeds. Unable to create PDA")]
    InvalidSeeds,
    #[error("LyraeErrorCode::InvalidOracleType The oracle account was not recognized")]
    InvalidOracleType,
    #[error("LyraeErrorCode::InvalidOraclePrice")]
    InvalidOraclePrice,
    #[error("LyraeErrorCode::MaxAccountsReached The maximum number of accounts for this group has been reached")]
    MaxAccountsReached,

    #[error("LyraeErrorCode::Default Check the source code for more info")] // 40
    Default = u32::MAX_VALUE,
}

impl From<LyraeError> for ProgramError {
    fn from(e: LyraeError) -> ProgramError {
        match e {
            LyraeError::ProgramError(pe) => pe,
            LyraeError::LyraeErrorCode { lyrae_error_code, line: _, source_file_id: _ } => {
                ProgramError::Custom(lyrae_error_code.into())
            }
        }
    }
}

impl From<serum_dex::error::DexError> for LyraeError {
    fn from(de: serum_dex::error::DexError) -> Self {
        let pe: ProgramError = de.into();
        pe.into()
    }
}

#[inline]
pub fn check_assert(
    cond: bool,
    lyrae_error_code: LyraeErrorCode,
    line: u32,
    source_file_id: SourceFileId,
) -> LyraeResult<()> {
    if cond {
        Ok(())
    } else {
        Err(LyraeError::LyraeErrorCode { lyrae_error_code, line, source_file_id })
    }
}

#[macro_export]
macro_rules! declare_check_assert_macros {
    ($source_file_id:expr) => {
        #[allow(unused_macros)]
        macro_rules! check {
            ($cond:expr, $err:expr) => {
                check_assert($cond, $err, line!(), $source_file_id)
            };
        }

        #[allow(unused_macros)]
        macro_rules! check_eq {
            ($x:expr, $y:expr, $err:expr) => {
                check_assert($x == $y, $err, line!(), $source_file_id)
            };
        }

        #[allow(unused_macros)]
        macro_rules! throw {
            () => {
                LyraeError::LyraeErrorCode {
                    lyrae_error_code: LyraeErrorCode::Default,
                    line: line!(),
                    source_file_id: $source_file_id,
                }
            };
        }

        #[allow(unused_macros)]
        macro_rules! throw_err {
            ($err:expr) => {
                LyraeError::LyraeErrorCode {
                    lyrae_error_code: $err,
                    line: line!(),
                    source_file_id: $source_file_id,
                }
            };
        }

        #[allow(unused_macros)]
        macro_rules! math_err {
            () => {
                LyraeError::LyraeErrorCode {
                    lyrae_error_code: LyraeErrorCode::MathError,
                    line: line!(),
                    source_file_id: $source_file_id,
                }
            };
        }
    };
}

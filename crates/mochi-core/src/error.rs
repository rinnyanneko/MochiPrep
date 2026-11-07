use mochi_decimal::DecError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    /// A decimal string that does not parse, or a division by zero. TypeScript throws on both.
    #[error(transparent)]
    Decimal(#[from] DecError),
    #[error("No calculation model for {0}")]
    NoCalculationModel(String),
}

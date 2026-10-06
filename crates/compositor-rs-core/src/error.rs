//! Errors and the value types every crate shares.

use thiserror::Error;

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("the document has no canvas")]
    NoDocument,
    #[error("no layer is active")]
    NoActiveLayer,
    #[error("'{0}' is not a layer of this document")]
    MissingLayer(String),
    #[error("a layer cannot be its own parent")]
    CyclicParent,
    #[error("the edit cannot run while another is in flight")]
    Busy,
    #[error("the surface would exceed the {0}-megapixel limit")]
    TooLarge(usize),
    #[error("{0}")]
    Message(String),
}

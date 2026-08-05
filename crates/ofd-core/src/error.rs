//! Error types for the OFD core.

use thiserror::Error;

/// Result alias used throughout the core.
pub type Result<T> = std::result::Result<T, OfdError>;

/// Errors that can occur while reading, parsing, or rendering an OFD document.
#[derive(Debug, Error)]
pub enum OfdError {
    /// The underlying ZIP container could not be read.
    #[error("invalid OFD container: {0}")]
    Container(#[from] zip::result::ZipError),

    /// An expected entry was missing from the container.
    #[error("missing entry in OFD container: {0}")]
    MissingEntry(String),

    /// XML could not be parsed.
    #[error("xml parse error: {0}")]
    Xml(String),

    /// A referenced resource (font, image, color space) could not be resolved.
    #[error("unresolved resource id {0}")]
    UnresolvedResource(u64),

    /// A numeric or structural value in the document was malformed.
    #[error("malformed document: {0}")]
    Malformed(String),

    /// A configured safety limit was exceeded while parsing or rendering.
    #[error("resource limit exceeded: {0}")]
    ResourceLimit(String),

    /// A resource required for faithful rendering was missing or unusable.
    #[error("render error: {0}")]
    Render(String),

    /// Generic I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<roxmltree::Error> for OfdError {
    fn from(e: roxmltree::Error) -> Self {
        OfdError::Xml(e.to_string())
    }
}

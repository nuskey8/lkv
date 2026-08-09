use std::fmt;
use std::io;

/// The class of integrity failure that was detected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CorruptionKind {
    Structure,
    MetadataChecksum,
    BlockChecksum,
    LogChecksum,
}

/// Location and category of a database integrity failure.
#[derive(Debug)]
pub struct Corruption {
    kind: CorruptionKind,
    segment_offset: Option<u64>,
    block_index: Option<u64>,
    message: String,
}

impl Corruption {
    pub fn kind(&self) -> CorruptionKind {
        self.kind
    }

    pub fn segment_offset(&self) -> Option<u64> {
        self.segment_offset
    }

    pub fn block_index(&self) -> Option<u64> {
        self.block_index
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Errors returned by `lkv` API.
#[derive(Debug)]
pub enum Error {
    /// The database file is malformed or failed an integrity check.
    Corrupted(Corruption),
    /// Another process or handle already owns the database writer lock.
    DatabaseAlreadyOpen(io::Error),
    /// The file format or requested operation is unsupported.
    Unsupported(io::Error),
    /// A key, value, batch, or option is invalid.
    InvalidArgument(io::Error),
    /// The configured maximum database size would be exceeded.
    DatabaseFull(io::Error),
    /// The active Overlay exceeded its configured memory limit. Compact or
    /// vacuum the database before starting another write transaction.
    MaintenanceRequired { limit: usize, actual: usize },
    /// A previous write-path I/O failure left this handle unsafe for more writes.
    Poisoned,
    /// An underlying filesystem operation failed.
    Io(io::Error),
}

impl Error {
    /// Returns the underlying I/O error kind when one is available.
    pub fn kind(&self) -> io::ErrorKind {
        match self {
            Self::Corrupted(_) => io::ErrorKind::InvalidData,
            Self::DatabaseAlreadyOpen(error)
            | Self::Unsupported(error)
            | Self::InvalidArgument(error)
            | Self::DatabaseFull(error)
            | Self::Io(error) => error.kind(),
            Self::MaintenanceRequired { .. } => io::ErrorKind::Other,
            Self::Poisoned => io::ErrorKind::Other,
        }
    }

    pub(crate) fn from_io(
        kind: io::ErrorKind,
        message: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        io::Error::new(kind, message).into()
    }

    pub(crate) fn other(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        io::Error::other(message).into()
    }

    pub(crate) fn database_full(limit: u64, required: u64) -> Self {
        Self::DatabaseFull(io::Error::other(format!(
            "database size limit of {limit} bytes would be exceeded (requires {required} bytes)"
        )))
    }

    pub(crate) fn corrupted_block(segment_offset: u64, block_index: u64) -> Self {
        Self::Corrupted(Corruption {
            kind: CorruptionKind::BlockChecksum,
            segment_offset: Some(segment_offset),
            block_index: Some(block_index),
            message: format!(
                "block checksum mismatch at segment offset {segment_offset} block {block_index}"
            ),
        })
    }

    pub(crate) fn corrupted_metadata(message: impl Into<String>) -> Self {
        Self::Corrupted(Corruption {
            kind: CorruptionKind::MetadataChecksum,
            segment_offset: None,
            block_index: None,
            message: message.into(),
        })
    }

    pub(crate) fn corrupted_log(offset: u64, message: impl Into<String>) -> Self {
        Self::Corrupted(Corruption {
            kind: CorruptionKind::LogChecksum,
            segment_offset: Some(offset),
            block_index: None,
            message: message.into(),
        })
    }

    pub(crate) fn invalid_base(message: &str) -> Error {
        Error::from_io(
            std::io::ErrorKind::InvalidData,
            format!("invalid base: {message}"),
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corrupted(corruption) => corruption.message.fmt(formatter),
            Self::DatabaseAlreadyOpen(error)
            | Self::Unsupported(error)
            | Self::InvalidArgument(error)
            | Self::DatabaseFull(error)
            | Self::Io(error) => error.fmt(formatter),
            Self::MaintenanceRequired { limit, actual } => write!(
                formatter,
                "overlay memory usage of {actual} bytes exceeds the {limit} byte limit; compact or vacuum before writing"
            ),
            Self::Poisoned => formatter.write_str(
                "database is poisoned by a previous I/O failure; reopen it before writing",
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Corrupted(_) => None,
            Self::DatabaseAlreadyOpen(error)
            | Self::Unsupported(error)
            | Self::InvalidArgument(error)
            | Self::DatabaseFull(error)
            | Self::Io(error) => Some(error),
            Self::MaintenanceRequired { .. } => None,
            Self::Poisoned => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::InvalidData => Self::Corrupted(Corruption {
                kind: CorruptionKind::Structure,
                segment_offset: None,
                block_index: None,
                message: error.to_string(),
            }),
            io::ErrorKind::WouldBlock => Self::DatabaseAlreadyOpen(error),
            io::ErrorKind::Unsupported => Self::Unsupported(error),
            io::ErrorKind::InvalidInput => Self::InvalidArgument(error),
            _ => Self::Io(error),
        }
    }
}

/// Result type for `lkv` APIs.
pub type Result<T> = std::result::Result<T, Error>;

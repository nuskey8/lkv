/// Integrity work performed while opening a database.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VerificationMode {
    /// Verify content checksums and the complete hash index before returning.
    Full,
    /// Open from trusted metadata, then check record bounds and immutable data blocks on first access.
    #[default]
    OnRead,
}

/// Provides configuration options for the database.
#[derive(Clone, Debug)]
pub struct DatabaseOptions {
    /// Controls startup integrity verification.
    pub(crate) verification: VerificationMode,
    /// Maximum serialized size of the database.
    pub(crate) max_database_bytes: u64,
    /// Soft limit charged for keys and inline-sized values in the active overlay KeyDir.
    pub(crate) overlay_memory_limit: usize,
}

impl DatabaseOptions {
    pub fn with_verification(mut self, verification: VerificationMode) -> Self {
        self.verification = verification;
        self
    }

    pub fn with_max_database_bytes(mut self, max_database_bytes: u64) -> Self {
        self.max_database_bytes = max_database_bytes;
        self
    }

    pub fn with_overlay_memory_limit(mut self, overlay_memory_limit: usize) -> Self {
        self.overlay_memory_limit = overlay_memory_limit;
        self
    }
}

impl Default for DatabaseOptions {
    fn default() -> Self {
        Self {
            verification: VerificationMode::OnRead,
            max_database_bytes: 1 << 40,
            overlay_memory_limit: 64 * 1024 * 1024,
        }
    }
}

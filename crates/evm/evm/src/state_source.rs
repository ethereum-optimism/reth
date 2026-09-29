//! Operation-local sources of independent execution readers.

use reth_storage_errors::provider::ProviderError;
use revm::Database;
use std::{fmt, sync::Arc};

/// Reopens the exact base state used by canonical execution, including its in-memory ancestry.
///
/// The factory is shareable, but each returned reader belongs to the calling worker. Consumers
/// must open, use and destroy readers on that worker and drain them before ending the operation.
/// Implementations must return an error if their fixed base can no longer be reconstructed.
pub trait StateReadFactory: Send + Sync + 'static {
    /// Opens a private reader. The reader need not implement Send or Sync.
    fn open(&self) -> Result<Box<dyn Database<Error = ProviderError>>, ProviderError>;
}

impl<F> StateReadFactory for F
where
    F: Fn() -> Result<Box<dyn Database<Error = ProviderError>>, ProviderError>
        + Send
        + Sync
        + 'static,
{
    fn open(&self) -> Result<Box<dyn Database<Error = ProviderError>>, ProviderError> {
        self()
    }
}

/// An owned source scoped to one execution operation, not a node-wide configuration.
#[derive(Clone)]
pub struct ExecutionStateSource(pub Arc<dyn StateReadFactory>);

impl fmt::Debug for ExecutionStateSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ExecutionStateSource").finish_non_exhaustive()
    }
}

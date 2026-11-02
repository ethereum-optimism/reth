use super::TracingCtx;
use alloy_evm::{Evm, IntoTxEnv};
use revm::DatabaseCommit;

/// Executes `transactions` on `evm` in order and invokes `callback` with each one's index, the
/// transaction and its [`TracingCtx`].
///
/// `inspector_setup` provides the inspector for the next transaction once the previous one is
/// handed to the callback. Iteration stops at the first transaction that fails to execute, and
/// the state of the last executed transaction is not committed.
pub(super) fn trace_transactions<E, Setup, Transactions, F, R, Error>(
    mut evm: E,
    transactions: Transactions,
    mut inspector_setup: Setup,
    mut callback: F,
) -> Result<Vec<R>, Error>
where
    E: Evm<DB: DatabaseCommit>,
    Setup: FnMut() -> E::Inspector,
    Transactions: IntoIterator<Item: Clone + IntoTxEnv<E::Tx>>,
    F: FnMut(usize, Transactions::Item, TracingCtx<'_, E::Inspector, E>) -> Result<R, Error>,
{
    let mut results = Vec::new();
    let mut transactions = transactions.into_iter().enumerate().peekable();
    while let Some((index, transaction)) = transactions.next() {
        let Ok(result) = evm.transact(transaction.clone()) else {
            break;
        };
        let (database, inspector, _) = evm.components_mut();
        let inspector = core::mem::replace(inspector, inspector_setup());
        let output = callback(
            index,
            transaction,
            TracingCtx { result: &result, db: &mut *database, inspector },
        );
        if transactions.peek().is_some() {
            database.commit(result.state);
        }
        results.push(output?);
    }
    Ok(results)
}

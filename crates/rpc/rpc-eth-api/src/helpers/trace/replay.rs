use super::TracingCtx;
use alloy_evm::{Evm, IntoTxEnv};
use revm::{context_interface::result::ResultAndState, DatabaseCommit};

pub(super) fn trace_transactions<E, Transactions, F, R, Error>(
    mut evm: E,
    transactions: Transactions,
    mut callback: F,
) -> Result<Vec<R>, Error>
where
    E: Evm<DB: DatabaseCommit, Inspector: Clone>,
    Transactions: IntoIterator<Item: Clone + IntoTxEnv<E::Tx>>,
    F: FnMut(usize, TracingCtx<'_, Transactions::Item, E>) -> Result<R, Error>,
{
    let fused_inspector = evm.inspector_mut().clone();
    let mut results = Vec::new();
    let mut transactions = transactions.into_iter().enumerate().peekable();
    while let Some((index, transaction)) = transactions.next() {
        let Ok(ResultAndState { result, state }) = evm.transact(transaction.clone()) else {
            break;
        };
        let mut was_fused = false;
        let (database, inspector, _) = evm.components_mut();
        let output = callback(
            index,
            TracingCtx::new(
                transaction,
                result,
                &state,
                inspector,
                database,
                &fused_inspector,
                &mut was_fused,
            ),
        );
        if transactions.peek().is_some() {
            database.commit(state);
        }
        if !was_fused {
            *inspector = fused_inspector.clone();
        }
        results.push(output?);
    }
    Ok(results)
}

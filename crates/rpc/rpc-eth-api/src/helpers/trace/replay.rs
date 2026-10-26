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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_evm::{
        eth::{EthEvm, EthEvmFactory},
        tracing::TxTracer,
        EvmEnv, EvmFactory,
    };
    use alloy_primitives::{Address, Bytes, TxKind};
    use revm::{
        bytecode::Bytecode,
        context::TxEnv,
        context_interface::result::{ExecutionResult, HaltReason},
        database::{CacheDB, EmptyDB},
        interpreter::Interpreter,
        state::{AccountInfo, EvmState},
        Database, Inspector,
    };
    use rstest::rstest;

    const CALLER: Address = Address::repeat_byte(0x11);
    const CONTRACT: Address = Address::repeat_byte(0x22);

    type TestDatabase = CacheDB<EmptyDB>;
    type Snapshot = (ExecutionResult<HaltReason>, EvmState, usize);

    #[derive(Clone, Debug, Default)]
    struct CountingInspector {
        calls: usize,
    }

    impl<Context> Inspector<Context> for CountingInspector {
        fn initialize_interp(&mut self, _interpreter: &mut Interpreter, _context: &mut Context) {
            self.calls += 1;
        }
    }

    fn database() -> TestDatabase {
        let mut database = CacheDB::new(EmptyDB::default());
        database.insert_account_info(
            CONTRACT,
            AccountInfo::default().with_code(Bytecode::new_legacy(Bytes::from_static(&[0x00]))),
        );
        database
    }

    fn evm(
        database: &mut TestDatabase,
    ) -> EthEvm<&mut TestDatabase, CountingInspector, alloy_evm::precompiles::PrecompilesMap> {
        EthEvmFactory::default().create_evm_with_inspector(
            database,
            EvmEnv::default(),
            CountingInspector::default(),
        )
    }

    fn transaction(nonce: u64) -> TxEnv {
        TxEnv {
            caller: CALLER,
            gas_limit: 100_000,
            kind: TxKind::Call(CONTRACT),
            nonce,
            ..Default::default()
        }
    }

    #[rstest]
    #[case(0, false)]
    #[case(1, false)]
    #[case(3, false)]
    #[case(3, true)]
    fn replay_matches_tx_tracer(#[case] transaction_count: u64, #[case] take_inspector: bool) {
        let transactions = (0..transaction_count).map(transaction).collect::<Vec<_>>();
        let mut expected_database = database();
        let mut tracer = TxTracer::new(evm(&mut expected_database));
        let expected = tracer
            .trace_many(transactions.clone(), |mut context| {
                let calls = if take_inspector {
                    context.take_inspector().calls
                } else {
                    context.inspector.calls
                };
                (context.result, context.state.clone(), calls)
            })
            .collect::<Result<Vec<Snapshot>, _>>()
            .unwrap();

        let mut actual_database = database();
        let actual: Result<Vec<Snapshot>, ()> =
            trace_transactions(evm(&mut actual_database), transactions, |_, mut context| {
                let calls = if take_inspector {
                    context.take_inspector().calls
                } else {
                    context.inspector.calls
                };
                Ok((context.result, context.state.clone(), calls))
            });
        assert_eq!(actual.unwrap(), expected);
        assert_eq!(
            actual_database.basic(CALLER).unwrap(),
            expected_database.basic(CALLER).unwrap()
        );
        assert_eq!(
            actual_database.basic(CALLER).unwrap().unwrap_or_default().nonce,
            transaction_count.saturating_sub(1),
        );
        assert!(expected.iter().all(|snapshot| snapshot.2 == 1));
    }

    #[test]
    fn callback_failure_preserves_legacy_commit_order() {
        let mut database = database();
        let result: Result<Vec<()>, usize> =
            trace_transactions(evm(&mut database), (0..3).map(transaction), |index, _| {
                if index == 1 {
                    Err(index)
                } else {
                    Ok(())
                }
            });
        assert_eq!(result, Err(1));
        assert_eq!(database.basic(CALLER).unwrap().unwrap().nonce, 2);
    }

    #[test]
    fn transaction_failure_preserves_legacy_truncation() {
        let mut database = database();
        let result: Result<Vec<usize>, ()> = trace_transactions(
            evm(&mut database),
            [transaction(0), transaction(7), transaction(2)],
            |index, _| Ok(index),
        );
        assert_eq!(result, Ok(vec![0]));
        assert_eq!(database.basic(CALLER).unwrap().unwrap().nonce, 1);
    }
}

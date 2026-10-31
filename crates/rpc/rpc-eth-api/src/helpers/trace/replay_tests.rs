use super::replay::trace_transactions;
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

fn assert_replay_matches_tx_tracer(transaction_count: u64, take_inspector: bool) {
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
    assert_eq!(actual_database.basic(CALLER).unwrap(), expected_database.basic(CALLER).unwrap());
    assert_eq!(
        actual_database.basic(CALLER).unwrap().unwrap_or_default().nonce,
        transaction_count.saturating_sub(1),
    );
    assert!(expected.iter().all(|snapshot| snapshot.2 == 1));
}

#[test]
fn replay_matches_tx_tracer_for_empty_input() {
    assert_replay_matches_tx_tracer(0, false);
}

#[test]
fn replay_matches_tx_tracer_for_multiple_transactions() {
    assert_replay_matches_tx_tracer(3, false);
}

#[test]
fn replay_matches_tx_tracer_for_single_transaction() {
    assert_replay_matches_tx_tracer(1, false);
}

#[test]
fn replay_matches_tx_tracer_with_taken_inspector() {
    assert_replay_matches_tx_tracer(3, true);
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

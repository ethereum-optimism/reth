//! Loads a pending block from database. Helper trait for `eth_` call and trace RPC methods.

use super::{Call, LoadBlock, LoadState, LoadTransaction};
use crate::{FromEthApiError, FromEvmError};
use alloy_consensus::{transaction::TxHashRef, BlockHeader};
use alloy_eip7928::bal::DecodedBal;
use alloy_primitives::B256;
use alloy_rpc_types_eth::{BlockId, TransactionInfo};
use alloy_rpc_types_trace::geth::{GethDebugTracingOptions, TraceResult};
use futures::Future;
use reth_errors::RethError;
use reth_evm::{
    block::BlockExecutor, ConfigureEvm, Evm, EvmEnvFor, EvmFor, HaltReasonFor, InspectorFor,
    IntoTxEnv, TxEnvFor,
};
use reth_primitives_traits::{BlockBody, BlockTy, Recovered, RecoveredBlock};
use reth_rpc_eth_types::cache::db::{attach_bal_before_tx, StateCacheDb};
use reth_storage_api::{HashedPostStateProvider, ProviderBlock, ProviderTx, StateRootProvider};
use revm::{
    context::Block,
    context_interface::result::{ExecutionResult, ResultAndState},
    database::states::bundle_state::BundleRetention,
    state::{bal::Bal as RevmBal, EvmState},
    DatabaseCommit,
};
use revm_inspectors::tracing::{
    DebugInspector, TracingInspector, TracingInspectorConfig, TransactionContext,
};
use std::sync::Arc;

mod replay;

/// Executes CPU heavy tasks.
pub trait Trace: LoadState<Error: FromEvmError<Self::Evm>> + Call {
    /// Executes the [`TxEnvFor`] with [`reth_evm::EvmEnv`] against the given [`StateCacheDb`]
    /// without committing state changes.
    fn inspect<'a>(
        &self,
        db: &'a mut StateCacheDb,
        evm_env: EvmEnvFor<Self::Evm>,
        tx_env: impl IntoTxEnv<TxEnvFor<Self::Evm>>,
        inspector: impl InspectorFor<Self::Evm, &'a mut StateCacheDb>,
    ) -> Result<ResultAndState<HaltReasonFor<Self::Evm>>, Self::Error> {
        self.evm_config()
            .evm_with_env_and_inspector(db, evm_env, inspector)
            .transact(tx_env)
            .map_err(Self::Error::from_evm_err)
    }

    /// Retrieves the transaction if it exists and returns its trace.
    ///
    /// Before the transaction is traced, the state is positioned right before the transaction,
    /// either by attaching the block's cached BAL or by executing all previous transactions in
    /// the block.
    /// The callback `f` is invoked with the [`ResultAndState`] after the transaction was executed
    /// and the database that points to the beginning of the transaction. The database may have
    /// the block's BAL attached and must only be used for reads, because an attached BAL takes
    /// read precedence over state committed on top, see [`attach_bal_before_tx`].
    ///
    /// Note: Implementers should use a threadpool where blocking is allowed, such as
    /// [`BlockingTaskPool`](reth_tasks::pool::BlockingTaskPool).
    fn spawn_trace_transaction_in_block<F, R>(
        &self,
        hash: B256,
        config: TracingInspectorConfig,
        f: F,
    ) -> impl Future<Output = Result<Option<R>, Self::Error>> + Send
    where
        Self: LoadTransaction,
        F: FnOnce(
                TransactionInfo,
                TracingInspector,
                ResultAndState<HaltReasonFor<Self::Evm>>,
                StateCacheDb,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        R: Send + 'static,
    {
        self.spawn_trace_transaction_in_block_with_inspector(hash, TracingInspector::new(config), f)
    }

    /// Retrieves the transaction if it exists and returns its trace.
    ///
    /// Before the transaction is traced, the state is positioned right before the transaction,
    /// either by attaching the block's cached BAL or by executing all previous transactions in
    /// the block.
    /// The callback `f` is invoked with the [`ResultAndState`] after the transaction was executed
    /// and the database that points to the beginning of the transaction. The database may have
    /// the block's BAL attached and must only be used for reads, because an attached BAL takes
    /// read precedence over state committed on top, see [`attach_bal_before_tx`].
    ///
    /// Note: Implementers should use a threadpool where blocking is allowed, such as
    /// [`BlockingTaskPool`](reth_tasks::pool::BlockingTaskPool).
    fn spawn_trace_transaction_in_block_with_inspector<Insp, F, R>(
        &self,
        hash: B256,
        mut inspector: Insp,
        f: F,
    ) -> impl Future<Output = Result<Option<R>, Self::Error>> + Send
    where
        Self: LoadTransaction,
        F: FnOnce(
                TransactionInfo,
                Insp,
                ResultAndState<HaltReasonFor<Self::Evm>>,
                StateCacheDb,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        Insp: for<'a> InspectorFor<Self::Evm, &'a mut StateCacheDb> + Send + 'static,
        R: Send + 'static,
    {
        async move {
            let (transaction, block, bal) =
                match self.transaction_and_block_and_maybe_bal(hash).await? {
                    None => return Ok(None),
                    Some(res) => res,
                };
            let (_, tx_info) = transaction.split();

            // we need to get the state of the parent block because we're essentially replaying the
            // block the transaction is included in
            let parent_block = block.parent_hash();

            self.spawn_with_state_at_block(parent_block, move |this, mut db| {
                let (res, _) = this.inspect_canonical_transaction_in_block(
                    &block,
                    &mut db,
                    &mut inspector,
                    // index should always be available because `transaction_and_block` only
                    // returns transactions included in a block
                    tx_info.index.expect("transaction_and_block only returns block transactions")
                        as usize,
                    bal.as_deref(),
                )?;
                f(tx_info, inspector, res, db)
            })
            .await
            .map(Some)
        }
    }

    /// Positions the state of `db` right before the transaction at the target index.
    ///
    /// If the block's cached BAL is given, it is attached to the database at the target index and
    /// no transactions are executed, see [`attach_bal_before_tx`]. Otherwise all transactions
    /// before the target transaction are executed and their changes are written to the
    /// _runtime_ db ([`StateCacheDb`]).
    ///
    /// If the target index is greater than or equal to the block's transaction count, all
    /// transactions are replayed.
    fn replay_block_until(
        &self,
        db: &mut StateCacheDb,
        block: &RecoveredBlock<BlockTy<Self::Primitives>>,
        target_tx_index: usize,
        bal: Option<&DecodedBal<Arc<RevmBal>>>,
    ) -> Result<(), Self::Error> {
        if let Some(bal) = bal {
            attach_bal_before_tx(db, bal, target_tx_index);
            return Ok(())
        }

        self.apply_pre_execution_changes(block, db)?;

        let evm_env = self.evm_env_for_header(block.sealed_block().sealed_header())?;
        let mut evm = self.evm_config().evm_with_env(db, evm_env);
        self.replay_transactions_until_with_evm(
            &mut evm,
            block.transactions_recovered(),
            target_tx_index,
        )
    }

    /// Executes the target transaction with the configured inspector on the state right before
    /// the transaction.
    ///
    /// If the block's cached BAL is given, the state is positioned by attaching the BAL at the
    /// target index, see [`attach_bal_before_tx`]. Otherwise all transactions before the target
    /// transaction are replayed without inspection first.
    #[expect(clippy::type_complexity)]
    fn inspect_transaction_in_block<'a>(
        &self,
        block: &RecoveredBlock<BlockTy<Self::Primitives>>,
        db: &'a mut StateCacheDb,
        inspector: impl InspectorFor<Self::Evm, &'a mut StateCacheDb>,
        target_tx_index: usize,
        target_tx_env: impl IntoTxEnv<TxEnvFor<Self::Evm>>,
        bal: Option<&DecodedBal<Arc<RevmBal>>>,
    ) -> Result<(ResultAndState<HaltReasonFor<Self::Evm>>, EvmEnvFor<Self::Evm>), Self::Error> {
        if let Some(bal) = bal {
            // the BAL also covers the block's pre-execution changes
            attach_bal_before_tx(db, bal, target_tx_index);
        } else {
            self.apply_pre_execution_changes(block, db)?;
        }

        let evm_env = self.evm_env_for_header(block.sealed_block().sealed_header())?;
        let mut evm = self.evm_config().evm_with_env_and_inspector(db, evm_env, inspector);

        if bal.is_none() {
            evm.disable_inspector();
            self.replay_transactions_until_with_evm(
                &mut evm,
                block.transactions_recovered(),
                target_tx_index,
            )?;
            evm.enable_inspector();
        }

        let res = evm.transact(target_tx_env).map_err(Self::Error::from_evm_err)?;

        let (_, evm_env) = evm.finish();

        Ok((res, evm_env))
    }

    /// Traces the signed transaction recorded at `target_tx_index` in `block`.
    fn inspect_canonical_transaction_in_block<'a>(
        &self,
        block: &RecoveredBlock<BlockTy<Self::Primitives>>,
        db: &'a mut StateCacheDb,
        inspector: impl InspectorFor<Self::Evm, &'a mut StateCacheDb>,
        target_tx_index: usize,
        bal: Option<&DecodedBal<Arc<RevmBal>>>,
    ) -> TransactionReplayResult<Self::Evm, Self::Error> {
        let transaction = block.transactions_recovered().nth(target_tx_index).ok_or_else(|| {
            Self::Error::from_eth_err(reth_rpc_eth_types::EthApiError::InvalidParams(format!(
                "tx_index {target_tx_index} out of bounds for block with {} transactions",
                block.transaction_count(),
            )))
        })?;
        self.inspect_transaction_in_block(
            block,
            db,
            inspector,
            target_tx_index,
            self.evm_config().tx_env(transaction),
            bal,
        )
    }

    /// Executes all transactions of a block up to a given index.
    ///
    /// If a `highest_index` is given, this will only execute the first `highest_index`
    /// transactions, in other words, it will stop executing transactions after the
    /// `highest_index`th transaction. If `highest_index` is `None`, all transactions
    /// are executed.
    fn trace_block_until<F, R>(
        &self,
        block_id: BlockId,
        block: Option<Arc<RecoveredBlock<ProviderBlock<Self::Provider>>>>,
        highest_index: Option<u64>,
        config: TracingInspectorConfig,
        f: F,
    ) -> impl Future<Output = Result<Option<Vec<R>>, Self::Error>> + Send
    where
        Self: LoadBlock,
        F: Fn(
                TransactionInfo,
                TracingCtx<
                    '_,
                    Recovered<&ProviderTx<Self::Provider>>,
                    EvmFor<Self::Evm, &mut StateCacheDb, TracingInspector>,
                >,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        R: Send + 'static,
    {
        self.trace_block_until_with_inspector(
            block_id,
            block,
            highest_index,
            move || TracingInspector::new(config),
            f,
        )
    }

    /// Executes all transactions of a block.
    ///
    /// If a `highest_index` is given, this will only execute the first `highest_index`
    /// transactions, in other words, it will stop executing transactions after the
    /// `highest_index`th transaction.
    ///
    /// Note: This expect tx index to be 0-indexed, so the first transaction is at index 0.
    ///
    /// This accepts a `inspector_setup` closure that returns the inspector to be used for tracing
    /// the transactions.
    fn trace_block_until_with_inspector<Setup, Insp, F, R>(
        &self,
        block_id: BlockId,
        block: Option<Arc<RecoveredBlock<ProviderBlock<Self::Provider>>>>,
        highest_index: Option<u64>,
        mut inspector_setup: Setup,
        f: F,
    ) -> impl Future<Output = Result<Option<Vec<R>>, Self::Error>> + Send
    where
        Self: LoadBlock,
        F: Fn(
                TransactionInfo,
                TracingCtx<
                    '_,
                    Recovered<&ProviderTx<Self::Provider>>,
                    EvmFor<Self::Evm, &mut StateCacheDb, Insp>,
                >,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        Setup: FnMut() -> Insp + Send + 'static,
        Insp: Clone + for<'a> InspectorFor<Self::Evm, &'a mut StateCacheDb>,
        R: Send + 'static,
    {
        async move {
            let block =
                if block.is_some() { block } else { self.recovered_block(block_id).await? };

            let Some(block) = block else { return Ok(None) };
            let evm_env = self.evm_env_for_header(block.sealed_block().sealed_header())?;

            if block.body().transactions().is_empty() {
                // nothing to trace
                return Ok(Some(Vec::new()))
            }

            // replay all transactions of the block
            // we need to get the state of the parent block because we're replaying this block
            // on top of its parent block's state
            self.spawn_with_state_at_block(block.parent_hash(), move |this, mut db| {
                // prepare transactions, we do everything upfront to reduce time spent with open
                // state
                let max_transactions = highest_index.map_or_else(
                    || block.body().transaction_count(),
                    |highest| {
                        // we need + 1 because the index is 0-based
                        highest as usize + 1
                    },
                );

                this.trace_block_transactions(
                    &mut db,
                    &block,
                    evm_env,
                    max_transactions,
                    inspector_setup(),
                    f,
                )
                .map(Some)
            })
            .await
        }
    }

    /// Executes all transactions of a block and returns a list of callback results invoked for each
    /// transaction in the block.
    ///
    /// This
    /// 1. fetches all transactions of the block
    /// 2. configures the EVM env
    /// 3. loops over all transactions and executes them
    /// 4. calls the callback with the transaction info, the execution result, the changed state
    ///    _after_ the transaction [`StateCacheDb`] and the database that points to the state right
    ///    _before_ the transaction.
    fn trace_block_with<F, R>(
        &self,
        block_id: BlockId,
        block: Option<Arc<RecoveredBlock<ProviderBlock<Self::Provider>>>>,
        config: TracingInspectorConfig,
        f: F,
    ) -> impl Future<Output = Result<Option<Vec<R>>, Self::Error>> + Send
    where
        Self: LoadBlock,
        // This is the callback that's invoked for each transaction with the inspector, the result,
        // state and db
        F: Fn(
                TransactionInfo,
                TracingCtx<
                    '_,
                    Recovered<&ProviderTx<Self::Provider>>,
                    EvmFor<Self::Evm, &mut StateCacheDb, TracingInspector>,
                >,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        R: Send + 'static,
    {
        self.trace_block_until(block_id, block, None, config, f)
    }

    /// Executes all transactions of a block and returns a list of callback results invoked for each
    /// transaction in the block.
    ///
    /// This
    /// 1. fetches all transactions of the block
    /// 2. configures the EVM env
    /// 3. loops over all transactions and executes them
    /// 4. calls the callback with the transaction info, the execution result, the changed state
    ///    _after_ the transaction `EvmState` and the database that points to the state right
    ///    _before_ the transaction, in other words the state the transaction was executed on:
    ///    `changed_state = tx(cached_state)`
    ///
    /// This accepts a `inspector_setup` closure that returns the inspector to be used for tracing
    /// a transaction. This is invoked for each transaction.
    fn trace_block_inspector<Setup, Insp, F, R>(
        &self,
        block_id: BlockId,
        block: Option<Arc<RecoveredBlock<ProviderBlock<Self::Provider>>>>,
        insp_setup: Setup,
        f: F,
    ) -> impl Future<Output = Result<Option<Vec<R>>, Self::Error>> + Send
    where
        Self: LoadBlock,
        // This is the callback that's invoked for each transaction with the inspector, the result,
        // state and db
        F: Fn(
                TransactionInfo,
                TracingCtx<
                    '_,
                    Recovered<&ProviderTx<Self::Provider>>,
                    EvmFor<Self::Evm, &mut StateCacheDb, Insp>,
                >,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        Setup: FnMut() -> Insp + Send + 'static,
        Insp: Clone + for<'a> InspectorFor<Self::Evm, &'a mut StateCacheDb>,
        R: Send + 'static,
    {
        self.trace_block_until_with_inspector(block_id, block, None, insp_setup, f)
    }

    /// Applies chain-specific state transitions required before executing a block.
    ///
    /// Note: This should only be called when tracing an entire block vs individual transactions.
    /// When tracing transactions on top of an already committed block state, those transitions are
    /// already applied.
    fn apply_pre_execution_changes(
        &self,
        block: &RecoveredBlock<ProviderBlock<Self::Provider>>,
        db: &mut StateCacheDb,
    ) -> Result<(), Self::Error> {
        self.evm_config()
            .executor_for_block(db, block.sealed_block())
            .map_err(RethError::other)
            .map_err(Self::Error::from_eth_err)?
            .apply_pre_execution_changes()
            .map_err(Self::Error::from_eth_err)?;
        Ok(())
    }

    /// Executes the first `max_transactions` transactions of `block` on `db`, which holds the
    /// state of the block's parent, and invokes `f` with each transaction's [`TracingCtx`].
    ///
    /// Backs [`Trace::trace_block_until_with_inspector`]. Iteration stops at the first transaction
    /// that fails to execute, and the state of the last executed transaction is not committed.
    fn trace_block_transactions<Insp, F, R>(
        &self,
        db: &mut StateCacheDb,
        block: &RecoveredBlock<ProviderBlock<Self::Provider>>,
        evm_env: EvmEnvFor<Self::Evm>,
        max_transactions: usize,
        inspector: Insp,
        f: F,
    ) -> Result<Vec<R>, Self::Error>
    where
        F: Fn(
            TransactionInfo,
            TracingCtx<
                '_,
                Recovered<&ProviderTx<Self::Provider>>,
                EvmFor<Self::Evm, &mut StateCacheDb, Insp>,
            >,
        ) -> Result<R, Self::Error>,
        Insp: Clone + for<'a> InspectorFor<Self::Evm, &'a mut StateCacheDb>,
    {
        let block_hash = block.hash();
        let block_number = evm_env.block_env.number().saturating_to();
        let block_timestamp = evm_env.block_env.timestamp().saturating_to();
        let base_fee = evm_env.block_env.basefee();

        self.apply_pre_execution_changes(block, db)?;

        let evm = self.evm_config().evm_with_env_and_inspector(db, evm_env, inspector);
        replay::trace_transactions(
            evm,
            block.transactions_recovered().take(max_transactions),
            |index, context| {
                let transaction_info = TransactionInfo {
                    base_fee: Some(base_fee),
                    block_hash: Some(block_hash),
                    block_number: Some(block_number),
                    block_timestamp: Some(block_timestamp),
                    hash: Some(*context.tx.tx_hash()),
                    index: Some(index as u64),
                };
                f(transaction_info, context)
            },
        )
    }

    /// Traces every transaction of `block` with a [`DebugInspector`] configured by `opts`, on `db`,
    /// which holds the state of the block's parent.
    ///
    /// Backs `debug_traceBlockByHash`, `debug_traceBlockByNumber`, `debug_traceBlock` and
    /// `debug_traceBadBlock`.
    fn debug_trace_block_transactions(
        &self,
        db: &mut StateCacheDb,
        block: &RecoveredBlock<ProviderBlock<Self::Provider>>,
        evm_env: EvmEnvFor<Self::Evm>,
        _mode: BlockReplayMode,
        opts: GethDebugTracingOptions,
    ) -> Result<Vec<TraceResult>, Self::Error> {
        self.debug_trace_unvalidated_block_transactions(db, block, evm_env, opts)
    }

    /// Traces transactions without imposing block executor admission checks.
    fn debug_trace_unvalidated_block_transactions(
        &self,
        db: &mut StateCacheDb,
        block: &RecoveredBlock<ProviderBlock<Self::Provider>>,
        evm_env: EvmEnvFor<Self::Evm>,
        opts: GethDebugTracingOptions,
    ) -> Result<Vec<TraceResult>, Self::Error> {
        let mut results = Vec::with_capacity(block.body().transactions().len());

        self.apply_pre_execution_changes(block, db)?;

        let block_env = evm_env.block_env.clone();

        let mut transactions = block.transactions_recovered().enumerate().peekable();
        let inspector = DebugInspector::new(opts).map_err(Self::Error::from_eth_err)?;
        let mut evm = self.evm_config().evm_with_env_and_inspector(db, evm_env, inspector);
        while let Some((index, tx)) = transactions.next() {
            let tx_env = self.evm_config().tx_env(tx);

            let res = evm.transact(tx_env.clone()).map_err(Self::Error::from_evm_err)?;

            let (db, inspector, _) = evm.components_mut();
            let result = inspector
                .get_result(
                    Some(TransactionContext {
                        block_hash: Some(block.hash()),
                        tx_hash: Some(*tx.tx_hash()),
                        tx_index: Some(index),
                    }),
                    &tx_env,
                    &block_env,
                    &res,
                    db,
                )
                .map_err(Self::Error::from_eth_err)?;

            results.push(TraceResult::Success { result, tx_hash: Some(*tx.tx_hash()) });
            if transactions.peek().is_some() {
                inspector.fuse().map_err(Self::Error::from_eth_err)?;
                // need to apply the state changes of this transaction before executing the
                // next transaction
                db.commit(res.state)
            }
        }

        Ok(results)
    }

    /// Traces every transaction of `block` like [`Trace::debug_trace_block_transactions`], for
    /// `debug_traceChain`: a transaction that fails to trace is reported as a
    /// [`TraceResult::Error`] and ends the block, and the transactions after it are `None`.
    fn debug_trace_chain_block_transactions(
        &self,
        db: &mut StateCacheDb,
        block: &RecoveredBlock<ProviderBlock<Self::Provider>>,
        evm_env: EvmEnvFor<Self::Evm>,
        opts: GethDebugTracingOptions,
    ) -> Result<Vec<Option<TraceResult>>, Self::Error> {
        let tx_count = block.body().transactions().len();
        let mut results = Vec::with_capacity(tx_count);

        self.apply_pre_execution_changes(block, db)?;

        let block_env = evm_env.block_env.clone();
        let mut transactions = block.transactions_recovered().enumerate().peekable();
        let inspector = match DebugInspector::new(opts) {
            Ok(inspector) => inspector,
            Err(err) => {
                if let Some((_, tx)) = transactions.peek() {
                    results.push(Some(TraceResult::Error {
                        error: err.to_string(),
                        tx_hash: Some(*tx.tx_hash()),
                    }));
                }
                results.resize(tx_count, None);
                return Ok(results)
            }
        };
        let mut evm = self.evm_config().evm_with_env_and_inspector(db, evm_env, inspector);

        while let Some((index, tx)) = transactions.next() {
            let tx_hash = *tx.tx_hash();
            let tx_env = self.evm_config().tx_env(tx);
            let res = match evm.transact(tx_env.clone()) {
                Ok(res) => res,
                Err(err) => {
                    results.push(Some(TraceResult::Error {
                        error: err.to_string(),
                        tx_hash: Some(tx_hash),
                    }));
                    break
                }
            };

            let (db, inspector, _) = evm.components_mut();
            let result = match inspector.get_result(
                Some(TransactionContext {
                    block_hash: Some(block.hash()),
                    tx_hash: Some(tx_hash),
                    tx_index: Some(index),
                }),
                &tx_env,
                &block_env,
                &res,
                db,
            ) {
                Ok(result) => result,
                Err(err) => {
                    results.push(Some(TraceResult::Error {
                        error: err.to_string(),
                        tx_hash: Some(tx_hash),
                    }));
                    break
                }
            };

            results.push(Some(TraceResult::Success { result, tx_hash: Some(tx_hash) }));
            if let Some((_, next_tx)) = transactions.peek() {
                if let Err(err) = inspector.fuse() {
                    results.push(Some(TraceResult::Error {
                        error: err.to_string(),
                        tx_hash: Some(*next_tx.tx_hash()),
                    }));
                    break
                }
                db.commit(res.state);
            }
        }

        results.resize(tx_count, None);
        Ok(results)
    }

    /// Executes every transaction of `block` on `db`, which holds the state of the block's
    /// parent, and returns the state root after each one.
    ///
    /// Backs `debug_intermediateRoots`.
    fn block_intermediate_roots(
        &self,
        db: &mut StateCacheDb,
        block: &RecoveredBlock<ProviderBlock<Self::Provider>>,
        evm_env: EvmEnvFor<Self::Evm>,
    ) -> Result<Vec<B256>, Self::Error> {
        // Enable transition tracking so that merge_transitions works
        db.transition_state = Some(Default::default());

        self.apply_pre_execution_changes(block, db)?;

        let mut roots = Vec::with_capacity(block.body().transactions().len());
        let mut evm = self.evm_config().evm_with_env(db, evm_env);
        for tx in block.transactions_recovered() {
            let tx_env = self.evm_config().tx_env(tx);
            evm.transact_commit(tx_env).map_err(Self::Error::from_evm_err)?;

            let state = evm.db_mut();
            // Merge transitions into cumulative bundle_state
            state.merge_transitions(BundleRetention::PlainState);
            // Compute state root from the accumulated state changes
            let hashed_state = state
                .database
                .hashed_post_state(&state.bundle_state)
                .map_err(Self::Error::from_eth_err)?;
            let root =
                state.database.state_root(hashed_state).map_err(Self::Error::from_eth_err)?;
            roots.push(root);
        }

        Ok(roots)
    }
}

/// Execution output and the environment used for a transaction replay.
pub type TransactionReplayResult<Evm, Error> =
    Result<(ResultAndState<HaltReasonFor<Evm>>, EvmEnvFor<Evm>), Error>;

/// Validation guarantees supplied by the source of a block replay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockReplayMode {
    /// A block obtained through the node's block provider.
    Canonical,
    /// A caller-supplied or rejected block that may fail executor validation.
    Unvalidated,
}

/// Context for one transaction of a block traced by [`Trace::trace_block_transactions`].
///
/// Explicit callback annotations must import this type instead of alloy-evm's tracing context.
#[derive(Debug)]
pub struct TracingCtx<'a, T, E: Evm> {
    /// The transaction that was just executed.
    pub tx: T,
    /// Result of transaction execution.
    pub result: ExecutionResult<E::HaltReason>,
    /// State changes after transaction.
    pub state: &'a EvmState,
    /// Inspector state after transaction.
    pub inspector: &'a mut E::Inspector,
    /// Database used when executing the transaction, _before_ committing the state changes.
    pub db: &'a mut E::DB,
    /// Inspector state before any transaction was executed.
    fused_inspector: &'a E::Inspector,
    /// Whether the inspector was already reset by [`TracingCtx::take_inspector`].
    was_fused: &'a mut bool,
}

impl<'a, T, E: Evm> TracingCtx<'a, T, E> {
    /// Creates the context for a transaction. `fused_inspector` is the inspector state before any
    /// transaction was executed; `was_fused` is set when the callback takes the inspector, and the
    /// caller resets the inspector to `fused_inspector` otherwise.
    pub const fn new(
        tx: T,
        result: ExecutionResult<E::HaltReason>,
        state: &'a EvmState,
        inspector: &'a mut E::Inspector,
        db: &'a mut E::DB,
        fused_inspector: &'a E::Inspector,
        was_fused: &'a mut bool,
    ) -> Self {
        Self { tx, result, state, inspector, db, fused_inspector, was_fused }
    }
}

impl<T, E: Evm<Inspector: Clone>> TracingCtx<'_, T, E> {
    /// Returns the inspector state of this transaction, resetting the inspector for the next one.
    pub fn take_inspector(&mut self) -> E::Inspector {
        *self.was_fused = true;
        core::mem::replace(self.inspector, self.fused_inspector.clone())
    }
}

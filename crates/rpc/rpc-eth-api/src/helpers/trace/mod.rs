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
use reth_primitives_traits::{BlockBody, BlockTy, RecoveredBlock};
use reth_rpc_eth_types::{
    cache::db::{attach_bal_before_tx, StateCacheDb},
    EthApiError,
};
use reth_storage_api::{HashedPostStateProvider, ProviderBlock, StateRootProvider};
use revm::{
    context::Block, context_interface::result::ResultAndState,
    database::states::bundle_state::BundleRetention, state::bal::Bal as RevmBal, DatabaseCommit,
};
use revm_inspectors::tracing::{
    DebugInspector, TracingInspector, TracingInspectorConfig, TransactionContext,
};
use std::sync::Arc;

mod replay;

#[cfg(test)]
mod replay_tests;

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

    /// Executes the signed transaction recorded at `target_tx_index` in `block` with the
    /// configured inspector on the state right before it, see
    /// [`Trace::inspect_transaction_in_block`].
    ///
    /// Returns [`EthApiError::InvalidParams`] if the block has no transaction at that index.
    #[expect(clippy::type_complexity)]
    fn inspect_canonical_transaction_in_block<'a>(
        &self,
        block: &RecoveredBlock<BlockTy<Self::Primitives>>,
        db: &'a mut StateCacheDb,
        inspector: impl InspectorFor<Self::Evm, &'a mut StateCacheDb>,
        target_tx_index: usize,
        bal: Option<&DecodedBal<Arc<RevmBal>>>,
    ) -> Result<(ResultAndState<HaltReasonFor<Self::Evm>>, EvmEnvFor<Self::Evm>), Self::Error> {
        let Some(tx) = block.transactions_recovered().nth(target_tx_index) else {
            return Err(Self::Error::from_eth_err(EthApiError::InvalidParams(format!(
                "tx_index {target_tx_index} out of bounds for block with {} transactions",
                block.transaction_count()
            ))))
        };
        self.inspect_transaction_in_block(
            block,
            db,
            inspector,
            target_tx_index,
            self.evm_config().tx_env(tx),
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
                    TracingInspector,
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
        inspector_setup: Setup,
        f: F,
    ) -> impl Future<Output = Result<Option<Vec<R>>, Self::Error>> + Send
    where
        Self: LoadBlock,
        F: Fn(
                TransactionInfo,
                TracingCtx<'_, Insp, EvmFor<Self::Evm, &mut StateCacheDb, Insp>>,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        Setup: FnMut() -> Insp + Send + 'static,
        Insp: for<'a> InspectorFor<Self::Evm, &'a mut StateCacheDb>,
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
                    inspector_setup,
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
                    TracingInspector,
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
                TracingCtx<'_, Insp, EvmFor<Self::Evm, &mut StateCacheDb, Insp>>,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        Setup: FnMut() -> Insp + Send + 'static,
        Insp: for<'a> InspectorFor<Self::Evm, &'a mut StateCacheDb>,
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
    /// Backs [`Trace::trace_block_until_with_inspector`]: `inspector_setup` is invoked once per
    /// transaction. Iteration stops at the first transaction that fails to execute, and the
    /// state of the last executed transaction is not committed.
    fn trace_block_transactions<Setup, Insp, F, R>(
        &self,
        db: &mut StateCacheDb,
        block: &RecoveredBlock<ProviderBlock<Self::Provider>>,
        evm_env: EvmEnvFor<Self::Evm>,
        max_transactions: usize,
        mut inspector_setup: Setup,
        f: F,
    ) -> Result<Vec<R>, Self::Error>
    where
        F: Fn(
            TransactionInfo,
            TracingCtx<'_, Insp, EvmFor<Self::Evm, &mut StateCacheDb, Insp>>,
        ) -> Result<R, Self::Error>,
        Setup: FnMut() -> Insp,
        Insp: for<'a> InspectorFor<Self::Evm, &'a mut StateCacheDb>,
    {
        let block_hash = block.hash();
        let block_number = evm_env.block_env.number().saturating_to();
        let block_timestamp = evm_env.block_env.timestamp().saturating_to();
        let base_fee = evm_env.block_env.basefee();

        self.apply_pre_execution_changes(block, db)?;

        let evm = self.evm_config().evm_with_env_and_inspector(db, evm_env, inspector_setup());
        replay::trace_transactions(
            evm,
            block.transactions_recovered().take(max_transactions),
            inspector_setup,
            |index, tx, context| {
                let transaction_info = TransactionInfo {
                    base_fee: Some(base_fee),
                    block_hash: Some(block_hash),
                    block_number: Some(block_number),
                    block_timestamp: Some(block_timestamp),
                    hash: Some(*tx.tx_hash()),
                    index: Some(index as u64),
                };
                f(transaction_info, context)
            },
        )
    }

    /// Traces every transaction of a block the node has validated, on `db`, which holds the
    /// state of the block's parent.
    ///
    /// Backs `debug_traceBlockByHash` and `debug_traceBlockByNumber`. Chains whose block execution
    /// changes state beyond the transactions themselves can override this to replay the block
    /// through their block executor, while
    /// [`Trace::debug_trace_unvalidated_block_transactions`] keeps serving blocks such an executor
    /// could reject.
    fn debug_trace_block_transactions(
        &self,
        db: &mut StateCacheDb,
        block: &RecoveredBlock<ProviderBlock<Self::Provider>>,
        evm_env: EvmEnvFor<Self::Evm>,
        opts: GethDebugTracingOptions,
    ) -> Result<Vec<TraceResult>, Self::Error> {
        self.debug_trace_unvalidated_block_transactions(db, block, evm_env, opts)
    }

    /// Traces every transaction of a caller-supplied or rejected block on `db`, which holds the
    /// state of the block's parent, without block-level validation.
    ///
    /// Backs `debug_traceBlock` and `debug_traceBadBlock`.
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

/// Context passed to per-transaction trace callbacks.
///
/// Explicit callback annotations must import this type instead of alloy-evm's tracing context.
pub struct TracingCtx<'a, Insp, E: Evm> {
    /// Execution result and state changes for the transaction.
    pub result: &'a ResultAndState<E::HaltReason>,
    /// Database pointing to the transaction pre-state.
    pub db: &'a mut E::DB,
    /// Inspector used while executing the transaction.
    pub inspector: Insp,
}

impl<Insp, E: Evm> core::fmt::Debug for TracingCtx<'_, Insp, E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TracingCtx").finish_non_exhaustive()
    }
}

impl<Insp, E: Evm> TracingCtx<'_, Insp, E> {
    /// Consumes this context and returns the inspector.
    pub fn take_inspector(self) -> Insp {
        self.inspector
    }
}

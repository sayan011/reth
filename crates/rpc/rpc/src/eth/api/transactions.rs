//! Contains RPC handler implementations specific to transactions
use crate::{
    eth::{
        api::pending_block::PendingBlockEnv,
        error::{EthApiError, EthResult, SignError},
        revm_utils::{
            inspect, inspect_and_return_db, prepare_call_env, replay_transactions_until, transact,
            EvmOverrides,
        },
        utils::recover_raw_transaction,
    },
    EthApi, EthApiSpec,
};
use async_trait::async_trait;
use reth_network_api::NetworkInfo;
use reth_primitives::{
    eip4844::calc_blob_gasprice,
    revm::env::{fill_block_env_with_coinbase, tx_env_with_recovered},
    revm_primitives::{db::DatabaseCommit, Env, ExecutionResult, ResultAndState, SpecId, State},
    Address, BlockId, BlockNumberOrTag, Bytes, FromRecoveredPooledTransaction, Header,
    IntoRecoveredTransaction, Receipt, SealedBlock, SealedBlockWithSenders,
    TransactionKind::{Call, Create},
    TransactionMeta, TransactionSigned, TransactionSignedEcRecovered, B256, U128, U256, U64,
};
use reth_provider::{
    BlockReaderIdExt, ChainSpecProvider, EvmEnvProvider, StateProviderBox, StateProviderFactory,
};
use reth_revm::{
    database::StateProviderDatabase,
    tracing::{TracingInspector, TracingInspectorConfig},
};
use reth_rpc_types::{
    CallRequest, Index, Log, Transaction, TransactionInfo, TransactionReceipt, TransactionRequest,
    TypedTransactionRequest,
};
use reth_rpc_types_compat::transaction::from_recovered_with_block_context;
use reth_transaction_pool::{TransactionOrigin, TransactionPool};
use revm::{
    db::CacheDB,
    primitives::{BlockEnv, CfgEnv},
    Inspector,
};

#[cfg(feature = "optimism")]
use crate::eth::api::optimism::OptimismTxMeta;
#[cfg(feature = "optimism")]
use reth_revm::optimism::RethL1BlockInfo;
#[cfg(feature = "optimism")]
use revm::L1BlockInfo;
#[cfg(feature = "optimism")]
use std::ops::Div;

/// Helper alias type for the state's [CacheDB]
pub(crate) type StateCacheDB = CacheDB<StateProviderDatabase<StateProviderBox>>;

/// Commonly used transaction related functions for the [EthApi] type in the `eth_` namespace.
///
/// Async functions that are spawned onto the
/// [BlockingTaskPool](crate::blocking_pool::BlockingTaskPool) begin with `spawn_`
#[async_trait::async_trait]
pub trait EthTransactions: Send + Sync {
    /// Returns default gas limit to use for `eth_call` and tracing RPC methods.
    fn call_gas_limit(&self) -> u64;

    /// Returns the state at the given [BlockId]
    fn state_at(&self, at: BlockId) -> EthResult<StateProviderBox>;

    /// Executes the closure with the state that corresponds to the given [BlockId].
    fn with_state_at_block<F, T>(&self, at: BlockId, f: F) -> EthResult<T>
    where
        F: FnOnce(StateProviderBox) -> EthResult<T>;

    /// Executes the closure with the state that corresponds to the given [BlockId] on a new task
    async fn spawn_with_state_at_block<F, T>(&self, at: BlockId, f: F) -> EthResult<T>
    where
        F: FnOnce(StateProviderBox) -> EthResult<T> + Send + 'static,
        T: Send + 'static;

    /// Returns the revm evm env for the requested [BlockId]
    ///
    /// If the [BlockId] this will return the [BlockId] of the block the env was configured
    /// for.
    /// If the [BlockId] is pending, this will return the "Pending" tag, otherwise this returns the
    /// hash of the exact block.
    async fn evm_env_at(&self, at: BlockId) -> EthResult<(CfgEnv, BlockEnv, BlockId)>;

    /// Returns the revm evm env for the raw block header
    ///
    /// This is used for tracing raw blocks
    async fn evm_env_for_raw_block(&self, at: &Header) -> EthResult<(CfgEnv, BlockEnv)>;

    /// Get all transactions in the block with the given hash.
    ///
    /// Returns `None` if block does not exist.
    async fn transactions_by_block(&self, block: B256)
        -> EthResult<Option<Vec<TransactionSigned>>>;

    /// Get the entire block for the given id.
    ///
    /// Returns `None` if block does not exist.
    async fn block_by_id(&self, id: BlockId) -> EthResult<Option<SealedBlock>>;

    /// Get the entire block for the given id.
    ///
    /// Returns `None` if block does not exist.
    async fn block_by_id_with_senders(
        &self,
        id: BlockId,
    ) -> EthResult<Option<SealedBlockWithSenders>>;

    /// Get all transactions in the block with the given hash.
    ///
    /// Returns `None` if block does not exist.
    async fn transactions_by_block_id(
        &self,
        block: BlockId,
    ) -> EthResult<Option<Vec<TransactionSigned>>>;

    /// Returns the transaction by hash.
    ///
    /// Checks the pool and state.
    ///
    /// Returns `Ok(None)` if no matching transaction was found.
    async fn transaction_by_hash(&self, hash: B256) -> EthResult<Option<TransactionSource>>;

    /// Returns the transaction by including its corresponding [BlockId]
    ///
    /// Note: this supports pending transactions
    async fn transaction_by_hash_at(
        &self,
        hash: B256,
    ) -> EthResult<Option<(TransactionSource, BlockId)>>;

    /// Returns the _historical_ transaction and the block it was mined in
    async fn historical_transaction_by_hash_at(
        &self,
        hash: B256,
    ) -> EthResult<Option<(TransactionSource, B256)>>;

    /// Returns the transaction receipt for the given hash.
    ///
    /// Returns None if the transaction does not exist or is pending
    /// Note: The tx receipt is not available for pending transactions.
    async fn transaction_receipt(&self, hash: B256) -> EthResult<Option<TransactionReceipt>>;

    /// Decodes and recovers the transaction and submits it to the pool.
    ///
    /// Returns the hash of the transaction.
    async fn send_raw_transaction(&self, tx: Bytes) -> EthResult<B256>;

    /// Signs transaction with a matching signer, if any and submits the transaction to the pool.
    /// Returns the hash of the signed transaction.
    async fn send_transaction(&self, request: TransactionRequest) -> EthResult<B256>;

    /// Prepares the state and env for the given [CallRequest] at the given [BlockId] and executes
    /// the closure on a new task returning the result of the closure.
    async fn spawn_with_call_at<F, R>(
        &self,
        request: CallRequest,
        at: BlockId,
        overrides: EvmOverrides,
        f: F,
    ) -> EthResult<R>
    where
        F: FnOnce(StateCacheDB, Env) -> EthResult<R> + Send + 'static,
        R: Send + 'static;

    /// Executes the call request at the given [BlockId].
    async fn transact_call_at(
        &self,
        request: CallRequest,
        at: BlockId,
        overrides: EvmOverrides,
    ) -> EthResult<(ResultAndState, Env)>;

    /// Executes the call request at the given [BlockId] on a new task and returns the result of the
    /// inspect call.
    async fn spawn_inspect_call_at<I>(
        &self,
        request: CallRequest,
        at: BlockId,
        overrides: EvmOverrides,
        inspector: I,
    ) -> EthResult<(ResultAndState, Env)>
    where
        I: Inspector<StateCacheDB> + Send + 'static;

    /// Executes the transaction on top of the given [BlockId] with a tracer configured by the
    /// config.
    ///
    /// The callback is then called with the [TracingInspector] and the [ResultAndState] after the
    /// configured [Env] was inspected.
    ///
    /// Caution: this is blocking
    fn trace_at<F, R>(
        &self,
        env: Env,
        config: TracingInspectorConfig,
        at: BlockId,
        f: F,
    ) -> EthResult<R>
    where
        F: FnOnce(TracingInspector, ResultAndState) -> EthResult<R>;

    /// Same as [Self::trace_at] but also provides the used database to the callback.
    ///
    /// Executes the transaction on top of the given [BlockId] with a tracer configured by the
    /// config.
    ///
    /// The callback is then called with the [TracingInspector] and the [ResultAndState] after the
    /// configured [Env] was inspected.
    async fn spawn_trace_at_with_state<F, R>(
        &self,
        env: Env,
        config: TracingInspectorConfig,
        at: BlockId,
        f: F,
    ) -> EthResult<R>
    where
        F: FnOnce(TracingInspector, ResultAndState, StateCacheDB) -> EthResult<R> + Send + 'static,
        R: Send + 'static;

    /// Fetches the transaction and the transaction's block
    async fn transaction_and_block(
        &self,
        hash: B256,
    ) -> EthResult<Option<(TransactionSource, SealedBlock)>>;

    /// Retrieves the transaction if it exists and returns its trace.
    ///
    /// Before the transaction is traced, all previous transaction in the block are applied to the
    /// state by executing them first.
    /// The callback `f` is invoked with the [ResultAndState] after the transaction was executed and
    /// the database that points to the beginning of the transaction.
    ///
    /// Note: Implementers should use a threadpool where blocking is allowed, such as
    /// [BlockingTaskPool](crate::blocking_pool::BlockingTaskPool).
    async fn spawn_trace_transaction_in_block<F, R>(
        &self,
        hash: B256,
        config: TracingInspectorConfig,
        f: F,
    ) -> EthResult<Option<R>>
    where
        F: FnOnce(TransactionInfo, TracingInspector, ResultAndState, StateCacheDB) -> EthResult<R>
            + Send
            + 'static,
        R: Send + 'static;

    /// Executes all transactions of a block and returns a list of callback results invoked for each
    /// transaction in the block.
    ///
    /// This
    /// 1. fetches all transactions of the block
    /// 2. configures the EVM evn
    /// 3. loops over all transactions and executes them
    /// 4. calls the callback with the transaction info, the execution result, the changed state
    /// _after_ the transaction [StateProviderDatabase] and the database that points to the state
    /// right _before_ the transaction.
    async fn trace_block_with<F, R>(
        &self,
        block_id: BlockId,
        config: TracingInspectorConfig,
        f: F,
    ) -> EthResult<Option<Vec<R>>>
    where
        // This is the callback that's invoked for each transaction with
        F: for<'a> Fn(
                TransactionInfo,
                TracingInspector,
                ExecutionResult,
                &'a State,
                &'a CacheDB<StateProviderDatabase<StateProviderBox>>,
            ) -> EthResult<R>
            + Send
            + 'static,
        R: Send + 'static;

    /// Executes all transactions of a block.
    ///
    /// If a `highest_index` is given, this will only execute the first `highest_index`
    /// transactions, in other words, it will stop executing transactions after the
    /// `highest_index`th transaction.
    async fn trace_block_until<F, R>(
        &self,
        block_id: BlockId,
        highest_index: Option<u64>,
        config: TracingInspectorConfig,
        f: F,
    ) -> EthResult<Option<Vec<R>>>
    where
        F: for<'a> Fn(
                TransactionInfo,
                TracingInspector,
                ExecutionResult,
                &'a State,
                &'a CacheDB<StateProviderDatabase<StateProviderBox>>,
            ) -> EthResult<R>
            + Send
            + 'static,
        R: Send + 'static;
}

#[async_trait]
impl<Provider, Pool, Network> EthTransactions for EthApi<Provider, Pool, Network>
where
    Pool: TransactionPool + Clone + 'static,
    Provider:
        BlockReaderIdExt + ChainSpecProvider + StateProviderFactory + EvmEnvProvider + 'static,
    Network: NetworkInfo + Send + Sync + 'static,
{
    fn call_gas_limit(&self) -> u64 {
        self.inner.gas_cap
    }

    fn state_at(&self, at: BlockId) -> EthResult<StateProviderBox> {
        self.state_at_block_id(at)
    }

    fn with_state_at_block<F, T>(&self, at: BlockId, f: F) -> EthResult<T>
    where
        F: FnOnce(StateProviderBox) -> EthResult<T>,
    {
        let state = self.state_at(at)?;
        f(state)
    }

    async fn spawn_with_state_at_block<F, T>(&self, at: BlockId, f: F) -> EthResult<T>
    where
        F: FnOnce(StateProviderBox) -> EthResult<T> + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_tracing_task_with(move |this| {
            let state = this.state_at(at)?;
            f(state)
        })
        .await
    }

    async fn evm_env_at(&self, at: BlockId) -> EthResult<(CfgEnv, BlockEnv, BlockId)> {
        if at.is_pending() {
            let PendingBlockEnv { cfg, block_env, origin } = self.pending_block_env_and_cfg()?;
            Ok((cfg, block_env, origin.state_block_id()))
        } else {
            //  Use cached values if there is no pending block
            let block_hash = self
                .provider()
                .block_hash_for_id(at)?
                .ok_or_else(|| EthApiError::UnknownBlockNumber)?;
            let (cfg, env) = self.cache().get_evm_env(block_hash).await?;
            Ok((cfg, env, block_hash.into()))
        }
    }

    async fn evm_env_for_raw_block(&self, header: &Header) -> EthResult<(CfgEnv, BlockEnv)> {
        // get the parent config first
        let (cfg, mut block_env, _) = self.evm_env_at(header.parent_hash.into()).await?;

        let after_merge = cfg.spec_id >= SpecId::MERGE;
        fill_block_env_with_coinbase(&mut block_env, header, after_merge, header.beneficiary);

        Ok((cfg, block_env))
    }

    async fn transactions_by_block(
        &self,
        block: B256,
    ) -> EthResult<Option<Vec<TransactionSigned>>> {
        Ok(self.cache().get_block_transactions(block).await?)
    }

    async fn block_by_id(&self, id: BlockId) -> EthResult<Option<SealedBlock>> {
        self.block(id).await
    }

    async fn block_by_id_with_senders(
        &self,
        id: BlockId,
    ) -> EthResult<Option<SealedBlockWithSenders>> {
        self.block_with_senders(id).await
    }

    async fn transactions_by_block_id(
        &self,
        block: BlockId,
    ) -> EthResult<Option<Vec<TransactionSigned>>> {
        self.block_by_id(block).await.map(|block| block.map(|block| block.body))
    }

    async fn transaction_by_hash(&self, hash: B256) -> EthResult<Option<TransactionSource>> {
        // Try to find the transaction on disk
        let mut resp = self
            .on_blocking_task(|this| async move {
                match this.provider().transaction_by_hash_with_meta(hash)? {
                    None => Ok(None),
                    Some((tx, meta)) => {
                        // Note: we assume this transaction is valid, because it's mined (or part of
                        // pending block) and already. We don't need to
                        // check for pre EIP-2 because this transaction could be pre-EIP-2.
                        let transaction = tx
                            .into_ecrecovered_unchecked()
                            .ok_or(EthApiError::InvalidTransactionSignature)?;

                        let tx = TransactionSource::Block {
                            transaction,
                            index: meta.index,
                            block_hash: meta.block_hash,
                            block_number: meta.block_number,
                            base_fee: meta.base_fee,
                        };
                        Ok(Some(tx))
                    }
                }
            })
            .await?;

        if resp.is_none() {
            // tx not found on disk, check pool
            if let Some(tx) =
                self.pool().get(&hash).map(|tx| tx.transaction.to_recovered_transaction())
            {
                resp = Some(TransactionSource::Pool(tx));
            }
        }

        Ok(resp)
    }

    async fn transaction_by_hash_at(
        &self,
        transaction_hash: B256,
    ) -> EthResult<Option<(TransactionSource, BlockId)>> {
        match self.transaction_by_hash(transaction_hash).await? {
            None => return Ok(None),
            Some(tx) => {
                let res = match tx {
                    tx @ TransactionSource::Pool(_) => {
                        (tx, BlockId::Number(BlockNumberOrTag::Pending))
                    }
                    TransactionSource::Block {
                        transaction,
                        index,
                        block_hash,
                        block_number,
                        base_fee,
                    } => {
                        let at = BlockId::Hash(block_hash.into());
                        let tx = TransactionSource::Block {
                            transaction,
                            index,
                            block_hash,
                            block_number,
                            base_fee,
                        };
                        (tx, at)
                    }
                };
                Ok(Some(res))
            }
        }
    }

    async fn historical_transaction_by_hash_at(
        &self,
        hash: B256,
    ) -> EthResult<Option<(TransactionSource, B256)>> {
        match self.transaction_by_hash_at(hash).await? {
            None => Ok(None),
            Some((tx, at)) => Ok(at.as_block_hash().map(|hash| (tx, hash))),
        }
    }

    async fn transaction_receipt(&self, hash: B256) -> EthResult<Option<TransactionReceipt>> {
        let result = self
            .on_blocking_task(|this| async move {
                let (tx, meta) = match this.provider().transaction_by_hash_with_meta(hash)? {
                    Some((tx, meta)) => (tx, meta),
                    None => return Ok(None),
                };

                let receipt = match this.provider().receipt_by_hash(hash)? {
                    Some(recpt) => recpt,
                    None => return Ok(None),
                };

                Ok(Some((tx, meta, receipt)))
            })
            .await?;

        let (tx, meta, receipt) = match result {
            Some((tx, meta, receipt)) => (tx, meta, receipt),
            None => return Ok(None),
        };

        self.build_transaction_receipt(tx, meta, receipt).await.map(Some)
    }

    async fn send_raw_transaction(&self, tx: Bytes) -> EthResult<B256> {
        // On optimism, transactions are forwarded directly to the sequencer to be included in
        // blocks that it builds.
        #[cfg(feature = "optimism")]
        self.forward_to_sequencer(&tx).await?;

        let recovered = recover_raw_transaction(tx)?;
        let pool_transaction = <Pool::Transaction>::from_recovered_pooled_transaction(recovered);

        // submit the transaction to the pool with a `Local` origin
        let hash = self.pool().add_transaction(TransactionOrigin::Local, pool_transaction).await?;

        Ok(hash)
    }

    async fn send_transaction(&self, mut request: TransactionRequest) -> EthResult<B256> {
        let from = match request.from {
            Some(from) => from,
            None => return Err(SignError::NoAccount.into()),
        };

        // set nonce if not already set before
        if request.nonce.is_none() {
            let nonce =
                self.get_transaction_count(from, Some(BlockId::Number(BlockNumberOrTag::Pending)))?;
            // note: `.to()` can't panic because the nonce is constructed from a `u64`
            request.nonce = Some(U64::from(nonce.to::<u64>()));
        }

        let chain_id = self.chain_id();
        // TODO: we need an oracle to fetch the gas price of the current chain
        let gas_price = request.gas_price.unwrap_or_default();
        let max_fee_per_gas = request.max_fee_per_gas.unwrap_or_default();

        let estimated_gas = self
            .estimate_gas_at(
                CallRequest {
                    from: Some(from),
                    to: request.to,
                    gas: request.gas,
                    gas_price: Some(U256::from(gas_price)),
                    max_fee_per_gas: Some(U256::from(max_fee_per_gas)),
                    value: request.value,
                    input: request.input.clone().into(),
                    nonce: request.nonce,
                    chain_id: Some(chain_id),
                    access_list: request.access_list.clone(),
                    max_priority_fee_per_gas: Some(U256::from(max_fee_per_gas)),
                    transaction_type: None,
                    blob_versioned_hashes: None,
                    max_fee_per_blob_gas: None,
                },
                BlockId::Number(BlockNumberOrTag::Pending),
                None,
            )
            .await?;
        let gas_limit = estimated_gas;

        let transaction = match request.into_typed_request() {
            Some(TypedTransactionRequest::Legacy(mut m)) => {
                m.chain_id = Some(chain_id.to());
                m.gas_limit = gas_limit;
                m.gas_price = gas_price;

                TypedTransactionRequest::Legacy(m)
            }
            Some(TypedTransactionRequest::EIP2930(mut m)) => {
                m.chain_id = chain_id.to();
                m.gas_limit = gas_limit;
                m.gas_price = gas_price;

                TypedTransactionRequest::EIP2930(m)
            }
            Some(TypedTransactionRequest::EIP1559(mut m)) => {
                m.chain_id = chain_id.to();
                m.gas_limit = gas_limit;
                m.max_fee_per_gas = max_fee_per_gas;

                TypedTransactionRequest::EIP1559(m)
            }
            Some(TypedTransactionRequest::EIP4844(mut m)) => {
                m.chain_id = chain_id.to();
                m.gas_limit = gas_limit;
                m.max_fee_per_gas = max_fee_per_gas;

                TypedTransactionRequest::EIP4844(m)
            }
            None => return Err(EthApiError::ConflictingFeeFieldsInRequest),
        };

        let signed_tx = self.sign_request(&from, transaction)?;

        let recovered =
            signed_tx.into_ecrecovered().ok_or(EthApiError::InvalidTransactionSignature)?;

        let pool_transaction =
            <Pool::Transaction>::from_recovered_pooled_transaction(recovered.into());

        // submit the transaction to the pool with a `Local` origin
        let hash = self.pool().add_transaction(TransactionOrigin::Local, pool_transaction).await?;

        Ok(hash)
    }

    async fn spawn_with_call_at<F, R>(
        &self,
        request: CallRequest,
        at: BlockId,
        overrides: EvmOverrides,
        f: F,
    ) -> EthResult<R>
    where
        F: FnOnce(StateCacheDB, Env) -> EthResult<R> + Send + 'static,
        R: Send + 'static,
    {
        let (cfg, block_env, at) = self.evm_env_at(at).await?;
        let this = self.clone();
        self.inner
            .blocking_task_pool
            .spawn(move || {
                let state = this.state_at(at)?;
                let mut db = CacheDB::new(StateProviderDatabase::new(state));

                let env = prepare_call_env(
                    cfg,
                    block_env,
                    request,
                    this.call_gas_limit(),
                    &mut db,
                    overrides,
                )?;
                f(db, env)
            })
            .await
            .map_err(|_| EthApiError::InternalBlockingTaskError)?
    }

    async fn transact_call_at(
        &self,
        request: CallRequest,
        at: BlockId,
        overrides: EvmOverrides,
    ) -> EthResult<(ResultAndState, Env)> {
        self.spawn_with_call_at(request, at, overrides, move |mut db, env| transact(&mut db, env))
            .await
    }

    async fn spawn_inspect_call_at<I>(
        &self,
        request: CallRequest,
        at: BlockId,
        overrides: EvmOverrides,
        inspector: I,
    ) -> EthResult<(ResultAndState, Env)>
    where
        I: Inspector<StateCacheDB> + Send + 'static,
    {
        self.spawn_with_call_at(request, at, overrides, move |db, env| inspect(db, env, inspector))
            .await
    }

    fn trace_at<F, R>(
        &self,
        env: Env,
        config: TracingInspectorConfig,
        at: BlockId,
        f: F,
    ) -> EthResult<R>
    where
        F: FnOnce(TracingInspector, ResultAndState) -> EthResult<R>,
    {
        self.with_state_at_block(at, |state| {
            let db = CacheDB::new(StateProviderDatabase::new(state));

            let mut inspector = TracingInspector::new(config);
            let (res, _) = inspect(db, env, &mut inspector)?;

            f(inspector, res)
        })
    }

    async fn spawn_trace_at_with_state<F, R>(
        &self,
        env: Env,
        config: TracingInspectorConfig,
        at: BlockId,
        f: F,
    ) -> EthResult<R>
    where
        F: FnOnce(TracingInspector, ResultAndState, StateCacheDB) -> EthResult<R> + Send + 'static,
        R: Send + 'static,
    {
        self.spawn_with_state_at_block(at, move |state| {
            let db = CacheDB::new(StateProviderDatabase::new(state));
            let mut inspector = TracingInspector::new(config);
            let (res, _, db) = inspect_and_return_db(db, env, &mut inspector)?;

            f(inspector, res, db)
        })
        .await
    }

    async fn transaction_and_block(
        &self,
        hash: B256,
    ) -> EthResult<Option<(TransactionSource, SealedBlock)>> {
        let (transaction, at) = match self.transaction_by_hash_at(hash).await? {
            None => return Ok(None),
            Some(res) => res,
        };

        // Note: this is always either hash or pending
        let block_hash = match at {
            BlockId::Hash(hash) => hash.block_hash,
            _ => return Ok(None),
        };
        let block = self.cache().get_block(block_hash).await?;
        Ok(block.map(|block| (transaction, block.seal(block_hash))))
    }

    async fn spawn_trace_transaction_in_block<F, R>(
        &self,
        hash: B256,
        config: TracingInspectorConfig,
        f: F,
    ) -> EthResult<Option<R>>
    where
        F: FnOnce(TransactionInfo, TracingInspector, ResultAndState, StateCacheDB) -> EthResult<R>
            + Send
            + 'static,
        R: Send + 'static,
    {
        let (transaction, block) = match self.transaction_and_block(hash).await? {
            None => return Ok(None),
            Some(res) => res,
        };
        let (tx, tx_info) = transaction.split();

        let (cfg, block_env, _) = self.evm_env_at(block.hash.into()).await?;

        // we need to get the state of the parent block because we're essentially replaying the
        // block the transaction is included in
        let parent_block = block.parent_hash;
        let block_txs = block.body;

        self.spawn_with_state_at_block(parent_block.into(), move |state| {
            let mut db = CacheDB::new(StateProviderDatabase::new(state));

            // replay all transactions prior to the targeted transaction
            replay_transactions_until(&mut db, cfg.clone(), block_env.clone(), block_txs, tx.hash)?;

            let env = Env { cfg, block: block_env, tx: tx_env_with_recovered(&tx) };

            let mut inspector = TracingInspector::new(config);
            let (res, _, db) = inspect_and_return_db(db, env, &mut inspector)?;
            f(tx_info, inspector, res, db)
        })
        .await
        .map(Some)
    }

    async fn trace_block_with<F, R>(
        &self,
        block_id: BlockId,
        config: TracingInspectorConfig,
        f: F,
    ) -> EthResult<Option<Vec<R>>>
    where
        // This is the callback that's invoked for each transaction with
        F: for<'a> Fn(
                TransactionInfo,
                TracingInspector,
                ExecutionResult,
                &'a State,
                &'a CacheDB<StateProviderDatabase<StateProviderBox>>,
            ) -> EthResult<R>
            + Send
            + 'static,
        R: Send + 'static,
    {
        self.trace_block_until(block_id, None, config, f).await
    }

    async fn trace_block_until<F, R>(
        &self,
        block_id: BlockId,
        highest_index: Option<u64>,
        config: TracingInspectorConfig,
        f: F,
    ) -> EthResult<Option<Vec<R>>>
    where
        F: for<'a> Fn(
                TransactionInfo,
                TracingInspector,
                ExecutionResult,
                &'a State,
                &'a CacheDB<StateProviderDatabase<StateProviderBox>>,
            ) -> EthResult<R>
            + Send
            + 'static,
        R: Send + 'static,
    {
        let ((cfg, block_env, _), block) =
            futures::try_join!(self.evm_env_at(block_id), self.block_with_senders(block_id))?;

        let Some(block) = block else { return Ok(None) };

        // replay all transactions of the block
        self.spawn_tracing_task_with(move |this| {
            // we need to get the state of the parent block because we're replaying this block on
            // top of its parent block's state
            let state_at = block.parent_hash;
            let block_hash = block.hash;

            let block_number = block_env.number.saturating_to::<u64>();
            let base_fee = block_env.basefee.saturating_to::<u64>();

            // prepare transactions, we do everything upfront to reduce time spent with open state
            let max_transactions =
                highest_index.map_or(block.body.len(), |highest| highest as usize);
            let mut results = Vec::with_capacity(max_transactions);

            let mut transactions = block
                .into_transactions_ecrecovered()
                .take(max_transactions)
                .enumerate()
                .map(|(idx, tx)| {
                    let tx_info = TransactionInfo {
                        hash: Some(tx.hash()),
                        index: Some(idx as u64),
                        block_hash: Some(block_hash),
                        block_number: Some(block_number),
                        base_fee: Some(base_fee),
                    };
                    let tx_env = tx_env_with_recovered(&tx);
                    (tx_info, tx_env)
                })
                .peekable();

            // now get the state
            let state = this.state_at(state_at.into())?;
            let mut db = CacheDB::new(StateProviderDatabase::new(state));

            while let Some((tx_info, tx)) = transactions.next() {
                let env = Env { cfg: cfg.clone(), block: block_env.clone(), tx };

                let mut inspector = TracingInspector::new(config);
                let (res, _) = inspect(&mut db, env, &mut inspector)?;
                let ResultAndState { result, state } = res;
                results.push(f(tx_info, inspector, result, &state, &db)?);

                // need to apply the state changes of this transaction before executing the
                // next transaction
                if transactions.peek().is_some() {
                    // need to apply the state changes of this transaction before executing
                    // the next transaction
                    db.commit(state)
                }
            }

            Ok(results)
        })
        .await
        .map(Some)
    }
}

// === impl EthApi ===

impl<Provider, Pool, Network> EthApi<Provider, Pool, Network>
where
    Pool: TransactionPool + Clone + 'static,
    Provider:
        BlockReaderIdExt + ChainSpecProvider + StateProviderFactory + EvmEnvProvider + 'static,
    Network: NetworkInfo + Send + Sync + 'static,
{
    /// Spawns the given closure on a new blocking tracing task
    async fn spawn_tracing_task_with<F, T>(&self, f: F) -> EthResult<T>
    where
        F: FnOnce(Self) -> EthResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let this = self.clone();
        self.inner
            .blocking_task_pool
            .spawn(move || f(this))
            .await
            .map_err(|_| EthApiError::InternalBlockingTaskError)?
    }
}

impl<Provider, Pool, Network> EthApi<Provider, Pool, Network>
where
    Provider:
        BlockReaderIdExt + ChainSpecProvider + StateProviderFactory + EvmEnvProvider + 'static,
    Network: NetworkInfo + 'static,
{
    /// Helper function for `eth_getTransactionReceipt`
    ///
    /// Returns the receipt
    #[cfg(not(feature = "optimism"))]
    pub(crate) async fn build_transaction_receipt(
        &self,
        tx: TransactionSigned,
        meta: TransactionMeta,
        receipt: Receipt,
    ) -> EthResult<TransactionReceipt> {
        // get all receipts for the block
        let all_receipts = match self.cache().get_receipts(meta.block_hash).await? {
            Some(recpts) => recpts,
            None => return Err(EthApiError::UnknownBlockNumber),
        };
        build_transaction_receipt_with_block_receipts(tx, meta, receipt, &all_receipts)
    }

    /// Helper function for `eth_getTransactionReceipt` (optimism)
    ///
    /// Returns the receipt
    #[cfg(feature = "optimism")]
    pub(crate) async fn build_transaction_receipt(
        &self,
        tx: TransactionSigned,
        meta: TransactionMeta,
        receipt: Receipt,
    ) -> EthResult<TransactionReceipt> {
        let (block, receipts) = self
            .cache()
            .get_block_and_receipts(meta.block_hash)
            .await?
            .ok_or(EthApiError::UnknownBlockNumber)?;

        let block = block.unseal();
        let l1_block_info = reth_revm::optimism::extract_l1_info(&block).ok();
        let optimism_tx_meta = self.build_op_tx_meta(&tx, l1_block_info, block.timestamp)?;

        build_transaction_receipt_with_block_receipts(
            tx,
            meta,
            receipt,
            &receipts,
            optimism_tx_meta,
        )
    }

    /// Builds [OptimismTxMeta] object using the provided [TransactionSigned],
    /// [L1BlockInfo] and `block_timestamp`. The [L1BlockInfo] is used to calculate
    /// the l1 fee and l1 data gas for the transaction.
    /// If the [L1BlockInfo] is not provided, the [OptimismTxMeta] will be empty.
    #[cfg(feature = "optimism")]
    pub(crate) fn build_op_tx_meta(
        &self,
        tx: &TransactionSigned,
        l1_block_info: Option<L1BlockInfo>,
        block_timestamp: u64,
    ) -> EthResult<OptimismTxMeta> {
        if let Some(l1_block_info) = l1_block_info {
            let envelope_buf: Bytes = {
                let mut envelope_buf = bytes::BytesMut::default();
                tx.encode_enveloped(&mut envelope_buf);
                envelope_buf.freeze().into()
            };

            let (l1_fee, l1_data_gas) = match (!tx.is_deposit())
                .then(|| {
                    let inner_l1_fee = match l1_block_info.l1_tx_data_fee(
                        &self.inner.provider.chain_spec(),
                        block_timestamp,
                        &envelope_buf,
                        tx.is_deposit(),
                    ) {
                        Ok(inner_l1_fee) => inner_l1_fee,
                        Err(e) => return Err(e),
                    };
                    let inner_l1_data_gas = match l1_block_info.l1_data_gas(
                        &self.inner.provider.chain_spec(),
                        block_timestamp,
                        &envelope_buf,
                    ) {
                        Ok(inner_l1_data_gas) => inner_l1_data_gas,
                        Err(e) => return Err(e),
                    };
                    Ok((inner_l1_fee, inner_l1_data_gas))
                })
                .transpose()
                .map_err(|_| EthApiError::InternalEthError)?
            {
                Some((l1_fee, l1_data_gas)) => (Some(l1_fee), Some(l1_data_gas)),
                None => (None, None),
            };

            Ok(OptimismTxMeta::new(Some(l1_block_info), l1_fee, l1_data_gas))
        } else {
            Ok(OptimismTxMeta::default())
        }
    }

    /// Helper function for `eth_sendRawTransaction` for Optimism.
    ///
    /// Forwards the raw transaction bytes to the configured sequencer endpoint.
    /// This is a no-op if the sequencer endpoint is not configured.
    #[cfg(feature = "optimism")]
    pub async fn forward_to_sequencer(&self, tx: &Bytes) -> EthResult<()> {
        if let Some(endpoint) = self.network().sequencer_endpoint() {
            let body = serde_json::to_string(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_sendRawTransaction",
                "params": [format!("0x{}", alloy_primitives::hex::encode(tx))],
                "id": self.network().chain_id()
            }))
            .map_err(|_| {
                tracing::warn!(
                    target = "rpc::eth",
                    "Failed to serialize transaction for forwarding to sequencer"
                );
                EthApiError::InternalEthError
            })?;

            self.inner
                .http_client
                .post(endpoint)
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .map_err(|_| EthApiError::InternalEthError)?;
        }
        Ok(())
    }
}

impl<Provider, Pool, Network> EthApi<Provider, Pool, Network>
where
    Pool: TransactionPool + 'static,
    Provider:
        BlockReaderIdExt + ChainSpecProvider + StateProviderFactory + EvmEnvProvider + 'static,
    Network: NetworkInfo + Send + Sync + 'static,
{
    pub(crate) fn sign_request(
        &self,
        from: &Address,
        request: TypedTransactionRequest,
    ) -> EthResult<TransactionSigned> {
        for signer in self.inner.signers.iter() {
            if signer.is_signer_for(from) {
                return match signer.sign_transaction(request, from) {
                    Ok(tx) => Ok(tx),
                    Err(e) => Err(e.into()),
                }
            }
        }
        Err(EthApiError::InvalidTransactionSignature)
    }

    /// Get Transaction by [BlockId] and the index of the transaction within that Block.
    ///
    /// Returns `Ok(None)` if the block does not exist, or the block as fewer transactions
    pub(crate) async fn transaction_by_block_and_tx_index(
        &self,
        block_id: impl Into<BlockId>,
        index: Index,
    ) -> EthResult<Option<Transaction>> {
        if let Some(block) = self.block_with_senders(block_id.into()).await? {
            let block_hash = block.hash;
            let block_number = block.number;
            let base_fee_per_gas = block.base_fee_per_gas;
            if let Some(tx) = block.into_transactions_ecrecovered().nth(index.into()) {
                return Ok(Some(from_recovered_with_block_context(
                    tx,
                    block_hash,
                    block_number,
                    base_fee_per_gas,
                    index.into(),
                )))
            }
        }

        Ok(None)
    }
}
/// Represents from where a transaction was fetched.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TransactionSource {
    /// Transaction exists in the pool (Pending)
    Pool(TransactionSignedEcRecovered),
    /// Transaction already included in a block
    ///
    /// This can be a historical block or a pending block (received from the CL)
    Block {
        /// Transaction fetched via provider
        transaction: TransactionSignedEcRecovered,
        /// Index of the transaction in the block
        index: u64,
        /// Hash of the block.
        block_hash: B256,
        /// Number of the block.
        block_number: u64,
        /// base fee of the block.
        base_fee: Option<u64>,
    },
}

// === impl TransactionSource ===

impl TransactionSource {
    /// Consumes the type and returns the wrapped transaction.
    pub fn into_recovered(self) -> TransactionSignedEcRecovered {
        self.into()
    }

    /// Returns the transaction and block related info, if not pending
    pub fn split(self) -> (TransactionSignedEcRecovered, TransactionInfo) {
        match self {
            TransactionSource::Pool(tx) => {
                let hash = tx.hash();
                (
                    tx,
                    TransactionInfo {
                        hash: Some(hash),
                        index: None,
                        block_hash: None,
                        block_number: None,
                        base_fee: None,
                    },
                )
            }
            TransactionSource::Block { transaction, index, block_hash, block_number, base_fee } => {
                let hash = transaction.hash();
                (
                    transaction,
                    TransactionInfo {
                        hash: Some(hash),
                        index: Some(index),
                        block_hash: Some(block_hash),
                        block_number: Some(block_number),
                        base_fee,
                    },
                )
            }
        }
    }
}

impl From<TransactionSource> for TransactionSignedEcRecovered {
    fn from(value: TransactionSource) -> Self {
        match value {
            TransactionSource::Pool(tx) => tx,
            TransactionSource::Block { transaction, .. } => transaction,
        }
    }
}

impl From<TransactionSource> for Transaction {
    fn from(value: TransactionSource) -> Self {
        match value {
            TransactionSource::Pool(tx) => reth_rpc_types_compat::transaction::from_recovered(tx),
            TransactionSource::Block { transaction, index, block_hash, block_number, base_fee } => {
                from_recovered_with_block_context(
                    transaction,
                    block_hash,
                    block_number,
                    base_fee,
                    U256::from(index),
                )
            }
        }
    }
}

/// Helper function to construct a transaction receipt
///
/// Note: This requires _all_ block receipts because we need to calculate the gas used by the
/// transaction.
pub(crate) fn build_transaction_receipt_with_block_receipts(
    transaction: TransactionSigned,
    meta: TransactionMeta,
    receipt: Receipt,
    all_receipts: &[Receipt],
    #[cfg(feature = "optimism")] optimism_tx_meta: OptimismTxMeta,
) -> EthResult<TransactionReceipt> {
    // Note: we assume this transaction is valid, because it's mined (or part of pending block) and
    // we don't need to check for pre EIP-2
    let from =
        transaction.recover_signer_unchecked().ok_or(EthApiError::InvalidTransactionSignature)?;

    // get the previous transaction cumulative gas used
    let gas_used = if meta.index == 0 {
        receipt.cumulative_gas_used
    } else {
        let prev_tx_idx = (meta.index - 1) as usize;
        all_receipts
            .get(prev_tx_idx)
            .map(|prev_receipt| receipt.cumulative_gas_used - prev_receipt.cumulative_gas_used)
            .unwrap_or_default()
    };

    #[allow(clippy::needless_update)]
    let mut res_receipt = TransactionReceipt {
        transaction_hash: Some(meta.tx_hash),
        transaction_index: U64::from(meta.index),
        block_hash: Some(meta.block_hash),
        block_number: Some(U256::from(meta.block_number)),
        from,
        to: None,
        cumulative_gas_used: U256::from(receipt.cumulative_gas_used),
        gas_used: Some(U256::from(gas_used)),
        contract_address: None,
        logs: Vec::with_capacity(receipt.logs.len()),
        effective_gas_price: U128::from(transaction.effective_gas_price(meta.base_fee)),
        transaction_type: transaction.transaction.tx_type().into(),
        // TODO pre-byzantium receipts have a post-transaction state root
        state_root: None,
        logs_bloom: receipt.bloom_slow(),
        status_code: if receipt.success { Some(U64::from(1)) } else { Some(U64::from(0)) },
        // EIP-4844 fields
        blob_gas_price: meta.excess_blob_gas.map(calc_blob_gasprice).map(U128::from),
        blob_gas_used: transaction.transaction.blob_gas_used().map(U128::from),
        // Optimism fields
        #[cfg(feature = "optimism")]
        deposit_nonce: receipt.deposit_nonce.map(U64::from),
        ..Default::default()
    };

    #[cfg(feature = "optimism")]
    if let Some(l1_block_info) = optimism_tx_meta.l1_block_info {
        if !transaction.is_deposit() {
            res_receipt.l1_fee = optimism_tx_meta.l1_fee;
            res_receipt.l1_gas_used =
                optimism_tx_meta.l1_data_gas.map(|dg| dg + l1_block_info.l1_fee_overhead);
            res_receipt.l1_fee_scalar =
                Some(l1_block_info.l1_fee_scalar.div(U256::from(1_000_000)));
            res_receipt.l1_gas_price = Some(l1_block_info.l1_base_fee);
        }
    }

    match transaction.transaction.kind() {
        Create => {
            res_receipt.contract_address = Some(from.create(transaction.transaction.nonce()));
        }
        Call(addr) => {
            res_receipt.to = Some(*addr);
        }
    }

    // get number of logs in the block
    let mut num_logs = 0;
    for prev_receipt in all_receipts.iter().take(meta.index as usize) {
        num_logs += prev_receipt.logs.len();
    }

    for (tx_log_idx, log) in receipt.logs.into_iter().enumerate() {
        let rpclog = Log {
            address: log.address,
            topics: log.topics,
            data: log.data,
            block_hash: Some(meta.block_hash),
            block_number: Some(U256::from(meta.block_number)),
            transaction_hash: Some(meta.tx_hash),
            transaction_index: Some(U256::from(meta.index)),
            log_index: Some(U256::from(num_logs + tx_log_idx)),
            removed: false,
        };
        res_receipt.logs.push(rpclog);
    }

    Ok(res_receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        eth::{
            cache::EthStateCache, gas_oracle::GasPriceOracle, FeeHistoryCache,
            FeeHistoryCacheConfig,
        },
        BlockingTaskPool, EthApi,
    };
    use reth_network_api::noop::NoopNetwork;
    use reth_primitives::{constants::ETHEREUM_BLOCK_GAS_LIMIT, hex_literal::hex, Bytes};
    use reth_provider::test_utils::NoopProvider;
    use reth_transaction_pool::{test_utils::testing_pool, TransactionPool};

    #[tokio::test]
    async fn send_raw_transaction() {
        let noop_provider = NoopProvider::default();
        let noop_network_provider = NoopNetwork::default();

        let pool = testing_pool();

        let cache = EthStateCache::spawn(noop_provider, Default::default());
        let fee_history_cache =
            FeeHistoryCache::new(cache.clone(), FeeHistoryCacheConfig::default());
        let eth_api = EthApi::new(
            noop_provider,
            pool.clone(),
            noop_network_provider,
            cache.clone(),
            GasPriceOracle::new(noop_provider, Default::default(), cache.clone()),
            ETHEREUM_BLOCK_GAS_LIMIT,
            BlockingTaskPool::build().expect("failed to build tracing pool"),
            fee_history_cache,
        );

        // https://etherscan.io/tx/0xa694b71e6c128a2ed8e2e0f6770bddbe52e3bb8f10e8472f9a79ab81497a8b5d
        let tx_1 = Bytes::from(hex!("02f871018303579880850555633d1b82520894eee27662c2b8eba3cd936a23f039f3189633e4c887ad591c62bdaeb180c080a07ea72c68abfb8fca1bd964f0f99132ed9280261bdca3e549546c0205e800f7d0a05b4ef3039e9c9b9babc179a1878fb825b5aaf5aed2fa8744854150157b08d6f3"));

        let tx_1_result = eth_api.send_raw_transaction(tx_1).await.unwrap();
        assert_eq!(
            pool.len(),
            1,
            "expect 1 transactions in the pool, but pool size is {}",
            pool.len()
        );

        // https://etherscan.io/tx/0x48816c2f32c29d152b0d86ff706f39869e6c1f01dc2fe59a3c1f9ecf39384694
        let tx_2 = Bytes::from(hex!("02f9043c018202b7843b9aca00850c807d37a08304d21d94ef1c6e67703c7bd7107eed8303fbe6ec2554bf6b881bc16d674ec80000b903c43593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000063e2d99f00000000000000000000000000000000000000000000000000000000000000030b000800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000001bc16d674ec80000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000065717fe021ea67801d1088cc80099004b05b64600000000000000000000000000000000000000000000000001bc16d674ec80000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002bc02aaa39b223fe8d0a0e5c4f27ead9083c756cc20001f4a0b86991c6218b36c1d19d4a2e9eb0ce3606eb480000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000000180000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000009e95fd5965fd1f1a6f0d4600000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48000000000000000000000000428dca9537116148616a5a3e44035af17238fe9dc080a0c6ec1e41f5c0b9511c49b171ad4e04c6bb419c74d99fe9891d74126ec6e4e879a032069a753d7a2cfa158df95421724d24c0e9501593c09905abf3699b4a4405ce"));

        let tx_2_result = eth_api.send_raw_transaction(tx_2).await.unwrap();
        assert_eq!(
            pool.len(),
            2,
            "expect 2 transactions in the pool, but pool size is {}",
            pool.len()
        );

        assert!(pool.get(&tx_1_result).is_some(), "tx1 not found in the pool");
        assert!(pool.get(&tx_2_result).is_some(), "tx2 not found in the pool");
    }
}

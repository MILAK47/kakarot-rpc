pub mod config;
pub mod constants;
pub mod errors;
pub mod helpers;
#[cfg(test)]
pub mod tests;
pub mod waiter;

use std::sync::Arc;

use eyre::Result;
use futures::future::join_all;
use reqwest::Client;
use reth_primitives::{
    AccessList, Address, BlockId, BlockNumberOrTag, Bytes, Signature, Transaction, TransactionKind, TxEip1559, H256,
    U128, U256, U64,
};
use reth_rpc_types::{BlockTransactions, CallRequest, FeeHistory, Index, RichBlock, Transaction as EtherTransaction};
use starknet::core::types::{
    BlockId as StarknetBlockId, BlockTag, BroadcastedInvokeTransaction, EmittedEvent, EventFilterWithPage, EventsPage,
    FieldElement, MaybePendingBlockWithTxHashes, MaybePendingBlockWithTxs, MaybePendingTransactionReceipt,
    StarknetError, Transaction as TransactionType, TransactionReceipt as StarknetTransactionReceipt,
};
use starknet::providers::sequencer::models::{FeeEstimate, FeeUnit, TransactionSimulationInfo, TransactionTrace};
use starknet::providers::{MaybeUnknownErrorCode, Provider, ProviderError, StarknetErrorWithMessage};

use self::config::{KakarotRpcConfig, Network};
use self::constants::gas::{BASE_FEE_PER_GAS, MAX_PRIORITY_FEE_PER_GAS, MINIMUM_GAS_FEE};
use self::constants::{
    CHAIN_ID, COUNTER_CALL_MAINNET, COUNTER_CALL_TESTNET1, COUNTER_CALL_TESTNET2,
    DUMMY_ARGENT_GAS_PRICE_ACCOUNT_ADDRESS, ESTIMATE_GAS, MAX_FEE, STARKNET_NATIVE_TOKEN,
};
use self::errors::EthApiError;
use self::helpers::{bytes_to_felt_vec, raw_kakarot_calldata};
use self::waiter::TransactionWaiter;
use crate::contracts::account::{Account, KakarotAccount};
use crate::contracts::contract_account::ContractAccount;
use crate::contracts::erc20::ethereum_erc20::EthereumErc20;
use crate::contracts::erc20::starknet_erc20::StarknetErc20;
use crate::contracts::kakarot::KakarotContract;
use crate::models::balance::{FutureTokenBalance, TokenBalances};
use crate::models::block::{BlockWithTxHashes, BlockWithTxs, EthBlockId};
use crate::models::convertible::{
    ConvertibleSignedTransaction, ConvertibleStarknetBlock, ConvertibleStarknetTransaction,
};
use crate::models::felt::Felt252Wrapper;
use crate::models::transaction::transaction::{StarknetTransaction, StarknetTransactions};
use crate::models::transaction::transaction_signed::StarknetTransactionSigned;
use crate::models::ConversionError;

pub struct KakarotClient<P: Provider + Send + Sync> {
    starknet_provider: Arc<P>,
    kakarot_contract: KakarotContract<P>,
    network: Network,
}

impl<P: Provider + Send + Sync + 'static> KakarotClient<P> {
    /// Create a new `KakarotClient`.
    pub fn new(starknet_config: KakarotRpcConfig, starknet_provider: Arc<P>) -> Self {
        let KakarotRpcConfig {
            kakarot_address,
            proxy_account_class_hash,
            externally_owned_account_class_hash,
            contract_account_class_hash,
            network,
        } = starknet_config;

        let kakarot_contract = KakarotContract::new(
            Arc::clone(&starknet_provider),
            kakarot_address,
            proxy_account_class_hash,
            externally_owned_account_class_hash,
            contract_account_class_hash,
        );

        Self { starknet_provider, network, kakarot_contract }
    }

    /// Returns the result of executing a call on a ethereum address for a given calldata and block
    /// without creating a transaction.
    pub async fn call(
        &self,
        origin: Address,
        to: Address,
        calldata: Bytes,
        block_id: BlockId,
    ) -> Result<Bytes, EthApiError<P::Error>> {
        let starknet_block_id: StarknetBlockId = EthBlockId::new(block_id).try_into()?;

        let to: Felt252Wrapper = to.into();
        let to = to.into();

        let origin: FieldElement = Felt252Wrapper::from(origin).into();

        let calldata = bytes_to_felt_vec(&calldata);

        let result = self.kakarot_contract.eth_call(&origin, &to, calldata, &starknet_block_id).await?;

        Ok(result)
    }

    /// Returns the number of transactions in a block given a block id.
    pub async fn get_transaction_count_by_block(&self, block_id: BlockId) -> Result<U64, EthApiError<P::Error>> {
        let starknet_block_id: StarknetBlockId = EthBlockId::new(block_id).try_into()?;
        let starknet_block = self.starknet_provider.get_block_with_txs(starknet_block_id).await?;

        let block_transactions = match starknet_block {
            MaybePendingBlockWithTxs::PendingBlock(pending_block_with_txs) => {
                self.filter_starknet_into_eth_txs(pending_block_with_txs.transactions.into(), None, None).await
            }
            MaybePendingBlockWithTxs::Block(block_with_txs) => {
                let block_hash: Felt252Wrapper = block_with_txs.block_hash.into();
                let block_hash = Some(block_hash.into());
                let block_number: Felt252Wrapper = block_with_txs.block_number.into();
                let block_number = Some(block_number.into());
                self.filter_starknet_into_eth_txs(block_with_txs.transactions.into(), block_hash, block_number).await
            }
        };
        let len = match block_transactions {
            BlockTransactions::Full(transactions) => transactions.len(),
            BlockTransactions::Hashes(_) => 0,
            BlockTransactions::Uncle => 0,
        };
        Ok(U64::from(len))
    }

    /// Returns the transaction for a given block id and transaction index.
    pub async fn transaction_by_block_id_and_index(
        &self,
        block_id: BlockId,
        tx_index: Index,
    ) -> Result<EtherTransaction, EthApiError<P::Error>> {
        let index: u64 = usize::from(tx_index) as u64;
        let starknet_block_id: StarknetBlockId = EthBlockId::new(block_id).try_into()?;

        let starknet_tx: StarknetTransaction =
            self.starknet_provider.get_transaction_by_block_id_and_index(starknet_block_id, index).await?.into();

        let tx_hash: FieldElement = starknet_tx.transaction_hash()?.into();

        let tx_receipt = self.starknet_provider.get_transaction_receipt(tx_hash).await?;
        let (block_hash, block_num) = match tx_receipt {
            MaybePendingTransactionReceipt::Receipt(StarknetTransactionReceipt::Invoke(tr)) => {
                let block_hash: Felt252Wrapper = tr.block_hash.into();
                (Some(block_hash.into()), Some(U256::from(tr.block_number)))
            }
            _ => (None, None), // skip all transactions other than Invoke, covers the pending case
        };

        let eth_tx = starknet_tx.to_eth_transaction(self, block_hash, block_num, Some(U256::from(index))).await?;
        Ok(eth_tx)
    }

    /// Returns the nonce for a given ethereum address
    /// if it's an EOA, use native nonce and if it's a contract account, use managed nonce
    /// if ethereum -> stark mapping doesn't exist in the starknet provider, we translate
    /// ContractNotFound errors into zeros
    pub async fn nonce(&self, ethereum_address: Address, block_id: BlockId) -> Result<U256, EthApiError<P::Error>> {
        let starknet_block_id: StarknetBlockId = EthBlockId::new(block_id).try_into()?;
        let starknet_address = self.compute_starknet_address(&ethereum_address, &starknet_block_id).await?;

        // Get the implementation of the account
        let account = KakarotAccount::new(starknet_address, &self.starknet_provider);
        let class_hash = match account.implementation(&starknet_block_id).await {
            Ok(class_hash) => class_hash,
            Err(err) => match err {
                EthApiError::RequestError(ProviderError::StarknetError(StarknetErrorWithMessage {
                    code: MaybeUnknownErrorCode::Known(StarknetError::ContractNotFound),
                    ..
                })) => return Ok(U256::from(0)), // Return 0 if the account doesn't exist
                _ => return Err(err), // Propagate the error
            },
        };

        if class_hash == self.kakarot_contract.contract_account_class_hash {
            // Get the nonce of the contract account
            let contract_account = ContractAccount::new(starknet_address, &self.starknet_provider);
            contract_account.nonce(&starknet_block_id).await
        } else {
            // Get the nonce of the EOA
            let nonce = self.starknet_provider.get_nonce(starknet_block_id, starknet_address).await?;
            Ok(Felt252Wrapper::from(nonce).into())
        }
    }

    /// Returns the balance in Starknet's native token of a specific EVM address.
    pub async fn balance(&self, ethereum_address: Address, block_id: BlockId) -> Result<U256, EthApiError<P::Error>> {
        let starknet_block_id: StarknetBlockId = EthBlockId::new(block_id).try_into()?;
        let starknet_address = self.compute_starknet_address(&ethereum_address, &starknet_block_id).await?;

        let native_token_address = FieldElement::from_hex_be(STARKNET_NATIVE_TOKEN).unwrap();
        let provider = self.starknet_provider();
        let native_token = StarknetErc20::new(&provider, native_token_address);
        let balance = native_token.balance_of(&starknet_address, &starknet_block_id).await?;

        Ok(balance)
    }

    /// Returns the storage value at a specific index of a contract given its address and a block
    /// id.
    pub async fn storage_at(
        &self,
        address: Address,
        index: U256,
        block_id: BlockId,
    ) -> Result<U256, EthApiError<P::Error>> {
        let starknet_block_id: StarknetBlockId = EthBlockId::new(block_id).try_into()?;

        let address: Felt252Wrapper = address.into();
        let address = address.into();

        let starknet_contract_address =
            self.kakarot_contract.compute_starknet_address(&address, &starknet_block_id).await?;

        let key_low = index & U256::from(u128::MAX);
        let key_low: Felt252Wrapper = key_low.try_into()?;

        let key_high = index >> 128;
        let key_high: Felt252Wrapper = key_high.try_into()?;

        let provider = self.starknet_provider();
        let contract_account = ContractAccount::new(starknet_contract_address, &provider);
        let storage_value = contract_account.storage(&key_low.into(), &key_high.into(), &starknet_block_id).await?;

        Ok(storage_value)
    }

    /// Returns token balances for a specific address given a list of contracts addresses.
    pub async fn token_balances(
        &self,
        address: Address,
        token_addresses: Vec<Address>,
    ) -> Result<TokenBalances, EthApiError<P::Error>> {
        let block_id = BlockId::Number(BlockNumberOrTag::Latest);

        let handles = token_addresses.into_iter().map(|token_address| {
            let token_addr: Felt252Wrapper = token_address.into();
            let token = EthereumErc20::new(token_addr.into(), &self.kakarot_contract);

            FutureTokenBalance::<P, _>::new(token.balance_of(address.into(), block_id), token_address)
        });

        let token_balances = join_all(handles).await;

        Ok(TokenBalances { address, token_balances })
    }

    /// Sends raw Ethereum transaction bytes to Kakarot
    pub async fn send_transaction(&self, bytes: Bytes) -> Result<H256, EthApiError<P::Error>> {
        let transaction: StarknetTransactionSigned = bytes.into();

        let invoke_transaction = transaction.to_broadcasted_invoke_transaction(self).await?;

        let transaction_result = self.starknet_provider.add_invoke_transaction(&invoke_transaction).await?;
        let waiter =
            TransactionWaiter::new(self.starknet_provider(), transaction_result.transaction_hash, 1000, 15_000);
        waiter.poll().await?;

        Ok(H256::from(transaction_result.transaction_hash.to_bytes_be()))
    }

    /// Returns the fixed base_fee_per_gas of Kakarot
    /// Since Starknet works on a FCFS basis (FIFO queue), it is not possible to tip miners to
    /// incentivize faster transaction inclusion
    /// As a result, in Kakarot, gas_price := base_fee_per_gas
    pub fn base_fee_per_gas(&self) -> U256 {
        U256::from(BASE_FEE_PER_GAS)
    }

    /// Returns the max_priority_fee_per_gas of Kakarot
    pub fn max_priority_fee_per_gas(&self) -> U128 {
        MAX_PRIORITY_FEE_PER_GAS
    }

    /// Returns the fee history of Kakarot ending at the newest block and going back `block_count`
    pub async fn fee_history(
        &self,
        block_count: U256,
        newest_block: BlockNumberOrTag,
        _reward_percentiles: Option<Vec<f64>>,
    ) -> Result<FeeHistory, EthApiError<P::Error>> {
        let block_count_usize =
            usize::try_from(block_count).map_err(|e| ConversionError::<()>::ValueOutOfRange(e.to_string()))?;

        let base_fee = self.base_fee_per_gas();

        let base_fee_per_gas: Vec<U256> = vec![base_fee; block_count_usize + 1];
        let newest_block = match newest_block {
            BlockNumberOrTag::Number(n) => n,
            // TODO: Add Genesis block number
            BlockNumberOrTag::Earliest => 1_u64,
            _ => self.starknet_provider().block_number().await?,
        };

        let gas_used_ratio: Vec<f64> = vec![0.9; block_count_usize];
        let newest_block = U256::from(newest_block);
        let oldest_block: U256 = if newest_block >= block_count { newest_block - block_count } else { U256::from(0) };

        // TODO: transition `reward` hardcoded default out of nearing-demo-day hack and seeing how to
        // properly source/translate this value
        Ok(FeeHistory { base_fee_per_gas, gas_used_ratio, oldest_block, reward: Some(vec![vec![]]) })
    }

    /// Returns the estimated gas for a transaction
    pub async fn estimate_gas(&self, request: CallRequest, block_id: BlockId) -> Result<U256, EthApiError<P::Error>> {
        match self.network {
            Network::MainnetGateway | Network::Goerli1Gateway | Network::Goerli2Gateway => (),
            _ => {
                return Ok(*ESTIMATE_GAS);
            }
        };

        let chain_id = request.chain_id.unwrap_or(CHAIN_ID.into());

        let from = request.from.ok_or_else(|| EthApiError::MissingParameterError("from for estimate_gas".into()))?;
        let nonce = self.nonce(from, block_id).await?.try_into().map_err(ConversionError::<u64>::from)?;

        let gas_limit = request.gas.unwrap_or(U256::ZERO).try_into().map_err(ConversionError::<u64>::from)?;
        let max_fee_per_gas = request
            .max_fee_per_gas
            .unwrap_or_else(|| U256::from(BASE_FEE_PER_GAS))
            .try_into()
            .map_err(ConversionError::<u128>::from)?;
        let max_priority_fee_per_gas = request
            .max_priority_fee_per_gas
            .unwrap_or_else(|| U256::from(self.max_priority_fee_per_gas()))
            .try_into()
            .map_err(ConversionError::<u128>::from)?;

        let to = request.to.map_or(TransactionKind::Create, TransactionKind::Call);

        let value = request.value.unwrap_or(U256::ZERO).try_into().map_err(ConversionError::<u128>::from)?;

        let data = request.input.data.unwrap_or_default();

        let tx = Transaction::Eip1559(TxEip1559 {
            chain_id: chain_id.low_u64(),
            nonce,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to,
            value,
            access_list: AccessList(vec![]),
            input: data,
        });

        let starknet_block_id: StarknetBlockId = EthBlockId::new(block_id).try_into()?;
        let block_number = self.map_block_id_to_block_number(&starknet_block_id).await?;

        let sender_address = self.compute_starknet_address(&from, &starknet_block_id).await?;

        let mut data = vec![];
        tx.encode_with_signature(&Signature::default(), &mut data, false);
        let data = data.into_iter().map(FieldElement::from).collect();
        let calldata = raw_kakarot_calldata(self.kakarot_address(), data);

        let tx = BroadcastedInvokeTransaction {
            max_fee: FieldElement::ZERO,
            signature: vec![],
            sender_address,
            nonce: nonce.into(),
            calldata,
            is_query: false,
        };

        let fee_estimate = self.simulate_transaction(tx, block_number, true).await?.fee_estimation;
        if fee_estimate.gas_usage < MINIMUM_GAS_FEE {
            return Ok(U256::from(MINIMUM_GAS_FEE));
        }
        Ok(U256::from(fee_estimate.gas_usage))
    }

    /// Returns the gas price on the network
    pub async fn gas_price(&self) -> Result<U256, EthApiError<P::Error>> {
        let call = match self.network {
            Network::MainnetGateway => COUNTER_CALL_MAINNET.clone(),
            Network::Goerli1Gateway => COUNTER_CALL_TESTNET1.clone(),
            Network::Goerli2Gateway => COUNTER_CALL_TESTNET2.clone(),
            _ => return Ok(self.base_fee_per_gas()),
        };

        let raw_calldata: Vec<FieldElement> = call.into();

        let block_id = StarknetBlockId::Tag(BlockTag::Latest);
        let nonce = self.starknet_provider.get_nonce(block_id, *DUMMY_ARGENT_GAS_PRICE_ACCOUNT_ADDRESS).await?;

        let tx = BroadcastedInvokeTransaction {
            max_fee: FieldElement::ZERO,
            signature: vec![],
            sender_address: *DUMMY_ARGENT_GAS_PRICE_ACCOUNT_ADDRESS,
            nonce,
            calldata: raw_calldata,
            is_query: true,
        };

        let block_number = self.starknet_provider().block_number().await?;
        let fee_estimate = self.simulate_transaction(tx, block_number, true).await?.fee_estimation;

        Ok(U256::from(fee_estimate.gas_price))
    }
    /// Returns the Kakarot contract address.
    pub fn kakarot_address(&self) -> FieldElement {
        self.kakarot_contract.address
    }

    /// Returns the Kakarot externally owned account class hash.
    pub fn externally_owned_account_class_hash(&self) -> FieldElement {
        self.kakarot_contract.externally_owned_account_class_hash
    }

    /// Returns the Kakarot contract account class hash.
    pub fn contract_account_class_hash(&self) -> FieldElement {
        self.kakarot_contract.contract_account_class_hash
    }

    /// Returns the Kakarot proxy account class hash.
    pub fn proxy_account_class_hash(&self) -> FieldElement {
        self.kakarot_contract.proxy_account_class_hash
    }

    /// Returns a reference to the Starknet provider.
    pub fn starknet_provider(&self) -> Arc<P> {
        Arc::clone(&self.starknet_provider)
    }

    /// Returns the Starknet block number for a given block id.
    pub async fn map_block_id_to_block_number(&self, block_id: &StarknetBlockId) -> Result<u64, EthApiError<P::Error>> {
        match block_id {
            StarknetBlockId::Number(n) => Ok(*n),
            StarknetBlockId::Tag(_) => Ok(self.starknet_provider().block_number().await?),
            StarknetBlockId::Hash(_) => {
                let block = self.starknet_provider.get_block_with_tx_hashes(block_id).await?;
                match block {
                    MaybePendingBlockWithTxHashes::Block(block_with_tx_hashes) => Ok(block_with_tx_hashes.block_number),
                    _ => Err(ProviderError::StarknetError(StarknetErrorWithMessage {
                        code: MaybeUnknownErrorCode::Known(StarknetError::BlockNotFound),
                        message: "".to_string(),
                    })
                    .into()),
                }
            }
        }
    }

    /// Returns the EVM address associated with a given Starknet address for a given block id
    /// by calling the `get_evm_address` function on the Kakarot contract.
    pub async fn get_evm_address(
        &self,
        starknet_address: &FieldElement,
        starknet_block_id: &StarknetBlockId,
    ) -> Result<Address, EthApiError<P::Error>> {
        let kakarot_account = KakarotAccount::new(*starknet_address, &self.starknet_provider);
        kakarot_account.get_evm_address(starknet_block_id).await
    }

    /// Returns the EVM address associated with a given Starknet address for a given block id
    /// by calling the `compute_starknet_address` function on the Kakarot contract.
    pub async fn compute_starknet_address(
        &self,
        ethereum_address: &Address,
        starknet_block_id: &StarknetBlockId,
    ) -> Result<FieldElement, EthApiError<P::Error>> {
        let ethereum_address: Felt252Wrapper = (*ethereum_address).into();
        let ethereum_address = ethereum_address.into();

        self.kakarot_contract.compute_starknet_address(&ethereum_address, starknet_block_id).await
    }

    /// Returns the Ethereum transactions executed by the Kakarot contract by filtering the provided
    /// Starknet transaction.
    pub async fn filter_starknet_into_eth_txs(
        &self,
        initial_transactions: StarknetTransactions,
        block_hash: Option<H256>,
        block_number: Option<U256>,
    ) -> BlockTransactions {
        let handles = Into::<Vec<TransactionType>>::into(initial_transactions).into_iter().map(|tx| async move {
            let tx = Into::<StarknetTransaction>::into(tx);
            tx.to_eth_transaction(self, block_hash, block_number, None).await
        });
        let transactions_vec = join_all(handles).await.into_iter().filter_map(|transaction| transaction.ok()).collect();
        BlockTransactions::Full(transactions_vec)
    }

    /// Get the Kakarot eth block provided a Starknet block id.
    pub async fn get_eth_block_from_starknet_block(
        &self,
        block_id: StarknetBlockId,
        hydrated_tx: bool,
    ) -> Result<RichBlock, EthApiError<P::Error>> {
        if hydrated_tx {
            let block = self.starknet_provider.get_block_with_txs(block_id).await?;
            let starknet_block = BlockWithTxs::new(block);
            Ok(starknet_block.to_eth_block(self).await)
        } else {
            let block = self.starknet_provider.get_block_with_tx_hashes(block_id).await?;
            let starknet_block = BlockWithTxHashes::new(block);
            Ok(starknet_block.to_eth_block(self).await)
        }
    }

    /// Get the simulation of the BroadcastedInvokeTransactionV1 result
    /// FIXME 306: make simulate_transaction agnostic of the provider (rn only works for
    /// a SequencerGatewayProvider on testnets and mainnet)
    pub async fn simulate_transaction(
        &self,
        request: BroadcastedInvokeTransaction,
        block_number: u64,
        skip_validate: bool,
    ) -> Result<TransactionSimulationInfo, EthApiError<P::Error>> {
        let client = Client::new();

        // build the url for simulate transaction
        let url = self.network.gateway_url();

        // if the url is invalid, return an empty simulation (allows to call simulate_transaction on Kakana,
        // Madara, etc.)
        if url.is_err() {
            let gas_usage = (*ESTIMATE_GAS).try_into().map_err(ConversionError::UintConversionError)?;
            let gas_price: Felt252Wrapper = (*MAX_FEE).into();
            let overall_fee = Felt252Wrapper::from(gas_usage) * gas_price.clone();
            return Ok(TransactionSimulationInfo {
                trace: TransactionTrace {
                    function_invocation: None,
                    fee_transfer_invocation: None,
                    validate_invocation: None,
                    signature: vec![],
                },
                fee_estimation: FeeEstimate {
                    gas_usage,
                    gas_price: gas_price.try_into()?,
                    overall_fee: overall_fee.try_into()?,
                    unit: FeeUnit::Wei,
                },
            });
        }

        let mut url = url
            .unwrap() // safe unwrap because we checked for error above
            .join("simulate_transaction")
            .map_err(|e| EthApiError::FeederGatewayError(format!("gateway url parsing error: {:?}", e)))?;

        // add the block number and skipValidate query params
        url.query_pairs_mut()
            .append_pair("blockNumber", &block_number.to_string())
            .append_pair("skipValidate", &skip_validate.to_string());

        // serialize the request
        let mut request = serde_json::to_value(request)
            .map_err(|e| EthApiError::FeederGatewayError(format!("request serializing error: {:?}", e)))?;
        // BroadcastedInvokeTransactionV1 gets serialized with type="INVOKE" but the simulate endpoint takes
        // type="INVOKE_FUNCTION"
        request["type"] = "INVOKE_FUNCTION".into();

        // post to the gateway
        let response = client
            .post(url)
            .json(&request)
            .send()
            .await
            .map_err(|e| EthApiError::FeederGatewayError(format!("gateway post error: {:?}", e)))?;

        // decode the response to a `TransactionSimulationInfo`
        let resp: TransactionSimulationInfo = response
            .error_for_status()
            .map_err(|e| EthApiError::FeederGatewayError(format!("http error: {:?}", e)))?
            .json()
            .await
            .map_err(|e| {
                EthApiError::FeederGatewayError(format!(
                    "error while decoding response body to TransactionSimulationInfo: {:?}",
                    e
                ))
            })?;

        Ok(resp)
    }

    pub async fn filter_events(&self, filter: EventFilterWithPage) -> Result<Vec<EmittedEvent>, EthApiError<P::Error>> {
        let provider = self.starknet_provider();

        let chunk_size = filter.result_page_request.chunk_size;
        let continuation_token = filter.result_page_request.continuation_token;
        let filter = filter.event_filter;

        let mut result = EventsPage { events: Vec::new(), continuation_token };
        let mut events = vec![];

        loop {
            result = provider.get_events(filter.clone(), result.continuation_token, chunk_size).await?;
            events.append(&mut result.events);

            if result.continuation_token.is_none() {
                break;
            }
        }

        Ok(events)
    }

    /// Given a transaction hash, waits for it to be confirmed on L2
    pub async fn wait_for_confirmation_on_l2(
        &self,
        transaction_hash: FieldElement,
    ) -> Result<(), EthApiError<P::Error>> {
        let waiter = TransactionWaiter::new(self.starknet_provider(), transaction_hash, 1000, 15_000);
        waiter.poll().await?;
        Ok(())
    }
}

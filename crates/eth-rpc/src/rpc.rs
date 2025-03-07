use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use jsonrpsee::core::Error;
use jsonrpsee::{Methods, RpcModule};
use kakarot_rpc_core::client::KakarotClient;
use starknet::providers::Provider;

use crate::api::alchemy_api::AlchemyApiServer;
use crate::api::eth_api::EthApiServer;
use crate::api::net_api::NetApiServer;
use crate::api::web3_api::Web3ApiServer;
use crate::servers::alchemy_rpc::AlchemyRpc;
use crate::servers::eth_rpc::KakarotEthRpc;
use crate::servers::net_rpc::NetRpc;
use crate::servers::web3_rpc::Web3Rpc;

/// Represents RPC modules that are supported by reth
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum KakarotRpcModule {
    Eth,
    Alchemy,
    Web3,
    Net,
}

pub struct KakarotRpcModuleBuilder<P: Provider + Send + Sync + 'static> {
    modules: HashMap<KakarotRpcModule, Methods>,
    _phantom: PhantomData<P>,
}

impl<P: Provider + Send + Sync + 'static> KakarotRpcModuleBuilder<P> {
    pub fn new(kakarot_client: Arc<KakarotClient<P>>) -> Self {
        let eth_rpc_module = KakarotEthRpc::new(kakarot_client.clone()).into_rpc();
        let alchemy_rpc_module = AlchemyRpc::new(kakarot_client).into_rpc();
        let web3_rpc_module = Web3Rpc::default().into_rpc();
        let net_rpc_module = NetRpc::default().into_rpc();

        let mut modules: HashMap<KakarotRpcModule, Methods> = HashMap::new();

        modules.insert(KakarotRpcModule::Eth, eth_rpc_module.into());
        modules.insert(KakarotRpcModule::Alchemy, alchemy_rpc_module.into());
        modules.insert(KakarotRpcModule::Web3, web3_rpc_module.into());
        modules.insert(KakarotRpcModule::Net, net_rpc_module.into());

        Self { modules, _phantom: PhantomData }
    }

    pub fn rpc_module(&self) -> Result<RpcModule<()>, Error> {
        let mut rpc_module = RpcModule::new(());

        for methods in self.modules.values().cloned() {
            rpc_module.merge(methods)?;
        }

        Ok(rpc_module)
    }
}

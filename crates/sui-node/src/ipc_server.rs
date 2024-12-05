// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use fastcrypto::encoding::Base64;
use interprocess::local_socket::{
    tokio::{prelude::*, Stream},
    GenericNamespaced, ListenerOptions,
};
use sui_json_rpc_api::WriteApiServer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, info};

use sui_config::NodeConfig;
use sui_core::transaction_orchestrator::TransactiondOrchestrator;
use sui_core::{authority::AuthorityState, authority_client::NetworkAuthorityClient};
use sui_json_rpc::transaction_execution_api::TransactionExecutionApi;
use sui_json_rpc_api::JsonRpcMetrics;

const REQUEST_MAX_SIZE: usize = 10 * 1024 * 1024;
const IPC_PATH: &str = "/home/ubuntu/sui/sui-mainnet.ipc";

pub struct IpcServer {
    listener: LocalSocketListener,
    api: Arc<TransactionExecutionApi>,
}

impl IpcServer {
    pub async fn new(path: &str, api: Arc<TransactionExecutionApi>) -> Result<Self> {
        let _ = fs::remove_file(path);

        let name = path.to_ns_name::<GenericNamespaced>()?;
        let opts = ListenerOptions::new().name(name);
        let listener = opts.create_tokio()?;

        Ok(Self { listener, api })
    }

    pub async fn run(&self) -> Result<()> {
        loop {
            let conn = match self.listener.accept().await {
                Ok(c) => c,
                Err(error) => {
                    error!(%error, "IpcServer error while accepting connection");
                    continue;
                }
            };
            let api = self.api.clone();

            tokio::spawn(async move {
                if let Err(error) = IpcServer::handle_conn(api, conn).await {
                    error!(%error, "IpcServer error while handling connection");
                }
            });
        }
    }

    async fn handle_conn(api: Arc<TransactionExecutionApi>, conn: Stream) -> Result<()> {
        let (recver, sender) = conn.split();
        let mut recver = BufReader::new(recver);
        let mut sender = sender;
        let mut buffer = String::with_capacity(REQUEST_MAX_SIZE);

        loop {
            buffer.clear();
            tokio::select! {
                recv_len = recver.read_line(&mut buffer) => {
                    if recv_len? == 0 {
                        continue;
                    }
                    debug!(%buffer, "IpcServer received request");

                    let timer = std::time::Instant::now();
                    // {tx_b64};{override_objects_b64}\n
                    let (tx, override_objects) = buffer.trim().split_once(';').context("Invalid request")?;
                    let tx = Base64::try_from(tx.to_string())?;
                    let override_objects = Base64::try_from(override_objects.to_string())?;

                    let resp = api.dry_run_transaction_block_override(tx, override_objects).await.map_err(|e| anyhow!(e))?;
                    let resp_b64 = format!("{}\n", Base64::from_bytes(&bcs::to_bytes(&resp)?).encoded());
                    info!(elapsed = ?timer.elapsed(), %resp_b64, "IpcServer sending response");

                    sender.write_all(resp_b64.as_bytes()).await?;
                }
                else => break,
            }
        }
        Ok(())
    }
}

pub async fn build_ipc_server(
    state: Arc<AuthorityState>,
    transaction_orchestrator: &Option<Arc<TransactiondOrchestrator<NetworkAuthorityClient>>>,
    config: &NodeConfig,
    metrics: Arc<JsonRpcMetrics>,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    // Validators do not expose these APIs
    if config.consensus_config().is_some() {
        return Ok(None);
    }

    let transaction_orchestrator = if let Some(to) = transaction_orchestrator {
        to.clone()
    } else {
        return Ok(None);
    };

    let tx_execution_api =
        TransactionExecutionApi::new(state, transaction_orchestrator.clone(), metrics);
    let api = Arc::new(tx_execution_api);

    let server = IpcServer::new(IPC_PATH, api).await?;

    let handle = tokio::spawn(async move {
        if let Err(error) = server.run().await {
            error!(%error, "IpcServer error while running");
        }
    });
    info!(ipc_path = IPC_PATH, "IpcServer started");

    Ok(Some(handle))
}

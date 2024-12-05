use std::sync::Arc;

use anyhow::{Context, Result};
use fastcrypto::encoding::{Base64, Encoding};
use interprocess::local_socket::{
    tokio::{prelude::*, RecvHalf, SendHalf, Stream},
    GenericNamespaced,
};
use sui_json_rpc_types::DryRunTransactionBlockResponse;
use sui_types::{base_types::ObjectID, object::Object, transaction::TransactionData};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::Mutex,
};

const REQUEST_MAX_SIZE: usize = 10 * 1024 * 1024;

#[derive(Clone)]
pub struct IpcClient {
    path: String,
    inner: Arc<Mutex<Option<IpcClientInner>>>,
}

struct IpcClientInner {
    sender: SendHalf,
    recver: BufReader<RecvHalf>,
    buffer: String,
}

impl IpcClientInner {
    async fn connect(path: &str) -> Result<Self> {
        let name = path.to_ns_name::<GenericNamespaced>()?;
        let conn = Stream::connect(name).await?;
        let (recver, sender) = conn.split();
        let recver = BufReader::new(recver);
        Ok(Self {
            sender,
            recver,
            buffer: String::with_capacity(REQUEST_MAX_SIZE),
        })
    }
}

impl IpcClient {
    pub async fn new(path: &str) -> Result<Self> {
        let inner = IpcClientInner::connect(path).await?;
        Ok(Self {
            path: path.to_string(),
            inner: Arc::new(Mutex::new(Some(inner))),
        })
    }

    pub async fn dry_run_transaction_block_override(
        &self,
        tx: TransactionData,
        override_objects: Vec<(ObjectID, Object)>,
    ) -> Result<DryRunTransactionBlockResponse> {
        self.validate_connection().await?;

        match self.try_dry_run_tx_override(tx, override_objects).await {
            Ok(response) => Ok(response),
            Err(e) => {
                self.disconnect().await;
                Err(e)
            }
        }
    }

    async fn try_dry_run_tx_override(
        &self,
        tx: TransactionData,
        override_objects: Vec<(ObjectID, Object)>,
    ) -> Result<DryRunTransactionBlockResponse> {
        let tx_b64 = Base64::from_bytes(&bcs::to_bytes(&tx)?);
        let override_objects_b64 = Base64::from_bytes(&bcs::to_bytes(&override_objects)?);
        let request = format!("{};{}\n", tx_b64.encoded(), override_objects_b64.encoded());

        let mut inner = self.inner.lock().await;
        let inner = inner.as_mut().context("Connection not established")?;
        let IpcClientInner {
            sender,
            recver,
            buffer,
        } = inner;

        sender.write_all(request.as_bytes()).await?;
        buffer.clear();
        recver.read_line(buffer).await?;

        let response: DryRunTransactionBlockResponse =
            bcs::from_bytes(&Base64::decode(buffer.trim())?)?;
        Ok(response)
    }

    #[inline]
    async fn validate_connection(&self) -> Result<()> {
        let mut inner_lock = self.inner.lock().await;
        if inner_lock.is_none() {
            tracing::warn!("Reconnecting to IPC server");
            let inner = IpcClientInner::connect(&self.path).await?;
            *inner_lock = Some(inner);
        }
        Ok(())
    }

    #[inline]
    async fn disconnect(&self) {
        let mut inner = self.inner.lock().await;
        *inner = None;
    }
}

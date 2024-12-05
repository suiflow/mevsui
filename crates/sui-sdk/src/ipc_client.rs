use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use fastcrypto::encoding::Base64;
use interprocess::local_socket::{
    tokio::{prelude::*, RecvHalf, SendHalf, Stream},
    GenericNamespaced,
};
use sui_json_rpc_types::DryRunTransactionBlockResponse;
use sui_types::{base_types::ObjectID, object::Object, transaction::TransactionData};
use tokio::sync::MutexGuard;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::Mutex,
};

const REQUEST_MAX_SIZE: usize = 10 * 1024 * 1024;

#[derive(Clone)]
pub struct IpcClient {
    path: String,
    conn_pool: Arc<Vec<Mutex<Option<Conn>>>>,
    counter: Arc<AtomicUsize>,
}

struct Conn {
    sender: SendHalf,
    recver: BufReader<RecvHalf>,
    buffer: String,
}

impl Conn {
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
    pub async fn new(path: &str, pool_size: usize) -> Result<Self> {
        let mut conn_pool = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let conn = Conn::connect(path).await?;
            conn_pool.push(Mutex::new(Some(conn)));
        }
        Ok(Self {
            path: path.to_string(),
            conn_pool: Arc::new(conn_pool),
            counter: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub async fn dry_run_transaction_block_override(
        &self,
        tx: TransactionData,
        override_objects: Vec<(ObjectID, Object)>,
    ) -> Result<DryRunTransactionBlockResponse> {
        let tx_b64 = Base64::from_bytes(&bcs::to_bytes(&tx)?);
        let override_objects_b64 = Base64::from_bytes(&bcs::to_bytes(&override_objects)?);
        let request = format!("{};{}\n", tx_b64.encoded(), override_objects_b64.encoded());

        let mut conn_guard = self.get_conn().await?;
        let conn = conn_guard.as_mut().context("Connection not established")?;
        let Conn {
            sender,
            recver,
            buffer,
        } = conn;

        match Self::send_request(sender, recver, buffer, &request).await {
            Ok(response) => Ok(response),
            Err(e) => {
                *conn_guard = None;
                Err(e)
            }
        }
    }

    async fn get_conn(&self) -> Result<MutexGuard<'_, Option<Conn>>> {
        let index = self.counter.fetch_add(1, Ordering::SeqCst) % self.conn_pool.len();
        if index >= self.conn_pool.len() - 1 {
            self.counter.store(0, Ordering::SeqCst);
        }

        let mut conn = self.conn_pool[index].lock().await;
        if conn.is_none() {
            tracing::warn!("Reconnecting to IPC server");
            let new_conn = Conn::connect(&self.path).await?;
            *conn = Some(new_conn);
        }

        Ok(conn)
    }

    #[inline]
    async fn send_request(
        sender: &mut SendHalf,
        recver: &mut BufReader<RecvHalf>,
        buffer: &mut String,
        request: &str,
    ) -> Result<DryRunTransactionBlockResponse> {
        sender.write_all(request.as_bytes()).await?;
        buffer.clear();
        recver.read_line(buffer).await?;
        let response: DryRunTransactionBlockResponse = serde_json::from_str(buffer.trim())?;
        Ok(response)
    }
}

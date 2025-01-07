use std::{fs, sync::Arc};

use anyhow::Result;
use interprocess::local_socket::{
    tokio::{prelude::*, Stream},
    GenericNamespaced, ListenerOptions,
};
use sui_types::effects::TransactionEffects;
use tokio::{io::AsyncWriteExt, sync::Mutex};

pub const TX_SOCKET_PATH: &str = "/tmp/sui_tx.sock";

#[derive(Clone)]
pub struct TxHandler {
    path: String,
    conns: Arc<Mutex<Vec<Stream>>>,
}

impl Default for TxHandler {
    fn default() -> Self {
        Self::new(TX_SOCKET_PATH)
    }
}

impl Drop for TxHandler {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl TxHandler {
    pub fn new(path: &str) -> Self {
        let _ = fs::remove_file(path);

        let name = path
            .to_ns_name::<GenericNamespaced>()
            .expect("Invalid tx socket path");
        let opts = ListenerOptions::new().name(name);
        let listener = opts.create_tokio().expect("Failed to bind tx socket");
        let conns = Arc::new(Mutex::new(vec![]));
        let conns_clone = conns.clone();

        tokio::spawn(async move {
            loop {
                let conn = match listener.accept().await {
                    Ok(c) => c,
                    _err => {
                        continue;
                    }
                };

                conns_clone.lock().await.push(conn);
            }
        });

        Self {
            path: path.to_string(),
            conns,
        }
    }

    pub async fn send_tx_effects(&self, effects: &TransactionEffects) -> Result<()> {
        let effects_bytes = bcs::to_bytes(effects)?;
        let len = (effects_bytes.len() as u32).to_be_bytes();

        let mut conns = self.conns.lock().await;
        let mut active_conns = Vec::new();

        while let Some(mut conn) = conns.pop() {
            let result: Result<()> = async {
                conn.write_all(&len).await?;
                conn.write_all(&effects_bytes).await?;
                Ok(())
            }
            .await;

            if result.is_ok() {
                active_conns.push(conn);
            }
        }

        *conns = active_conns;

        Ok(())
    }
}

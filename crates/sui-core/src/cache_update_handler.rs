use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::{UnixListener, UnixStream};

use sui_types::committee::EpochId;
use tracing::{error, info};

use crate::transaction_outputs::TransactionOutputs;

const SOCKET_PATH: &str = "/tmp/sui_cache_updates.sock";

#[derive(Debug)]
pub struct CacheUpdateHandler {
    socket_path: PathBuf,
    connections: Arc<Mutex<Vec<BufWriter<UnixStream>>>>,
    running: Arc<AtomicBool>,
}

impl CacheUpdateHandler {
    pub fn new() -> Self {
        let socket_path = PathBuf::from(SOCKET_PATH);
        // Remove existing socket file if it exists
        let _ = std::fs::remove_file(&socket_path);

        let listener = tokio::runtime::Handle::current().block_on(async {
            UnixListener::bind(&socket_path).expect("Failed to bind Unix socket")
        });

        let connections = Arc::new(Mutex::new(Vec::new()));
        let running = Arc::new(AtomicBool::new(true));

        let connections_clone = connections.clone();
        let running_clone = running.clone();

        // Spawn connection acceptor task
        tokio::spawn(async move {
            while running_clone.load(Ordering::SeqCst) {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        info!("New client connected to cache update socket");
                        connections_clone.lock().push(BufWriter::new(stream));
                    }
                    Err(e) => {
                        error!("Error accepting connection: {}", e);
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    }
                }
            }
        });

        Self {
            socket_path,
            connections,
            running,
        }
    }

    pub async fn update_cache(&self, epoch_id: EpochId, tx_outputs: Arc<TransactionOutputs>) {
        let serialized = match bincode::serialize(&(epoch_id, tx_outputs)) {
            Ok(serialized) => serialized,
            Err(e) => {
                error!("Error serializing cache update: {}", e);
                return;
            }
        };

        let mut connections = self.connections.lock();
        let len = serialized.len() as u32;
        let len_bytes = len.to_le_bytes();

        // Remove dead connections while sending updates
        connections.retain_mut(|stream| {
            // Try to write length prefix and payload
            let write_fut = async {
                if let Err(e) = stream.write_all(&len_bytes).await {
                    error!("Error writing length prefix to client: {}", e);
                    return false;
                }
                if let Err(e) = stream.write_all(&serialized).await {
                    error!("Error writing to client: {}", e);
                    return false;
                }
                if let Err(e) = stream.flush().await {
                    error!("Error flushing client stream: {}", e);
                    return false;
                }
                true
            };
            tokio::runtime::Handle::current().block_on(write_fut)
        });
    }
}

impl Default for CacheUpdateHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CacheUpdateHandler {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

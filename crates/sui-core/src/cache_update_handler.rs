use parking_lot::Mutex;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sui_types::committee::EpochId;
use tracing::{error, info};

use crate::transaction_outputs::TransactionOutputs;

const SOCKET_PATH: &str = "/tmp/sui_cache_updates.sock";

#[derive(Debug)]
pub struct CacheUpdateHandler {
    socket_path: PathBuf,
    connections: Arc<Mutex<Vec<UnixStream>>>,
    running: Arc<AtomicBool>,
}

impl CacheUpdateHandler {
    pub fn new() -> Self {
        let socket_path = PathBuf::from(SOCKET_PATH);
        // Remove existing socket file if it exists
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path).expect("Failed to bind Unix socket");
        listener.set_nonblocking(true).unwrap();

        let connections = Arc::new(Mutex::new(Vec::new()));
        let running = Arc::new(AtomicBool::new(true));

        let connections_clone = connections.clone();
        let running_clone = running.clone();

        // Spawn connection acceptor task
        tokio::spawn(async move {
            while running_clone.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        info!("New client connected to cache update socket");
                        connections_clone.lock().push(stream);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    }
                    Err(e) => {
                        error!("Error accepting connection: {}", e);
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
        connections.retain(|mut stream| {
            // Try to write length prefix and payload
            if let Err(e) = stream.write_all(&len_bytes) {
                error!("Error writing length prefix to client: {}", e);
                return false;
            }
            if let Err(e) = stream.write_all(&serialized) {
                error!("Error writing to client: {}", e);
                return false;
            }
            true
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

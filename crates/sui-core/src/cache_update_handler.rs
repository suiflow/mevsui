use dashmap::DashSet;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use sui_types::base_types::ObjectID;
use sui_types::object::Object;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use tracing::{error, info};

const SOCKET_PATH: &str = "/tmp/sui_cache_updates.sock";
pub const POOL_RELATED_OBJECTS_PATH: &str = "/home/ubuntu/sui/pool_related_ids.txt";

pub fn pool_related_object_ids() -> DashSet<ObjectID> {
    let content = std::fs::read_to_string(POOL_RELATED_OBJECTS_PATH)
        .unwrap_or_else(|_| panic!("Failed to open: {}", POOL_RELATED_OBJECTS_PATH));

    let set = DashSet::new();
    content
        .trim()
        .split('\n')
        .map(|line| line.parse().expect("Failed to parse pool_related_ids"))
        .for_each(|id| {
            set.insert(id);
        });
    set
}

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

        let connections = Arc::new(Mutex::new(Vec::new()));
        let running = Arc::new(AtomicBool::new(true));

        let connections_clone = Arc::clone(&connections);
        let running_clone = Arc::clone(&running);

        // Spawn connection acceptor task
        tokio::spawn(async move {
            while running_clone.load(Ordering::SeqCst) {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        info!("New client connected to cache update socket");
                        let mut connections = connections_clone.lock().await;
                        connections.push(stream);
                    }
                    Err(e) => {
                        error!("Error accepting connection: {}", e);
                        // Optionally, decide whether to break the loop or continue
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

    pub async fn notify_written(&self, objects: Vec<(ObjectID, Object)>) {
        let serialized = bcs::to_bytes(&objects).expect("serialization error");
        let len = serialized.len() as u32;
        let len_bytes = len.to_le_bytes();

        let mut connections = self.connections.lock().await;

        // Iterate over connections and remove any that fail
        let mut i = 0;
        while i < connections.len() {
            let stream = &mut connections[i];

            // Attempt to write to the stream
            let result = async {
                if let Err(e) = stream.write_all(&len_bytes).await {
                    error!("Error writing length prefix to client: {}", e);
                    Err(e)
                } else if let Err(e) = stream.write_all(&serialized).await {
                    error!("Error writing to client: {}", e);
                    Err(e)
                } else {
                    Ok(())
                }
            }
            .await;

            // Remove connection if there was an error
            if result.is_err() {
                connections.remove(i);
            } else {
                i += 1;
            }
        }
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

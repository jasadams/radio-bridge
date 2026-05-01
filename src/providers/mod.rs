pub mod abc;

use std::sync::Arc;
use tokio::sync::RwLock;

pub trait MetadataProvider: Send + Sync + 'static {
    fn station_id_for(&self, guide_name: &str) -> Option<String>;
    fn logo_for(&self, guide_name: &str) -> Option<&'static [u8]>;
    fn start_poller(
        &self,
        station_id: &str,
        artwork_target: Arc<RwLock<Option<String>>>,
        track_target: Arc<RwLock<(String, String)>>,
    ) -> tokio::task::JoinHandle<()>;
}

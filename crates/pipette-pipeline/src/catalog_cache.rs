use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use pipette_store::CatalogStore;

use pipette_core::benchmark::Benchmark;
use pipette_plan_types::BenchmarkId;

struct CachedState {
    catalog: Arc<HashMap<BenchmarkId, Benchmark>>,
    loaded_at: Instant,
}

pub struct CatalogCache {
    state: RwLock<CachedState>,
    catalog_store: Arc<dyn CatalogStore>,
    ttl: Duration,
}

impl CatalogCache {
    pub fn new(
        catalog_store: Arc<dyn CatalogStore>,
        initial_catalog: HashMap<BenchmarkId, Benchmark>,
        ttl: Duration,
    ) -> Self {
        Self {
            state: RwLock::new(CachedState {
                catalog: Arc::new(initial_catalog),
                loaded_at: Instant::now(),
            }),
            catalog_store,
            ttl,
        }
    }

    pub async fn get(&self) -> anyhow::Result<Arc<HashMap<BenchmarkId, Benchmark>>> {
        {
            let state = self.state.read().await;
            if state.loaded_at.elapsed() < self.ttl {
                return Ok(state.catalog.clone());
            }
        }

        let mut state = self.state.write().await;
        // Double-check: another request may have reloaded while we waited
        if state.loaded_at.elapsed() < self.ttl {
            return Ok(state.catalog.clone());
        }

        let catalog = self.catalog_store.load_catalog().await?;
        tracing::debug!(count = catalog.len(), "reloaded benchmark catalog");
        let catalog = Arc::new(catalog);
        state.catalog = catalog.clone();
        state.loaded_at = Instant::now();
        Ok(catalog)
    }
}

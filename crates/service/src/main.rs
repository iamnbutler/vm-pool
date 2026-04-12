//! vm-pool service binary entry point.

use anyhow::Result;
use vm_pool_service::ServiceConfig;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
        )
        .init();

    let config = ServiceConfig::default();
    let service = vm_pool_service::Service::new(config).await?;
    service.run().await
}

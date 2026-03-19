use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    test_runner::run().await
}

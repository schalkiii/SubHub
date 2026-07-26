#[tokio::main]
async fn main() {
    subhub_server::run_server().await;
}

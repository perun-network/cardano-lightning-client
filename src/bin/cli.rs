use cardano_lightning_client::{CardanoAgent, CardanoConfig};

#[tokio::main]
async fn main() {
    let script_address = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("SCRIPT_ADDRESS").ok());

    let Some(script_address) = script_address else {
        eprintln!("Usage: cardano-lightning-cli <script_address>");
        eprintln!("   or: SCRIPT_ADDRESS=addr_test1... cardano-lightning-cli");
        std::process::exit(1);
    };

    let config = CardanoConfig {
        blockfrost_url: std::env::var("BLOCKFROST_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:8080/api/v1/".into()),
        blockfrost_key: std::env::var("BLOCKFROST_PROJECT_ID")
            .unwrap_or_else(|_| "local".into()),
        script_address,
    };

    let agent = CardanoAgent::new(config.clone());

    println!("Querying contract state...");
    println!("  API:     {}", config.blockfrost_url);
    println!("  Address: {}", config.script_address);
    println!();

    match agent.query_state().await {
        Ok(state) => print!("{}", state),
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

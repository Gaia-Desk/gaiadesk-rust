//! Mint a scoped agent token (as the desk's owner), list, and revoke it.
//!
//! GAIADESK_API_KEY=ak_… cargo run --example tokens -- 123456789

use std::time::Duration;

use gaiadesk::{scopes, Client, MintSpec};

#[tokio::main]
async fn main() -> gaiadesk::Result<()> {
    let client = Client::new(std::env::var("GAIADESK_API_KEY").unwrap_or_default())?;
    let id = std::env::args().nth(1).expect("a desk id");
    let spec = MintSpec::new("ci-bot").scopes([scopes::EXEC, scopes::JOBS]).expires_in(Duration::from_secs(3600));
    let minted = client.create_token([&id], &spec).await?;
    for t in &minted.tokens {
        println!("{}: {} (secret shown once)", t.desk, t.token.id);
    }
    let desk = client.desk(&id);
    for t in desk.tokens().await? {
        println!("{} {} {:?}", t.id, t.label, t.scopes);
    }
    let revoked = desk.revoke_token("ci-bot").await?;
    println!("revoked {}", revoked.revoked);
    Ok(())
}

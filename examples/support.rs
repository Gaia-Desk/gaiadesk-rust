//! Create a support session for the embed SDK and list open ones.
//!
//! GAIADESK_API_KEY=ak_… cargo run --example support

use gaiadesk::{Client, SupportMode, SupportSessionCreate};

#[tokio::main]
async fn main() -> gaiadesk::Result<()> {
    let client = Client::new(std::env::var("GAIADESK_API_KEY").unwrap_or_default())?;
    let s = client
        .create_support_session(&SupportSessionCreate::new().mode(SupportMode::Cobrowse).customer("name", "Ada").expires_in(1800))
        .await?;
    println!("session {}: join code {}, hand the page {}", s.session.id, s.session.join_code, s.embed_token);
    for s in client.support_sessions(false, Some(20)).await? {
        println!("{} {:?} present={}", s.id, s.state, s.customer_present);
    }
    Ok(())
}

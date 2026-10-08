//! A desk's LAN gateway, pinned to its certificate's fingerprint.
//!
//! GAIADESK_LAN_FP=ab:cd:… GAIADESK_DESK_TOKEN=gdagt_… cargo run --example lan -- 123456789

use gaiadesk::Client;

#[tokio::main]
async fn main() -> gaiadesk::Result<()> {
    let id = std::env::args().nth(1).expect("a desk id");
    let client = Client::builder()
        .lan(format!("https://gaiadesk-{id}.local:7443/v1"), std::env::var("GAIADESK_LAN_FP").unwrap_or_default())
        .desk_token(std::env::var("GAIADESK_DESK_TOKEN").unwrap_or_default())
        .build()?;
    let s = client.desk(&id).stats().await?;
    println!("{} ({}) cpu {:.0}%", s.hostname, s.os, s.cpu_percent);
    Ok(())
}

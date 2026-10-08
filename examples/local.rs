//! Code running ON a desk: its own local API (socket or pipe, admin token from its file).
//!
//! cargo run --example local

use gaiadesk::{Client, ExecSpec};

#[tokio::main]
async fn main() -> gaiadesk::Result<()> {
    let client = Client::local()?;
    let me = client.desks().await?.devices.into_iter().next().expect("this desk");
    let r = client.desk(&me.desk_id).exec(ExecSpec::command("whoami")).await?;
    println!("{} runs as {}", me.desk_id, r.stdout.trim());
    Ok(())
}

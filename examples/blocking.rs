//! The blocking client.
//!
//! GAIADESK_API_KEY=ak_… GAIADESK_DESK_TOKEN=gdagt_… cargo run --features blocking --example blocking -- 123456789

use gaiadesk::{blocking, ExecSpec};

fn main() -> gaiadesk::Result<()> {
    let client = blocking::Client::new(
        gaiadesk::Client::builder()
            .api_key(std::env::var("GAIADESK_API_KEY").unwrap_or_default())
            .desk_token(std::env::var("GAIADESK_DESK_TOKEN").unwrap_or_default())
            .build()?,
    )?;
    let desk = client.desk(std::env::args().nth(1).expect("a desk id"));
    for ev in desk.exec_stream(ExecSpec::command("ls"))? {
        println!("{:?}", ev?);
    }
    Ok(())
}

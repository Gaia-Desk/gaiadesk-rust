//! List desks, run a command, stream another.
//!
//! GAIADESK_API_KEY=ak_… GAIADESK_DESK_TOKEN=gdagt_… cargo run --example quickstart -- 123456789

use futures_util::StreamExt;
use gaiadesk::{Client, ExecEvent, ExecSpec};

#[tokio::main]
async fn main() -> gaiadesk::Result<()> {
    let client = Client::builder()
        .api_key(std::env::var("GAIADESK_API_KEY").unwrap_or_default())
        .desk_token(std::env::var("GAIADESK_DESK_TOKEN").unwrap_or_default())
        .build()?;

    for d in client.desks().await?.devices {
        println!("{} {:<20} online={:?}", d.desk_id, d.name.unwrap_or_default(), d.online);
    }

    let Some(id) = std::env::args().nth(1) else { return Ok(()) };
    let desk = client.desk(id);

    let r = desk.exec(ExecSpec::command("uname -a")).await?;
    println!("exit {}: {}", r.exit, r.stdout.trim());

    let mut s = desk.exec_stream(ExecSpec::command("for i in 1 2 3; do echo $i; sleep 1; done"))?;
    while let Some(ev) = s.next().await {
        match ev? {
            ExecEvent::Stdout(t) => print!("{t}"),
            ExecEvent::Stderr(t) => eprint!("{t}"),
            ExecEvent::Exit(x) => println!("exited {}", x.exit),
            _ => {}
        }
    }
    Ok(())
}

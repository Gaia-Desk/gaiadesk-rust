//! Subscribe a webhook, and verify a delivery.
//!
//! GAIADESK_API_KEY=ak_… cargo run --example webhooks -- https://example.com/hooks/gaiadesk

use gaiadesk::{webhook, Client, WebhookCreate, WebhookEventType};

#[tokio::main]
async fn main() -> gaiadesk::Result<()> {
    let client = Client::new(std::env::var("GAIADESK_API_KEY").unwrap_or_default())?;
    let url = std::env::args().nth(1).expect("an https:// URL");
    let created = client
        .create_webhook(&WebhookCreate::new(url, [WebhookEventType::DeskOnline, WebhookEventType::DeskOffline]).description("ops"))
        .await?;
    println!("subscribed {}; keep the secret: {}", created.webhook.id, created.secret);

    // In your endpoint: verify the raw body against the GaiaDesk-Signature header.
    let body = br#"{"id":"evt_1","type":"desk.online","created":1,"data":{}}"#;
    let header = webhook::sign(&created.secret, now(), body);
    match webhook::verify(&created.secret, &header, body) {
        Ok(event) => println!("verified {} ({:?})", event.id, event.kind),
        Err(e) => println!("rejected: {e}"),
    }
    Ok(())
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

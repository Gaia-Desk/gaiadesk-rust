//! The blocking client, from a plain thread (the mock runs on its own runtime).
#![cfg(feature = "blocking")]

mod common;

use common::MockDesk;
use gaiadesk::{blocking, Client, E2eMode, ExecEvent, ExecSpec};

#[test]
fn blocking_calls_and_an_iterator_stream() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let m = rt.block_on(common::start(vec![("123456789", MockDesk::keyed([5; 32]))]));
    let c = blocking::Client::new(
        Client::builder().api_key("ak_t").desk_token("gdagt_t").base_url(&m.url).e2e(E2eMode::Require).build().unwrap(),
    )
    .unwrap();
    assert_eq!(c.desks().unwrap().devices.len(), 1);
    let d = c.desk("123456789");
    assert_eq!(d.exec(ExecSpec::command("hi")).unwrap().exit, 0);
    let events: Vec<_> = d.exec_stream(ExecSpec::command("s")).unwrap().collect::<Result<_, _>>().unwrap();
    assert!(matches!(events.last(), Some(ExecEvent::Exit(_))));
    d.upload_bytes(b"x".to_vec(), "f").unwrap();
    assert_eq!(d.download_bytes("f").unwrap(), b"x");
    let logs: Vec<_> = d.follow_job_logs("j", None).unwrap().collect::<Result<_, _>>().unwrap();
    assert!(matches!(logs.last(), Some(gaiadesk::LogEvent::End(_))));
    assert_eq!(m.st().sealed.len(), 5);
}

//! The local transport over a Unix socket, and the LAN transport over TLS
//! pinned to the gateway's certificate.

#![cfg(any(feature = "local", feature = "lan"))]

mod common;

use common::MockDesk;
use gaiadesk::*;

const DESK: &str = "123456789";

#[cfg(all(unix, feature = "local"))]
mod local {
    use super::*;

    fn dir(tag: &str) -> std::path::PathBuf {
        // Short: a Unix socket path is limited to ~100 bytes.
        let d = std::path::PathBuf::from(format!("/tmp/gd-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn exec_with_the_admin_token_read_from_its_file_as_bearer() {
        let d = dir("a");
        let sock = d.join("api.sock");
        let m = common::start_unix(vec![(DESK, MockDesk::plain())], &sock).await;
        std::fs::write(d.join("api-token"), "gdlocal_test\n").unwrap();
        let c = Client::builder().local().socket_path(&sock).admin_token_file(d.join("api-token")).build().unwrap();
        assert_eq!(c.transport(), Transport::Local);
        let r = c.desk(DESK).exec(ExecSpec::command("hi")).await.unwrap();
        assert_eq!(r.exit, 0);
        let rec = m.last();
        assert_eq!(rec.headers["authorization"], "Bearer gdlocal_test");
        assert_eq!(rec.headers["host"], "localhost");
        assert_eq!(c.desks().await.unwrap().devices[0].desk_id, DESK);
        // Streams, files and a held wait over the socket.
        assert_eq!(c.desk(DESK).exec_stream(ExecSpec::command("s")).unwrap().collect_output().await.unwrap().stdout, "ran: s é\n");
        c.desk(DESK).upload_bytes(b"abc".to_vec(), "f").await.unwrap();
        assert_eq!(c.desk(DESK).download_bytes("f").await.unwrap(), b"abc");
        assert_eq!(c.desk(DESK).wait_job("held", None).await.unwrap().job.state, "exited");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn an_agent_token_is_sent_instead_with_no_authorization() {
        let d = dir("b");
        let sock = d.join("api.sock");
        let m = common::start_unix(vec![(DESK, MockDesk::plain())], &sock).await;
        let c = Client::builder().local().socket_path(&sock).desk_token("gdagt_x").build().unwrap();
        c.desk(DESK).stats().await.unwrap();
        let rec = m.last();
        assert_eq!(rec.headers["x-gaiadesk-desk-token"], "gdagt_x");
        assert!(!rec.headers.contains_key("authorization"));
        // A wrong admin token: the 401 envelope, typed.
        let c = Client::builder().local().socket_path(&sock).admin_token("gdlocal_wrong").build().unwrap();
        let e = c.desk(DESK).stats().await.unwrap_err();
        assert_eq!((e.status(), e.reason()), (Some(401), Some("unauthenticated")));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn a_missing_socket_or_token_file_says_how_to_turn_the_api_on_and_sends_nothing() {
        let d = dir("c");
        let c = Client::builder()
            .local()
            .socket_path(d.join("api.sock"))
            .admin_token("gdlocal_test")
            .retry(RetryPolicy::none())
            .build()
            .unwrap();
        let e = c.desks().await.unwrap_err();
        assert!(matches!(e, Error::Unreachable(_)), "{e:?}");
        assert_eq!(e.reason(), Some(reasons::LOCAL_API_UNAVAILABLE));
        assert!(e.message().contains("Local API on"));
        let c = Client::builder().local().socket_path(d.join("api.sock")).admin_token_file(d.join("no-token")).build().unwrap();
        let e = c.desks().await.unwrap_err();
        assert_eq!(e.reason(), Some(reasons::LOCAL_API_UNAVAILABLE));
        assert!(e.message().contains("no local admin token"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}

#[cfg(feature = "lan")]
mod lan {
    use super::*;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn the_pinned_fingerprint_goes_through_with_the_agent_token_and_no_authorization() {
        let (m, fp) = common::start_tls(vec![(DESK, MockDesk::plain())]).await;
        let c = Client::builder().lan(&m.url, fp.to_uppercase().replace(':', "")).desk_token("gdagt_lan").build().unwrap();
        assert_eq!(c.transport(), Transport::Lan);
        assert_eq!(c.desk(DESK).stats().await.unwrap().hostname, "studio");
        let rec = m.last();
        assert_eq!(rec.headers["x-gaiadesk-desk-token"], "gdagt_lan");
        assert!(!rec.headers.contains_key("authorization"));
        // Streams and files over the pinned connection.
        let mut s = c.desk(DESK).follow_job_logs("j", None).unwrap();
        let mut n = 0;
        while let Some(ev) = s.next().await {
            ev.unwrap();
            n += 1;
        }
        assert!(n >= 2);
        c.desk(DESK).upload_bytes(b"lan".to_vec(), "x").await.unwrap();
        assert_eq!(c.desk(DESK).download_bytes("x").await.unwrap(), b"lan");
    }

    #[tokio::test]
    async fn a_wrong_fingerprint_is_a_mismatch_and_the_gateway_never_receives_a_request() {
        let (m, fp) = common::start_tls(vec![(DESK, MockDesk::plain())]).await;
        let wrong = vec!["00"; 32].join(":");
        let c = Client::builder().lan(&m.url, &wrong).desk_token("gdagt_lan").retry(RetryPolicy::none()).build().unwrap();
        match c.desk(DESK).stats().await.unwrap_err() {
            Error::FingerprintMismatch { expected, actual, details } => {
                assert_eq!(expected, wrong);
                assert_eq!(actual, fp);
                assert_eq!(details.reason.as_deref(), Some(reasons::FINGERPRINT_MISMATCH));
            }
            e => panic!("{e:?}"),
        }
        assert!(m.requests().is_empty());
    }

    #[tokio::test]
    async fn options_are_checked_and_no_agent_token_sends_nothing() {
        let (m, fp) = common::start_tls(vec![(DESK, MockDesk::plain())]).await;
        let c = Client::builder().lan(&m.url, &fp).build().unwrap();
        assert!(matches!(c.desk(DESK).stats().await, Err(Error::Usage(_))));
        assert!(m.requests().is_empty());
        assert!(c.with_desk_token("gdagt_call").desk(DESK).stats().await.is_ok());
        assert!(Client::builder().lan("http://x:7443/v1", &fp).build().is_err());
        assert!(Client::builder().lan(&m.url, "ab:cd").build().is_err());
        assert!(matches!(c.audit(&AuditQuery::new()).await, Err(Error::Usage(_))));
    }
}

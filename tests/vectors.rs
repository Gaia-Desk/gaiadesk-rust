//! GaiaDesk's fixed end-to-end vectors (protocol/src/e2e/vectors.json), byte
//! for byte, and the crypto's refusals: tampering, reordering, another desk
//! or operation.

mod common;

use gaiadesk::e2e::{associated_data, b64, seal_request_with, x25519_public, CallerSeal, DeskEvent, SealedFrame, SealedRequest, Use};
use serde_json::{json, Value};

fn vectors() -> Value {
    serde_json::from_str(include_str!("vectors.json")).unwrap()
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn arr<const N: usize>(b: Vec<u8>) -> [u8; N] {
    b.try_into().unwrap()
}

/// The vectors' sealed request and the caller's seal.
fn sealed() -> (Value, SealedRequest, CallerSeal) {
    let v = vectors();
    let desk_pub = arr::<32>(b64::decode(v["desk_pub"].as_str().unwrap()).unwrap());
    let eph = arr::<32>(hex(v["eph_secret_hex"].as_str().unwrap()));
    let nonce = arr::<24>(b64::decode(v["request"]["nonce"].as_str().unwrap()).unwrap());
    let (req, seal) = seal_request_with(
        eph,
        nonce,
        &desk_pub,
        v["desk_id"].as_str().unwrap(),
        v["op"].as_str().unwrap(),
        v["request_plaintext"].as_str().unwrap().as_bytes(),
    )
    .unwrap();
    (v, req, seal)
}

#[test]
fn the_desk_key_the_sealed_request_and_its_header_byte_for_byte() {
    let (v, req, _) = sealed();
    let desk_secret = arr::<32>(hex(v["desk_secret_hex"].as_str().unwrap()));
    assert_eq!(b64::url(&x25519_public(desk_secret)), v["desk_pub"]);
    assert_eq!(serde_json::to_value(&req).unwrap(), v["request"]);
    assert_eq!(req.to_header(), v["request_header"]);
    assert_eq!(SealedRequest::from_header(v["request_header"].as_str().unwrap()).unwrap(), req);
}

#[test]
fn associated_data_byte_for_byte() {
    let v = vectors();
    let (desk, op) = (v["desk_id"].as_str().unwrap(), v["op"].as_str().unwrap());
    assert_eq!(associated_data(Use::Request, desk, op, None), hex(v["aad_request_hex"].as_str().unwrap()));
    assert_eq!(associated_data(Use::Event, desk, op, Some(1)), hex(v["aad_event_1_hex"].as_str().unwrap()));
}

#[test]
fn the_events_open_and_the_input_frames_seal_to_the_same_ciphertext() {
    let (v, _, mut seal) = sealed();
    for e in v["events"].as_array().unwrap() {
        let f: SealedFrame = serde_json::from_value(json!({"seq": e["seq"], "nonce": e["nonce"], "ciphertext": e["ciphertext"]})).unwrap();
        assert_eq!(seal.open_event(&f).unwrap(), e["plaintext"].as_str().unwrap().as_bytes());
    }
    let (_, _, mut seal) = sealed();
    for i in v["inputs"].as_array().unwrap() {
        let nonce = arr::<24>(b64::decode(i["nonce"].as_str().unwrap()).unwrap());
        let f = seal.seal_input_with(nonce, i["last"].as_bool().unwrap(), i["data"].as_str().unwrap().as_bytes());
        assert_eq!(f.seq, i["seq"].as_u64().unwrap());
        assert_eq!(f.nonce, i["nonce"].as_str().unwrap());
        assert_eq!(f.ciphertext, i["ciphertext"].as_str().unwrap());
    }
}

#[test]
fn the_desk_side_opens_the_request_and_the_input_frames() {
    let (v, req, _) = sealed();
    let secret = arr::<32>(hex(v["desk_secret_hex"].as_str().unwrap()));
    let (plain, mut desk) = common::desk::open_request(secret, v["desk_id"].as_str().unwrap(), "exec", &req).unwrap();
    assert_eq!(plain, v["request_plaintext"].as_str().unwrap().as_bytes());
    for i in v["inputs"].as_array().unwrap() {
        let f: SealedFrame = serde_json::from_value(json!({"seq": i["seq"], "nonce": i["nonce"], "ciphertext": i["ciphertext"]})).unwrap();
        let (last, data) = desk.open_input(&f).unwrap();
        assert_eq!(last, i["last"].as_bool().unwrap());
        assert_eq!(data, i["data"].as_str().unwrap().as_bytes());
    }
}

#[test]
fn opened_events_read_as_desk_events() {
    let (v, _, mut seal) = sealed();
    let e = &v["events"][0];
    let ev = seal.open_desk_event(&json!({"seq": e["seq"], "nonce": e["nonce"], "ciphertext": e["ciphertext"]})).unwrap();
    assert_eq!(ev, DeskEvent::Stdout { data: "dmVjdG9yCg==".into() });
    let e = &v["events"][1];
    let ev = seal.open_desk_event(&json!({"seq": e["seq"], "nonce": e["nonce"], "ciphertext": e["ciphertext"]})).unwrap();
    assert_eq!(ev, DeskEvent::Exit { result: json!({"exit": 0}) });
}

#[test]
fn round_trip_with_fresh_keys_through_the_desk_side() {
    let secret = [9u8; 32];
    let request = json!({"op": "exec", "spec": {"command": "x"}});
    let (req, mut seal) = gaiadesk::e2e::seal_request(&x25519_public(secret), "123456789", "exec", &request).unwrap();
    let (plain, mut desk) = common::desk::open_request(secret, "123456789", "exec", &req).unwrap();
    let inner: Value = serde_json::from_slice(&plain).unwrap();
    assert_eq!(inner["v"], 1);
    assert_eq!(inner["request"], request);
    let up = seal.seal_input(true, b"bytes");
    assert_eq!(desk.open_input(&up).unwrap(), (true, b"bytes".to_vec()));
    for n in 0..3 {
        let f = desk.seal_event(&json!({"event": "stdout", "data": b64::standard(format!("{n}").as_bytes())}));
        assert!(matches!(seal.open_desk_event(&serde_json::to_value(f).unwrap()).unwrap(), DeskEvent::Stdout { .. }));
    }
}

#[test]
fn tampering_reordering_and_the_wrong_desk_or_op_do_not_open() {
    let secret = [9u8; 32];
    let pubk = x25519_public(secret);
    let (req, _) = gaiadesk::e2e::seal_request(&pubk, "123456789", "exec", &json!({"op": "exec"})).unwrap();
    assert!(common::desk::open_request(secret, "987654321", "exec", &req).is_none(), "another desk");
    assert!(common::desk::open_request(secret, "123456789", "stats", &req).is_none(), "another operation");
    let mut flipped = req.clone();
    let mut c = b64::decode(&flipped.ciphertext).unwrap();
    c[3] ^= 1;
    flipped.ciphertext = b64::url(&c);
    assert!(common::desk::open_request(secret, "123456789", "exec", &flipped).is_none(), "a flipped bit");
    assert!(common::desk::open_request([8u8; 32], "123456789", "exec", &req).is_none(), "another key");

    let (req, mut seal) = gaiadesk::e2e::seal_request(&pubk, "123456789", "exec", &json!({"op": "exec"})).unwrap();
    let (_, mut desk) = common::desk::open_request(secret, "123456789", "exec", &req).unwrap();
    let f0 = desk.seal_event(&json!({"event": "stdout", "data": ""}));
    let f1 = desk.seal_event(&json!({"event": "exit", "result": {}}));
    assert_eq!(seal.open_event(&f1).unwrap_err().reason(), "e2e_decrypt_failed", "skipped");
    let mut renumbered = f1.clone();
    renumbered.seq = 0;
    assert!(seal.open_event(&renumbered).is_err(), "renumbered");
    seal.open_event(&f0).unwrap();
    assert!(seal.open_event(&f0).is_err(), "replayed");
    seal.open_event(&f1).unwrap();
    let bad = SealedFrame { seq: 2, nonce: "short".into(), ciphertext: "AAAA".into() };
    assert_eq!(seal.open_event(&bad).unwrap_err().reason(), "e2e_malformed");
}

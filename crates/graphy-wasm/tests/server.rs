//! The StoreServer core, exercised natively: in-process dispatch against
//! graphy-server's protocol router (the wasm host runs the identical core;
//! only the Promise boundary and wall-clock injection differ).

use graphy_wasm::StoreServer;

fn header<'a>(reply: &'a graphy_wasm::Reply, name: &str) -> Option<&'a str> {
    reply
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

async fn seed(server: &StoreServer, trig: &str) {
    let reply = server
        .handle(
            "PUT",
            "/graphs",
            &[("content-type".to_string(), "application/trig".to_string())],
            trig.as_bytes().to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(
        reply.status,
        204,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
}

#[tokio::test]
async fn protocol_round_trip() {
    let server = StoreServer::new(None, false, false).unwrap();

    // health + service description
    let reply = server
        .handle("GET", "/health", &[], Vec::new())
        .await
        .unwrap();
    assert_eq!(reply.status, 200);
    let reply = server
        .handle("GET", "/sparql/service", &[], Vec::new())
        .await
        .unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(header(&reply, "content-type"), Some("text/turtle"));

    // dataset-level GSP PUT (replace) seeds the store
    seed(
        &server,
        "@prefix ex: <http://e/> . ex:s ex:p 1, 2 . ex:g { ex:s ex:q ex:o . }",
    )
    .await;
    assert_eq!(server.size().unwrap(), 3);

    // SPARQL Protocol GET → results JSON with an ETag
    let reply = server
        .handle("GET", "/sparql?query=ASK%7B%7D", &[], Vec::new())
        .await
        .unwrap();
    assert_eq!(reply.status, 200);
    assert!(header(&reply, "etag").is_some());
    let v: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(v["boolean"], serde_json::json!(true));

    // direct POST query
    let reply = server
        .handle(
            "POST",
            "/sparql",
            &[(
                "content-type".to_string(),
                "application/sparql-query".to_string(),
            )],
            b"PREFIX ex: <http://e/> SELECT ?o { ex:s ex:p ?o } ORDER BY ?o".to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(reply.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(v["results"]["bindings"].as_array().unwrap().len(), 2);

    // update → 204, then GSP GET of the named graph as turtle
    let reply = server
        .handle(
            "POST",
            "/sparql",
            &[(
                "content-type".to_string(),
                "application/sparql-update".to_string(),
            )],
            b"PREFIX ex: <http://e/> INSERT DATA { GRAPH ex:g { ex:s ex:q ex:o2 } }".to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(
        reply.status,
        204,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    let reply = server
        .handle(
            "GET",
            "/graphs?graph=http%3A%2F%2Fe%2Fg",
            &[("accept".to_string(), "text/turtle".to_string())],
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(reply.status, 200);
    let turtle = String::from_utf8(reply.body).unwrap();
    assert!(turtle.contains("ex:o2"), "{turtle}");
}

#[tokio::test]
async fn read_only_rejects_writes() {
    let server = StoreServer::new(None, true, false).unwrap();
    let reply = server
        .handle(
            "POST",
            "/sparql",
            &[(
                "content-type".to_string(),
                "application/sparql-update".to_string(),
            )],
            b"INSERT DATA { <http://e/s> <http://e/p> 1 }".to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(reply.status, 403);
    let reply = server
        .handle(
            "PUT",
            "/graphs?default",
            &[("content-type".to_string(), "text/turtle".to_string())],
            b"<http://e/s> <http://e/p> 1 .".to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(reply.status, 403);
}

#[tokio::test]
async fn log_capture_restores() {
    let server = StoreServer::new(None, false, false).unwrap();
    seed(&server, "@prefix ex: <http://e/> . ex:s ex:p 1 .").await;
    let log = server.drain_log();
    assert!(!log.is_empty());

    // restore from the drained log
    let restored = StoreServer::new(Some(&log), false, false).unwrap();
    assert_eq!(restored.size().unwrap(), 1);
    let reply = restored
        .handle(
            "GET",
            "/sparql?query=ASK%20%7B%20%3Chttp%3A%2F%2Fe%2Fs%3E%20%3Fp%20%3Fo%20%7D",
            &[],
            Vec::new(),
        )
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(v["boolean"], serde_json::json!(true));

    // pack on the restored server, then boot a third from the packed image
    let packed = restored.pack_log().unwrap();
    let repacked = StoreServer::new(Some(&packed), false, false).unwrap();
    assert_eq!(repacked.size().unwrap(), 1);
}

#[tokio::test]
async fn strict_log_rejects_foreign_bytes() {
    // Torn-tail truncation is the *lenient* contract: garbage restores as an
    // empty store. Strict mode is for imported images, where that would
    // silently discard the user's file.
    let garbage = b"not a wal image";
    let lenient = StoreServer::new(Some(garbage), false, false).unwrap();
    assert_eq!(lenient.size().unwrap(), 0);
    assert!(StoreServer::new(Some(garbage), false, true).is_err());

    // A valid image passes strict; the same image with a torn tail fails it.
    let server = StoreServer::new(None, false, false).unwrap();
    seed(&server, "@prefix ex: <http://e/> . ex:s ex:p 1 .").await;
    let packed = server.pack_log().unwrap();
    assert_eq!(
        StoreServer::new(Some(&packed), false, true)
            .unwrap()
            .size()
            .unwrap(),
        1
    );
    let torn = &packed[..packed.len() - 1];
    assert!(StoreServer::new(Some(torn), false, true).is_err());
    assert_eq!(
        StoreServer::new(Some(torn), false, false)
            .unwrap()
            .size()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn lenient_restore_reports_the_valid_prefix() {
    // Two committed records, then a torn tail: the lenient restore replays
    // the first record and reports where the valid prefix ends, so a host
    // can truncate its durable log there before appending.
    let server = StoreServer::new(None, false, false).unwrap();
    seed(&server, "@prefix ex: <http://e/> . ex:s ex:p 1 .").await;
    let first = server.drain_log();
    seed(&server, "@prefix ex: <http://e/> . ex:s ex:p 1, 2 .").await;
    let second = server.drain_log();
    let mut log = [first.as_slice(), second.as_slice()].concat();
    log.truncate(first.len() + second.len() - 1); // tear the second record

    let restored = StoreServer::new(Some(&log), false, false).unwrap();
    assert_eq!(restored.size().unwrap(), 1);
    assert_eq!(restored.valid_log_len(), first.len() as u64);

    // Clean images report their full length; fresh boots report zero.
    let whole = [first.as_slice(), second.as_slice()].concat();
    let clean = StoreServer::new(Some(&whole), false, false).unwrap();
    assert_eq!(clean.valid_log_len(), whole.len() as u64);
    assert_eq!(
        StoreServer::new(Some(&whole), false, true)
            .unwrap()
            .valid_log_len(),
        whole.len() as u64
    );
    assert_eq!(
        StoreServer::new(None, false, false)
            .unwrap()
            .valid_log_len(),
        0
    );
}

//! The store as an in-process HTTP service: graphy-server's SPARQL 1.1
//! Protocol + Graph Store Protocol router, driven by `handleRequest`-style
//! calls instead of a listening socket. This is the per-dataset surface a
//! browser fabric worker hosts N of — each [`StoreServer`] owns one
//! persistent-capable store (WAL capture; see `drainLog`/`packLog`) and
//! answers `/sparql`, `/sparql/service`, `/graphs`, and `/health`.
//!
//! The dispatch pattern is the proven one: clone the router, drive it with a
//! `tower` oneshot, collect the body. On wasm the router's handlers run their
//! store work inline (graphy-server's `run_blocking` seam), so a dispatch
//! never blocks on a reactor that doesn't exist; natively the same core is
//! exercised under tokio in `tests/server.rs`.

use std::sync::Arc;

use graphy_store::{Order, QuadBatch, Store};
use http_body_util::BodyExt as _;
use tower::util::ServiceExt as _;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// One dispatched reply.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The target-agnostic core (natively testable): one store behind
/// graphy-server's protocol router. The [`GraphyStoreServer`] wasm-bindgen
/// wrapper is a thin `String`→`JsError` boundary over this.
#[derive(Debug)]
pub struct StoreServer {
    store: Arc<Store>,
    router: axum::Router,
    valid_log_len: u64,
}

impl StoreServer {
    /// A persistent-capable store behind the protocol router: pass the
    /// previously captured log to restore (or `None` to start fresh);
    /// commits accumulate WAL frames for [`StoreServer::drain_log`].
    /// `read_only` rejects updates and GSP writes with 403. `strict_log`
    /// makes a torn or foreign log tail an error instead of a truncation
    /// point — use it for imported images, never for a store's own durable
    /// log (a crash mid-append leaves a torn tail as a matter of course).
    pub fn new(
        log: Option<&[u8]>,
        read_only: bool,
        strict_log: bool,
    ) -> Result<StoreServer, String> {
        let (store, valid_log_len) = if strict_log {
            let store = Store::ephemeral_persistent_strict(log).map_err(err)?;
            // Strict restores replay the whole image or error out above.
            (store, log.map_or(0, |b| b.len() as u64))
        } else {
            Store::ephemeral_persistent_recovering(log).map_err(err)?
        };
        let store = Arc::new(store);
        let cfg = graphy_server::Config {
            read_only,
            ..graphy_server::Config::default()
        };
        let router = graphy_server::router(store.clone(), cfg);
        Ok(StoreServer {
            store,
            router,
            valid_log_len,
        })
    }

    /// Byte length of the restore image's valid prefix (0 without an
    /// image). When shorter than the image, the tail was torn (or foreign)
    /// and did not replay — the host must truncate its durable log to this
    /// length before appending, or frames written after the tear are
    /// unreachable on the next restore.
    pub fn valid_log_len(&self) -> u64 {
        self.valid_log_len
    }

    /// Dispatches one request against the router. `path_and_query` is the
    /// in-service path (`/sparql?query=…`, `/graphs?default`, …).
    pub async fn handle(
        &self,
        method: &str,
        path_and_query: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Reply, String> {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(path_and_query);
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        let request = builder
            .body(axum::body::Body::from(body))
            .map_err(|e| format!("invalid request: {e}"))?;
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .map_err(|_| "router dispatch failed".to_string())?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_string(), v.to_string()))
            })
            .collect();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("body error: {e}"))?
            .to_bytes()
            .to_vec();
        // Size-triggered ephemeral compaction (docs/11): a long-lived
        // embedded store otherwise accumulates delta history (every graph
        // replace, every delete/re-add) and its reads degrade
        // monotonically until the worker respawns. The check is a few
        // atomic loads; when due, the fold runs inline here — a bounded
        // pause on this worker instead of unbounded read decay. Failure
        // (e.g. a concurrent explicit compaction) never fails the request.
        self.store.compact_ephemeral_if_due().ok();
        Ok(Reply {
            status,
            headers,
            body,
        })
    }

    /// Committed WAL frames since the last drain — append them, verbatim
    /// and in order, to the durable log.
    pub fn drain_log(&self) -> Vec<u8> {
        self.store.drain_wal_capture()
    }

    /// Log compaction: the whole dataset as a single-transaction image.
    /// Atomically replace the durable log with this, then keep appending
    /// subsequent [`StoreServer::drain_log`] output.
    pub fn pack_log(&self) -> Result<Vec<u8>, String> {
        self.store.pack_log().map_err(err)
    }

    /// The number of quads in the store (counted without decoding — cheap
    /// enough for a dataset-info endpoint).
    pub fn size(&self) -> Result<u64, String> {
        let snap = self.store.snapshot();
        let Some(pat) = snap.resolve_pattern(None, None, None, None) else {
            return Ok(0);
        };
        let mut scan = snap.scan(&pat, Order::Spo).map_err(err)?;
        let mut batch = QuadBatch::new();
        let mut n = 0u64;
        while scan.next_batch(&mut batch).map_err(err)? {
            n += batch.len() as u64;
        }
        Ok(n)
    }
}

// ------------------------------------------------ wasm-bindgen boundary

/// The wasm-bindgen boundary over [`StoreServer`]. The `Arc` co-owns the
/// core across the Promise returned by `handleRequest`.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
#[derive(Debug)]
pub struct GraphyStoreServer {
    inner: Arc<StoreServer>,
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
impl GraphyStoreServer {
    /// Boot a store behind the protocol router: pass the previously captured
    /// log bytes to restore (or nothing to start fresh). See
    /// `drainLog`/`packLog` for the persistence contract. `strictLog`
    /// rejects torn/foreign images instead of truncating (for imports).
    #[wasm_bindgen::prelude::wasm_bindgen(constructor)]
    pub fn new(
        log: Option<Box<[u8]>>,
        read_only: Option<bool>,
        strict_log: Option<bool>,
    ) -> Result<GraphyStoreServer, wasm_bindgen::JsError> {
        Ok(GraphyStoreServer {
            inner: Arc::new(
                StoreServer::new(
                    log.as_deref(),
                    read_only.unwrap_or(false),
                    strict_log.unwrap_or(false),
                )
                .map_err(|e| wasm_bindgen::JsError::new(&e))?,
            ),
        })
    }

    /// Dispatches one request. `headers_json` is a JSON object of header
    /// name → value; resolves to `{status, headers, body}` (body as a
    /// Uint8Array).
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = handleRequest)]
    pub fn handle_request(
        &self,
        method: String,
        path_and_query: String,
        headers_json: String,
        body: Option<Vec<u8>>,
    ) -> js_sys::Promise {
        let inner = self.inner.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            crate::set_clock();
            let headers: std::collections::BTreeMap<String, String> =
                serde_json::from_str(&headers_json).map_err(|e| {
                    wasm_bindgen::JsValue::from_str(&format!("invalid headers: {e}"))
                })?;
            let pairs: Vec<(String, String)> = headers.into_iter().collect();
            let reply = inner
                .handle(&method, &path_and_query, &pairs, body.unwrap_or_default())
                .await
                .map_err(|e| wasm_bindgen::JsValue::from_str(&e))?;
            let header_object = js_sys::Object::new();
            for (name, value) in &reply.headers {
                js_sys::Reflect::set(
                    &header_object,
                    &wasm_bindgen::JsValue::from_str(name),
                    &wasm_bindgen::JsValue::from_str(value),
                )
                .ok();
            }
            let out = js_sys::Object::new();
            js_sys::Reflect::set(
                &out,
                &"status".into(),
                &wasm_bindgen::JsValue::from_f64(f64::from(reply.status)),
            )
            .ok();
            js_sys::Reflect::set(&out, &"headers".into(), &header_object).ok();
            js_sys::Reflect::set(
                &out,
                &"body".into(),
                &js_sys::Uint8Array::from(reply.body.as_slice()),
            )
            .ok();
            Ok(out.into())
        })
    }

    /// Byte length of the restore image's valid prefix. When shorter than
    /// the image passed at boot, the tail was torn and did not replay —
    /// truncate the durable log to this length before appending, or the
    /// appended frames are unreachable on the next restore.
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = validLogLen)]
    pub fn valid_log_len(&self) -> f64 {
        self.inner.valid_log_len() as f64
    }

    /// Committed WAL frames since the last drain — append them, verbatim
    /// and in order, to the durable log (e.g. an OPFS file).
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = drainLog)]
    pub fn drain_log(&self) -> Vec<u8> {
        self.inner.drain_log()
    }

    /// Log compaction: the whole dataset as a single-transaction image;
    /// atomically replace the durable log with it.
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = packLog)]
    pub fn pack_log(&self) -> Result<Vec<u8>, wasm_bindgen::JsError> {
        self.inner
            .pack_log()
            .map_err(|e| wasm_bindgen::JsError::new(&e))
    }

    /// The number of quads in the store.
    pub fn size(&self) -> Result<f64, wasm_bindgen::JsError> {
        self.inner
            .size()
            .map(|n| n as f64)
            .map_err(|e| wasm_bindgen::JsError::new(&e))
    }
}

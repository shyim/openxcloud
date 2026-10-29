//! Port of qt/src/xcloud/XCloudApiClient.{h,cpp} (`GamePassCatalogClient`).
//!
//! Game Pass catalog hydration used to enrich library items with artwork and
//! descriptions: product ids are posted to `catalog.gamepass.com/v3/products`
//! in an initial batch of 12 followed by batches of 100. At most four requests
//! run concurrently, including retries, and successful split responses are
//! handed to the caller immediately.
//!
//! Differences from the C++:
//! - The `std::function` batch callback is an `FnMut(&[CatalogProduct])`
//!   moved into the call; it is dropped when the call returns, which is what
//!   the C++ test (`weak.expired()`) asserted by hand.
//! - Batches run on `tokio::task::JoinSet`; the callback is invoked from the
//!   calling task in completion order, exactly like the C++ watcher slots.

use std::sync::Arc;

use serde_json::Value;
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use xcloud_proto::models::CatalogProduct;

use crate::transport::{Headers, HttpTransport};
use crate::{Error, Result};

const LOG: &str = "XCloudAPI";

/// Products per request.
const BATCH_SIZE: usize = 100;
/// Recent/visible games should not wait for a full catalog batch.
const FIRST_BATCH_SIZE: usize = 12;
const REQUEST_DEADLINE: Duration = Duration::from_secs(8);
const BATCH_DEADLINE: Duration = Duration::from_secs(30);

/// Bound both top-level batches and HTTP requests spawned by retry splitting.
const MAX_CONCURRENT_BATCHES: usize = 4;

const CATALOG_URL: &str = "https://catalog.gamepass.com/v3/products";

/// Query parameters and optional bearer token for a hydration call. The
/// defaults mirror the C++ default arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogQuery {
    pub market: String,
    pub language: String,
    pub hydration: String,
    /// Sent as `Authorization: Bearer ...` when non-empty.
    pub authorization_token: String,
}

impl Default for CatalogQuery {
    fn default() -> Self {
        Self {
            market: "US".to_owned(),
            language: "en-US".to_owned(),
            hydration: "RemoteHighSapphire0".to_owned(),
            authorization_token: String::new(),
        }
    }
}

/// Game Pass catalog hydration client.
#[derive(Clone)]
pub struct GamePassCatalogClient {
    transport: Arc<dyn HttpTransport>,
}

impl std::fmt::Debug for GamePassCatalogClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GamePassCatalogClient")
            .finish_non_exhaustive()
    }
}

impl GamePassCatalogClient {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self { transport }
    }

    /// `parseProducts`: the catalog returns `Products` either as an array or
    /// as a map keyed by product id (in which case the key fills a missing
    /// `ProductId`). Anything that is not an object yields no products.
    pub fn parse_products(body: &[u8]) -> Vec<CatalogProduct> {
        let Ok(Value::Object(document)) = serde_json::from_slice::<Value>(body) else {
            return Vec::new();
        };
        match document.get("Products") {
            Some(Value::Array(array)) => array
                .iter()
                .map(|value| CatalogProduct::from_json(&object_or_empty(value)))
                .collect(),
            Some(Value::Object(map)) => map
                .iter()
                .map(|(key, value)| {
                    let mut object = object_or_empty(value);
                    if let Value::Object(fields) = &mut object
                        && !fields.contains_key("ProductId")
                        && !fields.contains_key("StoreId")
                    {
                        fields.insert("ProductId".to_owned(), Value::String(key.clone()));
                    }
                    CatalogProduct::from_json(&object)
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// `hydrateProducts`: fetches catalog data for `product_ids` (duplicates
    /// removed, caller order preserved so recent games come first). `on_batch`
    /// receives each non-empty batch as it completes. Enrichment is best
    /// effort: partial catalog data is better than none, so the call only
    /// fails when nothing could be loaded at all.
    pub async fn hydrate_products<I, S, F>(
        &self,
        product_ids: I,
        query: &CatalogQuery,
        mut on_batch: F,
    ) -> Result<Vec<CatalogProduct>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
        F: FnMut(&[CatalogProduct]),
    {
        let mut unique: Vec<String> = Vec::new();
        for id in product_ids {
            let id = id.as_ref();
            if !unique.iter().any(|seen| seen == id) {
                unique.push(id.to_owned());
            }
        }
        let first = unique.len().min(FIRST_BATCH_SIZE);
        let mut batches: Vec<Vec<String>> = unique[..first]
            .chunks(FIRST_BATCH_SIZE)
            .map(<[String]>::to_vec)
            .collect();
        batches.extend(unique[first..].chunks(BATCH_SIZE).map(<[String]>::to_vec));
        if batches.is_empty() {
            return Ok(Vec::new());
        }

        let url = catalog_url(query)?;
        // Anything the requested market cannot serve is fetched again in US English so
        // titles and art never go missing because of the language choice.
        let fallback_query = CatalogQuery {
            market: "US".to_owned(),
            language: "en-US".to_owned(),
            ..query.clone()
        };
        let fallback = (fallback_query.market != query.market
            || fallback_query.language != query.language)
            .then(|| catalog_url(&fallback_query))
            .transpose()?;
        let headers = Arc::new(request_headers(&query.authorization_token));

        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<Vec<CatalogProduct>>();
        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_BATCHES));
        let mut in_flight: JoinSet<BatchResult> = JoinSet::new();
        let mut next = 0;
        let mut merged: Vec<CatalogProduct> = Vec::new();
        let mut first_error: Option<Error> = None;
        let mut failed_products = 0usize;
        let mut fallback_products = 0usize;

        loop {
            while in_flight.len() < MAX_CONCURRENT_BATCHES && next < batches.len() {
                let fetcher = BatchFetcher {
                    transport: self.transport.clone(),
                    url: url.clone(),
                    fallback: fallback.clone(),
                    headers: headers.clone(),
                    progress: progress_tx.clone(),
                    permits: permits.clone(),
                };
                let ids = batches[next].clone();
                next += 1;
                in_flight.spawn(async move { fetcher.fetch(ids).await });
            }

            let joined = tokio::select! {
                biased;
                Some(products) = progress_rx.recv() => {
                    on_batch(&products);
                    merged.extend(products);
                    continue;
                }
                joined = in_flight.join_next() => joined,
            };
            let Some(joined) = joined else {
                break;
            };
            match joined {
                Err(join_error) => {
                    tracing::warn!(target: LOG, "catalog batch task failed: {join_error}");
                    first_error.get_or_insert(Error::Other(join_error.to_string()));
                }
                Ok(result) => {
                    failed_products += result.failed;
                    fallback_products += result.via_fallback;
                    if let Some(error) = result.error {
                        first_error.get_or_insert(error);
                    }
                }
            }
        }

        if failed_products > 0 || fallback_products > 0 {
            tracing::warn!(
                target: LOG,
                "catalog {}/{}: {} products loaded, {} via en-US fallback, {} failed{}",
                query.language,
                query.market,
                merged.len(),
                fallback_products,
                failed_products,
                first_error.as_ref().map(|e| format!(" (first error: {e})")).unwrap_or_default(),
            );
        }
        match first_error {
            Some(error) if merged.is_empty() => Err(error),
            _ => Ok(merged),
        }
    }
}

fn catalog_url(query: &CatalogQuery) -> Result<String> {
    let mut url = url::Url::parse(CATALOG_URL).map_err(|e| Error::Other(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("market", &query.market)
        .append_pair("language", &query.language)
        .append_pair("hydration", &query.hydration);
    Ok(url.to_string())
}

/// What one top-level batch produced after retries, splitting and fallback.
#[derive(Default)]
struct BatchResult {
    products: Vec<CatalogProduct>,
    /// Products that could not be loaded at all.
    failed: usize,
    /// Products that only loaded through the en-US fallback.
    via_fallback: usize,
    error: Option<Error>,
}

struct BatchFetcher {
    transport: Arc<dyn HttpTransport>,
    url: String,
    fallback: Option<String>,
    headers: Arc<Headers>,
    progress: mpsc::UnboundedSender<Vec<CatalogProduct>>,
    permits: Arc<Semaphore>,
}

impl BatchFetcher {
    /// Fetches `ids`; on failure retries once, then splits the batch in halves so one
    /// product the service chokes on cannot take 99 others down with it. A single id
    /// that still fails is tried once more against the fallback market.
    fn fetch(
        self,
        ids: Vec<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = BatchResult> + Send>> {
        let this = Arc::new(self);
        Box::pin(async move {
            let count = ids.len();
            tokio::time::timeout(BATCH_DEADLINE, this.fetch_ids(ids))
                .await
                .unwrap_or_else(|_| BatchResult {
                    failed: count,
                    error: Some(Error::Timeout("catalog batch deadline exceeded".into())),
                    ..Default::default()
                })
        })
    }

    fn fetch_ids(
        self: &Arc<Self>,
        ids: Vec<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = BatchResult> + Send + '_>> {
        Box::pin(async move {
            let mut last_error = None;
            for attempt in 0..2 {
                match self.post(&self.url, &ids).await {
                    Ok(products) => {
                        let _ = self.progress.send(products.clone());
                        return BatchResult {
                            products,
                            ..BatchResult::default()
                        };
                    }
                    Err(error) => {
                        // Report the original failure, not the retry's.
                        last_error.get_or_insert(error);
                    }
                }
                if attempt == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
            }
            if ids.len() > 1 {
                let half = ids.len() / 2;
                let (left, right) = ids.split_at(half);
                let (a, b) = tokio::join!(
                    self.fetch_ids(left.to_vec()),
                    self.fetch_ids(right.to_vec())
                );
                let mut out = a;
                out.products.extend(b.products);
                out.failed += b.failed;
                out.via_fallback += b.via_fallback;
                if out.error.is_none() {
                    out.error = b.error;
                }
                return out;
            }
            if let Some(fallback) = &self.fallback
                && let Ok(products) = self.post(fallback, &ids).await
                && !products.is_empty()
            {
                let _ = self.progress.send(products.clone());
                return BatchResult {
                    via_fallback: products.len(),
                    products,
                    ..BatchResult::default()
                };
            }
            BatchResult {
                failed: ids.len(),
                error: last_error,
                ..BatchResult::default()
            }
        })
    }

    async fn post(&self, url: &str, ids: &[String]) -> Result<Vec<CatalogProduct>> {
        let body = serde_json::json!({ "Products": ids }).to_string();
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        let response = tokio::time::timeout(
            REQUEST_DEADLINE,
            self.transport
                .post_raw(url, body.into_bytes(), self.headers.as_ref().clone()),
        )
        .await
        .map_err(|_| Error::Timeout("catalog request deadline exceeded".into()))??;
        if !response.is_success() {
            return Err(Error::Status {
                status: response.status,
                body: response.body_text().chars().take(200).collect(),
            });
        }
        Ok(GamePassCatalogClient::parse_products(&response.body))
    }
}

/// `QJsonValue::toObject()`: a non-object reads as an empty object.
fn object_or_empty(value: &Value) -> Value {
    if value.is_object() {
        value.clone()
    } else {
        Value::Object(serde_json::Map::new())
    }
}

fn request_headers(authorization_token: &str) -> Headers {
    let mut headers = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Accept".to_owned(), "application/json".to_owned()),
        ("ms-cv".to_owned(), "0.0".to_owned()),
        (
            "calling-app-name".to_owned(),
            "Xbox Cloud Gaming Web".to_owned(),
        ),
        ("calling-app-version".to_owned(), "21.0.0".to_owned()),
        (
            "User-Agent".to_owned(),
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/119.0.0.0 Safari/537.36"
                .to_owned(),
        ),
    ];
    if !authorization_token.is_empty() {
        headers.push((
            "Authorization".to_owned(),
            format!("Bearer {authorization_token}"),
        ));
    }
    headers
}

#[cfg(test)]
mod tests {
    //! Port of qt/tests/tst_xcloudapi.cpp (`catalogReleasesBatchCallback`).
    use std::sync::{Arc, Weak};

    use super::*;
    use crate::transport::FakeTransport;

    #[tokio::test]
    async fn catalog_releases_batch_callback() {
        let transport = Arc::new(FakeTransport::new());
        let catalog = GamePassCatalogClient::new(transport.clone());
        transport.enqueue_json(
            "catalog.gamepass.com",
            r#"{"Products":{"P1":{"ProductTitle":"Game"}}}"#,
            200,
        );
        let marker = Arc::new(42);
        let weak: Weak<i32> = Arc::downgrade(&marker);
        let received = std::cell::Cell::new(false);
        let received_ref = &received;
        let result = catalog
            .hydrate_products(
                ["P1"],
                &CatalogQuery::default(),
                move |products: &[CatalogProduct]| {
                    // `marker` is owned by the callback, like the C++ capture.
                    assert_eq!(*marker, 42);
                    received_ref.set(true);
                    assert_eq!(products.len(), 1);
                    assert_eq!(products[0].product_id, "P1");
                    assert_eq!(products[0].product_title, "Game");
                },
            )
            .await;
        assert!(result.is_ok());
        assert!(received.get());
        // The loader must not retain its callback (and batch state) after completion.
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn hydrate_sends_query_headers_and_body() {
        let transport = Arc::new(FakeTransport::new());
        let catalog = GamePassCatalogClient::new(transport.clone());
        transport.enqueue_json("catalog.gamepass.com", r#"{"Products":[]}"#, 200);
        let query = CatalogQuery {
            market: "DE".to_owned(),
            language: "de-DE".to_owned(),
            authorization_token: "TOK".to_owned(),
            ..CatalogQuery::default()
        };
        let products = catalog
            .hydrate_products(["A", "B", "A"], &query, |_| {})
            .await
            .unwrap();
        assert!(products.is_empty());

        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.url,
            "https://catalog.gamepass.com/v3/products?market=DE&language=de-DE&hydration=RemoteHighSapphire0"
        );
        assert_eq!(request.header("Authorization"), Some("Bearer TOK"));
        assert_eq!(
            request.header("calling-app-name"),
            Some("Xbox Cloud Gaming Web")
        );
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["Products"], serde_json::json!(["A", "B"]));
    }

    #[tokio::test]
    async fn hydrate_prioritizes_a_small_first_batch_then_batches_by_hundred() {
        let transport = Arc::new(FakeTransport::new());
        let catalog = GamePassCatalogClient::new(transport.clone());
        for _ in 0..4 {
            transport.enqueue_json(
                "catalog.gamepass.com",
                r#"{"Products":[{"ProductId":"X","ProductTitle":"T"}]}"#,
                200,
            );
        }
        let ids: Vec<String> = (0..250).map(|i| format!("P{i}")).collect();
        let mut batches = 0;
        let products = catalog
            .hydrate_products(&ids, &CatalogQuery::default(), |_| batches += 1)
            .await
            .unwrap();
        assert_eq!(products.len(), 4);
        assert_eq!(batches, 4);
        let requests = transport.requests();
        assert_eq!(requests.len(), 4);
        let mut sizes: Vec<usize> = requests
            .iter()
            .map(|request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                body["Products"].as_array().unwrap().len()
            })
            .collect();
        sizes.sort_unstable();
        assert_eq!(sizes, vec![12, 38, 100, 100]);
    }

    #[tokio::test]
    async fn hydrate_is_best_effort() {
        let transport = Arc::new(FakeTransport::new());
        let catalog = GamePassCatalogClient::new(transport.clone());
        transport.enqueue_json(
            "catalog.gamepass.com",
            r#"{"Products":[{"ProductId":"X"}]}"#,
            200,
        );
        transport.enqueue_json("catalog.gamepass.com", r#"{"error":"nope"}"#, 500);
        let ids: Vec<String> = (0..150).map(|i| format!("P{i}")).collect();
        let products = catalog
            .hydrate_products(&ids, &CatalogQuery::default(), |_| {})
            .await
            .unwrap();
        assert_eq!(products.len(), 1);

        // Nothing loaded at all: the first failure is reported.
        let transport = Arc::new(FakeTransport::new());
        let catalog = GamePassCatalogClient::new(transport.clone());
        transport.enqueue_json("catalog.gamepass.com", r#"{"error":"nope"}"#, 500);
        let err = catalog
            .hydrate_products(["P1"], &CatalogQuery::default(), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Status { status: 500, .. }), "{err}");

        // No ids: no request, empty result.
        let empty: [&str; 0] = [];
        let products = catalog
            .hydrate_products(empty, &CatalogQuery::default(), |_| {})
            .await
            .unwrap();
        assert!(products.is_empty());
        // The failing single-id batch was retried once; no ids sent nothing.
        assert_eq!(transport.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_failing_batch_is_split_so_good_products_still_load() {
        let transport = Arc::new(FakeTransport::new());
        let catalog = GamePassCatalogClient::new(transport.clone());
        // 4 ids: whole batch fails twice, then the halves [A,B] and [C,D] succeed.
        transport.enqueue_json("catalog.gamepass.com", r#"{"error":"x"}"#, 504);
        transport.enqueue_json("catalog.gamepass.com", r#"{"error":"x"}"#, 504);
        transport.enqueue_json(
            "catalog.gamepass.com",
            r#"{"Products":{"A":{"ProductTitle":"A"},"B":{"ProductTitle":"B"}}}"#,
            200,
        );
        transport.enqueue_json(
            "catalog.gamepass.com",
            r#"{"Products":{"C":{"ProductTitle":"C"},"D":{"ProductTitle":"D"}}}"#,
            200,
        );
        let products = catalog
            .hydrate_products(["A", "B", "C", "D"], &CatalogQuery::default(), |_| {})
            .await
            .unwrap();
        let mut titles: Vec<&str> = products.iter().map(|p| p.product_title.as_str()).collect();
        titles.sort_unstable();
        assert_eq!(titles, ["A", "B", "C", "D"]);
    }

    #[tokio::test]
    async fn products_missing_in_the_chosen_market_fall_back_to_en_us() {
        let transport = Arc::new(FakeTransport::new());
        let catalog = GamePassCatalogClient::new(transport.clone());
        let query = CatalogQuery {
            market: "DE".into(),
            language: "de-DE".into(),
            ..CatalogQuery::default()
        };
        transport.enqueue_json("market=DE", r#"{"error":"x"}"#, 504);
        transport.enqueue_json("market=DE", r#"{"error":"x"}"#, 504);
        transport.enqueue_json(
            "market=US&language=en-US",
            r#"{"Products":{"A":{"ProductTitle":"Alpha"}}}"#,
            200,
        );
        let products = catalog
            .hydrate_products(["A"], &query, |_| {})
            .await
            .unwrap();
        assert_eq!(products.len(), 1);
        assert_eq!(products[0].product_title, "Alpha");
        let urls: Vec<String> = transport.requests().iter().map(|r| r.url.clone()).collect();
        assert!(urls[0].contains("market=DE&language=de-DE"), "{urls:?}");
        assert!(
            urls.last()
                .is_some_and(|u| u.contains("market=US&language=en-US"))
        );
    }

    #[tokio::test]
    async fn successful_split_is_published_before_its_sibling_finishes() {
        struct SplitTransport(tokio::sync::Notify);
        #[async_trait::async_trait]
        impl HttpTransport for SplitTransport {
            async fn send(
                &self,
                request: crate::transport::HttpRequest,
            ) -> Result<crate::transport::HttpResponse> {
                let body: Value = serde_json::from_slice(&request.body)?;
                let ids = body["Products"].as_array().unwrap();
                let (status, body) = if ids.len() > 1 {
                    (504, r#"{}"#)
                } else if ids[0] == "A" {
                    (200, r#"{"Products":{"A":{"ProductTitle":"Ready"}}}"#)
                } else {
                    self.0.notified().await;
                    (200, r#"{"Products":{"B":{"ProductTitle":"Later"}}}"#)
                };
                Ok(crate::transport::HttpResponse {
                    status,
                    body: body.as_bytes().to_vec(),
                    url: request.url,
                })
            }
        }
        let transport = Arc::new(SplitTransport(tokio::sync::Notify::new()));
        let catalog = GamePassCatalogClient::new(transport.clone());
        let mut order = Vec::new();
        let products = tokio::time::timeout(
            Duration::from_secs(2),
            catalog.hydrate_products(["A", "B"], &CatalogQuery::default(), |batch| {
                order.push(batch[0].product_id.clone());
                if batch[0].product_id == "A" {
                    transport.0.notify_one();
                }
            }),
        )
        .await
        .expect("ready products must not wait for the blocked sibling")
        .unwrap();
        assert_eq!(order, ["A", "B"]);
        assert_eq!(products.len(), 2);
    }

    #[test]
    fn parse_products_accepts_array_and_map_shapes() {
        let from_map = GamePassCatalogClient::parse_products(
            br#"{"Products":{"P1":{"ProductTitle":"Game"},"P2":{"StoreId":"S2"}}}"#,
        );
        assert_eq!(from_map.len(), 2);
        let p1 = from_map.iter().find(|p| p.product_title == "Game").unwrap();
        assert_eq!(p1.product_id, "P1");
        let p2 = from_map.iter().find(|p| p.store_id == "S2").unwrap();
        assert_eq!(p2.product_id, "S2");

        let from_array = GamePassCatalogClient::parse_products(
            br#"{"Products":[{"ProductId":"A"},{"ProductId":"B"}]}"#,
        );
        assert_eq!(from_array.len(), 2);
        assert_eq!(from_array[0].product_id, "A");

        assert!(GamePassCatalogClient::parse_products(b"[]").is_empty());
        assert!(GamePassCatalogClient::parse_products(b"not json").is_empty());
        assert!(GamePassCatalogClient::parse_products(br#"{"Products":1}"#).is_empty());
    }
}

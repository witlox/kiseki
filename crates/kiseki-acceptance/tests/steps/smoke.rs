//! Smoke-test steps for the server harness.
//!
//! These steps exercise the real `kiseki-server` binary through
//! network protocols — no in-process domain objects. They prove
//! the harness works before we migrate the rest of the steps.

use cucumber::{given, then, when};

use crate::KisekiWorld;

#[given("a running kiseki-server")]
async fn given_running_server(w: &mut KisekiWorld) {
    w.ensure_server()
        .await
        .expect("failed to start kiseki-server");
}

#[when(regex = r#"^I PUT "([^"]*)" to S3 key "([^"]*)"$"#)]
async fn when_s3_put(w: &mut KisekiWorld, body: String, key: String) {
    let url = w.server().s3_url(&key);
    let resp = w
        .server()
        .http
        .put(&url)
        .body(body.into_bytes())
        .send()
        .await
        .expect("HTTP PUT failed");
    assert!(
        resp.status().is_success(),
        "S3 PUT returned {}: {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );
    // Capture etag from response headers.
    if let Some(etag) = resp.headers().get("etag") {
        w.server_mut().last_etag = Some(etag.to_str().unwrap_or("").trim_matches('"').to_string());
    }
}

#[then(regex = r#"^I can GET S3 key "([^"]*)" and receive "([^"]*)"$"#)]
async fn then_s3_get(w: &mut KisekiWorld, key: String, expected: String) {
    let namespace = key.split('/').next().unwrap_or("default");
    let etag = w
        .server()
        .last_etag
        .as_ref()
        .expect("no etag from prior PUT")
        .clone();
    let url = w.server().s3_url(&format!("{}/{}", namespace, etag));
    let resp = w
        .server()
        .http
        .get(&url)
        .send()
        .await
        .expect("HTTP GET failed");
    assert!(
        resp.status().is_success(),
        "S3 GET returned {}",
        resp.status()
    );
    let body = resp.bytes().await.expect("read body");
    assert_eq!(body.as_ref(), expected.as_bytes(), "S3 GET body mismatch");
}

#[then("the gRPC health endpoint reports the server is ready")]
async fn then_grpc_health(w: &mut KisekiWorld) {
    use kiseki_proto::v1::key_manager_service_client::KeyManagerServiceClient;
    use kiseki_proto::v1::KeyManagerHealthRequest;

    let channel = w.server().grpc.clone();
    let mut client = KeyManagerServiceClient::new(channel);
    let resp = client
        .health(KeyManagerHealthRequest {})
        .await
        .expect("gRPC Health call failed");
    let health = resp.into_inner();
    let epoch = health
        .current_epoch
        .expect("health response should include current_epoch");
    assert!(
        epoch.value > 0,
        "server should have epoch > 0 after bootstrap, got {}",
        epoch.value
    );
}

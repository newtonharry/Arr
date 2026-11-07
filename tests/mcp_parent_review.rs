//! Independent review: a POST can commit before its reply is lost. The MCP must not tell
//! the user that nothing happened, retry it itself, or suggest falling back to Sonarr/Radarr.
use arr::fsutil;
use axum::{Json, Router, http::StatusCode, routing::post};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientConfig},
};
use serde_json::json;
use std::{
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[tokio::test]
async fn a_committed_post_with_a_lost_or_unusable_reply_is_reported_as_uncertain() {
    let temp = tempfile::tempdir().unwrap();
    let token = temp.path().join("token");
    let secret = "parent-review-private-token";
    fsutil::write_new_private_file(&token, secret.as_bytes()).unwrap();
    for delayed in [true, false] {
        let commits = Arc::new(AtomicUsize::new(0));
        let recorded = commits.clone();
        let app = Router::new()
            .route(
                "/api/v1/library",
                post(move || {
                    let recorded = recorded.clone();
                    async move {
                        recorded.fetch_add(1, Ordering::SeqCst); // The requested state was persisted.
                        if delayed {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                        (
                            StatusCode::BAD_GATEWAY,
                            "<html>committed, reply lost</html>",
                        )
                    }
                }),
            )
            .fallback(|| async { Json(json!({"unexpected": true})) });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_arr"))
            .args(["mcp", "--api-url", &api, "--token-file"])
            .arg(&token)
            .args(["--timeout-secs", "1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let transport = (child.stdout.take().unwrap(), child.stdin.take().unwrap());
        let client = ClientConfig::default().serve(transport).await.unwrap();
        let result = client
            .call_tool(
                CallToolRequestParams::new("add_media").with_arguments(
                    json!({"media_id": "movie:tmdb:603"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        let _ = client.cancel().await;
        let exit = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        server.abort();
        assert!(exit.success());
        assert_eq!(
            commits.load(Ordering::SeqCst),
            1,
            "a mutation is never retried by this adapter"
        );
        assert_eq!(result.is_error, Some(true));
        let data = result.structured_content.as_ref().unwrap();
        let text = data.to_string().to_lowercase();
        assert!(!text.contains(secret));
        assert!(
            !text.contains("nothing was requested"),
            "a lost reply cannot establish no side effect: {data}"
        );
        assert!(
            text.contains("requests") && text.contains("before"),
            "inspect-first guidance is required after a potentially committed write: {data}"
        );
        assert!(
            text.contains("no fallback"),
            "do not send an uncertain request to Sonarr/Radarr: {data}"
        );
    }
}

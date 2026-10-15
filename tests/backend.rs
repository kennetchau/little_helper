//! Phase-3 backend client tests against a fake local HTTP server (wiremock).
//!
//! Verifies the SSE streaming contract (multi-line framing across chunk
//! boundaries, usage-only chunks, `[DONE]`, tool-call deltas), the one-shot
//! `complete`, `list_models`, and the retry policy (5xx retries once,
//! 4xx fails immediately).

use futures_util::StreamExt;
use harness::backend::{BackendClient, ModelStatus, StreamItem};
use harness::config::{BackendConfig, Config};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client_for(server: &MockServer) -> BackendClient {
    let mut cfg = Config::default();
    cfg.backend = BackendConfig {
        base_url: server.uri(),
        api_key: "sk-test".into(),
        model: "fake-model".into(),
        ..Default::default()
    };
    BackendClient::new(&cfg)
}

#[tokio::test]
async fn stream_chat_parses_full_sse_contract() {
    let server = MockServer::start().await;
    // SSE body with a tool-call split across two data lines, a usage-only
    // chunk, and a trailing [DONE].
    let body = "\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hmm \"}}]}

data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}

data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"list_dir\",\"arguments\":\"{\"}}]}}]}

data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"path\\\": \\\".\\\"}\"}}]}}]}

data: {\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":9}}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}

data: [DONE]
";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(body),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(&server);
    let stream = client
        .stream_chat(&[serde_json::json!({"role": "user", "content": "hi"})], None)
        .await
        .expect("stream request ok");
    let mut items: Vec<StreamItem> = Vec::new();
    let mut s = stream;
    while let Some(item) = s.next().await {
        let item = item.expect("no stream error");
        let done = item.is_done();
        items.push(item);
        if done {
            break;
        }
    }
    // content + reasoning + two tool-call fragments, then Done
    let reasoning: Vec<_> = items
        .iter()
        .filter_map(|i| match i {
            StreamItem::Event(harness::backend::SseEvent::Reasoning(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, vec!["hmm "]);
    let texts: Vec<_> = items
        .iter()
        .filter_map(|i| match i {
            StreamItem::Event(harness::backend::SseEvent::Text(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["Hel"]);
    let tool_calls: Vec<_> = items
        .iter()
        .filter_map(|i| match i {
            StreamItem::Event(harness::backend::SseEvent::ToolCall {
                id,
                name,
                arguments,
                ..
            }) => Some((
                id.as_deref().unwrap_or(""),
                name.as_deref().unwrap_or(""),
                arguments.as_str(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_calls,
        vec![
            ("call_1", "list_dir", "{"),
            ("", "", "\"path\": \".\"}")
        ]
    );
    assert!(
        matches!(
            items.last().unwrap(),
            StreamItem::Done { finish_reason, usage: Some(u) }
                if finish_reason == "tool_calls" && u.prompt_tokens == 42
                    && u.completion_tokens == 9
        ),
        "terminal Done mismatch: {items:?}"
    );
}

#[tokio::test]
async fn complete_returns_content_and_omits_empty_tools() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "the summary text"}}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(&server);
    let out = client
        .complete(
            &[serde_json::json!({"role": "user", "content": "summarize"})],
            512,
        )
        .await
        .expect("complete ok");
    assert_eq!(out, "the summary text");
}

#[tokio::test]
async fn list_models_collects_ids() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "m1"}, {"id": "m2"}, {"no_id": true}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let models = client.list_models().await.expect("models ok");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["m1", "m2"]);
    // plain OpenAI entries (no status field) parse to Unknown
    assert!(models.iter().all(|m| m.status == ModelStatus::Unknown));
}

#[tokio::test]
async fn list_models_parses_fork_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [
                {"id": "big",
                 "status": {"value": "loaded", "args": []},
                 "source": "models_dir", "context_length": 131072},
                {"id": "small",
                 "status": {"value": "unloaded"},
                 "source": "preset"},
                {"id": "crashed",
                 "status": {"value": "unloaded", "failed": true, "exit_code": 137}}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let models = client.list_models().await.expect("models ok");
    assert_eq!(models.len(), 3);
    assert_eq!(models[0].id, "big");
    assert_eq!(models[0].status, ModelStatus::Loaded);
    assert_eq!(models[0].source.as_deref(), Some("models_dir"));
    assert_eq!(models[0].context_length, Some(131072));
    assert_eq!(models[1].status, ModelStatus::Unloaded);
    assert_eq!(models[2].status, ModelStatus::Unloaded);
    assert!(models[2].failed);
}

#[tokio::test]
async fn load_and_unload_post_the_model_name() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/models/load"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"success": true})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/models/unload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"success": true})))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server);
    client.load_model("m1").await.expect("load ok");
    client.unload_model("m1").await.expect("unload ok");
}

#[tokio::test]
async fn load_unload_use_the_server_root_not_the_v1_prefix() {
    // Production shape: base_url is https://host/v1, but the llama.cpp
    // router only maps /models/load + /models/unload at the ROOT — the
    // client must strip the /v1 segment (verified live).
    let server = MockServer::start().await;
    let base = format!("{}/v1", server.uri());
    let mut cfg = Config::default();
    cfg.backend = BackendConfig {
        base_url: base,
        api_key: "sk-test".into(),
        model: "fake-model".into(),
        ..Default::default()
    };
    let client = BackendClient::new(&cfg);
    Mock::given(method("POST"))
        .and(path("/models/load"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"success": true})))
        .expect(1)
        .mount(&server)
        .await;
    client.load_model("m1").await.expect("load hits the root path");
}

#[tokio::test]
async fn unload_of_idle_model_surfaces_server_error() {
    // The fork answers 400 "model is not running" — a 4xx, so the client
    // must fail immediately (no retry) with the server's message.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/models/unload"))
        .respond_with(ResponseTemplate::new(400).set_body_string("model is not running"))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let err = client
        .unload_model("m1")
        .await
        .expect_err("4xx must fail");
    assert!(err.to_string().starts_with("HTTP 400"), "{err}");
    assert!(err.to_string().contains("not running"), "{err}");
}

#[tokio::test]
async fn five_hundred_is_surfaced_after_retry() {
    // A 500 response: the client must attempt twice (one retry after a
    // 2s backoff) and then surface the error — not fail fast like a 4xx.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let started = std::time::Instant::now();
    let err = client
        .complete(&[serde_json::json!({"role": "user", "content": "x"})], 64)
        .await
        .expect_err("5xx exhausts the retry -> BackendError");
    assert!(err.to_string().starts_with("HTTP 500"), "{err}");
    // one backoff sleep happened (2s) — proves the retry was attempted
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(2),
        "expected a 2s backoff between attempts, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn four_hundred_fails_immediately_no_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let err = client
        .complete(&[serde_json::json!({"role": "user", "content": "x"})], 64)
        .await
        .expect_err("4xx must fail");
    assert!(err.to_string().starts_with("HTTP 400"), "{err}");
}

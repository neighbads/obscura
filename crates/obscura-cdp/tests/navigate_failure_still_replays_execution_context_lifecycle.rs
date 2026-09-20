// Third instance of the "utilityScript.evaluate is not a function" cascade,
// after the Runtime.evaluate (emit_post_eval_nav) and Input.dispatchMouseEvent
// paths: `Page.navigate` itself.
//
// `Page::navigate_single` calls `init_js` — rebuilding the JS runtime, which
// drops every CDP object handle for the old document — and the overall
// navigation can still fail *after* that, for example when the 30s navigation
// deadline fires while the new document is settling (seen live on
// imdb.com: `CDP error for Page.navigate: Network error: navigation exceeded
// 30000ms deadline`, followed by every later page.evaluate failing).
// `do_navigate` propagated that Err immediately and so skipped
// `emit_navigation_events` entirely, never telling the client the execution
// context it still held handles for had been torn down and replaced.
//
// Reproduced deterministically through the navigation-chain limit rather than
// a real timeout (network timing would make that flaky): the navigated-to
// page's own inline script immediately triggers a second, JS-initiated
// navigation, which with OBSCURA_NAV_CHAIN_LIMIT=1 exceeds the cap. The
// navigation fails with `TooManyClientNavigations` — but only after
// `navigate_single` already ran `init_js` for the loaded document.
use obscura_cdp::dispatch::{dispatch, CdpContext};
use obscura_cdp::types::CdpRequest;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = [0u8; 2048];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let _ = String::from_utf8_lossy(&buffer[..read]);
                // Immediately triggers a second, JS-initiated hard
                // navigation, which exceeds OBSCURA_NAV_CHAIN_LIMIT=1.
                let body = "<!doctype html><html><body>\
                        <script>location.href = '/next.html';</script>\
                    </body></html>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{address}/")
}

async fn cdp(
    ctx: &mut CdpContext,
    id: u64,
    method: &str,
    params: Value,
    session_id: Option<&str>,
) -> Result<Value, String> {
    let response = dispatch(
        &CdpRequest {
            id,
            method: method.to_string(),
            params,
            session_id: session_id.map(str::to_string),
        },
        ctx,
    )
    .await;
    match response.error {
        Some(e) => Err(e.message),
        None => Ok(response.result.unwrap_or_else(|| json!({}))),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn navigate_failure_still_replays_execution_context_lifecycle() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    std::env::set_var("OBSCURA_NAV_CHAIN_LIMIT", "1");
    let mut ctx = CdpContext::new();

    let created = cdp(&mut ctx, 1, "Target.createTarget", json!({"url": "about:blank"}), None)
        .await
        .unwrap();
    let page_id = created["targetId"].as_str().unwrap().to_string();
    let attached = cdp(
        &mut ctx,
        2,
        "Target.attachToTarget",
        json!({"targetId": page_id, "flatten": true}),
        None,
    )
    .await
    .unwrap();
    let session_id = attached["sessionId"].as_str().unwrap().to_string();

    cdp(&mut ctx, 3, "Runtime.enable", json!({}), Some(&session_id))
        .await
        .unwrap();

    let navigate_event_start = ctx.pending_events.len();
    let navigated = cdp(
        &mut ctx,
        4,
        "Page.navigate",
        json!({"url": serve().await, "waitUntil": "load"}),
        Some(&session_id),
    )
    .await;

    // The navigation-chain cap must still surface as a command failure —
    // this fix does not change that contract, only what happens before it.
    assert!(
        navigated.is_err(),
        "the too-many-navigations error must still fail Page.navigate: {:?}",
        navigated
    );

    let emitted = &ctx.pending_events[navigate_event_start..];
    assert!(
        emitted
            .iter()
            .any(|event| event.method == "Runtime.executionContextsCleared"
                && event.session_id.as_deref() == Some(session_id.as_str())),
        "a Page.navigate that rebuilt the execution context and *then* failed \
         must still clear the old context, or a client keeps using object \
         handles (like Playwright's cached utility script) from the torn-down \
         V8 runtime: {:?}",
        emitted.iter().map(|e| &e.method).collect::<Vec<_>>()
    );
    assert!(
        emitted
            .iter()
            .any(|event| event.method == "Runtime.executionContextCreated"
                && event.session_id.as_deref() == Some(session_id.as_str())),
        "a Page.navigate that rebuilt the execution context and *then* failed \
         must still advertise the new document's execution context: {:?}",
        emitted.iter().map(|e| &e.method).collect::<Vec<_>>()
    );

    // The rebuilt context must actually work afterward, exactly the
    // "utilityScript.evaluate is not a function" symptom this guards against.
    let sanity = cdp(
        &mut ctx,
        5,
        "Runtime.evaluate",
        json!({"expression": "1 + 1", "returnByValue": true}),
        Some(&session_id),
    )
    .await
    .unwrap();
    assert_eq!(sanity["result"]["value"].as_f64(), Some(2.0));
}

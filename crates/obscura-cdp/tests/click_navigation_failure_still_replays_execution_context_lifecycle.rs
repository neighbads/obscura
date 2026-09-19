// Deeper regression for the same "utilityScript.evaluate is not a function"
// cascade as click_navigation_execution_context_lifecycle.rs, but triggered
// by a *failed* navigation instead of a successful one — matching the
// original bug report ("after a resource/navigation failure, the entire CDP
// execution context becomes unusable") more literally.
//
// `Page::navigate_single` can call `init_js` (rebuilding the JS runtime,
// which drops every CDP object handle for the old document) and *then* the
// overall navigation can still fail later — for example a JS-triggered
// follow-up redirect that exceeds the navigation-chain limit, or (as seen
// live against reddit.com) a post-load wait that exceeds the navigation
// deadline. Either way `process_pending_navigation()` returns `Err`, even
// though the runtime was already rebuilt.
//
// This test reproduces that deterministically with the navigation-chain
// limit (real network timing would make a timeout-based reproduction
// flaky): the clicked link lands on a page whose own inline script
// immediately triggers a second, JS-initiated navigation. With
// OBSCURA_NAV_CHAIN_LIMIT=1 that second navigation exceeds the chain cap,
// so `process_pending_navigation()` returns
// `Err(TooManyClientNavigations)` — but only *after* the first hop's
// `navigate_single` already ran `init_js` for the clicked-to page.
//
// Before the fix, the click handler's `.map_err(|e| e.to_string())?`
// propagated that Err immediately, never emitting
// Runtime.executionContextsCleared/executionContextCreated for the runtime
// that had, in fact, already been torn down and rebuilt — leaving a CDP
// client (Playwright) with a stale cached object handle for the old
// document's context.
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
                let request = String::from_utf8_lossy(&buffer[..read]);
                let body = if request.starts_with("GET /page2.html ") {
                    // Immediately triggers a second, JS-initiated hard
                    // navigation, which exceeds OBSCURA_NAV_CHAIN_LIMIT=1.
                    "<!doctype html><html><body>\
                        <script>location.href = '/page3.html';</script>\
                    </body></html>"
                } else {
                    r##"<!doctype html><html><body>
                        <a id="go" href="/page2.html">go</a>
                    </body></html>"##
                };
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
async fn click_triggered_navigation_failure_still_replays_execution_context_lifecycle() {
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

    cdp(
        &mut ctx,
        4,
        "Page.navigate",
        json!({"url": serve().await, "waitUntil": "load"}),
        Some(&session_id),
    )
    .await
    .unwrap();

    cdp(
        &mut ctx,
        5,
        "Runtime.evaluate",
        json!({
            "expression": "document.elementFromPoint = () => document.getElementById('go')"
        }),
        Some(&session_id),
    )
    .await
    .unwrap();

    cdp(
        &mut ctx,
        6,
        "Input.dispatchMouseEvent",
        json!({"type": "mousePressed", "x": 0, "y": 0, "button": "left"}),
        Some(&session_id),
    )
    .await
    .unwrap();
    let click_event_start = ctx.pending_events.len();
    let release = cdp(
        &mut ctx,
        7,
        "Input.dispatchMouseEvent",
        json!({"type": "mouseReleased", "x": 0, "y": 0, "button": "left"}),
        Some(&session_id),
    )
    .await;

    // The navigation-chain cap must still surface as a command failure —
    // this fix does not change that contract, only what happens before it.
    assert!(
        release.is_err(),
        "the too-many-navigations error must still fail the triggering command: {:?}",
        release
    );

    let emitted = &ctx.pending_events[click_event_start..];
    assert!(
        emitted
            .iter()
            .any(|event| event.method == "Runtime.executionContextsCleared"
                && event.session_id.as_deref() == Some(session_id.as_str())),
        "a click whose navigation rebuilt the execution context and *then* \
         failed must still clear the old context, or a client keeps using \
         object handles (like Playwright's cached utility script) from the \
         torn-down V8 runtime: {:?}",
        emitted.iter().map(|e| &e.method).collect::<Vec<_>>()
    );
    assert!(
        emitted
            .iter()
            .any(|event| event.method == "Runtime.executionContextCreated"
                && event.session_id.as_deref() == Some(session_id.as_str())),
        "a click whose navigation rebuilt the execution context and *then* \
         failed must still advertise the new document's execution context: \
         {:?}",
        emitted.iter().map(|e| &e.method).collect::<Vec<_>>()
    );

    // The rebuilt context must actually work afterward, exactly the
    // "utilityScript.evaluate is not a function" symptom this guards
    // against.
    let sanity = cdp(
        &mut ctx,
        8,
        "Runtime.evaluate",
        json!({"expression": "1 + 1", "returnByValue": true}),
        Some(&session_id),
    )
    .await
    .unwrap();
    assert_eq!(sanity["result"]["value"].as_f64(), Some(2.0));
}

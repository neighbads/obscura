// Regression for the "utilityScript.evaluate is not a function" cascade: a
// click that navigates the page to a new document (an <a href> whose default
// action runs `location.assign`, not a same-document `history.pushState`)
// tears down the old V8 runtime and builds a fresh one (`Page::init_js`), the
// same as `Page.navigate` and a JS-initiated `location.href = ...` do. Those
// two other triggers already replay the CDP execution-context lifecycle
// (`Runtime.executionContextsCleared` + `Runtime.executionContextCreated`) so
// a client drops any object handle it cached for the old document — in
// particular Playwright's per-context cached "utility script" handle it
// reuses on every `page.evaluate()`. The `Input.dispatchMouseEvent` click
// path used to only emit `Page.frameNavigated`, so a client never learned the
// old context died and kept calling `Runtime.callFunctionOn` with a stale
// objectId, which is what actually threw `utilityScript.evaluate is not a
// function` on every subsequent evaluate.
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
                    "<!doctype html><html><body><p id=\"dest\">page2</p></body></html>"
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
) -> Value {
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
    assert!(
        response.error.is_none(),
        "CDP {method} failed: {:?}",
        response.error
    );
    response.result.unwrap_or_else(|| json!({}))
}

#[tokio::test(flavor = "current_thread")]
async fn click_triggered_document_navigation_replays_execution_context_lifecycle() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let mut ctx = CdpContext::new();

    let created = cdp(
        &mut ctx,
        1,
        "Target.createTarget",
        json!({"url": "about:blank"}),
        None,
    )
    .await;
    let page_id = created["targetId"].as_str().unwrap().to_string();
    let attached = cdp(
        &mut ctx,
        2,
        "Target.attachToTarget",
        json!({"targetId": page_id, "flatten": true}),
        None,
    )
    .await;
    let session_id = attached["sessionId"].as_str().unwrap().to_string();

    // Runtime.enable is what makes the dispatcher record execution-context
    // events for this session at all — without it `runtime_sessions_for_page`
    // is empty and neither the old nor the fixed path would emit anything.
    cdp(&mut ctx, 3, "Runtime.enable", json!({}), Some(&session_id)).await;

    cdp(
        &mut ctx,
        4,
        "Page.navigate",
        json!({"url": serve().await, "waitUntil": "load"}),
        Some(&session_id),
    )
    .await;

    // A real click resolves its target via document.elementFromPoint; stub it
    // so the synthetic (0,0) event lands on the link regardless of layout.
    cdp(
        &mut ctx,
        5,
        "Runtime.evaluate",
        json!({
            "expression": "document.elementFromPoint = () => document.getElementById('go')"
        }),
        Some(&session_id),
    )
    .await;

    cdp(
        &mut ctx,
        6,
        "Input.dispatchMouseEvent",
        json!({"type": "mousePressed", "x": 0, "y": 0, "button": "left"}),
        Some(&session_id),
    )
    .await;
    let click_event_start = ctx.pending_events.len();
    cdp(
        &mut ctx,
        7,
        "Input.dispatchMouseEvent",
        json!({"type": "mouseReleased", "x": 0, "y": 0, "button": "left"}),
        Some(&session_id),
    )
    .await;

    let emitted = &ctx.pending_events[click_event_start..];

    let frame_navigated = emitted.iter().find(|event| {
        event.method == "Page.frameNavigated" && event.params["frame"]["id"] == page_id
    });
    assert!(
        frame_navigated.is_some(),
        "click-triggered navigation must still emit Page.frameNavigated: {:?}",
        emitted.iter().map(|e| &e.method).collect::<Vec<_>>()
    );
    assert!(
        frame_navigated.unwrap().params["frame"]["url"]
            .as_str()
            .unwrap()
            .ends_with("/page2.html"),
        "frameNavigated must report the destination document: {:?}",
        frame_navigated
    );

    assert!(
        emitted
            .iter()
            .any(|event| event.method == "Runtime.executionContextsCleared"
                && event.session_id.as_deref() == Some(session_id.as_str())),
        "a click that loads a new document must clear the old execution \
         context the same way Page.navigate does, or a client keeps using \
         object handles (like Playwright's cached utility script) from the \
         torn-down V8 runtime: {:?}",
        emitted.iter().map(|e| &e.method).collect::<Vec<_>>()
    );
    assert!(
        emitted
            .iter()
            .any(|event| event.method == "Runtime.executionContextCreated"
                && event.session_id.as_deref() == Some(session_id.as_str())),
        "a click that loads a new document must advertise the new document's \
         execution context, or a client never rebuilds handles for it: {:?}",
        emitted.iter().map(|e| &e.method).collect::<Vec<_>>()
    );

    // The new document's context must actually work afterward.
    let title = cdp(
        &mut ctx,
        8,
        "Runtime.evaluate",
        json!({
            "expression": "document.getElementById('dest').textContent",
            "returnByValue": true,
        }),
        Some(&session_id),
    )
    .await;
    assert_eq!(title["result"]["value"], json!("page2"));
}

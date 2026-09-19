//! R-19: pressing Enter in a form field must behave like real Chrome:
//!
//! 1. Synthetic keydown/keyup/keypress `KeyboardEvent`s must expose the
//!    legacy `keyCode`/`which` numbers (derived from CDP's
//!    `windowsVirtualKeyCode`), not just `.key`/`.code`. Pages written
//!    before `event.key` existed — including Bing's own homepage search
//!    box — gate their Enter-to-search handler on `event.keyCode === 13`
//!    and silently never run it if `keyCode` is always 0.
//! 2. The engine's own default action for Enter (submit the nearest form)
//!    must not run when the page's own keydown/keypress listener already
//!    called `preventDefault()` — otherwise the engine's default and the
//!    page's own handling both fire, double-submitting or racing.
//!
//! Regression test for the Bing homepage search box, which relies on both:
//! its keydown handler checks `event.keyCode`, and on recognizing Enter it
//! preventDefault()s to submit the search its own way instead of a plain
//! form submit.

use obscura_cdp::dispatch::{dispatch, CdpContext};
use obscura_cdp::types::CdpRequest;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// Serves a page whose input field's keydown listener only recognizes Enter
// via the legacy `event.keyCode` (as Bing's own homepage script does) and
// preventDefault()s the keydown, then submits the form itself via script
// instead of letting the engine's default submit action run.
async fn serve_form() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = socket.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let (status, body) = if req.starts_with("GET /own-submit") {
                    ("200 OK", "<html><body>own-submit</body></html>")
                } else {
                    (
                        "200 OK",
                        r#"<html><body>
<form id="f" action="/default-submit">
  <input id="q" type="text" name="q">
</form>
<script>
globalThis.keyCodeSeen = null;
globalThis.whichSeen = null;
document.getElementById('q').addEventListener('keydown', function(e) {
  keyCodeSeen = e.keyCode;
  whichSeen = e.which;
  if (e.keyCode === 13) {
    e.preventDefault();
    location.href = '/own-submit';
  }
});
</script>
</body></html>"#,
                    )
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(resp.as_bytes()).await.unwrap();
            });
        }
    });
    format!("http://{addr}/")
}

async fn cdp(ctx: &mut CdpContext, id: u64, method: &str, params: Value, session_id: &str) -> Value {
    let resp = dispatch(
        &CdpRequest {
            id,
            method: method.to_string(),
            params,
            session_id: Some(session_id.to_string()),
        },
        ctx,
    )
    .await;
    assert!(resp.error.is_none(), "CDP {method} failed: {:?}", resp.error);
    resp.result.unwrap_or_else(|| json!({}))
}

async fn navigate(ctx: &mut CdpContext, url: &str, session_id: &str) {
    cdp(ctx, 1, "Page.navigate", json!({"url": url, "waitUntil": "load"}), session_id).await;
}

#[tokio::test(flavor = "current_thread")]
async fn enter_exposes_key_code_and_lets_a_prevent_defaulting_handler_own_submit() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let url = serve_form().await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session_id = "session-1";
    ctx.sessions.insert(session_id.to_string(), page_id.clone());

    navigate(&mut ctx, &url, session_id).await;

    cdp(
        &mut ctx,
        2,
        "Runtime.evaluate",
        json!({"expression": "document.getElementById('q').focus()"}),
        session_id,
    )
    .await;

    cdp(
        &mut ctx,
        3,
        "Input.dispatchKeyEvent",
        json!({"type": "keyDown", "key": "Enter", "code": "Enter", "text": "\r", "windowsVirtualKeyCode": 13}),
        session_id,
    )
    .await;
    cdp(
        &mut ctx,
        4,
        "Input.dispatchKeyEvent",
        json!({"type": "keyUp", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13}),
        session_id,
    )
    .await;

    let seen = cdp(
        &mut ctx,
        5,
        "Runtime.evaluate",
        json!({"expression": "JSON.stringify({keyCode: keyCodeSeen, which: whichSeen})", "returnByValue": true}),
        session_id,
    )
    .await;
    let seen: Value = serde_json::from_str(seen["result"]["value"].as_str().unwrap()).unwrap();
    assert_eq!(
        seen["keyCode"], 13,
        "KeyboardEvent.keyCode must carry CDP's windowsVirtualKeyCode so legacy keyCode-checking handlers (Bing's own) can recognize Enter"
    );
    assert_eq!(seen["which"], 13, "KeyboardEvent.which must mirror keyCode");

    let page = ctx.get_page_mut(&page_id).unwrap();
    assert_eq!(
        page.url.as_ref().unwrap().path(),
        "/own-submit",
        "a keydown listener that recognizes Enter via keyCode and preventDefault()s must be able to \
         own the submit itself, instead of the engine's default form-submit action also firing"
    );
}

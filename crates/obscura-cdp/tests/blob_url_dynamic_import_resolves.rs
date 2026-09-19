// `URL.createObjectURL` mints a `blob:` URL for an in-memory Blob, not a
// network resource. Bing's search-results page loads one of its bundle
// chunks via `import(blobURL)` (a common no-eval-generated-code pattern), and
// before this fix `ObscuraModuleLoader` routed every `blob:` specifier to the
// network client, which rejects the scheme outright ("Forbidden URL scheme
// 'blob'") — so the dynamic import always rejected and the page's bootstrap
// never finished. Regression test: a Blob containing a small ES module must
// be importable back through the URL `createObjectURL` returns.

use obscura_cdp::dispatch::{dispatch, CdpContext};
use obscura_cdp::types::CdpRequest;
use serde_json::{json, Value};

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

#[tokio::test(flavor = "current_thread")]
async fn dynamic_import_of_a_blob_url_module_resolves() {
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session_id = "session-1";
    ctx.sessions.insert(session_id.to_string(), page_id.clone());

    cdp(
        &mut ctx,
        1,
        "Page.navigate",
        json!({"url": "data:text/html,<div></div>", "waitUntil": "load"}),
        session_id,
    )
    .await;

    let v = cdp(
        &mut ctx,
        2,
        "Runtime.evaluate",
        json!({
            "expression": "(async () => {\
                const blob = new Blob(['export const value = 42;'], {type: 'text/javascript'});\
                const url = URL.createObjectURL(blob);\
                try {\
                    const mod = await import(url);\
                    return 'ok:' + mod.value;\
                } catch (e) {\
                    return 'rejected:' + (e && e.message || e);\
                }\
            })()",
            "awaitPromise": true,
            "returnByValue": true,
        }),
        session_id,
    )
    .await;

    let value = v["result"]["value"].as_str().unwrap_or("");
    assert_eq!(
        value, "ok:42",
        "dynamic import(blobURL) must resolve the Blob's module content instead of \
         being routed to the network client (got {:?})",
        value
    );
}

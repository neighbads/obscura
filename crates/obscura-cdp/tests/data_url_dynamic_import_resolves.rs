// `data:` URLs embed a module's bytes directly in the specifier (RFC 2397).
// The Fetch spec's "scheme fetch" algorithm has an explicit `data` branch,
// and "fetch a single module script" does not restrict module specifiers to
// http/https/file — Node.js and every major browser support
// `import("data:text/javascript,...")`. Before this fix `ObscuraModuleLoader`
// routed every `data:` module specifier to the network client (used for real
// HTTP module fetches), which enforces an http/https/file-only scheme
// whitelist and rejects it outright ("Forbidden URL scheme 'data' - only
// http, https, and file are allowed") — breaking pages (e.g. Reddit's
// bot-challenge script) that load a module this way. This only concerns
// module/sub-resource loading; top-level *navigation* to `data:` is a
// separate, already-existing code path and is untouched by this fix.

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
async fn dynamic_import_of_a_plain_data_url_module_resolves() {
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
                const url = 'data:text/javascript,export const value = 42;';\
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
        "dynamic import('data:text/javascript,...') must resolve the inline module \
         instead of being routed to the network client's scheme whitelist (got {:?})",
        value
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_import_of_a_base64_data_url_module_resolves() {
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

    // base64("export const value = 7;") — covers the `;base64` branch of
    // RFC 2397, not just the percent-encoded literal branch.
    let v = cdp(
        &mut ctx,
        2,
        "Runtime.evaluate",
        json!({
            "expression": "(async () => {\
                const url = 'data:text/javascript;base64,ZXhwb3J0IGNvbnN0IHZhbHVlID0gNzs=';\
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
        value, "ok:7",
        "dynamic import() of a base64-encoded data: URL module must decode and \
         resolve the module (got {:?})",
        value
    );
}

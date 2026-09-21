// Named access on Window (HTML §7.3.3) exposes every element id as a property
// of the WindowProperties object, which sits in Window's prototype chain. An
// assignment `window.foo = x` is therefore an ordinary [[Set]] that creates an
// own data property on Window and shadows the element.
//
// Obscura defined those names as getter-only own accessors on globalThis, so
// the assignment threw `TypeError: Cannot set property ... which has only a
// getter` under a strict-mode bundle. That killed DuckDuckGo's homepage:
// Next.js writes `window.__NEXT_DATA__` while the served markup carries
// `<script id="__NEXT_DATA__">`, so hydration aborted and the document stayed
// at the server-rendered shell.

use std::io::{Read, Write};

use obscura::Browser;

fn spawn_server() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut stream) = incoming else {
                continue;
            };
            std::thread::spawn(move || {
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                let body = r#"<!doctype html><html><head><title>fixture</title></head><body>
<script id="__NEXT_DATA__" type="application/json">{"props":{}}</script>
<div id="plain"></div>
<div id="untouched"></div>
</body></html>"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    format!("http://{}", addr)
}

#[tokio::test(flavor = "current_thread")]
async fn assigning_over_a_named_element_shadows_it() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let base = spawn_server();

    let browser = Browser::new().unwrap();
    let mut page = browser.new_page().await.unwrap();
    page.goto(&base).await.unwrap();

    let probes = page.evaluate(
        r#"(function () {
            'use strict';
            var out = {};
            // Before any assignment the id resolves to the element.
            out.before_tag = window.__NEXT_DATA__ ? window.__NEXT_DATA__.tagName : null;

            // The assignment a strict-mode bundle makes must not throw.
            out.assign_error = null;
            try { window.__NEXT_DATA__ = { props: { page: '/' } }; }
            catch (e) { out.assign_error = String(e && e.message || e); }
            out.after_is_object = typeof window.__NEXT_DATA__ === 'object'
                && window.__NEXT_DATA__ !== null
                && window.__NEXT_DATA__.tagName === undefined;
            out.after_value = window.__NEXT_DATA__ && window.__NEXT_DATA__.props
                ? window.__NEXT_DATA__.props.page : null;

            // The shadowing property is an ordinary own data property.
            var d = Object.getOwnPropertyDescriptor(window, '__NEXT_DATA__');
            out.is_data_property = !!d && 'value' in d;
            out.writable = !!d && d.writable === true;
            out.configurable = !!d && d.configurable === true;

            // A second write goes straight through.
            window.__NEXT_DATA__ = 42;
            out.rewritten = window.__NEXT_DATA__;

            // Bare-global assignment (no `window.` prefix) behaves the same.
            out.plain_error = null;
            try { plain = 'shadowed'; }
            catch (e) { out.plain_error = String(e && e.message || e); }
            out.plain_value = window.plain;

            // An id nobody assigned over still resolves to its element.
            out.untouched_tag = window.untouched ? window.untouched.tagName : null;
            return out;
        })()"#,
    );

    assert_eq!(probes["before_tag"], "SCRIPT");
    assert_eq!(probes["assign_error"], serde_json::Value::Null);
    assert_eq!(probes["after_is_object"], true);
    assert_eq!(probes["after_value"], "/");
    assert_eq!(probes["is_data_property"], true);
    assert_eq!(probes["writable"], true);
    assert_eq!(probes["configurable"], true);
    assert_eq!(probes["rewritten"], 42);
    assert_eq!(probes["plain_error"], serde_json::Value::Null);
    assert_eq!(probes["plain_value"], "shadowed");
    assert_eq!(probes["untouched_tag"], "DIV");
}

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

pub struct TestServer {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<String>>>,
    cache_counter: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl TestServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind 127.0.0.1:0");
        let addr = listener.local_addr().expect("local_addr");
        let hits = Arc::new(Mutex::new(Vec::new()));
        let hits_clone = Arc::clone(&hits);
        let cache_counter = Arc::new(AtomicUsize::new(0));
        let cache_clone = Arc::clone(&cache_counter);

        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let hits_req = Arc::clone(&hits_clone);
                let cache_req = Arc::clone(&cache_clone);
                tokio::spawn(async move {
                    let mut buf = Vec::with_capacity(4096);
                    let mut tmp = [0u8; 2048];
                    let header_end = loop {
                        match stream.read(&mut tmp).await {
                            Ok(0) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                                    break pos;
                                }
                                if buf.len() > 65536 {
                                    return;
                                }
                            }
                            Err(_) => return,
                        }
                    };

                    let header_text = String::from_utf8_lossy(&buf[..header_end]);
                    let mut lines = header_text.lines();
                    let req_line = lines.next().unwrap_or("GET / HTTP/1.1");
                    let mut parts = req_line.split_whitespace();
                    let method = parts.next().unwrap_or("GET");
                    let raw_target = parts.next().unwrap_or("/");
                    let (path, query) = match raw_target.split_once('?') {
                        Some((p, q)) => (p, q),
                        None => (raw_target, ""),
                    };

                    let mut headers = BTreeMap::new();
                    for line in lines {
                        if let Some((k, v)) = line.split_once(':') {
                            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                        }
                    }

                    hits_req.lock().push(format!("{method} {raw_target}"));

                    if (method, path) == ("GET", "/sse-stream") {
                        let hdr = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\n\r\ndata: hello\n\n";
                        if stream.write_all(hdr.as_bytes()).await.is_ok() {
                            let _ = stream.flush().await;
                            tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
                            let _ = stream.write_all(b"data: done\n\n").await;
                        }
                        let _ = stream.shutdown().await;
                        return;
                    }

                    let (status, content_type, extra_headers, body): (
                        &str,
                        &str,
                        Vec<(&str, String)>,
                        Vec<u8>,
                    ) = match (method, path) {
                        ("GET", "/") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Mimikyu M1 Home</title></head>
<body>
  <h1>Ready</h1>
  <button id="btn" onclick="window.__clicked = (window.__clicked || 0) + 1; document.getElementById('status').textContent = 'clicked:' + window.__clicked;">Go</button>
  <input id="inp" type="text" placeholder="Type here" />
  <input id="file-inp" type="file" />
  <div id="status">idle</div>
  <a id="next-link" href="/target">Next</a>
  <script>
    window.__mainSecret = 'main-only-value';
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/redirect") => (
                            "302 Found",
                            "text/plain",
                            vec![("Location", "/target".to_string())],
                            Vec::new(),
                        ),
                        ("GET", "/target") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html><html><head><title>Redirect Target</title></head><body><p id="ok">arrived</p></body></html>"#
                                .to_vec(),
                        ),
                        ("GET", "/echo-headers") | ("GET", "/echo-env") | ("GET", "/api/modify-headers") => (
                            "200 OK",
                            "application/json",
                            vec![],
                            serde_json::to_vec(&headers).unwrap_or_default(),
                        ),
                        ("GET", "/console") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html><html><head><title>Console</title></head><body><script>console.log("m1-log", 42);</script><p>logged</p></body></html>"#
                                .to_vec(),
                        ),
                        ("GET", "/cookie") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![("Set-Cookie", "mimikyu_test=1; Path=/".to_string())],
                            br#"<!doctype html><html><head><title>Cookie</title></head><body>cookie set</body></html>"#
                                .to_vec(),
                        ),
                        ("GET", "/network") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html><html><head><title>Network</title></head><body><div id="net">network</div></body></html>"#
                                .to_vec(),
                        ),
                        ("GET", "/blocked.png") | ("GET", "/assets/pic.png") | ("GET", "/assets/avatar") | ("GET", "/assets/sub-avatar") => (
                            "200 OK",
                            "image/png",
                            vec![],
                            vec![137, 80, 78, 71, 13, 10, 26, 10],
                        ),
                        ("GET", "/assets/clip.mp4") => (
                            "200 OK",
                            "video/mp4",
                            vec![],
                            b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom".to_vec(),
                        ),
                        ("GET", "/assets/tone") => (
                            "200 OK",
                            "audio/mpeg",
                            vec![],
                            b"ID3\x03\x00\x00\x00\x00\x00\x00".to_vec(),
                        ),
                        ("GET", "/assets/test.woff2") | ("GET", "/assets/font") => (
                            "200 OK",
                            "font/woff2",
                            vec![],
                            b"wOF2\x00\x01\x00\x00".to_vec(),
                        ),
                        ("GET", "/api/real") => (
                            "200 OK",
                            "application/json",
                            vec![],
                            br#"{"real":true}"#.to_vec(),
                        ),
                        ("GET", "/api/mock") => (
                            "200 OK",
                            "application/json",
                            vec![],
                            br#"{"real":true}"#.to_vec(),
                        ),
                        ("GET", "/api/data") => (
                            "200 OK",
                            "application/json",
                            vec![],
                            br#"{"data":"live"}"#.to_vec(),
                        ),
                        ("GET", "/api/abort-me") => (
                            "200 OK",
                            "application/json",
                            vec![],
                            br#"{"aborted":false}"#.to_vec(),
                        ),
                        ("GET", "/api/sub-mock") => (
                            "200 OK",
                            "application/json",
                            vec![],
                            br#"{"sub":"real"}"#.to_vec(),
                        ),
                        ("GET", "/isolation") | ("GET", "/iso/blank-host") | ("GET", "/geo-page") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html><html><head><title>Isolation</title></head><body><div id="iso">isolated</div></body></html>"#
                                .to_vec(),
                        ),
                        ("GET", "/iso/set-all") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![
                                ("Set-Cookie", "iso_http=http_secret; Path=/; HttpOnly; SameSite=Lax".to_string()),
                                ("Set-Cookie", "iso_js=js_secret; Path=/".to_string()),
                            ],
                            br#"<!doctype html>
<html>
<head><title>Iso Set</title></head>
<body>
  <div id="state">pending</div>
  <script>
    localStorage.setItem('iso_ls', 'ls_secret');
    sessionStorage.setItem('iso_ss', 'ss_secret');
    const req = indexedDB.open('iso_db', 1);
    req.onupgradeneeded = (e) => {
      const db = e.target.result;
      if (!db.objectStoreNames.contains('kv')) db.createObjectStore('kv');
    };
    req.onsuccess = (e) => {
      const db = e.target.result;
      const tx = db.transaction('kv', 'readwrite');
      tx.objectStore('kv').put('idb_secret', 'k1');
      tx.oncomplete = () => {
        document.getElementById('state').textContent = 'ready';
        window.__isoReady = true;
      };
    };
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/iso/echo-cookies") => {
                            let cookie = headers.get("cookie").cloned().unwrap_or_default();
                            (
                                "200 OK",
                                "application/json",
                                vec![],
                                serde_json::to_vec(&serde_json::json!({ "cookie": cookie }))
                                    .unwrap_or_default(),
                            )
                        }
                        ("GET", "/iso/clear-cookies") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![
                                ("Set-Cookie", "iso_http=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax".to_string()),
                                ("Set-Cookie", "iso_js=; Path=/; Max-Age=0".to_string()),
                            ],
                            br#"<!doctype html><html><body>cleared</body></html>"#.to_vec(),
                        ),
                        ("GET", "/iso/cache-counter") => {
                            let n = cache_req.fetch_add(1, Ordering::SeqCst) + 1;
                            (
                                "200 OK",
                                "application/json",
                                vec![("Cache-Control", "public, max-age=3600".to_string())],
                                serde_json::to_vec(&serde_json::json!({ "hits": n }))
                                    .unwrap_or_default(),
                            )
                        }
                        ("GET", "/env-probe") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Env Probe</title></head>
<body>
  <pre id="main-out"></pre>
  <pre id="frame-out"></pre>
  <pre id="worker-out"></pre>
  <pre id="mworker-out"></pre>
  <iframe id="env-frame" src="/env-probe-frame"></iframe>
  <script>
    let webglVendor = null;
    let webglRenderer = null;
    let glVendor = null;
    let glRenderer = null;
    let glVersion = null;
    try {
      const c = document.createElement('canvas');
      const gl = c.getContext('webgl2') || c.getContext('webgl') || c.getContext('experimental-webgl');
      if (gl) {
        glVendor = gl.getParameter(gl.VENDOR);
        glRenderer = gl.getParameter(gl.RENDERER);
        glVersion = gl.getParameter(gl.VERSION);
        const ext = gl.getExtension('WEBGL_debug_renderer_info');
        if (ext) {
          webglVendor = gl.getParameter(ext.UNMASKED_VENDOR_WEBGL);
          webglRenderer = gl.getParameter(ext.UNMASKED_RENDERER_WEBGL);
        }
      }
    } catch (e) {}
    let audioSampleRate = null;
    let audioBaseLatency = null;
    try {
      const AC = window.AudioContext || window.webkitAudioContext;
      if (AC) {
        const ac = new AC();
        audioSampleRate = ac.sampleRate;
        audioBaseLatency = ac.baseLatency;
        ac.close().catch(() => {});
      }
    } catch (e) {}
    let rtcBlocked = false;
    try {
      new RTCPeerConnection();
    } catch (e) {
      rtcBlocked = true;
    }
    document.getElementById('main-out').textContent = JSON.stringify({
      userAgent: navigator.userAgent,
      platform: navigator.platform,
      language: navigator.language,
      languages: Array.from(navigator.languages || []),
      hardwareConcurrency: navigator.hardwareConcurrency,
      deviceMemory: navigator.deviceMemory ?? null,
      webdriver: navigator.webdriver,
      userAgentData: navigator.userAgentData ? {
        platform: navigator.userAgentData.platform,
        mobile: navigator.userAgentData.mobile,
        brands: navigator.userAgentData.brands
      } : null,
      screenWidth: screen.width,
      screenHeight: screen.height,
      innerWidth: window.innerWidth,
      innerHeight: window.innerHeight,
      devicePixelRatio: window.devicePixelRatio,
      timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
      intlLocale: Intl.DateTimeFormat().resolvedOptions().locale,
      tzOffset: new Date().getTimezoneOffset(),
      webglVendor,
      webglRenderer,
      glVendor,
      glRenderer,
      glVersion,
      audioSampleRate,
      audioBaseLatency,
      rtcBlocked,
      noMimikyuGlobal: window.__mimikyu === undefined
    });
    window.addEventListener('message', (ev) => {
      if (ev.data && ev.data.__kind === 'frame') {
        document.getElementById('frame-out').textContent = JSON.stringify(ev.data.payload);
      }
    });
    const w = new Worker('/env-worker.js');
    w.onmessage = (ev) => {
      document.getElementById('worker-out').textContent = JSON.stringify(ev.data);
    };
    w.onerror = (err) => {
      document.getElementById('worker-out').textContent = JSON.stringify({ error: String(err.message || err) });
    };
    const mw = new Worker('/env-worker.mjs', { type: 'module' });
    mw.onmessage = (ev) => {
      document.getElementById('mworker-out').textContent = JSON.stringify(ev.data);
    };
    mw.onerror = (err) => {
      document.getElementById('mworker-out').textContent = JSON.stringify({ error: String(err.message || err) });
    };
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/env-probe-frame") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Env Frame</title></head>
<body>
  <script>
    const payload = {
      userAgent: navigator.userAgent,
      platform: navigator.platform,
      language: navigator.language,
      languages: Array.from(navigator.languages || []),
      hardwareConcurrency: navigator.hardwareConcurrency,
      deviceMemory: navigator.deviceMemory ?? null,
      webdriver: navigator.webdriver,
      screenWidth: screen.width,
      screenHeight: screen.height,
      timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
      noMimikyuGlobal: window.__mimikyu === undefined
    };
    window.parent.postMessage({ __kind: 'frame', payload }, '*');
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/env-worker.js") => (
                            "200 OK",
                            "application/javascript; charset=utf-8",
                            vec![],
                            br#"self.postMessage({
  userAgent: self.navigator.userAgent,
  platform: self.navigator.platform,
  language: self.navigator.language,
  languages: Array.from(self.navigator.languages || []),
  hardwareConcurrency: self.navigator.hardwareConcurrency,
  deviceMemory: self.navigator.deviceMemory ?? null,
  timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
  tzOffset: new Date().getTimezoneOffset()
});"#
                                .to_vec(),
                        ),
                        ("GET", "/helper.mjs") => (
                            "200 OK",
                            "application/javascript; charset=utf-8",
                            vec![],
                            br#"export const helperTag = 'relative-helper-ok';
export function helperMeta() {
  return {
    helperUrl: import.meta.url,
    platformFromHelper: self.navigator.platform,
    uaFromHelper: self.navigator.userAgent,
    tzFromHelper: Intl.DateTimeFormat().resolvedOptions().timeZone
  };
}"#
                                .to_vec(),
                        ),
                        ("GET", "/env-worker.mjs") => (
                            "200 OK",
                            "application/javascript; charset=utf-8",
                            vec![],
                            br#"import { helperTag, helperMeta } from './helper.mjs';
const payload = {
  userAgent: self.navigator.userAgent,
  platform: self.navigator.platform,
  language: self.navigator.language,
  languages: Array.from(self.navigator.languages || []),
  hardwareConcurrency: self.navigator.hardwareConcurrency,
  deviceMemory: self.navigator.deviceMemory ?? null,
  timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
  tzOffset: new Date().getTimezoneOffset(),
  helperTag,
  helperMeta: helperMeta(),
  workerMetaUrl: import.meta.url
};
self.postMessage(payload);
export default payload;"#
                                .to_vec(),
                        ),
                        ("GET", "/echo-worker.mjs") => {
                            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                            (
                                "200 OK",
                                "application/javascript; charset=utf-8",
                                vec![],
                                br#"import { helperTag, helperMeta } from './helper.mjs';
const onmessageMsgs = [];
const listenerMsgs = [];
const emit = (e) => {
  if (onmessageMsgs.length === listenerMsgs.length && onmessageMsgs.length > 0) {
    self.postMessage({
      onmessageMsgs: onmessageMsgs.slice(),
      listenerMsgs: listenerMsgs.slice(),
      origin: e.origin,
      lastEventId: e.lastEventId,
      portsLen: e.ports ? e.ports.length : 0,
      helperTag,
      helperMeta: helperMeta(),
      workerMetaUrl: import.meta.url,
      userAgent: self.navigator.userAgent,
      platform: self.navigator.platform,
      language: self.navigator.language,
      languages: Array.from(self.navigator.languages || []),
      hardwareConcurrency: self.navigator.hardwareConcurrency,
      timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
      tzOffset: new Date().getTimezoneOffset()
    });
  }
};
self.onmessage = (e) => {
  onmessageMsgs.push(e.data);
  if (e.ports && e.ports.length > 0) {
    e.ports[0].postMessage('port-pong:' + (e.data && e.data.seq));
  }
  emit(e);
};
self.addEventListener('message', (e) => {
  listenerMsgs.push(e.data);
  emit(e);
});"#
                                    .to_vec(),
                            )
                        }
                        ("GET", "/echo-fast-worker.mjs") => (
                            "200 OK",
                            "application/javascript; charset=utf-8",
                            vec![],
                            br#"import { helperTag, helperMeta } from './helper.mjs';
const onmessageMsgs = [];
const listenerMsgs = [];
const emit = (e) => {
  if (onmessageMsgs.length === listenerMsgs.length && onmessageMsgs.length > 0) {
    self.postMessage({
      onmessageMsgs: onmessageMsgs.slice(),
      listenerMsgs: listenerMsgs.slice(),
      origin: e.origin,
      lastEventId: e.lastEventId,
      portsLen: e.ports ? e.ports.length : 0,
      helperTag,
      helperMeta: helperMeta(),
      workerMetaUrl: import.meta.url,
      userAgent: self.navigator.userAgent,
      platform: self.navigator.platform,
      language: self.navigator.language,
      languages: Array.from(self.navigator.languages || []),
      hardwareConcurrency: self.navigator.hardwareConcurrency,
      timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
      tzOffset: new Date().getTimezoneOffset()
    });
  }
};
self.onmessage = (e) => {
  onmessageMsgs.push(e.data);
  emit(e);
};
self.addEventListener('message', (e) => {
  listenerMsgs.push(e.data);
  emit(e);
});"#
                                .to_vec(),
                        ),
                        ("GET", "/shared-echo-worker.mjs") => {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            (
                                "200 OK",
                                "application/javascript; charset=utf-8",
                                vec![],
                                br#"import { helperTag, helperMeta } from './helper.mjs';
let connects = 0;
self.onconnect = (e) => {
  connects += 1;
  const connIndex = connects;
  const port = e.ports[0];
  port.onmessage = (ev) => {
    port.postMessage({
      echoed: ev.data,
      connIndex,
      helperTag,
      helperMeta: helperMeta(),
      workerMetaUrl: import.meta.url,
      userAgent: self.navigator.userAgent,
      platform: self.navigator.platform,
      language: self.navigator.language,
      languages: Array.from(self.navigator.languages || []),
      hardwareConcurrency: self.navigator.hardwareConcurrency,
      timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
      tzOffset: new Date().getTimezoneOffset()
    });
  };
};"#
                                    .to_vec(),
                            )
                        }
                        ("GET", "/module-worker-probe") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Module Worker Probe</title></head>
<body>
  <pre id="main-out"></pre>
  <pre id="delay-out"></pre>
  <pre id="port-out"></pre>
  <pre id="fast-out"></pre>
  <pre id="err404-out"></pre>
  <pre id="shared1-out"></pre>
  <pre id="shared2-out"></pre>
  <script>
    document.getElementById('main-out').textContent = JSON.stringify({
      userAgent: navigator.userAgent,
      platform: navigator.platform,
      language: navigator.language,
      languages: Array.from(navigator.languages || []),
      hardwareConcurrency: navigator.hardwareConcurrency,
      timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
      tzOffset: new Date().getTimezoneOffset()
    });

    // 1. Delayed ES module Worker with immediate postMessage + MessageChannel port
    const ch = new MessageChannel();
    const wDelay = new Worker('/echo-worker.mjs', { type: 'module' });
    wDelay.onmessage = (ev) => {
      if (ev.data && Array.isArray(ev.data.onmessageMsgs) && ev.data.onmessageMsgs.length === 2) {
        document.getElementById('delay-out').textContent = JSON.stringify(ev.data);
      }
    };
    wDelay.onerror = (err) => {
      document.getElementById('delay-out').textContent = JSON.stringify({ error: String(err.message || err) });
    };
    wDelay.postMessage({ ping: 'immediate-delayed', seq: 1 });
    wDelay.postMessage({ ping: 'immediate-delayed-port', seq: 2 }, [ch.port2]);
    ch.port1.onmessage = (ev) => {
      document.getElementById('port-out').textContent = JSON.stringify({ portMsg: ev.data });
    };

    // 2. Fast (0ms) ES module Worker with immediate postMessage
    const wFast = new Worker('/echo-fast-worker.mjs', { type: 'module' });
    wFast.onmessage = (ev) => {
      if (ev.data && Array.isArray(ev.data.onmessageMsgs) && ev.data.onmessageMsgs.length === 2) {
        document.getElementById('fast-out').textContent = JSON.stringify(ev.data);
      }
    };
    wFast.onerror = (err) => {
      document.getElementById('fast-out').textContent = JSON.stringify({ error: String(err.message || err) });
    };
    wFast.postMessage({ ping: 'immediate-fast', seq: 10 });
    wFast.postMessage({ ping: 'immediate-fast', seq: 11 });

    // 3. 404 ES module Worker onerror probe
    const w404 = new Worker('/nonexistent-404-worker.mjs', { type: 'module' });
    w404.onerror = (err) => {
      document.getElementById('err404-out').textContent = JSON.stringify({
        fired: true,
        type: err.type || 'error',
        message: String(err.message || '')
      });
    };

    // 4. SharedWorker module probe (two callers sharing same name + URL)
    if (typeof SharedWorker === 'function') {
      const sw1 = new SharedWorker('/shared-echo-worker.mjs', { type: 'module', name: 'sw-mod' });
      sw1.port.onmessage = (ev) => {
        document.getElementById('shared1-out').textContent = JSON.stringify(ev.data);
      };
      sw1.onerror = (err) => {
        document.getElementById('shared1-out').textContent = JSON.stringify({ error: String(err.message || err) });
      };
      sw1.port.postMessage({ ping: 'shared-1', seq: 101 });

      const sw2 = new SharedWorker('/shared-echo-worker.mjs', { type: 'module', name: 'sw-mod' });
      sw2.port.onmessage = (ev) => {
        document.getElementById('shared2-out').textContent = JSON.stringify(ev.data);
      };
      sw2.onerror = (err) => {
        document.getElementById('shared2-out').textContent = JSON.stringify({ error: String(err.message || err) });
      };
      sw2.port.postMessage({ ping: 'shared-2', seq: 102 });
    } else {
      document.getElementById('shared1-out').textContent = JSON.stringify({ unsupported: true });
      document.getElementById('shared2-out').textContent = JSON.stringify({ unsupported: true });
    }
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/net/page") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head>
  <title>Network Page</title>
  <style>
    @font-face { font-family: 'TF1'; src: url('/assets/test.woff2'); }
    @font-face { font-family: 'TF2'; src: url('/assets/font?id=2'); }
    body { font-family: 'TF1', 'TF2', sans-serif; }
  </style>
</head>
<body>
  <p id="hello">Hello font</p>
  <img src="/assets/pic.png" alt="pic" />
  <img src="/assets/avatar?id=99" alt="extless-img" />
  <video src="/assets/clip.mp4" preload="auto"></video>
  <audio src="/assets/tone?id=1" preload="auto"></audio>
  <iframe id="net-subframe" src="/net/subframe"></iframe>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/net/subframe") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Net Subframe</title></head>
<body>
  <p id="sub-ok">sub-loaded</p>
  <img src="/assets/sub-avatar?id=7" alt="sub-img" />
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/net/console-and-errors") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Console and Errors</title></head>
<body>
  <script>
    console.log('net-log-1');
    console.info('net-info-2');
    console.warn('net-warn-3');
    console.error('net-err-4');
    setTimeout(() => { throw new Error('uncaught-boom-42'); }, 10);
  </script>
  <p>done</p>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/net/console-flood") => {
                            let n = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("n="))
                                .and_then(|v| v.parse::<usize>().ok())
                                .unwrap_or(1200);
                            let html = format!(
                                r#"<!doctype html><html><head><title>Flood</title></head><body><script>
                                for (let i = 0; i < {n}; i++) {{ console.log('flood-' + i); }}
                                window.__floodDone = true;
                                </script></body></html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/iframe-page") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Iframe Host</title></head>
<body>
  <img src="/blocked.png" alt="blocked" />
  <iframe id="subframe" src="/subframe"></iframe>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/subframe") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html><html><head><title>Subframe</title></head><body><p id="sub-ok">subframe-loaded</p><img src="/blocked.png" alt="sub-blocked" /></body></html>"#
                                .to_vec(),
                        ),
                        ("GET", "/form-keys") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Form Keys</title></head>
<body>
  <form id="key-form" onsubmit="event.preventDefault(); window.__formSubmitted = true; document.getElementById('form-status').textContent = 'submitted';">
    <input id="form-inp" type="text" />
  </form>
  <div id="form-status">pending</div>
  <textarea id="ta"></textarea>
  <input id="inp2" type="text" />
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/humanize-tracker") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Humanize Tracker</title></head>
<body style="margin:0; height:900px;">
  <input id="type-target" type="text" style="position:absolute; left:100px; top:120px; width:220px; height:32px;" />
  <button id="target-btn" style="position:absolute; left:520px; top:380px; width:120px; height:44px;">Track Click</button>
  <script>
    const mouseTrack = [];
    let clickCount = 0;
    const keyTrack = [];
    const sync = () => {
      document.documentElement.dataset.mouseTrack = JSON.stringify(mouseTrack);
      document.documentElement.dataset.clickCount = String(clickCount);
      document.documentElement.dataset.keyTrack = JSON.stringify(keyTrack);
    };
    sync();
    document.addEventListener('mousemove', (e) => {
      mouseTrack.push({ x: e.clientX, y: e.clientY, t: performance.now() });
      sync();
    });
    document.getElementById('target-btn').addEventListener('click', () => {
      clickCount += 1;
      sync();
    });
    document.getElementById('type-target').addEventListener('keydown', (e) => {
      keyTrack.push({ key: e.key, t: performance.now() });
      sync();
    });
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/edge-and-scroll") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Edge and Scroll</title></head>
<body style="margin:0;">
  <button id="top-edge-btn" style="position:fixed; top:-6px; left:50px; width:100px; height:12px;">TopEdge</button>
  <button id="above-edge-btn" style="position:fixed; top:-10px; left:200px; width:80px; height:8px;">AboveEdge</button>
  <button id="bottom-right-btn" style="position:fixed; right:-8px; bottom:-8px; width:60px; height:36px;">Corner</button>
  <div style="height:2200px;">spacer</div>
  <button id="offscreen-btn" style="width:140px; height:40px;">Offscreen</button>
  <script>
    const edgeHits = [];
    const scrollEvents = [];
    const sync = () => {
      document.documentElement.dataset.edgeHits = JSON.stringify(edgeHits);
      document.documentElement.dataset.scrollEvents = JSON.stringify(scrollEvents);
    };
    sync();
    for (const id of ['top-edge-btn', 'above-edge-btn', 'bottom-right-btn', 'offscreen-btn']) {
      document.getElementById(id).addEventListener('click', () => {
        edgeHits.push(id);
        sync();
      });
    }
    window.addEventListener('wheel', (e) => {
      scrollEvents.push({ dx: e.deltaX, dy: e.deltaY });
      sync();
    });
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/forms-rich") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Forms Rich</title></head>
<body>
  <h1>Rich Form</h1>
  <div style="display:none"><button id="hidden-css-btn">HiddenCss</button></div>
  <nav aria-hidden="true"><button id="aria-hidden-btn">AriaHidden</button></nav>
  <button id="zero-rect-btn" style="width:0; height:0; padding:0; border:0; overflow:hidden;">ZeroRect</button>
  <div style="display:contents"><button id="contents-btn">ContentsBtn</button></div>

  <label for="prefilled">Username</label>
  <input id="prefilled" type="text" value="old-secret-text" />
  <textarea id="notes"></textarea>
  <div id="ce" contenteditable="true" style="border:1px solid #888; min-height:24px;">initial</div>

  <label for="country">Country</label>
  <select id="country">
    <option value="us">United States</option>
    <option value="de">Germany</option>
    <option value="jp">Japan</option>
    <option value="dis" disabled>Disabled Country</option>
  </select>

  <label><input id="agree" type="checkbox" /> Accept Terms</label>
  <input id="upload" type="file" aria-label="Upload Files" multiple />
  <details><summary id="sum-btn">More Info</summary><p id="det-body">Extra info</p></details>

  <div id="banner-slot"></div>
  <div style="height:1400px;">spacer</div>
  <button id="submit-bottom">Submit Form</button>
  <div id="form-result">waiting</div>

  <script>
    const events = [];
    const chords = [];
    const sync = () => {
      document.documentElement.dataset.events = JSON.stringify(events);
      document.documentElement.dataset.chords = JSON.stringify(chords);
    };
    sync();
    for (const id of ['prefilled', 'notes', 'ce', 'country', 'agree']) {
      const el = document.getElementById(id);
      el.addEventListener('input', () => { events.push(id + ':input'); sync(); });
      el.addEventListener('change', () => { events.push(id + ':change'); sync(); });
    }
    document.getElementById('submit-bottom').addEventListener('click', () => {
      document.getElementById('form-result').textContent = 'submitted:' + document.getElementById('prefilled').value + ':' + document.getElementById('country').value;
    });
    document.addEventListener('keydown', (e) => {
      chords.push({
        key: e.key,
        code: e.code,
        ctrlKey: e.ctrlKey,
        altKey: e.altKey,
        shiftKey: e.shiftKey,
        metaKey: e.metaKey,
      });
      sync();
    });
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/api/delayed-items") => {
                            let delay_ms = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("delay="))
                                .and_then(|v| v.parse::<u64>().ok())
                                .unwrap_or(200)
                                .min(2000);
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            (
                                "200 OK",
                                "application/json",
                                vec![],
                                br#"{"items":["alpha","beta"]}"#.to_vec(),
                            )
                        }
                        ("GET", "/spa-delayed") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>SPA Delayed</title></head>
<body>
  <h1>SPA Dashboard</h1>
  <div id="moving-box" style="position:relative; left:0px; width:120px; height:40px; background:#369;">Loading</div>
  <ul id="items"></ul>
  <script>
    setTimeout(async () => {
      const res = await fetch('/api/delayed-items?delay=200');
      const data = await res.json();
      let step = 0;
      const box = document.getElementById('moving-box');
      const timer = setInterval(() => {
        step += 1;
        box.style.left = (step * 25) + 'px';
        if (step >= 4) {
          clearInterval(timer);
          box.textContent = 'Settled';
          const ul = document.getElementById('items');
          for (const item of data.items) {
            const li = document.createElement('li');
            li.textContent = item;
            ul.appendChild(li);
          }
          const badge = document.createElement('div');
          badge.id = 'hydrated-badge';
          badge.textContent = 'HYDRATED_OK';
          document.body.appendChild(badge);
        }
      }, 35);
    }, 80);
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/spa-short-polls") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Short Polls</title></head>
<body>
  <div id="poll-status">running</div>
  <script>
    (async () => {
      for (let i = 0; i < 3; i++) {
        await fetch('/api/delayed-items?delay=15');
        await new Promise(r => setTimeout(r, 40));
      }
      const d = document.createElement('div');
      d.id = 'polls-done';
      d.textContent = 'POLLS_FINISHED';
      document.body.appendChild(d);
    })();
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/slow-iframe") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Slow Iframe</title></head>
<body>
  <p>slow</p>
  <script>
    fetch('/api/delayed-items?delay=500').catch(() => {});
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/destroyed-iframe-fetch") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Destroyed Iframe</title></head>
<body>
  <iframe id="ephemeral-frame" src="/slow-iframe"></iframe>
  <div id="status">pending</div>
  <script>
    setTimeout(() => {
      const fr = document.getElementById('ephemeral-frame');
      if (fr) fr.remove();
      document.getElementById('status').textContent = 'removed';
    }, 80);
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/nested-frames-shadow") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Nested Frames and Shadow</title></head>
<body>
  <h1>Shadow and Frame Host</h1>
  <div id="open-host"></div>
  <div id="closed-host-1"></div>
  <iframe id="outer-frame" src="/outer-frame" style="width:420px; height:260px; border:2px solid #333;"></iframe>
  <div id="shadow-status">idle</div>
  <script>
    window.__mainSecret = 'top-secret';
    const openRoot = document.getElementById('open-host').attachShadow({ mode: 'open' });
    openRoot.innerHTML = '<button id="open-btn">OpenShadowBtn</button><span id="open-out">open-idle</span>';
    openRoot.getElementById('open-btn').addEventListener('click', () => {
      openRoot.getElementById('open-out').textContent = 'open-clicked';
    });

    const c1 = document.getElementById('closed-host-1').attachShadow({ mode: 'closed' });
    const c2Host = document.createElement('div');
    c2Host.id = 'closed-host-2';
    c1.appendChild(c2Host);
    const c2 = c2Host.attachShadow({ mode: 'closed' });
    c2.innerHTML = '<p>ClosedHeading</p><button id="closed-btn">ClosedShadowBtn</button><input id="closed-inp" aria-label="ClosedInput" type="text" /><a href="/target">ClosedLink</a><table><thead><tr><th>C1</th></tr></thead><tbody><tr><td>ClosedCell</td></tr></tbody></table>';
    c2.getElementById('closed-btn').addEventListener('click', () => {
      document.getElementById('shadow-status').textContent = 'closed-clicked:' + c2.getElementById('closed-inp').value;
    });
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/outer-frame") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Outer Frame</title></head>
<body>
  <button id="outer-btn">OuterBtn</button>
  <iframe id="inner-frame" src="/inner-frame" style="width:360px; height:180px; border:2px solid #666;"></iframe>
  <script>
    window.__outerSecret = 'outer-secret';
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/inner-frame") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Inner Frame</title></head>
<body>
  <button id="deep-btn" onclick="document.getElementById('deep-status').textContent='deep-clicked:' + document.getElementById('deep-inp').value;">DeepBtn</button>
  <input id="deep-inp" type="text" placeholder="DeepInput" />
  <div id="deep-status">idle</div>
  <script>
    window.__innerSecret = 'inner-secret';
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/extract-article") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head>
  <title>Extract Article</title>
  <style>
    @keyframes slide {
      from { transform: translateX(0px); }
      to { transform: translateX(120px); }
    }
    .anim-active {
      animation: slide 2s linear 1 forwards;
    }
  </style>
</head>
<body>
  <nav aria-hidden="true"><a href="/hidden-noise">HiddenNav</a></nav>
  <div style="display:none"><p>HiddenDisplayNone</p><a href="/hidden-none">HiddenLink</a></div>
  <main>
    <h1>Main Title</h1>
    <h2>Section One</h2>
    <p>Paragraph with <a href="/rel">Relative Link</a> and <a href="https://example.org/ext">External Link</a> and <a href="/icon"><img alt="Icon Alt" src="/blocked.png" /></a>.</p>
    <ul>
      <li>Item 1
        <ul><li>Sub 1</li></ul>
      </li>
      <li>Item 2</li>
    </ul>
    <pre><code class="language-rust">fn hello() { println!("hi"); }</code></pre>
    <blockquote>Quoted wisdom</blockquote>
    <table id="t1">
      <caption>Plan Matrix</caption>
      <thead><tr><th>Tier</th><th colspan="2">Specs</th></tr></thead>
      <tbody>
        <tr><td rowspan="2">Pro</td><td>CPU</td><td>8</td></tr>
        <tr><td>RAM</td><td>32G</td></tr>
      </tbody>
    </table>
    <table id="outer-tbl">
      <tbody><tr><td>OuterCell<table><tbody><tr><td>InnerCell</td></tr></tbody></table></td></tr></tbody>
    </table>
    <div id="art-open-shadow"></div>
    <div id="anim-box" style="width:80px; height:30px; background:#493;">Anim</div>
  </main>
  <div style="height:2000px;">spacer</div>
  <div id="offscreen-card" style="width:240px; height:80px; background:#225588; color:#fff;">Offscreen Card</div>
  <button id="offscreen-ref-btn" style="width:220px; height:70px; background:rgb(18, 52, 86); color:#fff;">Offscreen Ref Target</button>
  <script>
    const host = document.getElementById('art-open-shadow');
    const sr = host.attachShadow({ mode: 'open' });
    sr.innerHTML = '<p>OpenShadowParagraph</p><a href="/open-shadow-link">OpenShadowLink</a>';
    const ab = document.getElementById('anim-box');
    ab.dataset.animEnded = 'false';
    ab.addEventListener('animationend', () => { ab.dataset.animEnded = 'true'; });
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/spa-sse") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>SPA SSE</title></head>
<body>
  <div id="sse-status">init</div>
  <script>
    const es = new EventSource('/sse-stream');
    es.onmessage = () => {};
    fetch('/api/delayed-items?delay=100')
      .then(r => r.json())
      .then(() => {
        document.getElementById('sse-status').textContent = 'SSE_FETCH_OK';
      });
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/slow-iframe-900") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Slow Iframe 900</title></head>
<body>
  <p>slow900</p>
  <script>
    fetch('/api/delayed-items?delay=900').catch(() => {});
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/destroyed-iframe-fetch-900") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Destroyed Iframe 900</title></head>
<body>
  <iframe id="ephemeral-frame-900" src="/slow-iframe-900"></iframe>
  <div id="status">pending</div>
  <script>
    setTimeout(() => {
      const fr = document.getElementById('ephemeral-frame-900');
      if (fr) fr.remove();
      document.getElementById('status').textContent = 'removed';
    }, 80);
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/forms-zero-file") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Zero File Input</title></head>
<body>
  <input id="file-zero" type="file" aria-label="Zero File" style="width:0; height:0; opacity:0; padding:0; border:0;" />
  <input id="file-hidden-display" type="file" aria-label="Hidden Display File" style="display:none;" />
  <input id="file-hidden-vis" type="file" aria-label="Hidden Vis File" style="visibility:hidden;" />
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/offset-iframe-host") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Offset Iframe Host</title></head>
<body>
  <h1>Offset Host</h1>
  <iframe id="offset-frame" src="/offset-iframe-child" style="display:block; margin:40px; border:4px solid #333; padding:12px; width:360px; height:200px;"></iframe>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/offset-iframe-child") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Offset Iframe Child</title></head>
<body>
  <button id="offset-btn" onclick="document.getElementById('offset-status').textContent='offset-clicked'">OffsetAction</button>
  <div id="offset-status">idle</div>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/xo-child") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>XO Child</title></head>
<body>
  <button id="xo-btn" onclick="document.getElementById('xo-status').textContent='xo-clicked:' + document.getElementById('xo-inp').value + ':' + document.getElementById('xo-sel').value">XOClick</button>
  <input id="xo-inp" type="text" aria-label="XOInput" />
  <select id="xo-sel" aria-label="XOSelect">
    <option value="a">Alpha</option>
    <option value="b">Beta</option>
  </select>
  <input id="xo-file" type="file" aria-label="XOFile" />
  <div id="xo-status">xo-idle</div>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/mixed-xo-host") => {
                            let xo_url = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("xo="))
                                .unwrap_or("/xo-child");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>Mixed XO Host</title></head>
<body>
  <h1>Mixed Host</h1>
  <iframe id="so-frame" src="/offset-iframe-child" style="display:block; margin:20px; border:2px solid #222; padding:8px; width:320px; height:120px;"></iframe>
  <iframe id="xo-frame" src="{xo_url}" style="display:block; margin:30px; border:5px solid #444; padding:10px; width:380px; height:220px;"></iframe>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/iframe-closed-shadow-host") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Iframe Closed Shadow Host</title></head>
<body>
  <h1>Main Host Heading</h1>
  <iframe id="cs-frame" src="/iframe-closed-shadow-child" style="width:320px; height:180px;"></iframe>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/iframe-closed-shadow-child") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Iframe Closed Shadow Child</title></head>
<body>
  <div id="sub-cs-host"></div>
  <script>
    const sr = document.getElementById('sub-cs-host').attachShadow({ mode: 'closed' });
    sr.innerHTML = '<p>SubClosedHeading</p><button id="sub-cs-btn">SubClosedBtn</button>';
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/xo-closed-shadow-host") => {
                            let xo_url = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("xo="))
                                .unwrap_or("/xo-closed-shadow-child");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>XO Closed Shadow Host</title></head>
<body>
  <h1>XOMainHeading</h1>
  <iframe id="xo-cs-frame" src="{xo_url}" style="display:block; margin:24px; border:3px solid #333; padding:8px; width:420px; height:260px;"></iframe>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/xo-closed-shadow-child") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>XO Closed Shadow Child</title></head>
<body>
  <h2>XOPlainHeading</h2>
  <div id="xo-cs-host"></div>
  <script>
    const sr = document.getElementById('xo-cs-host').attachShadow({ mode: 'closed' });
    sr.innerHTML = '<p>XOClosedHeading</p><button id="xo-cs-btn">XOClosedBtn</button><a href="/xo-cs-link">XOClosedLink</a><table><thead><tr><th>XHdr</th></tr></thead><tbody><tr><td>XOClosedCell</td></tr></tbody></table>';
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/nested-so-xo-host") => {
                            let xo_url = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("xo="))
                                .unwrap_or("/xo-child");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>Nested SO XO Host</title></head>
<body>
  <h1>Nested SO XO Host</h1>
  <iframe id="mid-so-frame" src="/nested-so-xo-middle?xo={xo_url}" style="display:block; margin:20px; border:4px solid #222; padding:10px; width:460px; height:320px;"></iframe>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/nested-so-xo-middle") => {
                            let xo_url = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("xo="))
                                .unwrap_or("/xo-child");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>Nested SO XO Middle</title></head>
<body>
  <button id="mid-btn">MidAction</button>
  <iframe id="inner-xo-frame" src="{xo_url}" style="display:block; margin:16px; border:3px solid #555; padding:8px; width:380px; height:220px;"></iframe>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/iframe-chord-host") => {
                            let xo_url = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("xo="))
                                .unwrap_or("/xo-chord-child");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>Iframe Chord Host</title></head>
<body>
  <h1>Chord Host</h1>
  <iframe id="so-chord-frame" src="/so-chord-child" style="display:block; margin:12px; width:360px; height:120px;"></iframe>
  <iframe id="xo-chord-frame" src="{xo_url}" style="display:block; margin:12px; width:360px; height:120px;"></iframe>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/so-chord-child") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>SO Chord Child</title></head>
<body>
  <input id="so-chord-inp" type="text" aria-label="SOChordInput" value="so-sentinel" />
  <script>
    const chords = [];
    const sync = () => { document.documentElement.dataset.chords = JSON.stringify(chords); };
    sync();
    for (const evType of ['keydown', 'keyup']) {
      document.addEventListener(evType, (e) => {
        chords.push({ type: evType, key: e.key, code: e.code, altKey: e.altKey });
        sync();
      });
    }
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/xo-chord-child") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>XO Chord Child</title></head>
<body>
  <input id="xo-chord-inp" type="text" aria-label="XOChordInput" value="xo-sentinel" />
  <script>
    const chords = [];
    const sync = () => { document.documentElement.dataset.chords = JSON.stringify(chords); };
    sync();
    for (const evType of ['keydown', 'keyup']) {
      document.addEventListener(evType, (e) => {
        chords.push({ type: evType, key: e.key, code: e.code, altKey: e.altKey });
        sync();
      });
    }
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/nested-oopif-root") => {
                            let mid_url = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("mid="))
                                .unwrap_or("/nested-oopif-mid");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>Nested OOPIF Root</title></head>
<body>
  <h1>NestedOOPIFRoot</h1>
  <button id="nroot-btn">NRootBtn</button>
  <iframe id="nmid-frame" name="nested-mid" src="{mid_url}" style="display:block; margin:24px; border:4px solid #222; padding:10px; width:480px; height:320px;"></iframe>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/nested-oopif-mid") => {
                            let leaf_url = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("leaf="))
                                .unwrap_or("/nested-oopif-leaf");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>Nested OOPIF Mid</title></head>
<body>
  <p>NestedOOPIFMid</p>
  <iframe id="nleaf-frame" name="nested-leaf" src="{leaf_url}" style="display:block; margin:18px; border:3px solid #555; padding:8px; width:400px; height:220px;"></iframe>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/nested-oopif-leaf") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Nested OOPIF Leaf</title></head>
<body>
  <button id="nleaf-btn" onclick="document.getElementById('nleaf-status').textContent='nleaf-clicked'">NLeafBtn</button>
  <div id="nleaf-status">idle</div>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/oopif-same-sub-host") => {
                            let outer_url = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("outer="))
                                .unwrap_or("/oopif-outer");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>OOPIF Same Sub Host</title></head>
<body>
  <button id="sshost-btn">MainBtn</button>
  <iframe id="oopif-outer-frame" name="oopif-outer" src="{outer_url}" style="display:block; margin:30px; border:4px solid #222; padding:10px; width:460px; height:300px;"></iframe>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/oopif-outer") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>OOPIF Outer</title></head>
<body>
  <p>OuterOOPIFPara</p>
  <iframe id="oopif-inner-frame" name="oopif-inner" src="/oopif-inner" style="display:block; margin:26px; border:3px solid #444; padding:12px; width:360px; height:180px;"></iframe>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/oopif-inner") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>OOPIF Inner</title></head>
<body>
  <button id="inner-sub-btn" onclick="document.getElementById('inner-sub-status').textContent='inner-sub-clicked'">OOPIFSameSubBtn</button>
  <div id="inner-sub-status">idle</div>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/iframe-exec-ctx-host") => {
                            let xo_url = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("xo="))
                                .unwrap_or("/xo-ctx-child");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>Iframe Exec Ctx Host</title></head>
<body>
  <iframe id="so-ctx-frame" src="/so-ctx-child" style="width:240px; height:90px;"></iframe>
  <iframe id="xo-ctx-frame" src="{xo_url}" style="width:240px; height:90px;"></iframe>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/so-ctx-child") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>SO Ctx Child</title></head>
<body>
  <p>SOChild</p>
  <script>window.__soMarker = 'so-alive-42';</script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/xo-ctx-child") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>XO Ctx Child</title></head>
<body>
  <p>XOChild1</p>
  <script>window.__xoMarker = 'xo-1';</script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/xo-ctx-child-2") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>XO Ctx Child 2</title></head>
<body>
  <p>XOChild2</p>
  <script>window.__xoMarker = 'xo-2';</script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/iframe-focus-host") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Iframe Focus Host</title></head>
<body>
  <input id="main-inp" type="text" aria-label="MainFocusInput" value="main-init" />
  <iframe id="focus-so-frame" src="/so-chord-child" style="display:block; margin:12px; width:360px; height:120px;"></iframe>
  <script>
    const mchords = [];
    const msync = () => { document.documentElement.dataset.chords = JSON.stringify(mchords); };
    msync();
    for (const evType of ['keydown', 'keyup']) {
      document.addEventListener(evType, (e) => {
        mchords.push({ type: evType, key: e.key, code: e.code, altKey: e.altKey });
        msync();
      });
    }
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/nested-iframe-focus-host") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Nested Iframe Focus Host</title></head>
<body>
  <h1>Nested Focus Host</h1>
  <iframe id="outer-ifr" src="/nested-iframe-focus-outer" style="display:block; margin:10px; width:440px; height:320px;"></iframe>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/nested-iframe-focus-outer") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Nested Iframe Focus Outer</title></head>
<body>
  <button id="outer-focus-btn" onclick="document.getElementById('outer-status').textContent='outer-clicked'">OuterFocusBtn</button>
  <div id="outer-status">idle</div>
  <iframe id="inner-1" src="/nested-iframe-focus-inner1" style="display:block; margin:8px; width:380px; height:110px;"></iframe>
  <iframe id="inner-2" src="/nested-iframe-focus-inner2" style="display:block; margin:8px; width:380px; height:110px;"></iframe>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/nested-iframe-focus-inner1") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Nested Focus Inner 1</title></head>
<body>
  <input id="in1-inp" type="text" aria-label="Inner1Input" value="in1-sentinel" />
  <script>
    const chords = [];
    const sync = () => { document.documentElement.dataset.chords = JSON.stringify(chords); };
    sync();
    for (const evType of ['keydown', 'keyup']) {
      document.addEventListener(evType, (e) => {
        chords.push({ type: evType, key: e.key, code: e.code, altKey: e.altKey });
        sync();
      });
    }
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/nested-iframe-focus-inner2") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Nested Focus Inner 2</title></head>
<body>
  <input id="in2-inp" type="text" aria-label="Inner2Input" value="in2-sentinel" />
  <script>
    const chords = [];
    const fevs = [];
    const sync = () => {
      document.documentElement.dataset.chords = JSON.stringify(chords);
      document.documentElement.dataset.fevs = JSON.stringify(fevs);
    };
    sync();
    for (const evType of ['keydown', 'keyup']) {
      document.addEventListener(evType, (e) => {
        chords.push({ type: evType, key: e.key, code: e.code, altKey: e.altKey });
        sync();
      });
    }
    for (const evType of ['mousedown', 'focus', 'click']) {
      document.addEventListener(evType, (e) => {
        fevs.push(evType + ':' + (e.target && e.target.id ? e.target.id : ''));
        sync();
      }, true);
    }
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/xo-iframe-wait-host") => {
                            let xo_url = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("xo="))
                                .unwrap_or("/xo-chord-child");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>XO Iframe Wait Host</title></head>
<body>
  <div id="xo-host-ok">HOST_OK</div>
  <iframe id="xo-wait-frame" src="{xo_url}" style="width:320px; height:120px;"></iframe>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/xo-destroyed-iframe-host") => {
                            let xo_url = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("xo="))
                                .unwrap_or("/slow-iframe-doc?delay=900");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>XO Destroyed Iframe Host</title></head>
<body>
  <iframe id="xo-ephemeral-frame" src="{xo_url}"></iframe>
  <div id="status">pending</div>
  <script>
    setTimeout(() => {{
      const fr = document.getElementById('xo-ephemeral-frame');
      if (fr) fr.remove();
      document.getElementById('status').textContent = 'removed';
    }}, 80);
  </script>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/slow-iframe-doc") => {
                            let delay_ms = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("delay="))
                                .and_then(|v| v.parse::<u64>().ok())
                                .unwrap_or(900)
                                .min(2000);
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                br#"<!doctype html><html><head><title>Slow Doc</title></head><body><p>slow-doc-ok</p></body></html>"#
                                    .to_vec(),
                            )
                        }
                        ("GET", "/so-chord-child-2") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>SO Chord Child 2</title></head>
<body>
  <input id="so2-chord-inp" type="text" aria-label="SO2ChordInput" value="so2-sentinel" />
  <script>
    const chords = [];
    const fevs = [];
    const sync = () => {
      document.documentElement.dataset.chords = JSON.stringify(chords);
      document.documentElement.dataset.fevs = JSON.stringify(fevs);
    };
    sync();
    for (const evType of ['keydown', 'keyup']) {
      document.addEventListener(evType, (e) => {
        chords.push({ type: evType, key: e.key, code: e.code, altKey: e.altKey });
        sync();
      });
    }
    for (const evType of ['mousedown', 'focus', 'click']) {
      document.addEventListener(evType, (e) => {
        fevs.push(evType + ':' + (e.target && e.target.id ? e.target.id : ''));
        sync();
      }, true);
    }
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/shadow-iframe-focus-host") => {
                            let xo_url = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("xo="))
                                .unwrap_or("/xo-chord-child");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>Shadow Iframe Focus Host</title></head>
<body>
  <div id="open-sh-host"></div>
  <input id="main-after-sh" type="text" aria-label="MainAfterShadowInput" value="main-sh-init" style="display:block; margin:12px;" />
  <div id="closed-sh-host"></div>
  <div id="xo-closed-sh-host"></div>
  <script>
    const mchords = [];
    const msync = () => {{ document.documentElement.dataset.chords = JSON.stringify(mchords); }};
    msync();
    for (const evType of ['keydown', 'keyup']) {{
      document.addEventListener(evType, (e) => {{
        mchords.push({{ type: evType, key: e.key, code: e.code, altKey: e.altKey }});
        msync();
      }});
    }}
    const openSr = document.getElementById('open-sh-host').attachShadow({{ mode: 'open' }});
    openSr.innerHTML = '<iframe id="open-sh-ifr" src="/so-chord-child" style="display:block; margin:10px; width:360px; height:90px;"></iframe>';
    const closedSr = document.getElementById('closed-sh-host').attachShadow({{ mode: 'closed' }});
    closedSr.innerHTML = '<iframe id="closed-sh-ifr" src="/so-chord-child-2" style="display:block; margin:14px; border:3px solid #333; padding:6px; width:360px; height:90px;"></iframe>';
    const xoClosedSr = document.getElementById('xo-closed-sh-host').attachShadow({{ mode: 'closed' }});
    xoClosedSr.innerHTML = '<iframe id="xo-closed-sh-ifr" src="{xo_url}" style="display:block; margin:14px; border:3px solid #555; padding:6px; width:360px; height:90px;"></iframe>';
  </script>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/xo-oopif-fetch-child") => {
                            let delay_ms = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("ms="))
                                .and_then(|v| v.parse::<u64>().ok())
                                .unwrap_or(600)
                                .min(2000);
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>XO OOPIF Fetch Child</title></head>
<body>
  <div id="oopif-fetch-status">OOPIF_PENDING</div>
  <script>
    fetch('/api/delayed-items?delay={delay_ms}')
      .then(r => r.json())
      .then(() => {{
        document.getElementById('oopif-fetch-status').textContent = 'OOPIF_FETCH_DONE';
      }});
  </script>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/xo-oopif-sse-child") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>XO OOPIF SSE Child</title></head>
<body>
  <div id="oopif-sse-status">OOPIF_SSE_PENDING</div>
  <script>
    const es = new EventSource('/sse-stream');
    window.__oopifEs = es;
    fetch('/api/delayed-items?delay=250')
      .then(r => r.json())
      .then(() => {
        document.getElementById('oopif-sse-status').textContent = 'OOPIF_SSE_OK';
      });
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/regular-so-iframe-host") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Regular SO Iframe Host</title></head>
<body>
  <input id="main-inp" type="text" aria-label="MainFocusInput" value="main-init" style="display:block; margin:8px;" />
  <iframe id="reg-so-ifr" src="/so-chord-child" style="display:block; margin:10px; width:360px; height:90px;"></iframe>
  <iframe src="/so-chord-child-2" style="display:block; margin:10px; width:360px; height:90px;"></iframe>
  <script>
    const mchords = [];
    const msync = () => { document.documentElement.dataset.chords = JSON.stringify(mchords); };
    msync();
    for (const evType of ['keydown', 'keyup']) {
      document.addEventListener(evType, (e) => {
        mchords.push({ type: evType, key: e.key, code: e.code, altKey: e.altKey });
        msync();
      });
    }
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/so-upload-host") => {
                            let xo_url = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("xo="))
                                .unwrap_or("/xo-upload-outer");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>SO Upload Host</title></head>
<body>
  <iframe id="so-up-with-id" src="/so-upload-child?tag=withid" style="display:block; margin:8px; width:320px; height:70px;"></iframe>
  <iframe src="/so-upload-child?tag=noid" style="display:block; margin:8px; width:320px; height:70px;"></iframe>
  <iframe id="so-up-outer" src="/so-upload-outer" style="display:block; margin:8px; width:360px; height:110px;"></iframe>
  <iframe id="xo-up-outer" src="{xo_url}" style="display:block; margin:8px; width:360px; height:110px;"></iframe>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/so-upload-outer") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>SO Upload Outer</title></head>
<body>
  <iframe src="/so-upload-child?tag=nested2" style="display:block; margin:6px; width:320px; height:80px;"></iframe>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/xo-upload-outer") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>XO Upload Outer</title></head>
<body>
  <iframe src="/so-upload-child?tag=xoso" style="display:block; margin:6px; width:320px; height:80px;"></iframe>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/so-upload-child") => {
                            let tag = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("tag="))
                                .unwrap_or("default");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>SO Upload Child {tag}</title></head>
<body>
  <input id="file-inp-{tag}" type="file" aria-label="Upload-{tag}" />
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/shadow-scale-host") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Shadow Scale Host</title></head>
<body>
  <div id="sc-closed-host" style="transform: scale(1.6); transform-origin: top left; width: 320px;"></div>
  <div id="sc-reg-wrap" style="margin-top: 100px; transform: scale(1.5); transform-origin: top left; width: 320px;">
    <iframe id="sc-reg-ifr" src="/scaled-child-reg" style="display:block; margin:10px; border:2px solid #555; padding:4px; width:400px; height:100px;"></iframe>
  </div>
  <script>
    const closedSr = document.getElementById('sc-closed-host').attachShadow({ mode: 'closed' });
    closedSr.innerHTML = '<iframe id="sc-ifr" src="/scaled-child" style="display:block; margin:10px; border:3px solid #333; padding:6px; width:400px; height:100px;"></iframe>';
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/scaled-child") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Scaled Child</title></head>
<body>
  <input id="scaled-inp" type="text" aria-label="ScaledInput" style="margin-left:180px; margin-top:40px; width:80px; height:24px;" />
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/scaled-child-reg") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Scaled Child Reg</title></head>
<body>
  <input id="scaled-reg-inp" type="text" aria-label="ScaledRegInput" style="margin-left:180px; margin-top:40px; width:80px; height:24px;" />
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/scaled-complex-host") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Scaled Complex Host</title></head>
<body>
  <div style="transform: scale(1.4, 0.8); transform-origin: top left; width: 420px;">
    <iframe id="sc-nonuniform-ifr" src="/scaled-child-nonuniform" style="display:block; margin:10px; border:4px solid #333; padding:6px; width:400px; height:100px;"></iframe>
  </div>
  <div style="margin-top: 140px; transform: scale(1.5); transform-origin: top left; width: 460px;">
    <iframe id="sc-nested-outer-ifr" src="/scaled-nested-outer" style="display:block; margin:10px; border:2px solid #444; padding:4px; width:440px; height:180px;"></iframe>
  </div>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/scaled-child-nonuniform") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Scaled Child NonUniform</title></head>
<body>
  <input id="scaled-nonuniform-inp" type="text" aria-label="NonUniformScaledInput" style="margin-left:180px; margin-top:40px; width:80px; height:24px; box-sizing:border-box;" />
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/scaled-nested-outer") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Scaled Nested Outer</title></head>
<body>
  <div style="transform: scale(0.8); transform-origin: top left; width: 420px;">
    <iframe id="sc-nested-inner-ifr" src="/scaled-nested-inner" style="display:block; margin:8px; border:3px solid #666; padding:5px; width:380px; height:140px;"></iframe>
  </div>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/scaled-nested-inner") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Scaled Nested Inner</title></head>
<body>
  <input id="scaled-nested-inp" type="text" aria-label="NestedScaledInput" style="margin-left:180px; margin-top:40px; width:100px; height:30px; box-sizing:border-box;" />
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/scaled-xo-host") => {
                            let xo_url = query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("xo="))
                                .unwrap_or("/xo-scaled-child");
                            let html = format!(
                                r#"<!doctype html>
<html>
<head><title>Scaled XO Host</title></head>
<body>
  <div style="transform: scale(1.6); transform-origin: top left; width: 440px;">
    <iframe id="xo-sc-ifr" src="{xo_url}" style="display:block; margin:10px; border:4px solid #333; padding:6px; width:400px; height:120px;"></iframe>
  </div>
</body>
</html>"#
                            );
                            (
                                "200 OK",
                                "text/html; charset=utf-8",
                                vec![],
                                html.into_bytes(),
                            )
                        }
                        ("GET", "/xo-scaled-child") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>XO Scaled Child</title></head>
<body>
  <input id="xo-scaled-inp" type="text" aria-label="XOScaledInput" style="margin-left:200px; margin-top:40px; width:60px; height:24px; box-sizing:border-box;" />
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/scroll-click-batch") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Scroll Click Batch</title></head>
<body>
  <h1>Top</h1>
  <button id="scroll-target-btn" style="display:block; margin-top:900px; width:160px; height:40px;">Scroll Target</button>
  <div id="scroll-click-result">pending</div>
  <div style="height:1200px;">bottom spacer</div>
  <script>
    document.getElementById('scroll-target-btn').addEventListener('click', () => {
      document.getElementById('scroll-click-result').textContent = 'CLICKED_AT_SY:' + Math.round(window.scrollY);
    });
  </script>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/download-page") => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            vec![],
                            br#"<!doctype html>
<html>
<head><title>Download Test Page</title></head>
<body>
  <h1>Download Test</h1>
  <a id="dl-bin" href="/download/sample-rust-test.bin" download="sample-rust-test.bin">Download Sample Bin</a>
</body>
</html>"#
                                .to_vec(),
                        ),
                        ("GET", "/download/sample-rust-test.bin") => {
                            let payload: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
                            (
                                "200 OK",
                                "application/octet-stream",
                                vec![(
                                    "Content-Disposition",
                                    "attachment; filename=\"sample-rust-test.bin\"".to_string(),
                                )],
                                payload,
                            )
                        }
                        _ => (
                            "404 Not Found",
                            "text/plain",
                            vec![],
                            b"not found".to_vec(),
                        ),
                    };

                    let mut resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (k, v) in extra_headers {
                        resp.push_str(&format!("{k}: {v}\r\n"));
                    }
                    resp.push_str("\r\n");

                    let _ = stream.write_all(resp.as_bytes()).await;
                    if !body.is_empty() {
                        let _ = stream.write_all(&body).await;
                    }
                    let _ = stream.shutdown().await;
                });
            }
        });

        Self {
            addr,
            hits,
            cache_counter,
            task,
        }
    }

    #[allow(dead_code)]
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    #[allow(dead_code)]
    pub fn localhost_url(&self, path: &str) -> String {
        format!("http://localhost:{}{}", self.addr.port(), path)
    }

    #[allow(dead_code)]
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().clone()
    }

    #[allow(dead_code)]
    pub fn reset_hits(&self) {
        self.hits.lock().clear();
    }

    #[allow(dead_code)]
    pub fn cache_hits(&self) -> usize {
        self.cache_counter.load(Ordering::SeqCst)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[allow(dead_code)]
pub struct LocalHttpProxy {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl LocalHttpProxy {
    #[allow(dead_code)]
    pub async fn start(auth: Option<(String, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind proxy 127.0.0.1:0");
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let hits_clone = Arc::clone(&hits);
        let expected_auth = auth.map(|(u, p)| format!("Basic {}", BASE64.encode(format!("{u}:{p}"))));

        let task = tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    break;
                };
                let hits_req = Arc::clone(&hits_clone);
                let auth_req = expected_auth.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::with_capacity(2048);
                    let mut tmp = [0u8; 1024];
                    let pos = loop {
                        match client.read(&mut tmp).await {
                            Ok(0) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if let Some(p) = find_subslice(&buf, b"\r\n\r\n") {
                                    break p;
                                }
                                if buf.len() > 65536 {
                                    return;
                                }
                            }
                            Err(_) => return,
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..pos]);
                    let leftover = buf[pos + 4..].to_vec();
                    let mut lines = head.split("\r\n");
                    let req_line = lines.next().unwrap_or("");
                    let mut parts = req_line.split_whitespace();
                    let method = parts.next().unwrap_or("");
                    let target = parts.next().unwrap_or("");
                    let version = parts.next().unwrap_or("HTTP/1.1");
                    let rest_headers: Vec<&str> = lines.collect();

                    if let Some(ref exp) = auth_req {
                        let got = rest_headers.iter().find_map(|h| {
                            let (k, v) = h.split_once(':')?;
                            if k.trim().eq_ignore_ascii_case("proxy-authorization") {
                                Some(v.trim())
                            } else {
                                None
                            }
                        });
                        if got != Some(exp.as_str()) {
                            let _ = client
                                .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                .await;
                            return;
                        }
                    }

                    hits_req.lock().push(format!("{method} {target}"));

                    if method.eq_ignore_ascii_case("CONNECT") {
                        let Ok(mut up) = TcpStream::connect(target).await else {
                            return;
                        };
                        let _ = client
                            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                            .await;
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut up).await;
                        return;
                    }

                    let Ok(parsed) = url::Url::parse(target) else {
                        return;
                    };
                    let host = parsed.host_str().unwrap_or("127.0.0.1");
                    let port = parsed.port_or_known_default().unwrap_or(80);
                    let Ok(mut up) = TcpStream::connect((host, port)).await else {
                        return;
                    };

                    let mut path_q = parsed.path().to_string();
                    if let Some(q) = parsed.query() {
                        path_q.push('?');
                        path_q.push_str(q);
                    }
                    let mut out = format!("{method} {path_q} {version}\r\n");
                    for h in rest_headers {
                        let l = h.to_ascii_lowercase();
                        if l.starts_with("proxy-authorization:") || l.starts_with("proxy-connection:")
                        {
                            continue;
                        }
                        if !h.is_empty() {
                            out.push_str(h);
                            out.push_str("\r\n");
                        }
                    }
                    out.push_str("\r\n");
                    let _ = up.write_all(out.as_bytes()).await;
                    if !leftover.is_empty() {
                        let _ = up.write_all(&leftover).await;
                    }
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut up).await;
                });
            }
        });

        Self { addr, hits, task }
    }

    #[allow(dead_code)]
    pub fn proxy_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    #[allow(dead_code)]
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().clone()
    }
}

impl Drop for LocalHttpProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[allow(dead_code)]
pub struct LocalSocks5Proxy {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl LocalSocks5Proxy {
    #[allow(dead_code)]
    pub async fn start(auth: Option<(String, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind socks5 127.0.0.1:0");
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let hits_clone = Arc::clone(&hits);

        let task = tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    break;
                };
                let hits_req = Arc::clone(&hits_clone);
                let auth_req = auth.clone();
                tokio::spawn(async move {
                    let mut hdr = [0u8; 2];
                    if client.read_exact(&mut hdr).await.is_err() || hdr[0] != 0x05 {
                        return;
                    }
                    let mut methods = vec![0u8; hdr[1] as usize];
                    if client.read_exact(&mut methods).await.is_err() {
                        return;
                    }

                    if let Some((exp_u, exp_p)) = auth_req {
                        if !methods.contains(&0x02) {
                            let _ = client.write_all(&[0x05, 0xff]).await;
                            return;
                        }
                        if client.write_all(&[0x05, 0x02]).await.is_err() {
                            return;
                        }
                        let mut ver_len = [0u8; 2];
                        if client.read_exact(&mut ver_len).await.is_err() || ver_len[0] != 0x01 {
                            return;
                        }
                        let mut ubuf = vec![0u8; ver_len[1] as usize];
                        if client.read_exact(&mut ubuf).await.is_err() {
                            return;
                        }
                        let mut plen = [0u8; 1];
                        if client.read_exact(&mut plen).await.is_err() {
                            return;
                        }
                        let mut pbuf = vec![0u8; plen[0] as usize];
                        if client.read_exact(&mut pbuf).await.is_err() {
                            return;
                        }
                        if ubuf != exp_u.as_bytes() || pbuf != exp_p.as_bytes() {
                            let _ = client.write_all(&[0x01, 0x01]).await;
                            return;
                        }
                        if client.write_all(&[0x01, 0x00]).await.is_err() {
                            return;
                        }
                    } else if client.write_all(&[0x05, 0x00]).await.is_err() {
                        return;
                    }

                    let mut req = [0u8; 4];
                    if client.read_exact(&mut req).await.is_err() || req[0] != 0x05 || req[1] != 0x01 {
                        return;
                    }
                    let host = match req[3] {
                        0x01 => {
                            let mut ip = [0u8; 4];
                            if client.read_exact(&mut ip).await.is_err() {
                                return;
                            }
                            IpAddr::V4(Ipv4Addr::from(ip)).to_string()
                        }
                        0x03 => {
                            let mut len = [0u8; 1];
                            if client.read_exact(&mut len).await.is_err() {
                                return;
                            }
                            let mut name = vec![0u8; len[0] as usize];
                            if client.read_exact(&mut name).await.is_err() {
                                return;
                            }
                            String::from_utf8_lossy(&name).to_string()
                        }
                        0x04 => {
                            let mut ip = [0u8; 16];
                            if client.read_exact(&mut ip).await.is_err() {
                                return;
                            }
                            IpAddr::V6(Ipv6Addr::from(ip)).to_string()
                        }
                        _ => return,
                    };
                    let mut pbytes = [0u8; 2];
                    if client.read_exact(&mut pbytes).await.is_err() {
                        return;
                    }
                    let port = u16::from_be_bytes(pbytes);
                    hits_req.lock().push(format!("{host}:{port}"));

                    let Ok(mut up) = TcpStream::connect((host.as_str(), port)).await else {
                        let _ = client
                            .write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                            .await;
                        return;
                    };
                    if client
                        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut up).await;
                });
            }
        });

        Self { addr, hits, task }
    }

    #[allow(dead_code)]
    pub fn proxy_url(&self) -> String {
        format!("socks5://{}", self.addr)
    }

    #[allow(dead_code)]
    pub fn socks5h_url(&self) -> String {
        format!("socks5h://{}", self.addr)
    }

    #[allow(dead_code)]
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().clone()
    }
}

impl Drop for LocalSocks5Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[allow(dead_code)]
pub fn assert_pgid_gone(pgid: libc::pid_t) {
    let rc = unsafe { libc::kill(-pgid, 0) };
    assert_eq!(rc, -1, "process group {pgid} still alive");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[allow(dead_code)]
async fn read_http_head_from_buf(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
) -> Option<usize> {
    if let Some(pos) = find_subslice(buf, b"\r\n\r\n") {
        return Some(pos);
    }
    let mut tmp = [0u8; 2048];
    loop {
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = find_subslice(buf, b"\r\n\r\n") {
                    return Some(pos);
                }
                if buf.len() > 65536 {
                    return None;
                }
            }
        }
    }
}

#[allow(dead_code)]
async fn copy_exact_from_buf(
    buf: &mut Vec<u8>,
    src: &mut TcpStream,
    dst: &mut TcpStream,
    mut remaining: usize,
) -> bool {
    while remaining > 0 {
        if !buf.is_empty() {
            let take = buf.len().min(remaining);
            if dst.write_all(&buf[..take]).await.is_err() {
                return false;
            }
            buf.drain(..take);
            remaining -= take;
        } else {
            let mut tmp = [0u8; 4096];
            let max_r = tmp.len().min(remaining);
            match src.read(&mut tmp[..max_r]).await {
                Ok(0) | Err(_) => return false,
                Ok(n) => {
                    if dst.write_all(&tmp[..n]).await.is_err() {
                        return false;
                    }
                    remaining -= n;
                }
            }
        }
    }
    true
}

#[allow(dead_code)]
async fn copy_chunked_from_buf(
    buf: &mut Vec<u8>,
    src: &mut TcpStream,
    dst: &mut TcpStream,
) -> bool {
    loop {
        let line_end = loop {
            if let Some(pos) = find_subslice(buf, b"\r\n") {
                break pos;
            }
            if buf.len() > 65536 {
                return false;
            }
            let mut tmp = [0u8; 1024];
            match src.read(&mut tmp).await {
                Ok(0) | Err(_) => return false,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        };
        let line: Vec<u8> = buf.drain(..line_end + 2).collect();
        if dst.write_all(&line).await.is_err() {
            return false;
        }
        let Ok(line_str) = std::str::from_utf8(&line[..line.len() - 2]) else {
            return false;
        };
        let size_hex = line_str.split(';').next().unwrap_or("").trim();
        let Ok(sz) = usize::from_str_radix(size_hex, 16) else {
            return false;
        };
        if sz == 0 {
            loop {
                let t_end = loop {
                    if let Some(pos) = find_subslice(buf, b"\r\n") {
                        break pos;
                    }
                    let mut tmp = [0u8; 1024];
                    match src.read(&mut tmp).await {
                        Ok(0) | Err(_) => return false,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                };
                let tline: Vec<u8> = buf.drain(..t_end + 2).collect();
                if dst.write_all(&tline).await.is_err() {
                    return false;
                }
                if tline == b"\r\n" {
                    return true;
                }
            }
        }
        if !copy_exact_from_buf(buf, src, dst, sz + 2).await {
            return false;
        }
    }
}

/// HTTP/1.1 Keep-Alive test server that always replies with `Connection: keep-alive`
/// and keeps sockets open across requests (serving both Content-Length and chunked endpoints).
#[allow(dead_code)]
pub struct KeepAliveTestServer {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl KeepAliveTestServer {
    #[allow(dead_code)]
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind keepalive server");
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let hits_clone = Arc::clone(&hits);

        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let hits_req = Arc::clone(&hits_clone);
                tokio::spawn(async move {
                    let mut buf = Vec::with_capacity(4096);
                    loop {
                        let Some(header_end) = read_http_head_from_buf(&mut stream, &mut buf).await
                        else {
                            break;
                        };
                        let header_text = String::from_utf8_lossy(&buf[..header_end]).into_owned();
                        buf.drain(..header_end + 4);

                        let req_line = header_text.lines().next().unwrap_or("GET / HTTP/1.1");
                        let mut parts = req_line.split_whitespace();
                        let method = parts.next().unwrap_or("GET");
                        let raw_target = parts.next().unwrap_or("/");
                        let (path, query) = match raw_target.split_once('?') {
                            Some((p, q)) => (p, q),
                            None => (raw_target, ""),
                        };

                        hits_req.lock().push(format!("{method} {raw_target}"));

                        let resp: Vec<u8> = match (method, path) {
                            ("GET", "/ka-page") => {
                                let body = br#"<!doctype html>
<html>
<head><title>KeepAlive Probe</title></head>
<body>
  <pre id="ka-out"></pre>
  <script>
    (async () => {
      const r1 = await fetch('/api/ka?n=1');
      const r2 = await fetch('/api/ka?n=2');
      const r3 = await fetch('/api/ka?n=3');
      const r4 = await fetch('/api/ka-chunked');
      document.getElementById('ka-out').textContent = JSON.stringify({
        s1: r1.status, b1: await r1.text(),
        s2: r2.status, b2: await r2.text(),
        s3: r3.status, b3: await r3.text(),
        s4: r4.status, b4: await r4.text()
      });
    })().catch((e) => {
      document.getElementById('ka-out').textContent = JSON.stringify({ error: String(e) });
    });
  </script>
</body>
</html>"#;
                                let mut r = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: keep-alive\r\nKeep-Alive: timeout=60\r\n\r\n",
                                    body.len()
                                )
                                .into_bytes();
                                r.extend_from_slice(body);
                                r
                            }
                            ("GET", "/api/ka") => {
                                let body = format!("ka:{query}");
                                let mut r = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: keep-alive\r\nKeep-Alive: timeout=60\r\n\r\n",
                                    body.len()
                                )
                                .into_bytes();
                                r.extend_from_slice(body.as_bytes());
                                r
                            }
                            ("GET", "/api/ka-chunked") => {
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n7\r\nchunked\r\n3\r\n-ok\r\n0\r\n\r\n".to_vec()
                            }
                            ("GET", "/favicon.ico") => {
                                b"HTTP/1.1 204 No Content\r\nConnection: keep-alive\r\n\r\n".to_vec()
                            }
                            _ => {
                                let body = b"not found";
                                let mut r = format!(
                                    "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                                    body.len()
                                )
                                .into_bytes();
                                r.extend_from_slice(body);
                                r
                            }
                        };

                        if stream.write_all(&resp).await.is_err() || stream.flush().await.is_err()
                        {
                            break;
                        }
                    }
                });
            }
        });

        Self { addr, hits, task }
    }

    #[allow(dead_code)]
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    #[allow(dead_code)]
    pub fn localhost_url(&self, path: &str) -> String {
        format!("http://localhost:{}{}", self.addr.port(), path)
    }

    #[allow(dead_code)]
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().clone()
    }
}

impl Drop for KeepAliveTestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Strict HTTP proxy validating `Proxy-Authorization` on EVERY keep-alive request
/// on a persistent connection and preserving upstream `Connection: keep-alive` headers.
#[allow(dead_code)]
pub struct StrictKeepAliveHttpProxy {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<String>>>,
    auth_rejects: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl StrictKeepAliveHttpProxy {
    #[allow(dead_code)]
    pub async fn start(auth: (String, String)) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind strict proxy");
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let hits_clone = Arc::clone(&hits);
        let auth_rejects = Arc::new(AtomicUsize::new(0));
        let rejects_clone = Arc::clone(&auth_rejects);
        let expected_auth = format!("Basic {}", BASE64.encode(format!("{}:{}", auth.0, auth.1)));

        let task = tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    break;
                };
                let hits_req = Arc::clone(&hits_clone);
                let rejects_req = Arc::clone(&rejects_clone);
                let exp_auth = expected_auth.clone();
                tokio::spawn(async move {
                    let mut cbuf = Vec::with_capacity(4096);
                    loop {
                        let Some(pos) = read_http_head_from_buf(&mut client, &mut cbuf).await
                        else {
                            break;
                        };
                        let head = String::from_utf8_lossy(&cbuf[..pos]).into_owned();
                        cbuf.drain(..pos + 4);

                        let mut lines = head.split("\r\n");
                        let req_line = lines.next().unwrap_or("");
                        let mut parts = req_line.split_whitespace();
                        let method = parts.next().unwrap_or("");
                        let target = parts.next().unwrap_or("");
                        let version = parts.next().unwrap_or("HTTP/1.1");
                        let rest_headers: Vec<&str> = lines.collect();

                        let got = rest_headers.iter().find_map(|h| {
                            let (k, v) = h.split_once(':')?;
                            if k.trim().eq_ignore_ascii_case("proxy-authorization") {
                                Some(v.trim())
                            } else {
                                None
                            }
                        });
                        if got != Some(exp_auth.as_str()) {
                            rejects_req.fetch_add(1, Ordering::SeqCst);
                            let _ = client
                                .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"strict\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                .await;
                            break;
                        }

                        hits_req.lock().push(format!("{method} {target}"));

                        if method.eq_ignore_ascii_case("CONNECT") {
                            let Ok(mut up) = TcpStream::connect(target).await else {
                                break;
                            };
                            let _ = client
                                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                                .await;
                            let _ = tokio::io::copy_bidirectional(&mut client, &mut up).await;
                            break;
                        }

                        let Ok(parsed) = url::Url::parse(target) else {
                            break;
                        };
                        let host = parsed.host_str().unwrap_or("127.0.0.1");
                        let port = parsed.port_or_known_default().unwrap_or(80);
                        let Ok(mut up) = TcpStream::connect((host, port)).await else {
                            break;
                        };

                        let mut path_q = parsed.path().to_string();
                        if path_q.is_empty() {
                            path_q.push('/');
                        }
                        if let Some(q) = parsed.query() {
                            path_q.push('?');
                            path_q.push_str(q);
                        }
                        let mut out = format!("{method} {path_q} {version}\r\n");
                        for h in rest_headers {
                            if let Some((k, _)) = h.split_once(':') {
                                let kl = k.trim().to_ascii_lowercase();
                                if kl == "proxy-authorization" || kl == "proxy-connection" {
                                    continue;
                                }
                            }
                            if !h.is_empty() {
                                out.push_str(h);
                                out.push_str("\r\n");
                            }
                        }
                        out.push_str("\r\n");
                        if up.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }

                        let mut ubuf = Vec::with_capacity(4096);
                        let Some(upos) = read_http_head_from_buf(&mut up, &mut ubuf).await else {
                            break;
                        };
                        let rhead_bytes = ubuf[..upos + 4].to_vec();
                        let rhead = String::from_utf8_lossy(&ubuf[..upos]).into_owned();
                        ubuf.drain(..upos + 4);

                        let mut rlines = rhead.split("\r\n");
                        let status_line = rlines.next().unwrap_or("");
                        let status_code = status_line
                            .split_whitespace()
                            .nth(1)
                            .and_then(|s| s.parse::<u16>().ok());
                        let mut is_chunked = false;
                        let mut cl: Option<usize> = None;
                        for h in rlines {
                            if let Some((k, v)) = h.split_once(':') {
                                let kl = k.trim().to_ascii_lowercase();
                                if kl == "transfer-encoding"
                                    && v.to_ascii_lowercase().contains("chunked")
                                {
                                    is_chunked = true;
                                } else if kl == "content-length"
                                    && let Ok(n) = v.trim().parse::<usize>()
                                {
                                    cl = Some(n);
                                }
                            }
                        }

                        if client.write_all(&rhead_bytes).await.is_err() {
                            break;
                        }
                        let ok = if matches!(status_code, Some(204 | 304)) {
                            true
                        } else if is_chunked {
                            copy_chunked_from_buf(&mut ubuf, &mut up, &mut client).await
                        } else if let Some(n) = cl {
                            copy_exact_from_buf(&mut ubuf, &mut up, &mut client, n).await
                        } else {
                            if !ubuf.is_empty() && client.write_all(&ubuf).await.is_err() {
                                false
                            } else {
                                tokio::io::copy(&mut up, &mut client).await.is_ok()
                            }
                        };
                        if !ok || client.flush().await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        Self {
            addr,
            hits,
            auth_rejects,
            task,
        }
    }

    #[allow(dead_code)]
    pub fn proxy_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    #[allow(dead_code)]
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().clone()
    }

    #[allow(dead_code)]
    pub fn auth_rejects(&self) -> usize {
        self.auth_rejects.load(Ordering::SeqCst)
    }
}

impl Drop for StrictKeepAliveHttpProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

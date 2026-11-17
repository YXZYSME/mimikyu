mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use mimikyu_core::driver::{
    driver_for, ContextOptions, EngineKind, LaunchOptions, PermissionGrant, PermissionState,
    ProfileMode, ProxyConfig,
};
use mimikyu_core::error::MimikyuError;
use mimikyu_core::isolation::{
    export_storage_state, import_storage_state, EphemeralDir, ProfileLock, ProfileStore,
    VirtualContextPool,
};
use support::{assert_pgid_gone, LocalHttpProxy, TestServer};

#[test]
fn profile_store_sanitization_and_lifecycle() {
    let base = tempfile::tempdir().expect("tempdir");
    let store = ProfileStore::new(base.path().to_path_buf());

    let p1 = store.resolve("agent_alpha-1").expect("resolve named");
    assert!(p1.is_named);
    assert_eq!(p1.path, base.path().join("agent_alpha-1"));

    let rel = store.resolve("./local-prof").expect("resolve relative");
    assert!(!rel.is_named);
    assert!(rel.path.is_absolute());

    let abs = store.resolve("/tmp/mimikyu-abs-prof").expect("resolve abs");
    assert!(!abs.is_named);
    assert_eq!(abs.path, std::path::PathBuf::from("/tmp/mimikyu-abs-prof"));

    let tilde = store.resolve("~/mimikyu-home-prof").expect("resolve tilde");
    assert!(!tilde.is_named);

    for bad in [".", "..", "", "-flag", "bad\0name"] {
        assert!(store.resolve(bad).is_err(), "expected error for {bad:?}");
    }
    assert!(store.resolve_named("../escape").is_err());

    let prepared = store.prepare_persistent("agent_alpha-1").expect("prepare");
    assert!(prepared.exists());

    let listed = store.list_named().expect("list_named");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "agent_alpha-1");
    assert!(!listed[0].locked);

    store.delete_named("agent_alpha-1").expect("delete_named");
    assert!(!prepared.exists());
}

#[tokio::test]
async fn persistent_profile_flock_blocks_concurrent_launch() {
    let root = tempfile::tempdir().expect("tempdir");
    let prof_dir = root.path().join("locked-prof");
    let opts = LaunchOptions {
        engine: EngineKind::Chromium,
        headless: true,
        profile_mode: ProfileMode::Persistent(prof_dir.clone()),
        ..Default::default()
    };

    let s1 = driver_for(EngineKind::Chromium)
        .launch(opts.clone())
        .await
        .expect("first persistent launch");
    let pid1 = s1.info().pid.expect("pid1");
    assert!(ProfileLock::is_locked(&prof_dir).expect("is_locked"));

    let err = driver_for(EngineKind::Chromium)
        .launch(opts.clone())
        .await
        .err()
        .expect("concurrent launch on same persistent dir must fail");
    assert!(
        matches!(err, MimikyuError::ProfileLocked(_)),
        "expected ProfileLocked, got {err:?}"
    );

    s1.dispose().await.expect("dispose s1");
    assert_pgid_gone(pid1 as libc::pid_t);
    assert!(!ProfileLock::is_locked(&prof_dir).expect("unlocked after dispose"));

    let s2 = driver_for(EngineKind::Chromium)
        .launch(opts)
        .await
        .expect("relaunch after dispose succeeds");
    let pid2 = s2.info().pid.expect("pid2");
    s2.dispose().await.expect("dispose s2");
    assert_pgid_gone(pid2 as libc::pid_t);
}

#[test]
fn ephemeral_dir_cleans_up_on_drop_and_explicit_cleanup() {
    let mut eph = EphemeralDir::new().expect("ephemeral dir");
    let p1 = eph.path().to_path_buf();
    assert!(p1.exists());
    eph.cleanup().expect("cleanup");
    assert!(!p1.exists());

    let p2 = {
        let eph2 = EphemeralDir::new().expect("ephemeral dir 2");
        let p = eph2.path().to_path_buf();
        assert!(p.exists());
        p
    };
    assert!(!p2.exists());
}

async fn assert_ephemeral_drop_cleanup(engine: EngineKind) {
    let session = driver_for(engine)
        .launch(LaunchOptions {
            engine,
            headless: true,
            profile_mode: ProfileMode::EphemeralDir,
            ..Default::default()
        })
        .await
        .expect("launch ephemeral");
    let pid = session.info().pid.expect("pid");
    let pdir = session.info().profile_dir.clone().expect("profile_dir");
    assert!(pdir.exists());
    drop(session);
    assert!(!pdir.exists(), "ephemeral dir {pdir:?} leaked");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn ephemeral_dir_drop_cleanup_chromium() {
    assert_ephemeral_drop_cleanup(EngineKind::Chromium).await;
}

#[tokio::test]
async fn ephemeral_dir_drop_cleanup_firefox() {
    assert_ephemeral_drop_cleanup(EngineKind::Firefox).await;
}

#[tokio::test]
async fn cross_engine_storage_state_export_and_pre_navigation_import() {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let state_file = tmp.path().join("cross_state.json");

    // 1. Export from Chromium
    let crm = driver_for(EngineKind::Chromium)
        .launch(LaunchOptions {
            engine: EngineKind::Chromium,
            headless: true,
            ..Default::default()
        })
        .await
        .expect("launch chromium");
    let crm_pid = crm.info().pid.expect("crm pid");

    crm.navigate(&server.url("/iso/set-all"))
        .await
        .expect("navigate /iso/set-all");
    for _ in 0..40 {
        if crm
            .eval_isolated("Boolean(window.__isoReady)")
            .await
            .ok()
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let exported = export_storage_state(crm.as_ref(), &state_file)
        .await
        .expect("export storage state");
    assert!(exported.cookies.iter().any(|c| c.name == "iso_http" && c.http_only));
    assert!(exported.cookies.iter().any(|c| c.name == "iso_js"));
    assert!(!exported.origins.is_empty());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&state_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "StorageState file must have 0600 permissions");
    }

    crm.dispose().await.expect("dispose chromium");
    assert_pgid_gone(crm_pid as libc::pid_t);

    // 2. Import into fresh about:blank Firefox BEFORE navigating
    let ffx = driver_for(EngineKind::Firefox)
        .launch(LaunchOptions {
            engine: EngineKind::Firefox,
            headless: true,
            ..Default::default()
        })
        .await
        .expect("launch firefox");
    let ffx_pid = ffx.info().pid.expect("ffx pid");

    import_storage_state(ffx.as_ref(), &state_file)
        .await
        .expect("import into fresh firefox");

    ffx.navigate(&server.url("/iso/echo-cookies"))
        .await
        .expect("navigate /iso/echo-cookies");
    let cookie_body = ffx
        .eval_isolated("document.body.innerText")
        .await
        .expect("read cookie json");
    let cookie_json: serde_json::Value =
        serde_json::from_str(cookie_body.as_str().unwrap_or("{}")).expect("parse cookie json");
    let srv_cookie = cookie_json["cookie"].as_str().unwrap_or("");
    assert!(
        srv_cookie.contains("iso_http=http_secret"),
        "HttpOnly cookie missing on Firefox after cross-engine import: {srv_cookie}"
    );
    assert!(
        srv_cookie.contains("iso_js=js_secret"),
        "JS cookie missing on Firefox after cross-engine import: {srv_cookie}"
    );

    ffx.navigate(&server.url("/iso/blank-host"))
        .await
        .expect("navigate /iso/blank-host");
    let ls_val = ffx
        .eval_isolated("localStorage.getItem('iso_ls')")
        .await
        .expect("read localStorage");
    let ss_val = ffx
        .eval_isolated("sessionStorage.getItem('iso_ss')")
        .await
        .expect("read sessionStorage");
    assert_eq!(ls_val, "ls_secret");
    assert_eq!(ss_val, "ss_secret");

    // 3. Re-export from Firefox and import into fresh about:blank Chromium
    let state_file_2 = tmp.path().join("ffx_to_crm.json");
    export_storage_state(ffx.as_ref(), &state_file_2)
        .await
        .expect("export from firefox");
    ffx.dispose().await.expect("dispose firefox");
    assert_pgid_gone(ffx_pid as libc::pid_t);

    let crm2 = driver_for(EngineKind::Chromium)
        .launch(LaunchOptions {
            engine: EngineKind::Chromium,
            headless: true,
            ..Default::default()
        })
        .await
        .expect("launch chromium 2");
    let crm2_pid = crm2.info().pid.expect("crm2 pid");

    import_storage_state(crm2.as_ref(), &state_file_2)
        .await
        .expect("import into fresh chromium");
    crm2.navigate(&server.url("/iso/echo-cookies"))
        .await
        .expect("navigate crm2");
    let body2 = crm2
        .eval_isolated("document.body.innerText")
        .await
        .expect("read cookie json 2");
    let cj2: serde_json::Value =
        serde_json::from_str(body2.as_str().unwrap_or("{}")).expect("parse cookie json 2");
    let srv_c2 = cj2["cookie"].as_str().unwrap_or("");
    assert!(srv_c2.contains("iso_http=http_secret"));
    assert_eq!(
        crm2.eval_isolated("localStorage.getItem('iso_ls')")
            .await
            .unwrap(),
        "ls_secret"
    );

    crm2.dispose().await.expect("dispose crm2");
    assert_pgid_gone(crm2_pid as libc::pid_t);
}

#[tokio::test]
async fn virtual_context_latency_chromium_and_firefox() {
    // Chromium: createBrowserContext is <1ms; full new_context (with isolated renderer process boot + 7-domain init) is ~50-65ms vs ~400ms+ cold launch.
    let pool = VirtualContextPool::launch(LaunchOptions {
        engine: EngineKind::Chromium,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch chromium pool");
    let pid = pool.root_session().info().pid.expect("pid");

    let mut durs_ms = Vec::new();
    let mut contexts = Vec::new();
    for i in 0..10 {
        let ctx = pool
            .create_labeled_context(format!("ctx-{i}"), ContextOptions::default())
            .await
            .expect("create chromium virtual context");
        durs_ms.push(ctx.create_duration().as_secs_f64() * 1000.0);
        contexts.push(ctx);
    }
    assert_eq!(pool.active_count(), 10);

    let mut sorted = durs_ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median_ms = sorted[sorted.len() / 2];
    eprintln!(
        "Chromium VirtualContext full new_context latencies (ms): {:?}, median={median_ms:.2}ms",
        durs_ms
    );
    assert!(
        median_ms < 150.0,
        "expected Chromium VirtualContext median < 150ms (pipelined init + renderer boot), got {median_ms:.2}ms (all={durs_ms:?})"
    );

    drop(contexts);
    pool.shutdown().await.expect("shutdown chromium pool");
    assert_pgid_gone(pid as libc::pid_t);

    // Firefox: measure VirtualContext creation & verify clean shutdown
    let fpool = VirtualContextPool::launch(LaunchOptions {
        engine: EngineKind::Firefox,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch firefox pool");
    let fpid = fpool.root_session().info().pid.expect("fpid");
    let fctx = fpool
        .create_labeled_context("ffx-1", ContextOptions::default())
        .await
        .expect("create firefox virtual context");
    eprintln!(
        "Firefox VirtualContext creation latency: {:.2}ms",
        fctx.create_duration().as_secs_f64() * 1000.0
    );
    assert_eq!(fpool.active_count(), 1);
    fctx.dispose().await.expect("dispose fctx");
    assert_eq!(fpool.active_count(), 0);
    fpool.shutdown().await.expect("shutdown firefox pool");
    assert_pgid_gone(fpid as libc::pid_t);
}

async fn assert_virtual_context_full_isolation(engine: EngineKind) {
    let server = TestServer::start().await;
    let proxy_a = LocalHttpProxy::start(None).await;

    let pool = VirtualContextPool::launch(LaunchOptions {
        engine,
        headless: true,
        ..Default::default()
    })
    .await
    .expect("launch pool");
    let pid = pool.root_session().info().pid.expect("pid");

    let ctx_a = pool
        .create_labeled_context(
            "tenant-a",
            ContextOptions {
                proxy: Some(ProxyConfig {
                    server: proxy_a.proxy_url(),
                    username: None,
                    password: None,
                    bypass: Vec::new(),
                }),
                permissions: vec![PermissionGrant {
                    name: "geolocation".to_string(),
                    state: PermissionState::Granted,
                    origin: Some(server.url("")),
                }],
                ..Default::default()
            },
        )
        .await
        .expect("create ctx_a");

    let ctx_b = pool
        .create_labeled_context(
            "tenant-b",
            ContextOptions {
                permissions: vec![PermissionGrant {
                    name: "geolocation".to_string(),
                    state: PermissionState::Denied,
                    origin: Some(server.url("")),
                }],
                ..Default::default()
            },
        )
        .await
        .expect("create ctx_b");

    // Populate ctx_a: cookies, localStorage, sessionStorage, IndexedDB
    ctx_a
        .session()
        .navigate(&server.url("/iso/set-all"))
        .await
        .expect("ctx_a navigate /iso/set-all");
    for _ in 0..40 {
        if ctx_a
            .session()
            .eval_isolated("Boolean(window.__isoReady)")
            .await
            .ok()
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Verify HTTP cache works within ctx_a
    let c1 = ctx_a
        .session()
        .eval_isolated("fetch('/iso/cache-counter').then(r => r.json()).then(j => j.hits)")
        .await
        .expect("ctx_a cache fetch 1");
    let c2 = ctx_a
        .session()
        .eval_isolated("fetch('/iso/cache-counter').then(r => r.json()).then(j => j.hits)")
        .await
        .expect("ctx_a cache fetch 2");
    assert_eq!(c1, c2, "{engine}: second fetch in ctx_a should hit HTTP cache");
    let hits_before_b = server.cache_hits();

    // Verify per-context proxy routed ctx_a (record hit count AFTER all ctx_a requests)
    assert!(
        !proxy_a.hits().is_empty(),
        "{engine}: proxy_a must receive ctx_a traffic"
    );
    let proxy_hits_after_a = proxy_a.hits().len();

    // Verify geolocation permission in ctx_a
    let perm_a = ctx_a
        .session()
        .eval_isolated("navigator.permissions.query({name:'geolocation'}).then(p => p.state)")
        .await
        .expect("perm_a");
    assert_eq!(perm_a, "granted", "{engine}: ctx_a geolocation permission");

    // Verify ctx_b sees ZERO state from ctx_a (cookies, localStorage, sessionStorage, IndexedDB, cache, proxy, permission)
    ctx_b
        .session()
        .navigate(&server.url("/iso/echo-cookies"))
        .await
        .expect("ctx_b /iso/echo-cookies");
    let b_cookie_body = ctx_b
        .session()
        .eval_isolated("document.body.innerText")
        .await
        .unwrap();
    let b_cj: serde_json::Value =
        serde_json::from_str(b_cookie_body.as_str().unwrap_or("{}")).unwrap();
    assert_eq!(
        b_cj["cookie"], "",
        "{engine}: ctx_b must receive zero cookies from ctx_a"
    );

    ctx_b
        .session()
        .navigate(&server.url("/iso/blank-host"))
        .await
        .expect("ctx_b /iso/blank-host");
    let b_state = ctx_b
        .session()
        .eval_isolated(
            r#"(async () => {
                const idbVal = await new Promise((resolve) => {
                    const req = indexedDB.open('iso_db', 1);
                    req.onupgradeneeded = (e) => {
                        const db = e.target.result;
                        if (!db.objectStoreNames.contains('kv')) db.createObjectStore('kv');
                    };
                    req.onsuccess = (e) => {
                        const db = e.target.result;
                        const tx = db.transaction('kv', 'readonly');
                        const getReq = tx.objectStore('kv').get('k1');
                        getReq.onsuccess = () => resolve(getReq.result ?? null);
                        getReq.onerror = () => resolve(null);
                    };
                });
                const perm = await navigator.permissions.query({ name: 'geolocation' });
                return {
                    cookie: document.cookie,
                    ls: localStorage.getItem('iso_ls'),
                    ss: sessionStorage.getItem('iso_ss'),
                    idb: idbVal,
                    perm: perm.state
                };
            })()"#,
        )
        .await
        .expect("ctx_b state probe");
    assert_eq!(b_state["cookie"], "");
    assert!(b_state["ls"].is_null());
    assert!(b_state["ss"].is_null());
    assert!(b_state["idb"].is_null());
    assert_eq!(b_state["perm"], "denied");

    // ctx_b fetching /iso/cache-counter must NOT hit ctx_a's HTTP cache
    let b_cache = ctx_b
        .session()
        .eval_isolated("fetch('/iso/cache-counter').then(r => r.json()).then(j => j.hits)")
        .await
        .expect("ctx_b cache fetch");
    assert_eq!(
        b_cache.as_u64().unwrap(),
        (hits_before_b + 1) as u64,
        "{engine}: ctx_b must not share HTTP cache with ctx_a"
    );

    // proxy_a should NOT have received ctx_b's requests (except maybe any background cache, none for ctx_b)
    assert_eq!(
        proxy_a.hits().len(),
        proxy_hits_after_a,
        "{engine}: proxy_a must only carry ctx_a traffic"
    );

    // Multi-tab within ctx_a + clean disposal
    let _tab2 = ctx_a
        .new_tab(Some(&server.url("/target")))
        .await
        .expect("ctx_a new_tab");
    let tabs_a = ctx_a.session().list_tabs().await.expect("list_tabs a");
    let tabs_b = ctx_b.session().list_tabs().await.expect("list_tabs b");
    assert_eq!(tabs_a.len(), 2);
    assert_eq!(tabs_b.len(), 1);

    ctx_a.dispose().await.expect("dispose ctx_a");
    assert_eq!(pool.active_count(), 1);

    // Root and ctx_b remain usable after ctx_a disposal
    assert_eq!(
        ctx_b.session().eval_isolated("1 + 1").await.unwrap(),
        2
    );
    assert_eq!(
        pool.root_session().eval_isolated("2 + 2").await.unwrap(),
        4
    );

    pool.dispose_all().await.expect("dispose_all");
    assert_eq!(pool.active_count(), 0);
    // Keep unused BTreeMap import warning away? No unused imports here.
    let _unused_check: BTreeMap<String, String> = BTreeMap::new();

    pool.shutdown().await.expect("shutdown pool");
    assert_pgid_gone(pid as libc::pid_t);
}

#[tokio::test]
async fn virtual_context_full_isolation_chromium() {
    assert_virtual_context_full_isolation(EngineKind::Chromium).await;
}

#[tokio::test]
async fn virtual_context_full_isolation_firefox() {
    assert_virtual_context_full_isolation(EngineKind::Firefox).await;
}

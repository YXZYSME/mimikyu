use serde_json::json;

use crate::driver::{EngineKind, EnvProfile, WebRtcPolicy};

/// Generate an isolated, zero-global-leakage preload script for window/iframe realms.
/// Never defines `window.__mimikyu` or any `__mimikyu` identifier.
pub fn build_window_init_script(profile: &EnvProfile, engine: EngineKind) -> String {
    let config_json = build_js_config(profile, engine);
    let worker_script = if engine == EngineKind::Firefox {
        Some(build_worker_init_script(profile, engine, true))
    } else {
        None
    };
    let worker_prelude_json = serde_json::to_string(&worker_script.unwrap_or_default())
        .unwrap_or_else(|_| "\"\"".to_string());
    let wrap_workers = engine == EngineKind::Firefox;

    format!(
        r#""use strict";
(() => {{
  const cfg = {config_json};
  const defGetter = (obj, prop, getter) => {{
    if (!obj) return;
    try {{
      Object.defineProperty(obj, prop, {{
        get: getter,
        enumerable: true,
        configurable: true
      }});
    }} catch {{}}
  }};

  const navProto = typeof Navigator !== 'undefined' ? Navigator.prototype : null;
  if (navProto) {{
    if (cfg.userAgent) defGetter(navProto, 'userAgent', () => cfg.userAgent);
    if (cfg.platform) defGetter(navProto, 'platform', () => cfg.platform);
    if (cfg.languages && cfg.languages.length > 0) {{
      const frozenLangs = Object.freeze(cfg.languages.slice());
      defGetter(navProto, 'language', () => frozenLangs[0]);
      defGetter(navProto, 'languages', () => frozenLangs);
    }}
    if (cfg.hardwareConcurrency > 0) {{
      defGetter(navProto, 'hardwareConcurrency', () => cfg.hardwareConcurrency);
    }}
    if (cfg.isChromium && cfg.deviceMemory !== null) {{
      defGetter(navProto, 'deviceMemory', () => cfg.deviceMemory);
    }}
    if (cfg.disableAutomationControlled) {{
      defGetter(navProto, 'webdriver', () => false);
    }}
    if (cfg.isChromium && cfg.userAgentData) {{
      const uad = cfg.userAgentData;
      const uadObj = {{
        brands: Object.freeze(uad.brands.map(b => Object.freeze({{ brand: b.brand, version: b.version }}))),
        mobile: Boolean(uad.mobile),
        platform: uad.platform,
        toJSON() {{
          return {{ brands: this.brands, mobile: this.mobile, platform: this.platform }};
        }},
        async getHighEntropyValues(hints) {{
          const out = {{
            brands: this.brands,
            mobile: this.mobile,
            platform: this.platform
          }};
          if (Array.isArray(hints)) {{
            for (const h of hints) {{
              if (h === 'architecture') out.architecture = uad.architecture;
              else if (h === 'bitness') out.bitness = uad.bitness;
              else if (h === 'model') out.model = uad.model;
              else if (h === 'platformVersion') out.platformVersion = uad.platformVersion;
              else if (h === 'uaFullVersion') {{
                const full = uad.fullVersionList.find(b => b.brand === 'Google Chrome' || b.brand === 'Chromium');
                out.uaFullVersion = full ? full.version : '';
              }} else if (h === 'fullVersionList') {{
                out.fullVersionList = uad.fullVersionList.map(b => ({{ brand: b.brand, version: b.version }}));
              }} else if (h === 'wow64') out.wow64 = Boolean(uad.wow64);
            }}
          }}
          return out;
        }}
      }};
      defGetter(navProto, 'userAgentData', () => uadObj);
    }}
  }}

  const scrProto = typeof Screen !== 'undefined' ? Screen.prototype : null;
  if (scrProto && cfg.screenWidth > 0 && cfg.screenHeight > 0) {{
    defGetter(scrProto, 'width', () => cfg.screenWidth);
    defGetter(scrProto, 'height', () => cfg.screenHeight);
    defGetter(scrProto, 'availWidth', () => cfg.screenWidth);
    defGetter(scrProto, 'availHeight', () => cfg.screenHeight);
    defGetter(scrProto, 'colorDepth', () => 24);
    defGetter(scrProto, 'pixelDepth', () => 24);
  }}
  if (cfg.viewportWidth > 0 && cfg.viewportHeight > 0) {{
    defGetter(globalThis, 'outerWidth', () => cfg.viewportWidth);
    defGetter(globalThis, 'outerHeight', () => cfg.viewportHeight);
  }}
  if (cfg.deviceScaleFactor > 0) {{
    defGetter(globalThis, 'devicePixelRatio', () => cfg.deviceScaleFactor);
  }}

  if (cfg.webglVendor || cfg.webglRenderer) {{
    const patchGlProto = (proto) => {{
      if (!proto) return;
      const origGetExt = proto.getExtension;
      if (typeof origGetExt === 'function') {{
        proto.getExtension = function(name) {{
          const res = origGetExt.call(this, name);
          if (!res && name === 'WEBGL_debug_renderer_info') {{
            return {{ UNMASKED_VENDOR_WEBGL: 37445, UNMASKED_RENDERER_WEBGL: 37446 }};
          }}
          return res;
        }};
      }}
      const origGetParam = proto.getParameter;
      if (typeof origGetParam === 'function') {{
        proto.getParameter = function(pname) {{
          if (pname === 37445 && cfg.webglVendor) return cfg.webglVendor;
          if (pname === 37446 && cfg.webglRenderer) return cfg.webglRenderer;
          return origGetParam.call(this, pname);
        }};
      }}
    }};
    if (typeof WebGLRenderingContext !== 'undefined') patchGlProto(WebGLRenderingContext.prototype);
    if (typeof WebGL2RenderingContext !== 'undefined') patchGlProto(WebGL2RenderingContext.prototype);

    if (typeof HTMLCanvasElement !== 'undefined') {{
      const origGetContext = HTMLCanvasElement.prototype.getContext;
      if (typeof origGetContext === 'function') {{
        HTMLCanvasElement.prototype.getContext = function(kind, ...rest) {{
          const ctx = origGetContext.apply(this, [kind, ...rest]);
          if (ctx) return ctx;
          if (kind === 'webgl' || kind === 'experimental-webgl' || kind === 'webgl2') {{
            return {{
              getExtension(name) {{
                if (name === 'WEBGL_debug_renderer_info') {{
                  return {{ UNMASKED_VENDOR_WEBGL: 37445, UNMASKED_RENDERER_WEBGL: 37446 }};
                }}
                return null;
              }},
              getParameter(pname) {{
                if ((pname === 37445 || pname === 7936) && cfg.webglVendor) return cfg.webglVendor;
                if ((pname === 37446 || pname === 7937) && cfg.webglRenderer) return cfg.webglRenderer;
                return null;
              }},
              getSupportedExtensions() {{
                return ['WEBGL_debug_renderer_info'];
              }}
            }};
          }}
          return null;
        }};
      }}
    }}
  }}

  if (cfg.audioSampleRate !== null || cfg.audioBaseLatency !== null || cfg.audioChannelCount !== null) {{
    const patchAudio = (proto) => {{
      if (!proto) return;
      if (cfg.audioSampleRate !== null) {{
        defGetter(proto, 'sampleRate', () => cfg.audioSampleRate);
      }}
      if (cfg.audioBaseLatency !== null) {{
        defGetter(proto, 'baseLatency', () => cfg.audioBaseLatency);
      }}
    }};
    if (typeof BaseAudioContext !== 'undefined') patchAudio(BaseAudioContext.prototype);
    if (typeof AudioContext !== 'undefined') patchAudio(AudioContext.prototype);
    if (cfg.audioChannelCount !== null && typeof AudioDestinationNode !== 'undefined') {{
      defGetter(AudioDestinationNode.prototype, 'maxChannelCount', () => cfg.audioChannelCount);
    }}
  }}

  if (cfg.disableWebRtc) {{
    const blockedRtc = function() {{
      throw new DOMException('WebRTC is disabled by environment policy', 'NotSupportedError');
    }};
    for (const rtcName of ['RTCPeerConnection', 'webkitRTCPeerConnection', 'mozRTCPeerConnection']) {{
      try {{
        if (rtcName in globalThis) {{
          Object.defineProperty(globalThis, rtcName, {{
            value: blockedRtc,
            configurable: true,
            writable: true
          }});
        }}
      }} catch {{}}
    }}
  }}

  if ({wrap_workers}) {{
    const workerPrelude = {worker_prelude_json};
    const sharedBlobUrls = new Map();
    const modLoaderPrefix = [
      'const q = [];',
      'const buf = (ev) => {{ q.push(ev); }};',
      'const evTypes = "SharedWorkerGlobalScope" in globalThis ? ["connect", "message", "messageerror"] : ["message", "messageerror"];',
      'for (const t of evTypes) globalThis.addEventListener(t, buf);',
      'let loaded = false;',
      'try {{',
      '  await import('
    ].join('\n');
    const modLoaderSuffix = [
      ');',
      '  loaded = true;',
      '}} finally {{',
      '  for (const t of evTypes) globalThis.removeEventListener(t, buf);',
      '}}',
      'if (loaded) {{',
      '  for (const ev of q) {{',
      '    globalThis.dispatchEvent(new MessageEvent(ev.type, {{',
      '      data: ev.data,',
      '      origin: ev.origin,',
      '      lastEventId: ev.lastEventId,',
      '      source: ev.source ?? null,',
      '      ports: ev.ports ? Array.from(ev.ports) : []',
      '    }}));',
      '  }}',
      '}}'
    ].join('\n');
    for (const ctorName of ['Worker', 'SharedWorker']) {{
      const Orig = globalThis[ctorName];
      if (typeof Orig !== 'function') continue;
      const Wrapped = new Proxy(Orig, {{
        construct(target, args, newTarget) {{
          const scriptUrl = args[0];
          const opts = args[1];
          try {{
            if (typeof scriptUrl === 'string' || (typeof URL !== 'undefined' && scriptUrl instanceof URL)) {{
              const absUrl = new URL(String(scriptUrl), globalThis.location.href).href;
              const isMod = opts && typeof opts === 'object' && opts.type === 'module';
              const cacheKey = ctorName === 'SharedWorker' ? (isMod ? 'm:' : 'c:') + absUrl : null;
              let blobUrl = cacheKey ? sharedBlobUrls.get(cacheKey) : null;
              if (!blobUrl) {{
                const loader = isMod
                  ? modLoaderPrefix + JSON.stringify(absUrl) + modLoaderSuffix
                  : 'importScripts(' + JSON.stringify(absUrl) + ');';
                const blob = new Blob([workerPrelude + '\n' + loader], {{ type: 'application/javascript' }});
                blobUrl = URL.createObjectURL(blob);
                if (cacheKey) sharedBlobUrls.set(cacheKey, blobUrl);
              }}
              const nextArgs = args.length > 1 ? [blobUrl, ...args.slice(1)] : [blobUrl];
              return Reflect.construct(target, nextArgs, newTarget);
            }}
          }} catch {{}}
          return Reflect.construct(target, args, newTarget);
        }}
      }});
      try {{
        Object.defineProperty(globalThis, ctorName, {{
          value: Wrapped,
          configurable: true,
          writable: true
        }});
      }} catch {{}}
    }}
  }}
}})();"#
    )
}

/// Generate an isolated preload script for dedicated/shared/service Worker realms.
pub fn build_worker_init_script(
    profile: &EnvProfile,
    engine: EngineKind,
    include_tz_intl: bool,
) -> String {
    let config_json = build_js_config(profile, engine);
    let tz_patch = if include_tz_intl && !profile.timezone.is_empty() {
        let tz_json = serde_json::to_string(&profile.timezone).unwrap_or_else(|_| "\"UTC\"".into());
        let loc_json = serde_json::to_string(&profile.locale).unwrap_or_else(|_| "\"en-US\"".into());
        format!(
            r#"
  const targetTz = {tz_json};
  const targetLocale = {loc_json};
  if (typeof Intl !== 'undefined' && typeof Intl.DateTimeFormat === 'function') {{
    const OrigDTF = Intl.DateTimeFormat;
    const WrappedDTF = new Proxy(OrigDTF, {{
      construct(target, args, newTarget) {{
        const locales = args[0] === undefined && targetLocale ? targetLocale : args[0];
        const opts = Object.assign({{}}, args[1] || {{}});
        if (!opts.timeZone && targetTz) opts.timeZone = targetTz;
        return Reflect.construct(target, [locales, opts], newTarget);
      }},
      apply(target, thisArg, args) {{
        const locales = args[0] === undefined && targetLocale ? targetLocale : args[0];
        const opts = Object.assign({{}}, args[1] || {{}});
        if (!opts.timeZone && targetTz) opts.timeZone = targetTz;
        return Reflect.apply(target, thisArg, [locales, opts]);
      }}
    }});
    try {{ Intl.DateTimeFormat = WrappedDTF; }} catch {{}}
    if (targetTz) {{
      try {{
        const offsetFmt = new OrigDTF('en-US', {{ timeZone: targetTz, timeZoneName: 'shortOffset' }});
        Date.prototype.getTimezoneOffset = function() {{
          try {{
            const parts = offsetFmt.formatToParts(this);
            const tzPart = parts.find(p => p.type === 'timeZoneName');
            if (!tzPart) return 0;
            const v = tzPart.value;
            if (v === 'GMT' || v === 'UTC') return 0;
            const m = /^GMT([+-])(\d{{1,2}})(?::(\d{{2}}))?$/.exec(v);
            if (!m) return 0;
            const sign = m[1] === '+' ? -1 : 1;
            const hrs = parseInt(m[2], 10);
            const mins = m[3] ? parseInt(m[3], 10) : 0;
            return sign * (hrs * 60 + mins);
          }} catch {{
            return 0;
          }}
        }};
      }} catch {{}}
    }}
  }}"#
        )
    } else {
        String::new()
    };

    format!(
        r#""use strict";
(() => {{
  const cfg = {config_json};
  const defGetter = (obj, prop, getter) => {{
    if (!obj) return;
    try {{
      Object.defineProperty(obj, prop, {{
        get: getter,
        enumerable: true,
        configurable: true
      }});
    }} catch {{}}
  }};

  const wnavProto = typeof WorkerNavigator !== 'undefined'
    ? WorkerNavigator.prototype
    : (typeof navigator !== 'undefined' ? Object.getPrototypeOf(navigator) : null);
  const overrides = {{}};
  if (cfg.userAgent) overrides.userAgent = cfg.userAgent;
  if (cfg.platform) overrides.platform = cfg.platform;
  if (cfg.languages && cfg.languages.length > 0) {{
    const frozenLangs = Object.freeze(cfg.languages.slice());
    overrides.language = frozenLangs[0];
    overrides.languages = frozenLangs;
  }}
  if (cfg.hardwareConcurrency > 0) overrides.hardwareConcurrency = cfg.hardwareConcurrency;
  if (cfg.isChromium && cfg.deviceMemory !== null) overrides.deviceMemory = cfg.deviceMemory;
  if (cfg.isChromium && cfg.userAgentData) {{
    const uad = cfg.userAgentData;
    overrides.userAgentData = {{
      brands: Object.freeze(uad.brands.map(b => Object.freeze({{ brand: b.brand, version: b.version }}))),
      mobile: Boolean(uad.mobile),
      platform: uad.platform,
      toJSON() {{
        return {{ brands: this.brands, mobile: this.mobile, platform: this.platform }};
      }},
      async getHighEntropyValues(hints) {{
        const out = {{ brands: this.brands, mobile: this.mobile, platform: this.platform }};
        if (Array.isArray(hints)) {{
          for (const h of hints) {{
            if (h === 'architecture') out.architecture = uad.architecture;
            else if (h === 'bitness') out.bitness = uad.bitness;
            else if (h === 'model') out.model = uad.model;
            else if (h === 'platformVersion') out.platformVersion = uad.platformVersion;
            else if (h === 'fullVersionList') {{
              out.fullVersionList = uad.fullVersionList.map(b => ({{ brand: b.brand, version: b.version }}));
            }} else if (h === 'wow64') out.wow64 = Boolean(uad.wow64);
          }}
        }}
        return out;
      }}
    }};
  }}

  for (const [prop, val] of Object.entries(overrides)) {{
    const getter = () => val;
    if (wnavProto) defGetter(wnavProto, prop, getter);
    if (typeof navigator !== 'undefined') {{
      try {{
        if (navigator[prop] !== val) defGetter(navigator, prop, getter);
      }} catch {{
        defGetter(navigator, prop, getter);
      }}
    }}
  }}

  const needsNavProxy = typeof navigator !== 'undefined' && Object.keys(overrides).some((k) => {{
    try {{ return navigator[k] !== overrides[k]; }} catch {{ return true; }}
  }});
  if (needsNavProxy) {{
    const origNav = navigator;
    const navProxy = new Proxy(origNav, {{
      get(target, prop) {{
        if (Object.prototype.hasOwnProperty.call(overrides, prop)) return overrides[prop];
        const v = Reflect.get(target, prop, target);
        return typeof v === 'function' ? v.bind(target) : v;
      }}
    }});
    const wgProto = typeof WorkerGlobalScope !== 'undefined' ? WorkerGlobalScope.prototype : null;
    let wrapped = false;
    if (wgProto) {{
      try {{
        Object.defineProperty(wgProto, 'navigator', {{
          get: () => navProxy,
          enumerable: true,
          configurable: true
        }});
        wrapped = true;
      }} catch {{}}
    }}
    if (!wrapped) {{
      try {{
        Object.defineProperty(globalThis, 'navigator', {{
          get: () => navProxy,
          enumerable: true,
          configurable: true
        }});
      }} catch {{}}
    }}
  }}
{tz_patch}
}})();"#
    )
}

fn build_js_config(profile: &EnvProfile, engine: EngineKind) -> String {
    let uad = profile.user_agent_metadata.as_ref().map(|m| {
        json!({
            "brands": m.brands.iter().map(|b| json!({"brand": b.brand, "version": b.version})).collect::<Vec<_>>(),
            "fullVersionList": m.full_version_list.iter().map(|b| json!({"brand": b.brand, "version": b.version})).collect::<Vec<_>>(),
            "platform": m.platform,
            "platformVersion": m.platform_version,
            "architecture": m.architecture,
            "model": m.model,
            "mobile": m.mobile,
            "bitness": m.bitness,
            "wow64": m.wow64,
        })
    });

    json!({
        "isChromium": engine == EngineKind::Chromium,
        "userAgent": profile.user_agent,
        "platform": profile.platform,
        "languages": profile.languages,
        "hardwareConcurrency": profile.hardware_concurrency,
        "deviceMemory": profile.device_memory_gb,
        "disableAutomationControlled": profile.disable_automation_controlled,
        "userAgentData": uad,
        "viewportWidth": profile.viewport_width,
        "viewportHeight": profile.viewport_height,
        "screenWidth": profile.screen_width,
        "screenHeight": profile.screen_height,
        "deviceScaleFactor": profile.device_scale_factor,
        "webglVendor": profile.webgl_vendor,
        "webglRenderer": profile.webgl_renderer,
        "audioSampleRate": profile.audio_sample_rate,
        "audioBaseLatency": profile.audio_base_latency,
        "audioChannelCount": profile.audio_channel_count,
        "disableWebRtc": profile.webrtc_policy == WebRtcPolicy::DisableAll,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env_profile::presets::PresetKind;

    #[test]
    fn generated_scripts_contain_no_mimikyu_globals_or_identifiers() {
        for engine in [EngineKind::Chromium, EngineKind::Firefox] {
            let prof = PresetKind::MacOs.to_env_profile(engine);
            let win_js = build_window_init_script(&prof, engine);
            let wrk_js = build_worker_init_script(&prof, engine, engine == EngineKind::Firefox);
            assert!(!win_js.contains("__mimikyu"));
            assert!(!wrk_js.contains("__mimikyu"));
            assert!(win_js.contains("Navigator.prototype"));
            assert!(wrk_js.contains("WorkerNavigator.prototype"));
            if engine == EngineKind::Firefox {
                assert!(
                    win_js.contains("await import("),
                    "Firefox module worker wrapper must use dynamic await import(...) so workerPrelude runs first"
                );
                assert!(
                    !win_js.contains("'import ' + JSON.stringify(absUrl)"),
                    "Firefox module worker wrapper must not use hoisted static import"
                );
                assert!(win_js.contains("addEventListener"));
                assert!(win_js.contains("removeEventListener"));
                assert!(win_js.contains("dispatchEvent(new MessageEvent("));
                assert!(win_js.contains("SharedWorkerGlobalScope"));
                assert!(win_js.contains("\"connect\""));
                assert!(win_js.contains("\"messageerror\""));
                assert!(win_js.contains("sharedBlobUrls"));
            }
        }
    }
}

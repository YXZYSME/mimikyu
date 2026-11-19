use serde_json::{json, Value};

use crate::driver::{EngineKind, EnvProfile, WebRtcPolicy};

/// Generate an isolated, zero-global-leakage preload script for window/iframe realms.
/// Never defines `window.__mimikyu` or any `__mimikyu` identifier.
pub fn build_window_init_script(profile: &EnvProfile, engine: EngineKind) -> String {
    let config_json = build_js_config(profile, engine);
    let worker_script = if engine == EngineKind::Firefox {
        Some(build_worker_init_script_inner(profile, engine, true, true))
    } else {
        None
    };
    let worker_prelude_json = serde_json::to_string(&worker_script.unwrap_or_default())
        .unwrap_or_else(|_| "\"\"".to_string());
    let wrap_workers = engine == EngineKind::Firefox;

    format!(
        r#""use strict";
(() => {{
  const stateSym = Symbol.for('env.runtime.state');
  let state = globalThis[stateSym];
  if (!state) {{
    state = {{
      cfg: null,
      workerPrelude: '',
      wrapWorkers: false,
      version: 0,
      appliedVersion: new WeakMap(),
      origProtoCwGet: new WeakMap(),
      origDtfMap: new WeakMap(),
      origGetTzOffsetMap: new WeakMap(),
      hookedProtos: new WeakSet(),
      hookedOpenWins: new WeakSet(),
      hookedWorkerWins: new WeakSet(),
      hookedGlProtos: new WeakSet(),
      hookedCanvasProtos: new WeakSet(),
      hookedRtcWins: new WeakSet(),
      hookedRtcProtos: new WeakSet(),
      rtcOnIceMap: new WeakMap(),
      rtcCapListeners: new WeakMap(),
      rtcBubListeners: new WeakMap(),
      hookedDomRealms: new WeakSet(),
      hookedUrls: new WeakSet(),
      origUrlRevoke: new WeakMap(),
      pendingBlobUrls: new Map(),
      deferredRevokes: new Map(),
      sharedBlobUrls: new Map(),
      applyToWindow: null,
    }};
    try {{
      Object.defineProperty(globalThis, stateSym, {{
        value: state,
        enumerable: false,
        configurable: true,
        writable: true
      }});
    }} catch {{
      try {{ globalThis[stateSym] = state; }} catch {{}}
    }}
  }}
  state.cfg = {config_json};
  state.workerPrelude = {worker_prelude_json};
  state.wrapWorkers = {wrap_workers};
  state.version = (state.version || 0) + 1;

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

  const patchFrameElForState = (targetState, el) => {{
    if (!el) return;
    try {{
      let proto = null;
      try {{ proto = Object.getPrototypeOf(el); }} catch {{}}
      let getter = proto ? targetState.origProtoCwGet.get(proto) : null;
      if (!getter && el.ownerDocument && el.ownerDocument.defaultView) {{
        const dv = el.ownerDocument.defaultView;
        const tag = el.tagName;
        if (tag === 'FRAME' && dv.HTMLFrameElement) {{
          getter = targetState.origProtoCwGet.get(dv.HTMLFrameElement.prototype);
        }} else if (dv.HTMLIFrameElement) {{
          getter = targetState.origProtoCwGet.get(dv.HTMLIFrameElement.prototype);
        }}
      }}
      if (typeof getter === 'function') {{
        const cw = getter.call(el);
        if (cw) (targetState.applyToWindow || applyToWindow)(cw);
      }}
    }} catch {{}}
  }};

  const scanNodeFramesForState = (targetState, root) => {{
    if (!root) return;
    try {{
      const tag = root.tagName;
      if (tag === 'IFRAME' || tag === 'FRAME') {{
        patchFrameElForState(targetState, root);
      }}
      if (typeof root.querySelectorAll === 'function') {{
        const frames = root.querySelectorAll('iframe, frame');
        for (let i = 0; i < frames.length; i++) {{
          patchFrameElForState(targetState, frames[i]);
        }}
      }}
    }} catch {{}}
  }};

  const applyToWindow = (win) => {{
    if (!win) return;
    let navProto = null;
    let doc = null;
    try {{
      navProto = typeof win.Navigator !== 'undefined' ? win.Navigator.prototype : null;
      doc = win.document || null;
    }} catch {{
      return;
    }}

    let targetState = state;
    if (win !== globalThis) {{
      try {{
        const existing = win[stateSym];
        if (existing && existing !== state) {{
          if (existing.lastParentState !== state || existing.lastParentVersion !== state.version) {{
            existing.cfg = state.cfg;
            existing.workerPrelude = state.workerPrelude;
            existing.wrapWorkers = state.wrapWorkers;
            existing.version = (existing.version || 0) + 1;
            existing.applyToWindow = applyToWindow;
            existing.lastParentState = state;
            existing.lastParentVersion = state.version;
          }}
          targetState = existing;
        }} else if (!existing) {{
          try {{
            Object.defineProperty(win, stateSym, {{
              value: state,
              enumerable: false,
              configurable: true,
              writable: true
            }});
          }} catch {{
            try {{ win[stateSym] = state; }} catch {{}}
          }}
        }}
      }} catch {{
        return;
      }}
    }}

    const versionKey = navProto || doc;
    if (!versionKey) return;
    if (targetState.appliedVersion.get(versionKey) === targetState.version) return;
    targetState.appliedVersion.set(versionKey, targetState.version);
    const curCfg = targetState.cfg;

    if (navProto) {{
      if (curCfg.userAgent) defGetter(navProto, 'userAgent', () => targetState.cfg.userAgent);
      if (curCfg.platform) defGetter(navProto, 'platform', () => targetState.cfg.platform);
      if (curCfg.languages && curCfg.languages.length > 0) {{
        const frozenLangs = Object.freeze(curCfg.languages.slice());
        defGetter(navProto, 'language', () => frozenLangs[0]);
        defGetter(navProto, 'languages', () => frozenLangs);
      }}
      if (curCfg.hardwareConcurrency > 0) {{
        defGetter(navProto, 'hardwareConcurrency', () => targetState.cfg.hardwareConcurrency);
      }}
      if (curCfg.isChromium && curCfg.deviceMemory !== null) {{
        defGetter(navProto, 'deviceMemory', () => targetState.cfg.deviceMemory);
      }}
      if (curCfg.disableAutomationControlled) {{
        defGetter(navProto, 'webdriver', () => false);
      }}
      if (curCfg.isChromium && curCfg.userAgentData) {{
        const uad = curCfg.userAgentData;
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

    const scrProto = typeof win.Screen !== 'undefined' ? win.Screen.prototype : null;
    if (scrProto && curCfg.screenWidth > 0 && curCfg.screenHeight > 0) {{
      defGetter(scrProto, 'width', () => targetState.cfg.screenWidth);
      defGetter(scrProto, 'height', () => targetState.cfg.screenHeight);
      defGetter(scrProto, 'availWidth', () => targetState.cfg.screenWidth);
      defGetter(scrProto, 'availHeight', () => targetState.cfg.screenHeight);
      defGetter(scrProto, 'colorDepth', () => 24);
      defGetter(scrProto, 'pixelDepth', () => 24);
    }}
    if (curCfg.viewportWidth > 0 && curCfg.viewportHeight > 0) {{
      defGetter(win, 'outerWidth', () => targetState.cfg.viewportWidth);
      defGetter(win, 'outerHeight', () => targetState.cfg.viewportHeight);
    }}
    if (curCfg.deviceScaleFactor > 0) {{
      defGetter(win, 'devicePixelRatio', () => targetState.cfg.deviceScaleFactor);
    }}

    if (win !== globalThis && !curCfg.isChromium && curCfg.timezone && win.Intl && typeof win.Intl.DateTimeFormat === 'function') {{
      try {{
        if (!targetState.origDtfMap.has(win)) {{
          const OrigDTF = win.Intl.DateTimeFormat;
          const curTz = new OrigDTF().resolvedOptions().timeZone;
          if (curTz !== curCfg.timezone) {{
            targetState.origDtfMap.set(win, OrigDTF);
            const WrappedDTF = new Proxy(OrigDTF, {{
              construct(target, args, newTarget) {{
                const c = targetState.cfg;
                const locales = args[0] === undefined && c.locale ? c.locale : args[0];
                const opts = Object.assign({{}}, args[1] || {{}});
                if (!opts.timeZone && c.timezone) opts.timeZone = c.timezone;
                return Reflect.construct(target, [locales, opts], newTarget);
              }},
              apply(target, thisArg, args) {{
                const c = targetState.cfg;
                const locales = args[0] === undefined && c.locale ? c.locale : args[0];
                const opts = Object.assign({{}}, args[1] || {{}});
                if (!opts.timeZone && c.timezone) opts.timeZone = c.timezone;
                return Reflect.apply(target, thisArg, [locales, opts]);
              }}
            }});
            try {{ win.Intl.DateTimeFormat = WrappedDTF; }} catch {{}}
            if (win.Date && win.Date.prototype && !targetState.origGetTzOffsetMap.has(win)) {{
              try {{
                const origGetTzOffset = win.Date.prototype.getTimezoneOffset;
                targetState.origGetTzOffsetMap.set(win, origGetTzOffset);
                win.Date.prototype.getTimezoneOffset = function() {{
                  try {{
                    const tz = targetState.cfg.timezone;
                    if (!tz) return origGetTzOffset.call(this);
                    const offsetFmt = new OrigDTF('en-US', {{ timeZone: tz, timeZoneName: 'shortOffset' }});
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
          }}
        }}
      }} catch {{}}
    }}

    if (curCfg.webglVendor || curCfg.webglRenderer) {{
      const patchGlProto = (proto) => {{
        if (!proto || targetState.hookedGlProtos.has(proto)) return;
        targetState.hookedGlProtos.add(proto);
        const origGetExt = proto.getExtension;
        if (typeof origGetExt === 'function') {{
          proto.getExtension = function(name) {{
            const res = origGetExt.call(this, name);
            if (!res && name === 'WEBGL_debug_renderer_info' && (targetState.cfg.webglVendor || targetState.cfg.webglRenderer)) {{
              return {{ UNMASKED_VENDOR_WEBGL: 37445, UNMASKED_RENDERER_WEBGL: 37446 }};
            }}
            return res;
          }};
        }}
        const origGetParam = proto.getParameter;
        if (typeof origGetParam === 'function') {{
          proto.getParameter = function(pname) {{
            const c = targetState.cfg;
            if ((pname === 37445 || pname === 7936) && c.webglVendor) return c.webglVendor;
            if ((pname === 37446 || pname === 7937) && c.webglRenderer) return c.webglRenderer;
            return origGetParam.call(this, pname);
          }};
        }}
      }};
      if (typeof win.WebGLRenderingContext !== 'undefined') patchGlProto(win.WebGLRenderingContext.prototype);
      if (typeof win.WebGL2RenderingContext !== 'undefined') patchGlProto(win.WebGL2RenderingContext.prototype);

      if (typeof win.HTMLCanvasElement !== 'undefined') {{
        const cProto = win.HTMLCanvasElement.prototype;
        if (cProto && !targetState.hookedCanvasProtos.has(cProto)) {{
          targetState.hookedCanvasProtos.add(cProto);
          const origGetContext = cProto.getContext;
          if (typeof origGetContext === 'function') {{
            cProto.getContext = function(kind, ...rest) {{
              const ctx = origGetContext.apply(this, [kind, ...rest]);
              if (ctx) return ctx;
              const c = targetState.cfg;
              if ((c.webglVendor || c.webglRenderer) && (kind === 'webgl' || kind === 'experimental-webgl' || kind === 'webgl2')) {{
                const isWebgl2 = kind === 'webgl2';
                const glVersion = isWebgl2
                  ? (c.isChromium ? 'WebGL 2.0 (OpenGL ES 3.0 Chromium)' : 'WebGL 2.0')
                  : (c.isChromium ? 'WebGL 1.0 (OpenGL ES 2.0 Chromium)' : 'WebGL 1.0');
                const glslVersion = isWebgl2
                  ? (c.isChromium ? 'WebGL GLSL ES 3.00 (OpenGL ES GLSL ES 3.00 Chromium)' : 'WebGL GLSL ES 3.00')
                  : (c.isChromium ? 'WebGL GLSL ES 1.0 (OpenGL ES GLSL ES 1.0 Chromium)' : 'WebGL GLSL ES 1.0');
                return {{
                  VENDOR: 7936,
                  RENDERER: 7937,
                  VERSION: 7938,
                  SHADING_LANGUAGE_VERSION: 35724,
                  getExtension(name) {{
                    if (name === 'WEBGL_debug_renderer_info') {{
                      return {{ UNMASKED_VENDOR_WEBGL: 37445, UNMASKED_RENDERER_WEBGL: 37446 }};
                    }}
                    return null;
                  }},
                  getParameter(pname) {{
                    const cur = targetState.cfg;
                    if ((pname === 37445 || pname === 7936) && cur.webglVendor) return cur.webglVendor;
                    if ((pname === 37446 || pname === 7937) && cur.webglRenderer) return cur.webglRenderer;
                    if (pname === 7938) return glVersion;
                    if (pname === 35724) return glslVersion;
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
    }}

    if (curCfg.audioSampleRate !== null || curCfg.audioBaseLatency !== null || curCfg.audioChannelCount !== null) {{
      const patchAudio = (proto) => {{
        if (!proto) return;
        if (curCfg.audioSampleRate !== null) {{
          defGetter(proto, 'sampleRate', () => targetState.cfg.audioSampleRate);
        }}
        if (curCfg.audioBaseLatency !== null) {{
          defGetter(proto, 'baseLatency', () => targetState.cfg.audioBaseLatency);
        }}
      }};
      if (typeof win.BaseAudioContext !== 'undefined') patchAudio(win.BaseAudioContext.prototype);
      if (typeof win.AudioContext !== 'undefined') patchAudio(win.AudioContext.prototype);
      if (curCfg.audioChannelCount !== null && typeof win.AudioDestinationNode !== 'undefined') {{
        defGetter(win.AudioDestinationNode.prototype, 'maxChannelCount', () => targetState.cfg.audioChannelCount);
      }}
    }}

    if (!targetState.hookedRtcWins.has(win)) {{
      targetState.hookedRtcWins.add(win);
      if (!targetState.hookedRtcProtos) targetState.hookedRtcProtos = new WeakSet();
      if (!targetState.rtcOnIceMap) targetState.rtcOnIceMap = new WeakMap();
      if (!targetState.rtcCapListeners) targetState.rtcCapListeners = new WeakMap();
      if (!targetState.rtcBubListeners) targetState.rtcBubListeners = new WeakMap();
      const DomExc = win.DOMException || DOMException;
      const isHostCand = (cand) => {{
        if (!cand) return false;
        try {{
          if (cand.type === 'host') return true;
          const s = typeof cand.candidate === 'string' ? cand.candidate : '';
          if (/\btyp\s+host\b/i.test(s)) return true;
        }} catch {{}}
        return false;
      }};
      const stripHostSdp = (sdp) => {{
        if (typeof sdp !== 'string' || !sdp) return sdp;
        return sdp
          .split(/\r?\n/)
          .filter((line) => !(/^a=candidate:/i.test(line) && /\btyp\s+host\b/i.test(line)))
          .join('\r\n');
      }};
      const patchRtcProto = (rtcProto) => {{
        if (!rtcProto || targetState.hookedRtcProtos.has(rtcProto)) return;
        targetState.hookedRtcProtos.add(rtcProto);
        const origAdd = rtcProto.addEventListener;
        const origRem = rtcProto.removeEventListener;
        try {{
          const onIceDesc = Object.getOwnPropertyDescriptor(rtcProto, 'onicecandidate');
          if (onIceDesc && typeof onIceDesc.set === 'function' && typeof origAdd === 'function' && typeof origRem === 'function') {{
            const origSet = onIceDesc.set;
            const onIceWrapMap = new WeakMap();
            Object.defineProperty(rtcProto, 'onicecandidate', {{
              get() {{
                return targetState.rtcOnIceMap.has(this)
                  ? targetState.rtcOnIceMap.get(this)
                  : null;
              }},
              set(fn) {{
                try {{ origSet.call(this, null); }} catch {{}}
                const prevWrap = onIceWrapMap.get(this);
                if (prevWrap) {{
                  try {{ origRem.call(this, 'icecandidate', prevWrap); }} catch {{}}
                  onIceWrapMap.delete(this);
                }}
                if (typeof fn !== 'function') {{
                  targetState.rtcOnIceMap.set(this, null);
                  return;
                }}
                targetState.rtcOnIceMap.set(this, fn);
                const wrap = function(ev) {{
                  if (targetState.cfg && targetState.cfg.noHostWebRtc && ev && ev.candidate && isHostCand(ev.candidate)) {{
                    return;
                  }}
                  return fn.call(this, ev);
                }};
                onIceWrapMap.set(this, wrap);
                origAdd.call(this, 'icecandidate', wrap);
              }},
              enumerable: onIceDesc.enumerable,
              configurable: true
            }});
          }}
        }} catch {{}}
        try {{
          if (typeof origAdd === 'function' && typeof origRem === 'function') {{
            Object.defineProperty(rtcProto, 'addEventListener', {{
              value: function(type, listener, options) {{
                if (String(type) !== 'icecandidate' || (!listener || (typeof listener !== 'function' && typeof listener !== 'object'))) {{
                  return origAdd.apply(this, arguments);
                }}
                const isCap = typeof options === 'boolean' ? options : Boolean(options && options.capture);
                const isOnce = Boolean(options && typeof options === 'object' && options.once);
                const store = isCap ? targetState.rtcCapListeners : targetState.rtcBubListeners;
                let map = store.get(this);
                if (!map) {{
                  map = new WeakMap();
                  store.set(this, map);
                }}
                let wrapped = map.get(listener);
                if (!wrapped) {{
                  wrapped = function(ev) {{
                    if (targetState.cfg && targetState.cfg.noHostWebRtc && ev && ev.candidate && isHostCand(ev.candidate)) {{
                      return;
                    }}
                    if (isOnce) {{
                      map.delete(listener);
                      try {{ origRem.call(this, type, wrapped, isCap); }} catch {{}}
                    }}
                    if (typeof listener === 'function') {{
                      return listener.call(this, ev);
                    }}
                    if (listener && typeof listener.handleEvent === 'function') {{
                      return listener.handleEvent(ev);
                    }}
                  }};
                  map.set(listener, wrapped);
                }}
                const passOpts = (isOnce && options && typeof options === 'object')
                  ? Object.assign({{}}, options, {{ once: false }})
                  : options;
                return arguments.length > 2
                  ? origAdd.call(this, type, wrapped, passOpts)
                  : origAdd.call(this, type, wrapped);
              }},
              configurable: true,
              writable: true,
              enumerable: true
            }});
            Object.defineProperty(rtcProto, 'removeEventListener', {{
              value: function(type, listener, options) {{
                if (String(type) === 'icecandidate' && listener && (typeof listener === 'function' || typeof listener === 'object')) {{
                  const isCap = typeof options === 'boolean' ? options : Boolean(options && options.capture);
                  const store = isCap ? targetState.rtcCapListeners : targetState.rtcBubListeners;
                  const map = store.get(this);
                  const wrapped = map ? map.get(listener) : null;
                  if (wrapped) {{
                    map.delete(listener);
                    return arguments.length > 2
                      ? origRem.call(this, type, wrapped, options)
                      : origRem.call(this, type, wrapped);
                  }}
                }}
                return origRem.apply(this, arguments);
              }},
              configurable: true,
              writable: true,
              enumerable: true
            }});
          }}
        }} catch {{}}
        for (const descProp of ['localDescription', 'currentLocalDescription', 'pendingLocalDescription']) {{
          try {{
            const d = Object.getOwnPropertyDescriptor(rtcProto, descProp);
            if (d && typeof d.get === 'function') {{
              const origDescGet = d.get;
              Object.defineProperty(rtcProto, descProp, {{
                get() {{
                  const val = origDescGet.call(this);
                  if (!val || !(targetState.cfg && targetState.cfg.noHostWebRtc) || typeof val.sdp !== 'string') {{
                    return val;
                  }}
                  const cleaned = stripHostSdp(val.sdp);
                  if (cleaned === val.sdp) return val;
                  try {{
                    if (typeof win.RTCSessionDescription === 'function') {{
                      return new win.RTCSessionDescription({{ type: val.type, sdp: cleaned }});
                    }}
                  }} catch {{}}
                  return {{ type: val.type, sdp: cleaned, toJSON() {{ return {{ type: val.type, sdp: cleaned }}; }} }};
                }},
                enumerable: d.enumerable,
                configurable: true
              }});
            }}
          }} catch {{}}
        }}
      }};
      for (const rtcName of ['RTCPeerConnection', 'webkitRTCPeerConnection', 'mozRTCPeerConnection']) {{
        try {{
          if (rtcName in win && typeof win[rtcName] === 'function') {{
            const OrigRtc = win[rtcName];
            patchRtcProto(OrigRtc.prototype);
            const WrappedRtc = new Proxy(OrigRtc, {{
              construct(target, args, newTarget) {{
                if (targetState.cfg.disableWebRtc) {{
                  throw new DomExc('WebRTC is disabled by environment policy', 'NotSupportedError');
                }}
                return Reflect.construct(target, args, newTarget);
              }},
              apply(target, thisArg, args) {{
                if (targetState.cfg.disableWebRtc) {{
                  throw new DomExc('WebRTC is disabled by environment policy', 'NotSupportedError');
                }}
                return Reflect.apply(target, thisArg, args);
              }}
            }});
            Object.defineProperty(win, rtcName, {{
              value: WrappedRtc,
              configurable: true,
              writable: true
            }});
          }}
        }} catch {{}}
      }}
    }}

    if (targetState.wrapWorkers && !targetState.hookedWorkerWins.has(win)) {{
      targetState.hookedWorkerWins.add(win);
      const UrlCtor = win.URL || (typeof URL !== 'undefined' ? URL : null);
      const hookUrlRevoke = (uCtor) => {{
        if (!uCtor || typeof uCtor.revokeObjectURL !== 'function' || targetState.hookedUrls.has(uCtor)) return;
        targetState.hookedUrls.add(uCtor);
        const origRevoke = uCtor.revokeObjectURL;
        targetState.origUrlRevoke.set(uCtor, origRevoke);
        const desc = Object.getOwnPropertyDescriptor(uCtor, 'revokeObjectURL');
        const wrappedRevoke = new Proxy(origRevoke, {{
          apply(target, thisArg, args) {{
            const rawUrl = args && args.length > 0 ? args[0] : undefined;
            if (rawUrl !== undefined && rawUrl !== null) {{
              const rawKey = String(rawUrl);
              const key = rawKey.split('#')[0];
              targetState.sharedBlobUrls.delete(targetState.version + ':c:' + rawKey);
              targetState.sharedBlobUrls.delete(targetState.version + ':m:' + rawKey);
              targetState.sharedBlobUrls.delete(targetState.version + ':c:' + key);
              targetState.sharedBlobUrls.delete(targetState.version + ':m:' + key);
              if ((targetState.pendingBlobUrls.get(key) || 0) > 0) {{
                targetState.deferredRevokes.set(key, () => {{
                  try {{ Reflect.apply(target, thisArg || uCtor, args); }} catch {{}}
                }});
                return undefined;
              }}
            }}
            return Reflect.apply(target, thisArg || uCtor, args);
          }}
        }});
        try {{
          Object.defineProperty(uCtor, 'revokeObjectURL', {{
            value: wrappedRevoke,
            configurable: true,
            writable: true,
            enumerable: desc ? desc.enumerable : true
          }});
        }} catch {{}}
      }};
      hookUrlRevoke(UrlCtor);
      if (typeof URL !== 'undefined' && URL !== UrlCtor) hookUrlRevoke(URL);
      const modLoaderPrefix = [
        'const q = [];',
        'const buf = (ev) => {{ q.push(ev); }};',
        'const evTypes = "SharedWorkerGlobalScope" in globalThis ? ["connect", "message", "messageerror"] : ["message", "messageerror"];',
        'for (const t of evTypes) globalThis.addEventListener(t, buf);',
        'let loaded = false;',
        'try {{',
        '  await import('
      ].join('\n');
      const modLoaderMid = [
        ');',
        '  loaded = true;',
        '}} finally {{',
        '  for (const t of evTypes) globalThis.removeEventListener(t, buf);'
      ].join('\n');
      const modLoaderEnd = [
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
      const netSym = Symbol.for('net.runtime.state');
      const wrapMarkSym = Symbol.for('worker.wrap.marks');
      const wrapMarks = globalThis[wrapMarkSym] || (globalThis[wrapMarkSym] = new WeakSet());
      for (const ctorName of ['Worker', 'SharedWorker']) {{
        const Orig = win[ctorName];
        if (typeof Orig !== 'function' || wrapMarks.has(Orig)) continue;
        const Wrapped = new Proxy(Orig, {{
          construct(target, args, newTarget) {{
            const scriptUrl = args[0];
            const opts = args[1];
            try {{
              const isUrlObj = (typeof URL !== 'undefined' && scriptUrl instanceof URL)
                || (typeof win.URL !== 'undefined' && scriptUrl instanceof win.URL);
              if (typeof scriptUrl === 'string' || isUrlObj) {{
                const rawHref = win.location && typeof win.location.href === 'string'
                  ? win.location.href
                  : '';
                const docBase = win.document && typeof win.document.baseURI === 'string'
                  ? win.document.baseURI
                  : '';
                const gHref = globalThis.location && typeof globalThis.location.href === 'string'
                  ? globalThis.location.href
                  : '';
                const gBase = globalThis.document && typeof globalThis.document.baseURI === 'string'
                  ? globalThis.document.baseURI
                  : '';
                const fallbackBase = (gHref && gHref !== 'about:srcdoc' && !gHref.startsWith('about:'))
                  ? gHref
                  : gBase;
                const baseHref = (rawHref && rawHref !== 'about:srcdoc' && !rawHref.startsWith('about:'))
                  ? rawHref
                  : ((docBase && docBase !== 'about:srcdoc' && !docBase.startsWith('about:'))
                    ? docBase
                    : fallbackBase);
                const absUrl = baseHref
                  ? new URL(String(scriptUrl), baseHref).href
                  : new URL(String(scriptUrl)).href;
                const isMod = opts && typeof opts === 'object' && opts.type === 'module';
                const netState = ctorName === 'SharedWorker'
                  ? (win[netSym] || globalThis[netSym] || null)
                  : null;
                const swDec = (netState && netState.policy && netState.policy.active && typeof netState.decide === 'function')
                  ? netState.decide(absUrl, 'script', {{}})
                  : null;
                const BlobCtor = typeof win.Blob === 'function' ? win.Blob : Blob;
                if (swDec && swDec.kind === 'abort') {{
                  const errBlob = new BlobCtor(['throw new Error("Blocked");'], {{ type: 'application/javascript' }});
                  const errUrl = (UrlCtor && typeof UrlCtor.createObjectURL === 'function')
                    ? UrlCtor.createObjectURL(errBlob)
                    : URL.createObjectURL(errBlob);
                  const nextArgs = args.length > 1 ? [errUrl, ...args.slice(1)] : [errUrl];
                  return Reflect.construct(target, nextArgs, newTarget);
                }}
                const cacheKey = ctorName === 'SharedWorker' && (!swDec || swDec.kind === 'continue')
                  ? targetState.version + ':' + (isMod ? 'm:' : 'c:') + absUrl
                  : null;
                let blobUrl = cacheKey ? targetState.sharedBlobUrls.get(cacheKey) : null;
                let releaseBlob = null;
                if (!blobUrl) {{
                  const blobKey = absUrl.startsWith('blob:') ? absUrl.split('#')[0] : null;
                  const useDeferredRevoke = Boolean(blobKey && !targetState.deferredRevokes.has(blobKey));
                  const ackCh = useDeferredRevoke
                    ? 'w_' + Math.random().toString(36).slice(2) + '_' + Date.now().toString(36)
                    : null;
                  const ackSnippet = ackCh
                    ? '\ntry {{ if (typeof BroadcastChannel === "function") {{ const __bc = new BroadcastChannel(' + JSON.stringify(ackCh) + '); __bc.postMessage(1); __bc.close(); }} }} catch {{}}'
                    : '';
                  let loader = isMod
                    ? modLoaderPrefix + JSON.stringify(absUrl) + modLoaderMid + ackSnippet + '\n' + modLoaderEnd
                    : (ackCh
                      ? 'try {{ importScripts(' + JSON.stringify(absUrl) + '); }} finally {{' + ackSnippet + '\n}}'
                      : 'importScripts(' + JSON.stringify(absUrl) + ');');
                  if (swDec && swDec.kind === 'fulfill') {{
                    const rawBin = atob(swDec.bodyB64 || '');
                    const rawBytes = new Uint8Array(rawBin.length);
                    for (let i = 0; i < rawBin.length; i++) rawBytes[i] = rawBin.charCodeAt(i);
                    loader = new TextDecoder().decode(rawBytes) + ackSnippet;
                  }}
                  const netPrefix = (netState && netState.netInitSrc)
                    ? '(' + netState.netInitSrc + ')(' + JSON.stringify(netState.policy || {{}}) + ',' + JSON.stringify(netState.channelName || '') + ',' + JSON.stringify(absUrl) + ');\n'
                    : '';
                  const blob = new BlobCtor([
                    '"use strict";\n(' + targetState.workerPrelude + ')(' + JSON.stringify(absUrl) + ');\n' + netPrefix + loader
                  ], {{ type: 'application/javascript' }});
                  try {{
                    blobUrl = (UrlCtor && typeof UrlCtor.createObjectURL === 'function')
                      ? UrlCtor.createObjectURL(blob)
                      : URL.createObjectURL(blob);
                  }} catch {{
                    blobUrl = URL.createObjectURL(blob);
                  }}
                  if (cacheKey) targetState.sharedBlobUrls.set(cacheKey, blobUrl);
                  if (useDeferredRevoke) {{
                    targetState.pendingBlobUrls.set(blobKey, (targetState.pendingBlobUrls.get(blobKey) || 0) + 1);
                    let settled = false;
                    let bc = null;
                    let timer = null;
                    const ownWrapperUrl = !cacheKey ? blobUrl : null;
                    releaseBlob = () => {{
                      if (settled) return;
                      settled = true;
                      if (timer !== null) {{
                        try {{ (win.clearTimeout || clearTimeout)(timer); }} catch {{}}
                        timer = null;
                      }}
                      if (bc) {{
                        try {{ bc.close(); }} catch {{}}
                        bc = null;
                      }}
                      const cnt = (targetState.pendingBlobUrls.get(blobKey) || 0) - 1;
                      if (cnt <= 0) {{
                        targetState.pendingBlobUrls.delete(blobKey);
                        const fn = targetState.deferredRevokes.get(blobKey);
                        if (fn) {{
                          targetState.deferredRevokes.delete(blobKey);
                          fn();
                        }}
                      }} else {{
                        targetState.pendingBlobUrls.set(blobKey, cnt);
                      }}
                      if (ownWrapperUrl) {{
                        const origRev = (UrlCtor && targetState.origUrlRevoke.get(UrlCtor))
                          || (typeof URL !== 'undefined' && targetState.origUrlRevoke.get(URL));
                        if (typeof origRev === 'function') {{
                          try {{ origRev.call(UrlCtor || URL, ownWrapperUrl); }} catch {{}}
                        }}
                      }}
                    }};
                    try {{
                      const BC = win.BroadcastChannel || (typeof BroadcastChannel === 'function' ? BroadcastChannel : null);
                      if (BC) {{
                        bc = new BC(ackCh);
                        bc.onmessage = releaseBlob;
                      }}
                    }} catch {{}}
                    try {{
                      const setT = win.setTimeout || (typeof setTimeout === 'function' ? setTimeout : null);
                      if (setT) timer = setT(releaseBlob, 3000);
                    }} catch {{}}
                  }}
                }}
                const nextArgs = args.length > 1 ? [blobUrl, ...args.slice(1)] : [blobUrl];
                try {{
                  return Reflect.construct(target, nextArgs, newTarget);
                }} catch (err) {{
                  if (releaseBlob) releaseBlob();
                  throw err;
                }}
              }}
            }} catch {{}}
            return Reflect.construct(target, args, newTarget);
          }}
        }});
        wrapMarks.add(Wrapped);
        try {{
          Object.defineProperty(win, ctorName, {{
            value: Wrapped,
            configurable: true,
            writable: true
          }});
        }} catch {{}}
      }}
    }}

    const patchFrameGetters = (proto) => {{
      if (!proto || targetState.hookedProtos.has(proto)) return;
      targetState.hookedProtos.add(proto);
      const cwDesc = Object.getOwnPropertyDescriptor(proto, 'contentWindow');
      const cdDesc = Object.getOwnPropertyDescriptor(proto, 'contentDocument');
      const origCwGet = cwDesc && cwDesc.get;
      const origCdGet = cdDesc && cdDesc.get;
      if (typeof origCwGet === 'function') {{
        targetState.origProtoCwGet.set(proto, origCwGet);
        try {{
          Object.defineProperty(proto, 'contentWindow', {{
            get() {{
              const cw = origCwGet.call(this);
              if (cw) {{
                try {{ (targetState.applyToWindow || applyToWindow)(cw); }} catch {{}}
              }}
              return cw;
            }},
            enumerable: cwDesc.enumerable,
            configurable: true
          }});
        }} catch {{}}
      }}
      if (typeof origCdGet === 'function') {{
        try {{
          Object.defineProperty(proto, 'contentDocument', {{
            get() {{
              if (typeof origCwGet === 'function') {{
                try {{
                  const cw = origCwGet.call(this);
                  if (cw) (targetState.applyToWindow || applyToWindow)(cw);
                }} catch {{}}
              }}
              const cd = origCdGet.call(this);
              if (cd) {{
                try {{
                  if (cd.defaultView) (targetState.applyToWindow || applyToWindow)(cd.defaultView);
                }} catch {{}}
              }}
              return cd;
            }},
            enumerable: cdDesc.enumerable,
            configurable: true
          }});
        }} catch {{}}
      }}
    }};
    if (typeof win.HTMLIFrameElement !== 'undefined') {{
      patchFrameGetters(win.HTMLIFrameElement.prototype);
    }}
    if (typeof win.HTMLFrameElement !== 'undefined') {{
      patchFrameGetters(win.HTMLFrameElement.prototype);
    }}

    if (!curCfg.isChromium && !targetState.hookedDomRealms.has(win)) {{
      targetState.hookedDomRealms.add(win);
      const wrapDomMethod = (proto, methodName, getScanRoot) => {{
        if (!proto) return;
        try {{
          const orig = proto[methodName];
          if (typeof orig !== 'function') return;
          const desc = Object.getOwnPropertyDescriptor(proto, methodName);
          const wrapped = function(...args) {{
            const preRoot = getScanRoot ? getScanRoot(this, args) : this;
            const res = orig.apply(this, args);
            scanNodeFramesForState(targetState, preRoot || this);
            return res;
          }};
          Object.defineProperty(proto, methodName, {{
            value: wrapped,
            configurable: true,
            writable: true,
            enumerable: desc ? desc.enumerable : true
          }});
        }} catch {{}}
      }};
      const wrapHtmlSetter = (proto, prop, getScanRoot) => {{
        if (!proto) return;
        try {{
          const desc = Object.getOwnPropertyDescriptor(proto, prop);
          if (!desc || typeof desc.set !== 'function') return;
          const origSet = desc.set;
          Object.defineProperty(proto, prop, {{
            get: desc.get,
            set(val) {{
              const preRoot = getScanRoot ? getScanRoot(this) : this;
              const res = origSet.call(this, val);
              scanNodeFramesForState(targetState, preRoot || this);
              return res;
            }},
            enumerable: desc.enumerable,
            configurable: true
          }});
        }} catch {{}}
      }};
      const adjRoot = (el, args) => {{
        const p = args && args.length > 0 ? String(args[0] || '').toLowerCase() : '';
        return (p === 'beforebegin' || p === 'afterend') ? el.parentNode : el;
      }};
      if (typeof win.Node !== 'undefined') {{
        const np = win.Node.prototype;
        wrapDomMethod(np, 'appendChild');
        wrapDomMethod(np, 'insertBefore');
        wrapDomMethod(np, 'replaceChild');
      }}
      if (typeof win.Element !== 'undefined') {{
        const ep = win.Element.prototype;
        wrapDomMethod(ep, 'append');
        wrapDomMethod(ep, 'prepend');
        wrapDomMethod(ep, 'replaceChildren');
        wrapDomMethod(ep, 'before', (el) => el.parentNode);
        wrapDomMethod(ep, 'after', (el) => el.parentNode);
        wrapDomMethod(ep, 'replaceWith', (el) => el.parentNode);
        wrapDomMethod(ep, 'insertAdjacentElement', adjRoot);
        wrapDomMethod(ep, 'insertAdjacentHTML', adjRoot);
        wrapDomMethod(ep, 'setHTMLUnsafe');
        wrapHtmlSetter(ep, 'innerHTML');
        wrapHtmlSetter(ep, 'outerHTML', (el) => el.parentNode);
      }}
      if (typeof win.ShadowRoot !== 'undefined') {{
        const srp = win.ShadowRoot.prototype;
        wrapDomMethod(srp, 'append');
        wrapDomMethod(srp, 'prepend');
        wrapDomMethod(srp, 'replaceChildren');
        wrapDomMethod(srp, 'setHTMLUnsafe');
        wrapHtmlSetter(srp, 'innerHTML');
      }}
      if (typeof win.Document !== 'undefined') {{
        const dp = win.Document.prototype;
        wrapDomMethod(dp, 'append');
        wrapDomMethod(dp, 'prepend');
        wrapDomMethod(dp, 'replaceChildren');
        wrapDomMethod(dp, 'setHTMLUnsafe');
        wrapDomMethod(dp, 'write');
        wrapDomMethod(dp, 'writeln');
        if (typeof win.Document.parseHTMLUnsafe === 'function') {{
          try {{
            const origParseUnsafe = win.Document.parseHTMLUnsafe;
            const pDesc = Object.getOwnPropertyDescriptor(win.Document, 'parseHTMLUnsafe');
            Object.defineProperty(win.Document, 'parseHTMLUnsafe', {{
              value: function(...args) {{
                const parsedDoc = origParseUnsafe.apply(this, args);
                scanNodeFramesForState(targetState, parsedDoc);
                return parsedDoc;
              }},
              configurable: true,
              writable: true,
              enumerable: pDesc ? pDesc.enumerable : true
            }});
          }} catch {{}}
        }}
      }}
      if (typeof win.Range !== 'undefined') {{
        const rp = win.Range.prototype;
        const rangeRoot = (r) => {{
          try {{
            const sc = r && r.startContainer;
            if (!sc) return doc;
            return typeof sc.querySelectorAll === 'function' ? sc : (sc.parentNode || doc);
          }} catch {{
            return doc;
          }}
        }};
        wrapDomMethod(rp, 'insertNode', rangeRoot);
        wrapDomMethod(rp, 'surroundContents', rangeRoot);
      }}
      try {{
        const fDesc = Object.getOwnPropertyDescriptor(win, 'frames');
        if (fDesc && fDesc.configurable && typeof fDesc.get === 'function') {{
          const origFramesGet = fDesc.get;
          Object.defineProperty(win, 'frames', {{
            get() {{
              if (doc) scanNodeFramesForState(targetState, doc);
              return origFramesGet.call(this);
            }},
            enumerable: fDesc.enumerable,
            configurable: true
          }});
        }}
      }} catch {{}}
      if (doc) {{
        const MO = win.MutationObserver || (typeof MutationObserver !== 'undefined' ? MutationObserver : null);
        if (typeof MO === 'function') {{
          try {{
            const mo = new MO((records) => {{
              for (let i = 0; i < records.length; i++) {{
                const added = records[i].addedNodes;
                if (!added) continue;
                for (let j = 0; j < added.length; j++) {{
                  const n = added[j];
                  if (!n || n.nodeType !== 1) continue;
                  const tag = n.tagName;
                  if (tag === 'IFRAME' || tag === 'FRAME') {{
                    patchFrameElForState(targetState, n);
                  }} else if (n.firstElementChild) {{
                    scanNodeFramesForState(targetState, n);
                  }}
                }}
              }}
            }});
            mo.observe(doc, {{ childList: true, subtree: true }});
          }} catch {{}}
        }}
        if (typeof doc.addEventListener === 'function') {{
          try {{
            doc.addEventListener('DOMContentLoaded', () => {{
              scanNodeFramesForState(targetState, doc);
            }}, true);
          }} catch {{}}
        }}
      }}
    }}

    if (typeof win.open === 'function' && !targetState.hookedOpenWins.has(win)) {{
      targetState.hookedOpenWins.add(win);
      const origOpen = win.open;
      const wrappedOpen = new Proxy(origOpen, {{
        apply(target, thisArg, args) {{
          const pop = Reflect.apply(target, thisArg, args);
          if (pop) {{
            try {{ (targetState.applyToWindow || applyToWindow)(pop); }} catch {{}}
          }}
          return pop;
        }}
      }});
      try {{
        Object.defineProperty(win, 'open', {{
          value: wrappedOpen,
          configurable: true,
          writable: true
        }});
      }} catch {{}}
    }}

    if (doc) {{
      scanNodeFramesForState(targetState, doc);
    }}
  }};

  state.applyToWindow = applyToWindow;
  applyToWindow(globalThis);
}})();"#
    )
}

/// Generate an isolated preload script for dedicated/shared/service Worker realms.
pub fn build_worker_init_script(
    profile: &EnvProfile,
    engine: EngineKind,
    include_tz_intl: bool,
) -> String {
    build_worker_init_script_inner(profile, engine, include_tz_intl, false)
}

fn build_worker_init_script_inner(
    profile: &EnvProfile,
    engine: EngineKind,
    include_tz_intl: bool,
    as_base_url_fn: bool,
) -> String {
    let config_json = build_js_config(profile, engine);
    let base_url_patch = if as_base_url_fn {
        r#"
  if (typeof baseUrl === 'string' && /^https?:/i.test(baseUrl)) {
    const origFetch = typeof globalThis.fetch === 'function' ? globalThis.fetch : null;
    const origRequest = typeof globalThis.Request === 'function' ? globalThis.Request : null;
    if (origRequest) {
      const WrappedRequest = new Proxy(origRequest, {
        construct(target, args, newTarget) {
          if (args.length > 0 && !(args[0] instanceof origRequest) && args[0] !== undefined) {
            try {
              const nextArgs = args.slice();
              nextArgs[0] = new URL(String(args[0]), baseUrl).href;
              return Reflect.construct(target, nextArgs, newTarget);
            } catch {}
          }
          return Reflect.construct(target, args, newTarget);
        }
      });
      try { globalThis.Request = WrappedRequest; } catch {}
    }
    if (origFetch) {
      globalThis.fetch = function(input, init) {
        if (arguments.length > 0 && !(origRequest && input instanceof origRequest) && input !== undefined) {
          try {
            const resolved = new URL(String(input), baseUrl).href;
            return arguments.length > 1
              ? origFetch.call(this, resolved, init)
              : origFetch.call(this, resolved);
          } catch {}
        }
        return origFetch.apply(this, arguments);
      };
    }
    if (typeof XMLHttpRequest !== 'undefined' && XMLHttpRequest.prototype && typeof XMLHttpRequest.prototype.open === 'function') {
      const origXhrOpen = XMLHttpRequest.prototype.open;
      XMLHttpRequest.prototype.open = function(method, url, ...rest) {
        let nextUrl = url;
        if (url !== undefined && url !== null) {
          try { nextUrl = new URL(String(url), baseUrl).href; } catch {}
        }
        return origXhrOpen.call(this, method, nextUrl, ...rest);
      };
    }
    if (typeof globalThis.WebSocket === 'function') {
      const OrigWS = globalThis.WebSocket;
      const WrappedWS = new Proxy(OrigWS, {
        construct(target, args, newTarget) {
          if (args.length > 0 && args[0] !== undefined) {
            try {
              const u = new URL(String(args[0]), baseUrl);
              if (u.protocol === 'http:') u.protocol = 'ws:';
              else if (u.protocol === 'https:') u.protocol = 'wss:';
              const nextArgs = args.slice();
              nextArgs[0] = u.href;
              return Reflect.construct(target, nextArgs, newTarget);
            } catch {}
          }
          return Reflect.construct(target, args, newTarget);
        }
      });
      try { globalThis.WebSocket = WrappedWS; } catch {}
    }
    if (typeof globalThis.EventSource === 'function') {
      const OrigES = globalThis.EventSource;
      const WrappedES = new Proxy(OrigES, {
        construct(target, args, newTarget) {
          if (args.length > 0 && args[0] !== undefined) {
            try {
              const nextArgs = args.slice();
              nextArgs[0] = new URL(String(args[0]), baseUrl).href;
              return Reflect.construct(target, nextArgs, newTarget);
            } catch {}
          }
          return Reflect.construct(target, args, newTarget);
        }
      });
      try { globalThis.EventSource = WrappedES; } catch {}
    }
    if (typeof globalThis.importScripts === 'function') {
      const origImportScripts = globalThis.importScripts;
      globalThis.importScripts = function(...urls) {
        const resolved = urls.map((u) => {
          try { return new URL(String(u), baseUrl).href; } catch { return u; }
        });
        return origImportScripts.apply(this, resolved);
      };
    }
    try {
      const locUrl = new URL(baseUrl);
      const locProto = typeof WorkerLocation !== 'undefined'
        ? WorkerLocation.prototype
        : (typeof location !== 'undefined' ? Object.getPrototypeOf(location) : null);
      if (locProto) {
        for (const k of ['href', 'origin', 'protocol', 'host', 'hostname', 'port', 'pathname', 'search', 'hash']) {
          defGetter(locProto, k, () => locUrl[k]);
        }
        try {
          Object.defineProperty(locProto, 'toString', {
            value: () => locUrl.href,
            enumerable: true,
            configurable: true,
            writable: true
          });
        } catch {}
      }
    } catch {}
  }
  {
    const modLoaderPrefix = [
      'const q = [];',
      'const buf = (ev) => { q.push(ev); };',
      'const evTypes = "SharedWorkerGlobalScope" in globalThis ? ["connect", "message", "messageerror"] : ["message", "messageerror"];',
      'for (const t of evTypes) globalThis.addEventListener(t, buf);',
      'let loaded = false;',
      'try {',
      '  await import('
    ].join('\n');
    const modLoaderMid = [
      ');',
      '  loaded = true;',
      '} finally {',
      '  for (const t of evTypes) globalThis.removeEventListener(t, buf);'
    ].join('\n');
    const modLoaderEnd = [
      '}',
      'if (loaded) {',
      '  for (const ev of q) {',
      '    globalThis.dispatchEvent(new MessageEvent(ev.type, {',
      '      data: ev.data,',
      '      origin: ev.origin,',
      '      lastEventId: ev.lastEventId,',
      '      source: ev.source ?? null,',
      '      ports: ev.ports ? Array.from(ev.ports) : []',
      '    }));',
      '  }',
      '}'
    ].join('\n');
    const sharedBlobUrls = new Map();
    const pendingBlobUrls = new Map();
    const deferredRevokes = new Map();
    const origRevoke = (typeof URL !== 'undefined' && typeof URL.revokeObjectURL === 'function')
      ? URL.revokeObjectURL
      : null;
    if (origRevoke) {
      const desc = Object.getOwnPropertyDescriptor(URL, 'revokeObjectURL');
      const wrappedRevoke = new Proxy(origRevoke, {
        apply(target, thisArg, args) {
          const rawUrl = args && args.length > 0 ? args[0] : undefined;
          if (rawUrl !== undefined && rawUrl !== null) {
            const rawKey = String(rawUrl);
            const key = rawKey.split('#')[0];
            sharedBlobUrls.delete('c:' + rawKey);
            sharedBlobUrls.delete('m:' + rawKey);
            sharedBlobUrls.delete('c:' + key);
            sharedBlobUrls.delete('m:' + key);
            if ((pendingBlobUrls.get(key) || 0) > 0) {
              deferredRevokes.set(key, () => {
                try { Reflect.apply(target, thisArg || URL, args); } catch {}
              });
              return undefined;
            }
          }
          return Reflect.apply(target, thisArg || URL, args);
        }
      });
      try {
        Object.defineProperty(URL, 'revokeObjectURL', {
          value: wrappedRevoke,
          configurable: true,
          writable: true,
          enumerable: desc ? desc.enumerable : true
        });
      } catch {}
    }
    const resolvedBase = (typeof baseUrl === 'string' && /^https?:/i.test(baseUrl))
      ? baseUrl
      : (globalThis.location && typeof globalThis.location.href === 'string' ? globalThis.location.href : '');
    const selfSrc = _wInit.toString();
    for (const ctorName of ['Worker', 'SharedWorker']) {
      const Orig = globalThis[ctorName];
      if (typeof Orig !== 'function') continue;
      const Wrapped = new Proxy(Orig, {
        construct(target, args, newTarget) {
          const scriptUrl = args[0];
          const opts = args[1];
          try {
            if (typeof scriptUrl === 'string' || (typeof URL !== 'undefined' && scriptUrl instanceof URL)) {
              const absUrl = resolvedBase
                ? new URL(String(scriptUrl), resolvedBase).href
                : new URL(String(scriptUrl)).href;
              const isMod = opts && typeof opts === 'object' && opts.type === 'module';
              const cacheKey = ctorName === 'SharedWorker' ? (isMod ? 'm:' : 'c:') + absUrl : null;
              let blobUrl = cacheKey ? sharedBlobUrls.get(cacheKey) : null;
              let releaseBlob = null;
              if (!blobUrl) {
                const blobKey = absUrl.startsWith('blob:') ? absUrl.split('#')[0] : null;
                const useDeferredRevoke = Boolean(blobKey && !deferredRevokes.has(blobKey));
                const ackCh = useDeferredRevoke
                  ? 'w_' + Math.random().toString(36).slice(2) + '_' + Date.now().toString(36)
                  : null;
                const ackSnippet = ackCh
                  ? '\ntry { if (typeof BroadcastChannel === "function") { const __bc = new BroadcastChannel(' + JSON.stringify(ackCh) + '); __bc.postMessage(1); __bc.close(); } } catch {}'
                  : '';
                const loader = isMod
                  ? modLoaderPrefix + JSON.stringify(absUrl) + modLoaderMid + ackSnippet + '\n' + modLoaderEnd
                  : (ackCh
                    ? 'try { importScripts(' + JSON.stringify(absUrl) + '); } finally {' + ackSnippet + '\n}'
                    : 'importScripts(' + JSON.stringify(absUrl) + ');');
                const blob = new Blob([
                  '"use strict";\n(' + selfSrc + ')(' + JSON.stringify(absUrl) + ');\n' + loader
                ], { type: 'application/javascript' });
                blobUrl = URL.createObjectURL(blob);
                if (cacheKey) sharedBlobUrls.set(cacheKey, blobUrl);
                if (useDeferredRevoke) {
                  pendingBlobUrls.set(blobKey, (pendingBlobUrls.get(blobKey) || 0) + 1);
                  let settled = false;
                  let bc = null;
                  let timer = null;
                  const ownWrapperUrl = !cacheKey ? blobUrl : null;
                  releaseBlob = () => {
                    if (settled) return;
                    settled = true;
                    if (timer !== null) {
                      try { clearTimeout(timer); } catch {}
                      timer = null;
                    }
                    if (bc) {
                      try { bc.close(); } catch {}
                      bc = null;
                    }
                    const cnt = (pendingBlobUrls.get(blobKey) || 0) - 1;
                    if (cnt <= 0) {
                      pendingBlobUrls.delete(blobKey);
                      const fn = deferredRevokes.get(blobKey);
                      if (fn) {
                        deferredRevokes.delete(blobKey);
                        fn();
                      }
                    } else {
                      pendingBlobUrls.set(blobKey, cnt);
                    }
                    if (ownWrapperUrl && origRevoke) {
                      try { origRevoke.call(URL, ownWrapperUrl); } catch {}
                    }
                  };
                  try {
                    if (typeof BroadcastChannel === 'function') {
                      bc = new BroadcastChannel(ackCh);
                      bc.onmessage = releaseBlob;
                    }
                  } catch {}
                  try {
                    if (typeof setTimeout === 'function') timer = setTimeout(releaseBlob, 3000);
                  } catch {}
                }
              }
              const nextArgs = args.length > 1 ? [blobUrl, ...args.slice(1)] : [blobUrl];
              try {
                return Reflect.construct(target, nextArgs, newTarget);
              } catch (err) {
                if (releaseBlob) releaseBlob();
                throw err;
              }
            }
          } catch {}
          return Reflect.construct(target, args, newTarget);
        }
      });
      try {
        Object.defineProperty(globalThis, ctorName, {
          value: Wrapped,
          configurable: true,
          writable: true
        });
      } catch {}
    }
  }"#
    } else {
        ""
    };
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

    let (header, footer) = if as_base_url_fn {
        ("(function _wInit(baseUrl) {", "})")
    } else {
        ("\"use strict\";\n(() => {", "})();")
    };

    format!(
        r#"{header}
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
{base_url_patch}

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
{footer}"#
    )
}

const FIREFOX_SHARED_WORKER_NET_TEMPLATE: &str = r#""use strict";
(() => {
  const _wNetDecide = (policy, url, rtype) => {
    if (!policy || !policy.active) return { kind: 'continue' };
    const wc = (pat, text) => {
      let p = 0, t = 0, sp = -1, st = 0;
      while (t < text.length) {
        if (p < pat.length && (pat[p] === '?' || pat[p] === text[t])) {
          p++; t++;
        } else if (p < pat.length && pat[p] === '*') {
          sp = p; st = t; p++;
        } else if (sp !== -1) {
          p = sp + 1; st++; t = st;
        } else {
          return false;
        }
      }
      while (p < pat.length && pat[p] === '*') p++;
      return p === pat.length;
    };
    const matchPat = (m, u) => {
      if (!m) return false;
      if (m.kind === 'exact') return u === m.val;
      if (m.kind === 'prefix') return u.startsWith(m.val);
      if (m.kind === 'substring') return u.includes(m.val);
      if (m.kind === 'glob') return wc(m.val || '', u);
      if (m.kind === 'regex') {
        try { return new RegExp(m.pat, m.flags || '').test(u); } catch { return false; }
      }
      return false;
    };
    if (Array.isArray(policy.blockedUrls)) {
      for (let i = 0; i < policy.blockedUrls.length; i++) {
        if (matchPat(policy.blockedUrls[i], url)) return { kind: 'abort' };
      }
    }
    if (Array.isArray(policy.rules)) {
      for (let i = 0; i < policy.rules.length; i++) {
        const r = policy.rules[i];
        if (!matchPat(r.matcher, url)) continue;
        const rts = r.resourceTypes;
        if (Array.isArray(rts) && rts.length > 0) {
          const ok = rts.includes(rtype)
            || (rtype === 'fetch' && rts.includes('xhr'))
            || (rtype === 'xhr' && rts.includes('fetch'));
          if (!ok) continue;
        }
        return r.action || { kind: 'continue' };
      }
    }
    if (policy.blockImages || policy.blockMedia || policy.blockFonts) {
      if (rtype === 'image' && policy.blockImages) return { kind: 'abort' };
      if (rtype === 'media' && policy.blockMedia) return { kind: 'abort' };
      if (rtype === 'font' && policy.blockFonts) return { kind: 'abort' };
      try {
        const pathname = new URL(url).pathname.toLowerCase();
        const dot = pathname.lastIndexOf('.');
        const ext = dot >= 0 ? pathname.slice(dot + 1) : '';
        if (policy.blockImages && ['png','jpg','jpeg','gif','webp','svg','ico','avif'].includes(ext)) return { kind: 'abort' };
        if (policy.blockMedia && ['mp4','webm','ogg','mp3','wav','m3u8','mpd','m4a','flac'].includes(ext)) return { kind: 'abort' };
        if (policy.blockFonts && ['woff','woff2','ttf','otf','eot'].includes(ext)) return { kind: 'abort' };
      } catch {}
    }
    return { kind: 'continue' };
  };

  function _wNetInit(initialPolicy, channelName, baseUrl) {
    const initSym = Symbol.for('net.worker.init');
    const existing = globalThis[initSym];
    if (existing) {
      existing.policy = initialPolicy || {};
      return;
    }
    const state = { policy: initialPolicy || {} };
    try {
      Object.defineProperty(globalThis, initSym, {
        value: state,
        enumerable: false,
        configurable: true,
        writable: true
      });
    } catch {
      try { globalThis[initSym] = state; } catch {}
    }
    const decide = __DECIDE_FN__;
    try {
      if (channelName && typeof BroadcastChannel === 'function') {
        const bc = new BroadcastChannel(channelName);
        bc.onmessage = (ev) => {
          if (ev && ev.data && typeof ev.data === 'object') {
            state.policy = ev.data;
          }
        };
      }
    } catch {}
    const hasHttpBase = typeof baseUrl === 'string' && /^https?:/i.test(baseUrl);
    const resolveUrl = (u) => {
      const s = String(u);
      try {
        if (hasHttpBase) return new URL(s, baseUrl).href;
        if (globalThis.location && typeof globalThis.location.href === 'string') {
          return new URL(s, globalThis.location.href).href;
        }
        return new URL(s).href;
      } catch {
        return s;
      }
    };
    if (hasHttpBase) {
      try {
        const curHref = globalThis.location && typeof globalThis.location.href === 'string' ? globalThis.location.href : '';
        if (!/^https?:/i.test(curHref)) {
          const locUrl = new URL(baseUrl);
          const locProto = typeof WorkerLocation !== 'undefined'
            ? WorkerLocation.prototype
            : (typeof location !== 'undefined' ? Object.getPrototypeOf(location) : null);
          if (locProto) {
            for (const k of ['href', 'origin', 'protocol', 'host', 'hostname', 'port', 'pathname', 'search', 'hash']) {
              try {
                Object.defineProperty(locProto, k, { get: () => locUrl[k], enumerable: true, configurable: true });
              } catch {}
            }
            try {
              Object.defineProperty(locProto, 'toString', { value: () => locUrl.href, enumerable: true, configurable: true, writable: true });
            } catch {}
          }
        }
      } catch {}
    }
    const mergeHdrs = (baseObj, ovObj) => {
      const out = Object.assign({}, baseObj || {});
      if (!ovObj) return out;
      for (const [ok, ov] of Object.entries(ovObj)) {
        const low = ok.toLowerCase();
        for (const ek of Object.keys(out)) {
          if (ek.toLowerCase() === low) delete out[ek];
        }
        out[ok] = String(ov);
      }
      return out;
    };
    const b64ToBytes = (b64) => {
      const bin = atob(b64 || '');
      const bytes = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      return bytes;
    };
    const origFetch = typeof globalThis.fetch === 'function' ? globalThis.fetch : null;
    const origRequest = typeof globalThis.Request === 'function' ? globalThis.Request : null;
    if (origFetch) {
      globalThis.fetch = function(input, init) {
        let rawUrl = '';
        const hdrsObj = {};
        const isReq = Boolean(origRequest && input instanceof origRequest);
        if (isReq) {
          rawUrl = input.url;
          try { input.headers.forEach((v, k) => { hdrsObj[k] = v; }); } catch {}
        } else if (input !== undefined && input !== null) {
          rawUrl = String(input);
        }
        if (init && init.headers) {
          try {
            if (typeof Headers === 'function' && init.headers instanceof Headers) {
              init.headers.forEach((v, k) => { hdrsObj[k] = v; });
            } else if (Array.isArray(init.headers)) {
              for (const pair of init.headers) {
                if (Array.isArray(pair) && pair.length >= 2) hdrsObj[String(pair[0])] = String(pair[1]);
              }
            } else if (typeof init.headers === 'object') {
              for (const [k, v] of Object.entries(init.headers)) hdrsObj[k] = String(v);
            }
          } catch {}
        }
        const resolvedUrl = resolveUrl(rawUrl);
        const dec = decide(state.policy, resolvedUrl, 'fetch');
        if (dec.kind === 'abort') {
          return Promise.reject(new TypeError('Failed to fetch'));
        }
        if (dec.kind === 'fulfill') {
          const bytes = b64ToBytes(dec.bodyB64);
          const hdrs = new Headers();
          if (dec.headers) {
            for (const [k, v] of Object.entries(dec.headers)) {
              try { hdrs.set(k, String(v)); } catch {}
            }
          }
          const st = Number(dec.status) || 200;
          const isNullBody = st === 204 || st === 205 || st === 304 || (st >= 100 && st < 200);
          return Promise.resolve(new Response(isNullBody ? null : bytes, {
            status: st,
            statusText: dec.statusText || 'OK',
            headers: hdrs
          }));
        }
        if (dec.kind === 'modify_headers') {
          const merged = mergeHdrs(hdrsObj, dec.headers);
          const nextInit = Object.assign({}, init || {}, { headers: merged });
          if (isReq && origRequest) {
            return origFetch.call(this, new origRequest(input, nextInit));
          }
          return origFetch.call(this, resolvedUrl, nextInit);
        }
        if (!isReq && input !== undefined && input !== null) {
          return arguments.length > 1
            ? origFetch.call(this, resolvedUrl, init)
            : origFetch.call(this, resolvedUrl);
        }
        return origFetch.apply(this, arguments);
      };
    }
    if (typeof XMLHttpRequest !== 'undefined' && XMLHttpRequest.prototype) {
      const xp = XMLHttpRequest.prototype;
      const xMap = new WeakMap();
      const origOpen = xp.open;
      const origSetHdr = xp.setRequestHeader;
      const origSend = xp.send;
      const origGetHdr = xp.getResponseHeader;
      const origGetAllHdrs = xp.getAllResponseHeaders;
      const descReady = Object.getOwnPropertyDescriptor(xp, 'readyState');
      const descStatus = Object.getOwnPropertyDescriptor(xp, 'status');
      const descStatusText = Object.getOwnPropertyDescriptor(xp, 'statusText');
      const descRespText = Object.getOwnPropertyDescriptor(xp, 'responseText');
      const descResp = Object.getOwnPropertyDescriptor(xp, 'response');
      const descRespUrl = Object.getOwnPropertyDescriptor(xp, 'responseURL');
      const getRec = (xhr) => {
        let r = xMap.get(xhr);
        if (!r) { r = { url: '', async: true, headers: {}, mocked: null }; xMap.set(xhr, r); }
        return r;
      };
      if (typeof origOpen === 'function') {
        xp.open = function(method, url, asyncFlag, ...rest) {
          const r = getRec(this);
          const resolved = (url !== undefined && url !== null) ? resolveUrl(url) : url;
          r.url = String(resolved || '');
          r.async = asyncFlag !== false;
          r.headers = {};
          r.mocked = null;
          return origOpen.call(this, method, resolved, asyncFlag, ...rest);
        };
      }
      if (typeof origSetHdr === 'function') {
        xp.setRequestHeader = function(name, value) {
          const r = xMap.get(this);
          if (!r || this.readyState !== 1) {
            return origSetHdr.call(this, name, value);
          }
          const k = String(name);
          const low = k.toLowerCase();
          for (const ek of Object.keys(r.headers)) {
            if (ek.toLowerCase() === low) delete r.headers[ek];
          }
          r.headers[k] = String(value);
        };
      }
      if (typeof origSend === 'function') {
        xp.send = function(body) {
          const r = getRec(this);
          const dec = decide(state.policy, r.url, 'xhr');
          if (dec.kind === 'abort') {
            if (!r.async) {
              throw new DOMException('NetworkError', 'NetworkError');
            }
            r.mocked = { readyState: 1, status: 0, statusText: '', headers: {}, bytes: new Uint8Array(0), text: '', url: r.url };
            const fireErr = () => {
              r.mocked.readyState = 4;
              try { this.dispatchEvent(new Event('readystatechange')); } catch {}
              try { this.dispatchEvent(new ProgressEvent('error')); } catch {}
              try { this.dispatchEvent(new ProgressEvent('loadend')); } catch {}
            };
            Promise.resolve().then(fireErr);
            return;
          }
          if (dec.kind === 'fulfill') {
            const bytes = b64ToBytes(dec.bodyB64);
            const text = new TextDecoder().decode(bytes);
            const hdrs = {};
            if (dec.headers) {
              for (const [k, v] of Object.entries(dec.headers)) hdrs[k.toLowerCase()] = String(v);
            }
            r.mocked = {
              readyState: 1,
              status: Number(dec.status) || 200,
              statusText: dec.statusText || 'OK',
              headers: hdrs,
              bytes,
              text,
              url: r.url
            };
            const finish = () => {
              r.mocked.readyState = 4;
              try { this.dispatchEvent(new Event('readystatechange')); } catch {}
              try { this.dispatchEvent(new ProgressEvent('load', { loaded: bytes.length, total: bytes.length, lengthComputable: true })); } catch {}
              try { this.dispatchEvent(new ProgressEvent('loadend', { loaded: bytes.length, total: bytes.length, lengthComputable: true })); } catch {}
            };
            if (r.async) Promise.resolve().then(finish);
            else finish();
            return;
          }
          const finalHdrs = dec.kind === 'modify_headers'
            ? mergeHdrs(r.headers, dec.headers)
            : r.headers;
          for (const [k, v] of Object.entries(finalHdrs)) {
            origSetHdr.call(this, k, String(v));
          }
          return origSend.apply(this, arguments);
        };
      }
      if (typeof origGetHdr === 'function') {
        xp.getResponseHeader = function(name) {
          const r = xMap.get(this);
          if (r && r.mocked) {
            const k = String(name).toLowerCase();
            return Object.prototype.hasOwnProperty.call(r.mocked.headers, k) ? r.mocked.headers[k] : null;
          }
          return origGetHdr.call(this, name);
        };
      }
      if (typeof origGetAllHdrs === 'function') {
        xp.getAllResponseHeaders = function() {
          const r = xMap.get(this);
          if (r && r.mocked) {
            return Object.entries(r.mocked.headers).map(([k, v]) => k + ': ' + v).join('\r\n');
          }
          return origGetAllHdrs.call(this);
        };
      }
      const patchXhrProp = (prop, desc, fn) => {
        if (!desc || typeof desc.get !== 'function') return;
        try {
          Object.defineProperty(xp, prop, {
            get() {
              const r = xMap.get(this);
              if (r && r.mocked) return fn(this, r.mocked);
              return desc.get.call(this);
            },
            enumerable: desc.enumerable,
            configurable: true
          });
        } catch {}
      };
      patchXhrProp('readyState', descReady, (_, m) => m.readyState);
      patchXhrProp('status', descStatus, (_, m) => m.status);
      patchXhrProp('statusText', descStatusText, (_, m) => m.statusText);
      patchXhrProp('responseText', descRespText, (_, m) => m.text);
      patchXhrProp('responseURL', descRespUrl, (_, m) => m.url);
      patchXhrProp('response', descResp, (xhr, m) => {
        const rt = xhr.responseType || '';
        if (rt === 'json') { try { return JSON.parse(m.text); } catch { return null; } }
        if (rt === 'arraybuffer') return m.bytes.buffer.slice(0);
        if (rt === 'blob') return new Blob([m.bytes], { type: m.headers['content-type'] || '' });
        return m.text;
      });
    }
    if (typeof globalThis.importScripts === 'function') {
      const origImportScripts = globalThis.importScripts;
      globalThis.importScripts = function(...urls) {
        const nextUrls = [];
        for (const u of urls) {
          const resolved = resolveUrl(u);
          const dec = decide(state.policy, resolved, 'script');
          if (dec.kind === 'abort') {
            throw new DOMException('Blocked script: ' + resolved, 'NetworkError');
          }
          if (dec.kind === 'fulfill') {
            const bytes = b64ToBytes(dec.bodyB64);
            const code = new TextDecoder().decode(bytes);
            const b = new Blob([code], { type: 'application/javascript' });
            nextUrls.push(URL.createObjectURL(b));
          } else {
            nextUrls.push(resolved);
          }
        }
        return origImportScripts.apply(this, nextUrls);
      };
    }
  }

  const netSym = Symbol.for('net.runtime.state');
  const envSym = Symbol.for('env.runtime.state');
  const wrapMarkSym = Symbol.for('worker.wrap.marks');
  const wrapMarks = globalThis[wrapMarkSym] || (globalThis[wrapMarkSym] = new WeakSet());
  const policy = __POLICY_JSON__;
  const channelName = __CHANNEL_JSON__;
  const netInitSrc = _wNetInit.toString().replace('__DECIDE_FN__', _wNetDecide.toString());

  let netState = globalThis[netSym];
  if (!netState) {
    netState = {
      policy: policy,
      channelName: channelName,
      netInitSrc: netInitSrc,
      decide: null,
      hookedUrls: new WeakSet(),
      origUrlRevoke: new WeakMap(),
      pendingBlobUrls: new Map(),
      deferredRevokes: new Map()
    };
    try {
      Object.defineProperty(globalThis, netSym, {
        value: netState,
        enumerable: false,
        configurable: true,
        writable: true
      });
    } catch {
      try { globalThis[netSym] = netState; } catch {}
    }
  } else {
    netState.policy = policy;
    netState.channelName = channelName;
    netState.netInitSrc = netInitSrc;
    if (!netState.hookedUrls) netState.hookedUrls = new WeakSet();
    if (!netState.origUrlRevoke) netState.origUrlRevoke = new WeakMap();
    if (!netState.pendingBlobUrls) netState.pendingBlobUrls = new Map();
    if (!netState.deferredRevokes) netState.deferredRevokes = new Map();
  }
  netState.decide = (u, rt) => _wNetDecide(netState.policy, u, rt);

  const hookNetUrlRevoke = (uCtor) => {
    if (!uCtor || typeof uCtor.revokeObjectURL !== 'function' || netState.hookedUrls.has(uCtor)) return;
    netState.hookedUrls.add(uCtor);
    const origRevoke = uCtor.revokeObjectURL;
    netState.origUrlRevoke.set(uCtor, origRevoke);
    const desc = Object.getOwnPropertyDescriptor(uCtor, 'revokeObjectURL');
    const wrappedRevoke = new Proxy(origRevoke, {
      apply(target, thisArg, args) {
        const rawUrl = args && args.length > 0 ? args[0] : undefined;
        if (rawUrl !== undefined && rawUrl !== null) {
          const rawKey = String(rawUrl);
          const key = rawKey.split('#')[0];
          if (netState.sharedBlobUrls) {
            for (const k of Array.from(netState.sharedBlobUrls.keys())) {
              if (k.endsWith(':' + rawKey) || k.endsWith(':' + key)) {
                netState.sharedBlobUrls.delete(k);
              }
            }
          }
          if ((netState.pendingBlobUrls.get(key) || 0) > 0) {
            netState.deferredRevokes.set(key, () => {
              try { Reflect.apply(target, thisArg || uCtor, args); } catch {}
            });
            return undefined;
          }
        }
        return Reflect.apply(target, thisArg || uCtor, args);
      }
    });
    try {
      Object.defineProperty(uCtor, 'revokeObjectURL', {
        value: wrappedRevoke,
        configurable: true,
        writable: true,
        enumerable: desc ? desc.enumerable : true
      });
    } catch {}
  };
  if (typeof URL !== 'undefined') hookNetUrlRevoke(URL);

  try {
    if (channelName && typeof BroadcastChannel === 'function') {
      const bc = new BroadcastChannel(channelName);
      bc.postMessage(policy);
      bc.close();
    }
  } catch {}

  const OrigSW = globalThis.SharedWorker;
  if (typeof OrigSW === 'function' && !wrapMarks.has(OrigSW)) {
    const modLoaderPrefix = [
      'const q = [];',
      'const buf = (ev) => { q.push(ev); };',
      'const evTypes = ["connect", "message", "messageerror"];',
      'for (const t of evTypes) globalThis.addEventListener(t, buf);',
      'let loaded = false;',
      'try {',
      '  await import('
    ].join('\n');
    const modLoaderMid = [
      ');',
      '  loaded = true;',
      '} finally {',
      '  for (const t of evTypes) globalThis.removeEventListener(t, buf);'
    ].join('\n');
    const modLoaderEnd = [
      '}',
      'if (loaded) {',
      '  for (const ev of q) {',
      '    globalThis.dispatchEvent(new MessageEvent(ev.type, {',
      '      data: ev.data,',
      '      origin: ev.origin,',
      '      lastEventId: ev.lastEventId,',
      '      source: ev.source ?? null,',
      '      ports: ev.ports ? Array.from(ev.ports) : []',
      '    }));',
      '  }',
      '}'
    ].join('\n');
    const WrappedSW = new Proxy(OrigSW, {
      construct(target, args, newTarget) {
        const scriptUrl = args[0];
        const opts = args[1];
        try {
          if (typeof scriptUrl === 'string' || (typeof URL !== 'undefined' && scriptUrl instanceof URL)) {
            const rawHref = globalThis.location && typeof globalThis.location.href === 'string' ? globalThis.location.href : '';
            const docBase = globalThis.document && typeof globalThis.document.baseURI === 'string' ? globalThis.document.baseURI : '';
            const baseHref = (rawHref && !rawHref.startsWith('about:')) ? rawHref : ((docBase && !docBase.startsWith('about:')) ? docBase : '');
            const absUrl = baseHref ? new URL(String(scriptUrl), baseHref).href : new URL(String(scriptUrl)).href;
            const ns = globalThis[netSym];
            const es = globalThis[envSym];
            const dec = (ns && ns.policy && ns.policy.active && typeof ns.decide === 'function')
              ? ns.decide(absUrl, 'script')
              : { kind: 'continue' };
            if (dec.kind === 'abort') {
              const errBlob = new Blob(['throw new Error("Blocked");'], { type: 'application/javascript' });
              const errUrl = URL.createObjectURL(errBlob);
              const nextArgs = args.length > 1 ? [errUrl, ...args.slice(1)] : [errUrl];
              return Reflect.construct(target, nextArgs, newTarget);
            }
            if ((ns && ns.policy && ns.policy.active) || (es && es.wrapWorkers)) {
              const isMod = opts && typeof opts === 'object' && opts.type === 'module';
              if (!ns.sharedBlobUrls) ns.sharedBlobUrls = new Map();
              const envVer = es ? (es.version || 0) : 0;
              const cacheKey = (!absUrl.startsWith('blob:') && dec.kind === 'continue')
                ? envVer + ':' + (isMod ? 'm:' : 'c:') + absUrl
                : null;
              let blobUrl = cacheKey ? ns.sharedBlobUrls.get(cacheKey) : null;
              let releaseBlob = null;
              if (!blobUrl) {
                const blobKey = absUrl.startsWith('blob:') ? absUrl.split('#')[0] : null;
                const useDeferredRevoke = Boolean(blobKey && ns.deferredRevokes && !ns.deferredRevokes.has(blobKey));
                const ackCh = useDeferredRevoke
                  ? 'wn_' + Math.random().toString(36).slice(2) + '_' + Date.now().toString(36)
                  : null;
                const ackSnippet = ackCh
                  ? '\ntry { if (typeof BroadcastChannel === "function") { const __bc = new BroadcastChannel(' + JSON.stringify(ackCh) + '); __bc.postMessage(1); __bc.close(); } } catch {}'
                  : '';
                const envPrefix = (es && es.wrapWorkers && es.workerPrelude)
                  ? '(' + es.workerPrelude + ')(' + JSON.stringify(absUrl) + ');\n'
                  : '';
                const netPrefix = (ns && ns.netInitSrc)
                  ? '(' + ns.netInitSrc + ')(' + JSON.stringify(ns.policy || {}) + ',' + JSON.stringify(ns.channelName || '') + ',' + JSON.stringify(absUrl) + ');\n'
                  : '';
                let loader = isMod
                  ? modLoaderPrefix + JSON.stringify(absUrl) + modLoaderMid + ackSnippet + '\n' + modLoaderEnd
                  : (ackCh
                    ? 'try { importScripts(' + JSON.stringify(absUrl) + '); } finally {' + ackSnippet + '\n}'
                    : 'importScripts(' + JSON.stringify(absUrl) + ');');
                if (dec.kind === 'fulfill') {
                  const rawBin = atob(dec.bodyB64 || '');
                  const rawBytes = new Uint8Array(rawBin.length);
                  for (let i = 0; i < rawBin.length; i++) rawBytes[i] = rawBin.charCodeAt(i);
                  loader = new TextDecoder().decode(rawBytes) + ackSnippet;
                }
                const blob = new Blob(['"use strict";\n' + envPrefix + netPrefix + loader], { type: 'application/javascript' });
                blobUrl = URL.createObjectURL(blob);
                if (cacheKey) ns.sharedBlobUrls.set(cacheKey, blobUrl);
                if (useDeferredRevoke) {
                  ns.pendingBlobUrls.set(blobKey, (ns.pendingBlobUrls.get(blobKey) || 0) + 1);
                  let settled = false;
                  let bc = null;
                  let timer = null;
                  const ownWrapperUrl = !cacheKey ? blobUrl : null;
                  releaseBlob = () => {
                    if (settled) return;
                    settled = true;
                    if (timer !== null) {
                      try { clearTimeout(timer); } catch {}
                      timer = null;
                    }
                    if (bc) {
                      try { bc.close(); } catch {}
                      bc = null;
                    }
                    const cnt = (ns.pendingBlobUrls.get(blobKey) || 0) - 1;
                    if (cnt <= 0) {
                      ns.pendingBlobUrls.delete(blobKey);
                      const fn = ns.deferredRevokes.get(blobKey);
                      if (fn) {
                        ns.deferredRevokes.delete(blobKey);
                        fn();
                      }
                    } else {
                      ns.pendingBlobUrls.set(blobKey, cnt);
                    }
                    if (ownWrapperUrl && typeof URL !== 'undefined') {
                      const origRev = ns.origUrlRevoke && ns.origUrlRevoke.get(URL);
                      if (typeof origRev === 'function') {
                        try { origRev.call(URL, ownWrapperUrl); } catch {}
                      }
                    }
                  };
                  try {
                    if (typeof BroadcastChannel === 'function') {
                      bc = new BroadcastChannel(ackCh);
                      bc.onmessage = releaseBlob;
                    }
                  } catch {}
                  try {
                    if (typeof setTimeout === 'function') timer = setTimeout(releaseBlob, 3000);
                  } catch {}
                }
              }
              const nextArgs = args.length > 1 ? [blobUrl, ...args.slice(1)] : [blobUrl];
              try {
                return Reflect.construct(target, nextArgs, newTarget);
              } catch (err) {
                if (releaseBlob) releaseBlob();
                throw err;
              }
            }
          }
        } catch {}
        return Reflect.construct(target, args, newTarget);
      }
    });
    wrapMarks.add(WrappedSW);
    try {
      Object.defineProperty(globalThis, 'SharedWorker', {
        value: WrappedSW,
        configurable: true,
        writable: true
      });
    } catch {}
  }
})();"#;

/// Generate a Firefox page/sandbox script that enforces `SharedWorker` network blocking and route rules.
/// Never defines `window.__mimikyu` or any `__mimikyu` identifier.
pub fn build_firefox_shared_worker_network_script(policy: &Value, channel_name: &str) -> String {
    let policy_json = serde_json::to_string(policy).unwrap_or_else(|_| "{}".to_string());
    let channel_json = serde_json::to_string(channel_name).unwrap_or_else(|_| "\"\"".to_string());
    FIREFOX_SHARED_WORKER_NET_TEMPLATE
        .replace("__POLICY_JSON__", &policy_json)
        .replace("__CHANNEL_JSON__", &channel_json)
}

/// Generate a small script to deactivate `SharedWorker` network rules on open contexts/workers.
pub fn build_firefox_shared_worker_network_clear_script(channel_name: &str) -> String {
    let channel_json = serde_json::to_string(channel_name).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#""use strict";
(() => {{
  const ns = globalThis[Symbol.for('net.runtime.state')];
  const empty = {{ active: false, blockImages: false, blockMedia: false, blockFonts: false, blockedUrls: [], rules: [] }};
  if (ns) ns.policy = empty;
  try {{
    const ch = {channel_json};
    if (ch && typeof BroadcastChannel === 'function') {{
      const bc = new BroadcastChannel(ch);
      bc.postMessage(empty);
      bc.close();
    }}
  }} catch {{}}
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
        "timezone": profile.timezone,
        "locale": profile.locale,
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
        "noHostWebRtc": profile.webrtc_policy == WebRtcPolicy::DisableNonProxiedUdp,
    })
    .to_string()
}

/// Generate a minimal preload script that suppresses WebRTC `typ host` ICE candidates
/// and SDP host lines (e.g. for `VirtualContext` or proxied sessions without an `EnvProfile`)
/// without overriding UA, locale, screen, WebGL, or audio properties.
pub fn build_webrtc_no_host_init_script(engine: EngineKind) -> String {
    let minimal = EnvProfile {
        os_family: String::new(),
        user_agent: String::new(),
        platform: String::new(),
        accept_language: String::new(),
        locale: String::new(),
        languages: Vec::new(),
        timezone: String::new(),
        user_agent_metadata: None,
        viewport_width: 0,
        viewport_height: 0,
        screen_width: 0,
        screen_height: 0,
        device_scale_factor: 0.0,
        is_mobile: false,
        hardware_concurrency: 0,
        device_memory_gb: None,
        webgl_vendor: None,
        webgl_renderer: None,
        audio_sample_rate: None,
        audio_base_latency: None,
        audio_channel_count: None,
        geolocation: None,
        webrtc_policy: WebRtcPolicy::DisableNonProxiedUdp,
        extra_headers: std::collections::BTreeMap::new(),
        disable_automation_controlled: true,
    };
    build_window_init_script(&minimal, engine)
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
            assert!(win_js.contains("VENDOR: 7936"));
            assert!(win_js.contains("RENDERER: 7937"));
            assert!(win_js.contains("VERSION: 7938"));
            assert!(win_js.contains("SHADING_LANGUAGE_VERSION: 35724"));
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
                assert!(win_js.contains("WorkerLocation"));
                assert!(win_js.contains("XMLHttpRequest"));
                assert!(win_js.contains("EventSource"));
                assert!(win_js.contains("importScripts"));
                assert!(win_js.contains("JSON.stringify(absUrl)"));
                assert!(win_js.contains("about:srcdoc"));
                assert!(win_js.contains("_wInit.toString()"));
                assert!(win_js.contains("insertNode"));
                assert!(win_js.contains("surroundContents"));
                assert!(win_js.contains("replaceChildren"));
                assert!(win_js.contains("setHTMLUnsafe"));
                assert!(win_js.contains("parseHTMLUnsafe"));
                assert!(win_js.contains("MutationObserver"));
            }
        }
        let policy = json!({
            "active": true,
            "blockImages": false,
            "blockMedia": false,
            "blockFonts": false,
            "blockedUrls": [{ "kind": "substring", "val": "blocked-sw" }],
            "rules": []
        });
        let net_js = build_firefox_shared_worker_network_script(&policy, "nws_1_1");
        let clear_js = build_firefox_shared_worker_network_clear_script("nws_1_1");
        assert!(!net_js.contains("__mimikyu"));
        assert!(!clear_js.contains("__mimikyu"));
        assert!(net_js.contains("SharedWorker"));
        assert!(net_js.contains("BroadcastChannel"));
        assert!(net_js.contains("XMLHttpRequest"));
    }
}

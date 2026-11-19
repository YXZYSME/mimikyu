pub mod coherence;
pub mod geoip;
pub mod presets;
pub mod proxy_relay;
pub mod scripts;

pub use coherence::{
    validate_or_err, validate_profile, CoherenceIssue, CoherenceReport, CoherenceSeverity,
};
pub use geoip::{
    apply_geoip_to_profile, derive_locale_fields, lookup_geoip, GeoLocationProfile,
};
pub use presets::{PresetKind, PresetProfile};
pub use proxy_relay::ProxyRelay;
pub use scripts::{
    build_firefox_shared_worker_network_clear_script, build_firefox_shared_worker_network_script,
    build_webrtc_no_host_init_script, build_window_init_script, build_worker_init_script,
};

use crate::driver::{EngineKind, EnvProfile, GeolocationConfig, WebRtcPolicy};
use crate::error::Result;

/// Fluent builder composing `PresetKind`, offline GeoIP/locale alignment, overrides, and coherence validation.
#[derive(Debug, Clone)]
pub struct EnvProfileBuilder {
    profile: EnvProfile,
}

impl EnvProfileBuilder {
    pub fn from_preset(preset: PresetKind, engine: EngineKind) -> Self {
        Self {
            profile: preset.to_env_profile(engine),
        }
    }

    pub fn from_profile(profile: EnvProfile) -> Self {
        Self { profile }
    }

    pub fn geoip(mut self, query: &str) -> Result<Self> {
        apply_geoip_to_profile(&mut self.profile, query)?;
        Ok(self)
    }

    pub fn locale(mut self, locale: &str) -> Result<Self> {
        let (norm, langs, al) = derive_locale_fields(locale)?;
        self.profile.locale = norm;
        self.profile.languages = langs;
        self.profile.accept_language = al;
        Ok(self)
    }

    pub fn timezone(mut self, tz: impl Into<String>) -> Self {
        self.profile.timezone = tz.into();
        self
    }

    pub fn viewport(mut self, width: u32, height: u32) -> Self {
        self.profile.viewport_width = width;
        self.profile.viewport_height = height;
        if self.profile.screen_width < width {
            self.profile.screen_width = width;
        }
        if self.profile.screen_height < height {
            self.profile.screen_height = height;
        }
        self
    }

    pub fn screen(mut self, width: u32, height: u32) -> Self {
        self.profile.screen_width = width;
        self.profile.screen_height = height;
        self
    }

    pub fn dpr(mut self, dpr: f64) -> Self {
        self.profile.device_scale_factor = dpr;
        self
    }

    pub fn webrtc(mut self, policy: WebRtcPolicy) -> Self {
        self.profile.webrtc_policy = policy;
        self
    }

    pub fn geolocation(mut self, geo: GeolocationConfig) -> Self {
        self.profile.geolocation = Some(geo);
        self
    }

    pub fn extra_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.profile.extra_headers.insert(key.into(), value.into());
        self
    }

    pub fn build(self) -> Result<EnvProfile> {
        validate_or_err(&self.profile)?;
        Ok(self.profile)
    }
}

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::driver::{
    EngineKind, EnvProfile, UserAgentBrand, UserAgentMetadata, WebRtcPolicy,
};
use crate::error::{MimikyuError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PresetKind {
    Win11,
    MacOs,
    Linux,
}

impl fmt::Display for PresetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Win11 => write!(f, "win11"),
            Self::MacOs => write!(f, "macos"),
            Self::Linux => write!(f, "linux"),
        }
    }
}

impl FromStr for PresetKind {
    type Err = MimikyuError;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "win11" | "windows" | "win" => Ok(Self::Win11),
            "macos" | "mac" | "darwin" => Ok(Self::MacOs),
            "linux" => Ok(Self::Linux),
            other => Err(MimikyuError::InvalidConfig(format!(
                "unknown environment preset '{other}' (expected 'win11', 'macos', or 'linux')"
            ))),
        }
    }
}

impl PresetKind {
    pub fn to_env_profile(self, engine: EngineKind) -> EnvProfile {
        PresetProfile::new(self, engine).into_env_profile()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetProfile {
    pub kind: PresetKind,
    pub engine: EngineKind,
    pub profile: EnvProfile,
}

impl PresetProfile {
    pub fn new(kind: PresetKind, engine: EngineKind) -> Self {
        let profile = build_preset_profile(kind, engine);
        Self {
            kind,
            engine,
            profile,
        }
    }

    pub fn into_env_profile(self) -> EnvProfile {
        self.profile
    }
}

fn chromium_brands() -> (Vec<UserAgentBrand>, Vec<UserAgentBrand>) {
    let brands = vec![
        UserAgentBrand {
            brand: "Google Chrome".to_string(),
            version: "143".to_string(),
        },
        UserAgentBrand {
            brand: "Chromium".to_string(),
            version: "143".to_string(),
        },
        UserAgentBrand {
            brand: "Not A(Brand".to_string(),
            version: "24".to_string(),
        },
    ];
    let full_version_list = vec![
        UserAgentBrand {
            brand: "Google Chrome".to_string(),
            version: "143.0.7499.4".to_string(),
        },
        UserAgentBrand {
            brand: "Chromium".to_string(),
            version: "143.0.7499.4".to_string(),
        },
        UserAgentBrand {
            brand: "Not A(Brand".to_string(),
            version: "24.0.0.0".to_string(),
        },
    ];
    (brands, full_version_list)
}

fn build_preset_profile(kind: PresetKind, engine: EngineKind) -> EnvProfile {
    let (brands, full_version_list) = chromium_brands();

    match (kind, engine) {
        (PresetKind::Win11, EngineKind::Chromium) => EnvProfile {
            os_family: "windows".to_string(),
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36".to_string(),
            platform: "Win32".to_string(),
            accept_language: "en-US,en;q=0.9".to_string(),
            locale: "en-US".to_string(),
            languages: vec!["en-US".to_string(), "en".to_string()],
            timezone: "America/New_York".to_string(),
            user_agent_metadata: Some(UserAgentMetadata {
                brands,
                full_version_list,
                platform: "Windows".to_string(),
                platform_version: "15.0.0".to_string(),
                architecture: "x86".to_string(),
                model: String::new(),
                mobile: false,
                bitness: "64".to_string(),
                wow64: false,
            }),
            viewport_width: 1920,
            viewport_height: 1080,
            screen_width: 1920,
            screen_height: 1080,
            device_scale_factor: 1.0,
            is_mobile: false,
            hardware_concurrency: 8,
            device_memory_gb: Some(8.0),
            webgl_vendor: Some("Google Inc. (NVIDIA)".to_string()),
            webgl_renderer: Some("ANGLE (NVIDIA, NVIDIA GeForce RTX 3060 Direct3D11 vs_5_0 ps_5_0, D3D11)".to_string()),
            audio_sample_rate: Some(48000),
            audio_base_latency: Some(0.01),
            audio_channel_count: Some(2),
            geolocation: None,
            webrtc_policy: WebRtcPolicy::DisableNonProxiedUdp,
            extra_headers: BTreeMap::new(),
            disable_automation_controlled: true,
        },
        (PresetKind::MacOs, EngineKind::Chromium) => EnvProfile {
            os_family: "macos".to_string(),
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36".to_string(),
            platform: "MacIntel".to_string(),
            accept_language: "en-US,en;q=0.9".to_string(),
            locale: "en-US".to_string(),
            languages: vec!["en-US".to_string(), "en".to_string()],
            timezone: "America/New_York".to_string(),
            user_agent_metadata: Some(UserAgentMetadata {
                brands,
                full_version_list,
                platform: "macOS".to_string(),
                platform_version: "15.5.0".to_string(),
                architecture: "arm".to_string(),
                model: String::new(),
                mobile: false,
                bitness: "64".to_string(),
                wow64: false,
            }),
            viewport_width: 1440,
            viewport_height: 900,
            screen_width: 1440,
            screen_height: 900,
            device_scale_factor: 2.0,
            is_mobile: false,
            hardware_concurrency: 10,
            device_memory_gb: Some(8.0),
            webgl_vendor: Some("Google Inc. (Apple)".to_string()),
            webgl_renderer: Some("ANGLE (Apple, ANGLE Metal Renderer: Apple M2 Pro, Unspecified Version)".to_string()),
            audio_sample_rate: Some(48000),
            audio_base_latency: Some(0.005333333333333333),
            audio_channel_count: Some(2),
            geolocation: None,
            webrtc_policy: WebRtcPolicy::DisableNonProxiedUdp,
            extra_headers: BTreeMap::new(),
            disable_automation_controlled: true,
        },
        (PresetKind::Linux, EngineKind::Chromium) => EnvProfile {
            os_family: "linux".to_string(),
            user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36".to_string(),
            platform: "Linux x86_64".to_string(),
            accept_language: "en-US,en;q=0.9".to_string(),
            locale: "en-US".to_string(),
            languages: vec!["en-US".to_string(), "en".to_string()],
            timezone: "America/New_York".to_string(),
            user_agent_metadata: Some(UserAgentMetadata {
                brands,
                full_version_list,
                platform: "Linux".to_string(),
                platform_version: "6.8.0".to_string(),
                architecture: "x86".to_string(),
                model: String::new(),
                mobile: false,
                bitness: "64".to_string(),
                wow64: false,
            }),
            viewport_width: 1920,
            viewport_height: 1080,
            screen_width: 1920,
            screen_height: 1080,
            device_scale_factor: 1.0,
            is_mobile: false,
            hardware_concurrency: 8,
            device_memory_gb: Some(8.0),
            webgl_vendor: Some("Google Inc. (Mesa)".to_string()),
            webgl_renderer: Some("ANGLE (Mesa, Mesa Intel(R) UHD Graphics 770 (ADL-S GT1), OpenGL 4.6)".to_string()),
            audio_sample_rate: Some(48000),
            audio_base_latency: Some(0.01),
            audio_channel_count: Some(2),
            geolocation: None,
            webrtc_policy: WebRtcPolicy::DisableNonProxiedUdp,
            extra_headers: BTreeMap::new(),
            disable_automation_controlled: true,
        },
        (PresetKind::Win11, EngineKind::Firefox) => EnvProfile {
            os_family: "windows".to_string(),
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:144.0) Gecko/20100101 Firefox/144.0".to_string(),
            platform: "Win32".to_string(),
            accept_language: "en-US,en;q=0.9".to_string(),
            locale: "en-US".to_string(),
            languages: vec!["en-US".to_string(), "en".to_string()],
            timezone: "America/New_York".to_string(),
            user_agent_metadata: None,
            viewport_width: 1920,
            viewport_height: 1080,
            screen_width: 1920,
            screen_height: 1080,
            device_scale_factor: 1.0,
            is_mobile: false,
            hardware_concurrency: 8,
            device_memory_gb: None,
            webgl_vendor: Some("NVIDIA Corporation".to_string()),
            webgl_renderer: Some("NVIDIA GeForce RTX 3060/PCIe/SSE2".to_string()),
            audio_sample_rate: Some(48000),
            audio_base_latency: Some(0.0),
            audio_channel_count: Some(2),
            geolocation: None,
            webrtc_policy: WebRtcPolicy::DisableNonProxiedUdp,
            extra_headers: BTreeMap::new(),
            disable_automation_controlled: true,
        },
        (PresetKind::MacOs, EngineKind::Firefox) => EnvProfile {
            os_family: "macos".to_string(),
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:144.0) Gecko/20100101 Firefox/144.0".to_string(),
            platform: "MacIntel".to_string(),
            accept_language: "en-US,en;q=0.9".to_string(),
            locale: "en-US".to_string(),
            languages: vec!["en-US".to_string(), "en".to_string()],
            timezone: "America/New_York".to_string(),
            user_agent_metadata: None,
            viewport_width: 1440,
            viewport_height: 900,
            screen_width: 1440,
            screen_height: 900,
            device_scale_factor: 2.0,
            is_mobile: false,
            hardware_concurrency: 10,
            device_memory_gb: None,
            webgl_vendor: Some("Apple".to_string()),
            webgl_renderer: Some("Apple M2, or similar".to_string()),
            audio_sample_rate: Some(48000),
            audio_base_latency: Some(0.0),
            audio_channel_count: Some(2),
            geolocation: None,
            webrtc_policy: WebRtcPolicy::DisableNonProxiedUdp,
            extra_headers: BTreeMap::new(),
            disable_automation_controlled: true,
        },
        (PresetKind::Linux, EngineKind::Firefox) => EnvProfile {
            os_family: "linux".to_string(),
            user_agent: "Mozilla/5.0 (X11; Linux x86_64; rv:144.0) Gecko/20100101 Firefox/144.0".to_string(),
            platform: "Linux x86_64".to_string(),
            accept_language: "en-US,en;q=0.9".to_string(),
            locale: "en-US".to_string(),
            languages: vec!["en-US".to_string(), "en".to_string()],
            timezone: "America/New_York".to_string(),
            user_agent_metadata: None,
            viewport_width: 1920,
            viewport_height: 1080,
            screen_width: 1920,
            screen_height: 1080,
            device_scale_factor: 1.0,
            is_mobile: false,
            hardware_concurrency: 8,
            device_memory_gb: None,
            webgl_vendor: Some("Intel".to_string()),
            webgl_renderer: Some("Mesa Intel(R) UHD Graphics 770 (ADL-S GT1)".to_string()),
            audio_sample_rate: Some(48000),
            audio_base_latency: Some(0.0),
            audio_channel_count: Some(2),
            geolocation: None,
            webrtc_policy: WebRtcPolicy::DisableNonProxiedUdp,
            extra_headers: BTreeMap::new(),
            disable_automation_controlled: true,
        },
    }
}

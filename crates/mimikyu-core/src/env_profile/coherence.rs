use serde::{Deserialize, Serialize};

use crate::driver::EnvProfile;
use crate::error::{MimikyuError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoherenceSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoherenceIssue {
    pub code: String,
    pub field: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoherenceReport {
    pub valid: bool,
    pub errors: Vec<CoherenceIssue>,
    pub warnings: Vec<CoherenceIssue>,
}

fn push_err(
    errors: &mut Vec<CoherenceIssue>,
    code: &str,
    field: &str,
    message: impl Into<String>,
) {
    errors.push(CoherenceIssue {
        code: code.to_string(),
        field: field.to_string(),
        message: message.into(),
    });
}

fn push_warn(
    warnings: &mut Vec<CoherenceIssue>,
    code: &str,
    field: &str,
    message: impl Into<String>,
) {
    warnings.push(CoherenceIssue {
        code: code.to_string(),
        field: field.to_string(),
        message: message.into(),
    });
}

fn normalize_os_family(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "windows" | "win" | "win32" | "win11" => Some("windows"),
        "macos" | "mac" | "darwin" | "osx" => Some("macos"),
        "linux" => Some("linux"),
        "" => None,
        _ => Some("unknown"),
    }
}

fn infer_os_from_ua(ua: &str) -> Option<&'static str> {
    if ua.contains("Windows NT") || ua.contains("Win64") || ua.contains("Win32") {
        Some("windows")
    } else if ua.contains("Macintosh") || ua.contains("Mac OS X") {
        Some("macos")
    } else if ua.contains("X11; Linux") || ua.contains("Linux x86_64") || ua.contains("Linux aarch64")
    {
        Some("linux")
    } else {
        None
    }
}

fn infer_os_from_platform(platform: &str) -> Option<&'static str> {
    match platform.trim() {
        "Win32" | "Win64" => Some("windows"),
        "MacIntel" | "MacARM64" | "MacPPC" => Some("macos"),
        p if p.starts_with("Linux") => Some("linux"),
        _ => None,
    }
}

fn is_valid_bcp47(tag: &str) -> bool {
    let t = tag.trim();
    if t.is_empty() || t.len() > 35 {
        return false;
    }
    let parts: Vec<&str> = t.split('-').collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return false;
    }
    let primary = parts[0];
    if !(2..=3).contains(&primary.len()) || !primary.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    parts[1..]
        .iter()
        .all(|sub| sub.len() <= 8 && sub.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn parse_lang_region(tag: &str) -> (String, Option<String>) {
    let mut parts = tag.trim().split('-');
    let lang = parts.next().unwrap_or("").to_ascii_lowercase();
    let mut region = None;
    for sub in parts {
        if sub.len() == 2 && sub.chars().all(|c| c.is_ascii_alphabetic()) {
            region = Some(sub.to_ascii_uppercase());
            break;
        }
    }
    (lang, region)
}

fn is_valid_timezone_syntax(tz: &str) -> bool {
    let t = tz.trim();
    if t.is_empty() || t.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    if matches!(t, "UTC" | "GMT" | "Etc/UTC" | "Etc/GMT") {
        return true;
    }
    let parts: Vec<&str> = t.split('/').collect();
    if !(2..=3).contains(&parts.len()) {
        return false;
    }
    let area = parts[0];
    if !matches!(
        area,
        "Africa"
            | "America"
            | "Antarctica"
            | "Arctic"
            | "Asia"
            | "Atlantic"
            | "Australia"
            | "Europe"
            | "Indian"
            | "Pacific"
            | "Etc"
    ) {
        return false;
    }
    parts.iter().all(|seg| {
        !seg.is_empty()
            && seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'))
    })
}

fn extract_chrome_major(ua: &str) -> Option<&str> {
    let idx = ua.find("Chrome/")?;
    let rest = &ua[idx + "Chrome/".len()..];
    let major = rest.split(['.', ' ']).next()?;
    if major.is_empty() || !major.chars().all(|c| c.is_ascii_digit()) {
        None
    } else {
        Some(major)
    }
}

pub fn validate_profile(profile: &EnvProfile) -> CoherenceReport {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    let norm_os = normalize_os_family(&profile.os_family);
    if norm_os == Some("unknown") {
        push_err(
            &mut errors,
            "OS_UA_MISMATCH",
            "os_family",
            format!("unsupported os_family '{}'", profile.os_family),
        );
    }

    if !profile.user_agent.is_empty() {
        if !profile.is_mobile
            && (profile.user_agent.contains("Android")
                || profile.user_agent.contains("iPhone")
                || profile.user_agent.contains("iPad")
                || profile.user_agent.contains("Mobile"))
        {
            push_err(
                &mut errors,
                "OS_UA_MISMATCH",
                "user_agent",
                "mobile token in user_agent while is_mobile is false",
            );
        }
        if profile.user_agent.contains("HeadlessChrome") {
            push_warn(
                &mut warnings,
                "HEADLESS_UA_TOKEN",
                "user_agent",
                "user_agent contains HeadlessChrome token",
            );
        }
        if let (Some(os), Some(ua_os)) = (norm_os, infer_os_from_ua(&profile.user_agent))
            && os != "unknown"
            && os != ua_os
        {
            push_err(
                &mut errors,
                "OS_UA_MISMATCH",
                "user_agent",
                format!("os_family '{os}' contradicts user_agent OS '{ua_os}'"),
            );
        }
    }

    if !profile.platform.is_empty() {
        let plat_os = infer_os_from_platform(&profile.platform);
        if plat_os.is_none() {
            push_err(
                &mut errors,
                "OS_PLATFORM_MISMATCH",
                "platform",
                format!("unrecognized desktop navigator.platform '{}'", profile.platform),
            );
        } else if let (Some(os), Some(po)) = (norm_os, plat_os)
            && os != "unknown"
            && os != po
        {
            push_err(
                &mut errors,
                "OS_PLATFORM_MISMATCH",
                "platform",
                format!("os_family '{os}' contradicts platform '{}'", profile.platform),
            );
        }

        if !profile.user_agent.is_empty()
            && let (Some(ua_os), Some(po)) = (infer_os_from_ua(&profile.user_agent), plat_os)
            && ua_os != po
        {
            push_err(
                &mut errors,
                "UA_PLATFORM_MISMATCH",
                "platform",
                format!(
                    "user_agent OS '{ua_os}' contradicts navigator.platform '{}'",
                    profile.platform
                ),
            );
        }
    }

    if let Some(ref meta) = profile.user_agent_metadata {
        if profile.user_agent.contains("Firefox/") {
            push_err(
                &mut errors,
                "UA_METADATA_MISMATCH",
                "user_agent_metadata",
                "Firefox user_agent must not set Chromium user_agent_metadata",
            );
        }
        if meta.mobile != profile.is_mobile {
            push_err(
                &mut errors,
                "UA_METADATA_MISMATCH",
                "user_agent_metadata.mobile",
                "user_agent_metadata.mobile does not match is_mobile",
            );
        }
        let meta_os = match meta.platform.trim().to_ascii_lowercase().as_str() {
            "windows" => Some("windows"),
            "macos" | "mac os x" => Some("macos"),
            "linux" => Some("linux"),
            _ => None,
        };
        if let Some(mo) = meta_os {
            if let Some(os) = norm_os
                && os != "unknown"
                && os != mo
            {
                push_err(
                    &mut errors,
                    "UA_METADATA_MISMATCH",
                    "user_agent_metadata.platform",
                    format!("user_agent_metadata.platform '{}' mismatches os_family '{os}'", meta.platform),
                );
            }
            if !profile.user_agent.is_empty()
                && let Some(ua_os) = infer_os_from_ua(&profile.user_agent)
                && ua_os != mo
            {
                push_err(
                    &mut errors,
                    "UA_METADATA_MISMATCH",
                    "user_agent_metadata.platform",
                    format!(
                        "user_agent_metadata.platform '{}' mismatches user_agent OS '{ua_os}'",
                        meta.platform
                    ),
                );
            }
        } else {
            push_err(
                &mut errors,
                "UA_METADATA_MISMATCH",
                "user_agent_metadata.platform",
                format!("unsupported user_agent_metadata.platform '{}'", meta.platform),
            );
        }

        if meta.architecture == "arm" && profile.platform == "Linux x86_64" {
            push_err(
                &mut errors,
                "UA_METADATA_MISMATCH",
                "user_agent_metadata.architecture",
                "arm architecture contradicts Linux x86_64 platform",
            );
        }
        if meta.bitness == "32"
            && (profile.platform == "Win64" || profile.platform == "Linux x86_64")
            && !meta.wow64
        {
            push_err(
                &mut errors,
                "UA_METADATA_MISMATCH",
                "user_agent_metadata.bitness",
                "32-bit bitness contradicts 64-bit platform without wow64",
            );
        }
        if let Some(major) = extract_chrome_major(&profile.user_agent) {
            let brand_ok = meta.brands.iter().any(|b| {
                (b.brand == "Chromium" || b.brand == "Google Chrome")
                    && (b.version == major || b.version.split('.').next() == Some(major))
            });
            let full_ok = meta.full_version_list.iter().any(|b| {
                (b.brand == "Chromium" || b.brand == "Google Chrome")
                    && (b.version == major || b.version.split('.').next() == Some(major))
            });
            if !brand_ok || !full_ok {
                push_err(
                    &mut errors,
                    "UA_METADATA_MISMATCH",
                    "user_agent_metadata.brands",
                    format!("Client Hints brands/full_version_list must match Chrome/{major}"),
                );
            }
        }
    }

    if !is_valid_bcp47(&profile.locale) {
        push_err(
            &mut errors,
            "LOCALE_SYNTAX_INVALID",
            "locale",
            format!("invalid BCP-47 locale '{}'", profile.locale),
        );
    }
    if profile.languages.is_empty() {
        push_err(
            &mut errors,
            "LOCALE_LANGUAGES_MISMATCH",
            "languages",
            "languages list cannot be empty",
        );
    } else {
        for lang in &profile.languages {
            if !is_valid_bcp47(lang) {
                push_err(
                    &mut errors,
                    "LOCALE_SYNTAX_INVALID",
                    "languages",
                    format!("invalid BCP-47 tag '{lang}' in languages"),
                );
            }
        }
        let (loc_lang, loc_reg) = parse_lang_region(&profile.locale);
        let (first_lang, first_reg) = parse_lang_region(&profile.languages[0]);
        if loc_lang != first_lang
            || (loc_reg.is_some() && first_reg.is_some() && loc_reg != first_reg)
        {
            push_err(
                &mut errors,
                "LOCALE_LANGUAGES_MISMATCH",
                "languages",
                format!(
                    "locale '{}' does not match languages[0] '{}'",
                    profile.locale, profile.languages[0]
                ),
            );
        }
        if !profile.accept_language.trim().is_empty() {
            let first_al = profile
                .accept_language
                .split(',')
                .next()
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim();
            if !first_al.eq_ignore_ascii_case(&profile.languages[0]) {
                let (al_lang, al_reg) = parse_lang_region(first_al);
                if al_lang != first_lang
                    || (al_reg.is_some() && first_reg.is_some() && al_reg != first_reg)
                {
                    push_err(
                        &mut errors,
                        "ACCEPT_LANGUAGE_MISMATCH",
                        "accept_language",
                        format!(
                            "accept_language primary tag '{first_al}' mismatches languages[0] '{}'",
                            profile.languages[0]
                        ),
                    );
                }
            }
        }
    }

    if profile.viewport_width == 0
        || profile.viewport_height == 0
        || profile.screen_width == 0
        || profile.screen_height == 0
        || profile.viewport_width > profile.screen_width
        || profile.viewport_height > profile.screen_height
    {
        push_err(
            &mut errors,
            "VIEWPORT_EXCEEDS_SCREEN",
            "viewport_width",
            format!(
                "viewport {}x{} must be non-zero and fit within screen {}x{}",
                profile.viewport_width,
                profile.viewport_height,
                profile.screen_width,
                profile.screen_height
            ),
        );
    }

    if !profile.device_scale_factor.is_finite()
        || profile.device_scale_factor <= 0.0
        || profile.device_scale_factor > 8.0
    {
        push_err(
            &mut errors,
            "INVALID_DPR",
            "device_scale_factor",
            format!("invalid device_scale_factor {}", profile.device_scale_factor),
        );
    } else if norm_os == Some("macos") && (profile.device_scale_factor - 1.0).abs() < f64::EPSILON {
        push_warn(
            &mut warnings,
            "MACOS_DPR_ONE",
            "device_scale_factor",
            "macOS desktop environments typically use device_scale_factor 2.0",
        );
    }

    if profile.hardware_concurrency == 0 || profile.hardware_concurrency > 256 {
        push_err(
            &mut errors,
            "INVALID_HARDWARE_CONCURRENCY",
            "hardware_concurrency",
            format!(
                "hardware_concurrency {} out of range 1..=256",
                profile.hardware_concurrency
            ),
        );
    }

    if let Some(dm) = profile.device_memory_gb {
        if !dm.is_finite() || dm <= 0.0 || dm > 1024.0 {
            push_err(
                &mut errors,
                "INVALID_DEVICE_MEMORY",
                "device_memory_gb",
                format!("invalid device_memory_gb {dm}"),
            );
        } else if ![0.25, 0.5, 1.0, 2.0, 4.0, 8.0]
            .iter()
            .any(|&v| (v - dm).abs() < f64::EPSILON)
        {
            push_warn(
                &mut warnings,
                "NON_STANDARD_DEVICE_MEMORY",
                "device_memory_gb",
                format!("navigator.deviceMemory {dm} is outside standard Chromium buckets"),
            );
        }
    }

    if let Some(sr) = profile.audio_sample_rate
        && !(8000..=384000).contains(&sr)
    {
        push_err(
            &mut errors,
            "INVALID_AUDIO_CONFIG",
            "audio_sample_rate",
            format!("audio_sample_rate {sr} out of range 8000..=384000"),
        );
    }
    if let Some(lat) = profile.audio_base_latency
        && (!lat.is_finite() || !(0.0..=5.0).contains(&lat))
    {
        push_err(
            &mut errors,
            "INVALID_AUDIO_CONFIG",
            "audio_base_latency",
            format!("audio_base_latency {lat} out of range 0.0..=5.0"),
        );
    }
    if let Some(ch) = profile.audio_channel_count
        && (ch == 0 || ch > 64)
    {
        push_err(
            &mut errors,
            "INVALID_AUDIO_CONFIG",
            "audio_channel_count",
            format!("audio_channel_count {ch} out of range 1..=64"),
        );
    }

    if !is_valid_timezone_syntax(&profile.timezone) {
        push_err(
            &mut errors,
            "INVALID_TIMEZONE",
            "timezone",
            format!("invalid IANA timezone '{}'", profile.timezone),
        );
    }

    if let Some(ref geo) = profile.geolocation {
        if !geo.latitude.is_finite()
            || !(-90.0..=90.0).contains(&geo.latitude)
            || !geo.longitude.is_finite()
            || !(-180.0..=180.0).contains(&geo.longitude)
            || !geo.accuracy.is_finite()
            || geo.accuracy <= 0.0
        {
            push_err(
                &mut errors,
                "INVALID_GEOLOCATION",
                "geolocation",
                "geolocation latitude/longitude/accuracy out of valid bounds",
            );
        } else if profile.timezone.starts_with("America/") && geo.longitude > 30.0 {
            push_warn(
                &mut warnings,
                "GEO_TIMEZONE_DRIFT",
                "geolocation",
                "eastern-hemisphere longitude with America/* timezone",
            );
        } else if profile.timezone.starts_with("Asia/") && geo.longitude < -30.0 {
            push_warn(
                &mut warnings,
                "GEO_TIMEZONE_DRIFT",
                "geolocation",
                "western-hemisphere longitude with Asia/* timezone",
            );
        }
    }

    match (&profile.webgl_vendor, &profile.webgl_renderer) {
        (Some(_), None) | (None, Some(_)) => {
            push_warn(
                &mut warnings,
                "WEBGL_PARTIAL_PAIR",
                "webgl_renderer",
                "only one of webgl_vendor / webgl_renderer is set",
            );
        }
        _ => {}
    }

    let expected_webgl_os = norm_os
        .filter(|&os| os != "unknown")
        .or_else(|| infer_os_from_ua(&profile.user_agent))
        .or_else(|| infer_os_from_platform(&profile.platform));
    if let Some(ref renderer) = profile.webgl_renderer
        && let Some(os) = expected_webgl_os
    {
        if (renderer.contains("Direct3D") || renderer.contains("D3D11")) && os != "windows" {
            push_err(
                &mut errors,
                "WEBGL_OS_MISMATCH",
                "webgl_renderer",
                format!("Direct3D WebGL renderer is invalid on '{os}'"),
            );
        }
        if (renderer.contains("ANGLE Metal") || renderer.contains("Apple M")) && os != "macos" {
            push_err(
                &mut errors,
                "WEBGL_OS_MISMATCH",
                "webgl_renderer",
                format!("Apple/Metal WebGL renderer is invalid on '{os}'"),
            );
        }
        if renderer.contains("Mesa") && os == "macos" {
            push_err(
                &mut errors,
                "WEBGL_OS_MISMATCH",
                "webgl_renderer",
                "Mesa WebGL renderer is invalid on macOS",
            );
        }
    }

    CoherenceReport {
        valid: errors.is_empty(),
        errors,
        warnings,
    }
}

pub fn validate_or_err(profile: &EnvProfile) -> Result<CoherenceReport> {
    let report = validate_profile(profile);
    if !report.valid {
        let details: Vec<String> = report
            .errors
            .iter()
            .map(|e| format!("[{}] {}: {}", e.code, e.field, e.message))
            .collect();
        return Err(MimikyuError::InvalidConfig(format!(
            "environment profile coherence validation failed: {}",
            details.join("; ")
        )));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::EngineKind;
    use crate::env_profile::presets::PresetKind;

    #[test]
    fn presets_pass_coherence_validation() {
        for kind in [PresetKind::Win11, PresetKind::MacOs, PresetKind::Linux] {
            for engine in [EngineKind::Chromium, EngineKind::Firefox] {
                let prof = kind.to_env_profile(engine);
                let report = validate_or_err(&prof)
                    .unwrap_or_else(|e| panic!("preset {kind}/{engine} failed: {e}"));
                assert!(report.valid);
                assert!(report.errors.is_empty());
            }
        }
    }

    #[test]
    fn coherence_validator_rejects_contradictions() {
        let mut p = PresetKind::Win11.to_env_profile(EngineKind::Chromium);
        p.platform = "MacIntel".to_string();
        let rep = validate_profile(&p);
        assert!(!rep.valid);
        assert!(rep.errors.iter().any(|e| e.code == "OS_PLATFORM_MISMATCH"));

        let mut p2 = PresetKind::MacOs.to_env_profile(EngineKind::Chromium);
        p2.viewport_width = 3000;
        assert!(validate_or_err(&p2).is_err());

        let mut p3 = PresetKind::Linux.to_env_profile(EngineKind::Chromium);
        p3.locale = "de-DE".to_string();
        assert!(validate_or_err(&p3).is_err());

        let mut p4 = PresetKind::MacOs.to_env_profile(EngineKind::Firefox);
        p4.user_agent_metadata = PresetKind::MacOs
            .to_env_profile(EngineKind::Chromium)
            .user_agent_metadata;
        assert!(validate_or_err(&p4).is_err());

        let mut p5 = PresetKind::Win11.to_env_profile(EngineKind::Chromium);
        p5.timezone = "Invalid/DoesNotExist".to_string();
        // Wait: "Invalid/DoesNotExist" has area "Invalid", which is not a valid IANA area!
        assert!(validate_or_err(&p5).is_err());

        // Chrome major prefix ambiguity: Chrome/14 must not match brand version "143"
        let mut p6 = PresetKind::Win11.to_env_profile(EngineKind::Chromium);
        p6.user_agent = p6.user_agent.replace("Chrome/143.", "Chrome/14.");
        let rep6 = validate_profile(&p6);
        assert!(!rep6.valid);
        assert!(rep6.errors.iter().any(|e| e.code == "UA_METADATA_MISMATCH"));

        // Empty os_family & user_agent still infers OS from platform for WEBGL_OS_MISMATCH
        let mut p7 = PresetKind::Win11.to_env_profile(EngineKind::Chromium);
        p7.os_family.clear();
        p7.user_agent.clear();
        p7.user_agent_metadata = None;
        p7.platform = "Win32".to_string();
        p7.webgl_renderer = Some("ANGLE (Apple, ANGLE Metal Renderer: Apple M2 Pro)".to_string());
        let rep7 = validate_profile(&p7);
        assert!(!rep7.valid);
        assert!(rep7.errors.iter().any(|e| e.code == "WEBGL_OS_MISMATCH"));
    }
}

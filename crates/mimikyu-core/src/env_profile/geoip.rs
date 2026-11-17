use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use crate::driver::{EnvProfile, GeolocationConfig};
use crate::error::{MimikyuError, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeoLocationProfile {
    pub region_code: String,
    pub country_name: String,
    pub city: String,
    pub timezone: String,
    pub locale: String,
    pub accept_language: String,
    pub languages: Vec<String>,
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy: f64,
}

struct StaticRegion {
    code: &'static str,
    country: &'static str,
    city: &'static str,
    timezone: &'static str,
    locale: &'static str,
    latitude: f64,
    longitude: f64,
}

const REGIONS: &[StaticRegion] = &[
    StaticRegion {
        code: "US",
        country: "United States",
        city: "New York",
        timezone: "America/New_York",
        locale: "en-US",
        latitude: 40.7128,
        longitude: -74.0060,
    },
    StaticRegion {
        code: "US-NY",
        country: "United States",
        city: "New York",
        timezone: "America/New_York",
        locale: "en-US",
        latitude: 40.7128,
        longitude: -74.0060,
    },
    StaticRegion {
        code: "US-CA",
        country: "United States",
        city: "Los Angeles",
        timezone: "America/Los_Angeles",
        locale: "en-US",
        latitude: 34.0522,
        longitude: -118.2437,
    },
    StaticRegion {
        code: "US-TX",
        country: "United States",
        city: "Dallas",
        timezone: "America/Chicago",
        locale: "en-US",
        latitude: 32.7767,
        longitude: -96.7970,
    },
    StaticRegion {
        code: "GB",
        country: "United Kingdom",
        city: "London",
        timezone: "Europe/London",
        locale: "en-GB",
        latitude: 51.5074,
        longitude: -0.1278,
    },
    StaticRegion {
        code: "DE",
        country: "Germany",
        city: "Berlin",
        timezone: "Europe/Berlin",
        locale: "de-DE",
        latitude: 52.5200,
        longitude: 13.4050,
    },
    StaticRegion {
        code: "FR",
        country: "France",
        city: "Paris",
        timezone: "Europe/Paris",
        locale: "fr-FR",
        latitude: 48.8566,
        longitude: 2.3522,
    },
    StaticRegion {
        code: "JP",
        country: "Japan",
        city: "Tokyo",
        timezone: "Asia/Tokyo",
        locale: "ja-JP",
        latitude: 35.6762,
        longitude: 139.6503,
    },
    StaticRegion {
        code: "KR",
        country: "South Korea",
        city: "Seoul",
        timezone: "Asia/Seoul",
        locale: "ko-KR",
        latitude: 37.5665,
        longitude: 126.9780,
    },
    StaticRegion {
        code: "SG",
        country: "Singapore",
        city: "Singapore",
        timezone: "Asia/Singapore",
        locale: "en-SG",
        latitude: 1.3521,
        longitude: 103.8198,
    },
    StaticRegion {
        code: "AU",
        country: "Australia",
        city: "Sydney",
        timezone: "Australia/Sydney",
        locale: "en-AU",
        latitude: -33.8688,
        longitude: 151.2093,
    },
    StaticRegion {
        code: "CA",
        country: "Canada",
        city: "Toronto",
        timezone: "America/Toronto",
        locale: "en-CA",
        latitude: 43.6532,
        longitude: -79.3832,
    },
    StaticRegion {
        code: "NL",
        country: "Netherlands",
        city: "Amsterdam",
        timezone: "Europe/Amsterdam",
        locale: "nl-NL",
        latitude: 52.3676,
        longitude: 4.9041,
    },
    StaticRegion {
        code: "IN",
        country: "India",
        city: "Mumbai",
        timezone: "Asia/Kolkata",
        locale: "en-IN",
        latitude: 19.0760,
        longitude: 72.8777,
    },
    StaticRegion {
        code: "BR",
        country: "Brazil",
        city: "Sao Paulo",
        timezone: "America/Sao_Paulo",
        locale: "pt-BR",
        latitude: -23.5505,
        longitude: -46.6333,
    },
    StaticRegion {
        code: "SE",
        country: "Sweden",
        city: "Stockholm",
        timezone: "Europe/Stockholm",
        locale: "sv-SE",
        latitude: 59.3293,
        longitude: 18.0686,
    },
    StaticRegion {
        code: "CH",
        country: "Switzerland",
        city: "Zurich",
        timezone: "Europe/Zurich",
        locale: "de-CH",
        latitude: 47.3769,
        longitude: 8.5417,
    },
    StaticRegion {
        code: "IE",
        country: "Ireland",
        city: "Dublin",
        timezone: "Europe/Dublin",
        locale: "en-IE",
        latitude: 53.3498,
        longitude: -6.2603,
    },
    StaticRegion {
        code: "ES",
        country: "Spain",
        city: "Madrid",
        timezone: "Europe/Madrid",
        locale: "es-ES",
        latitude: 40.4168,
        longitude: -3.7038,
    },
    StaticRegion {
        code: "IT",
        country: "Italy",
        city: "Rome",
        timezone: "Europe/Rome",
        locale: "it-IT",
        latitude: 41.9028,
        longitude: 12.4964,
    },
    StaticRegion {
        code: "PL",
        country: "Poland",
        city: "Warsaw",
        timezone: "Europe/Warsaw",
        locale: "pl-PL",
        latitude: 52.2297,
        longitude: 21.0122,
    },
    StaticRegion {
        code: "HK",
        country: "Hong Kong",
        city: "Hong Kong",
        timezone: "Asia/Hong_Kong",
        locale: "zh-HK",
        latitude: 22.3193,
        longitude: 114.1694,
    },
    StaticRegion {
        code: "TW",
        country: "Taiwan",
        city: "Taipei",
        timezone: "Asia/Taipei",
        locale: "zh-TW",
        latitude: 25.0330,
        longitude: 121.5654,
    },
];

const CIDR_TABLE: &[([u8; 4], u8, &str)] = &[
    // Well-known DNS & RFC 5737 test blocks
    ([8, 8, 8, 0], 24, "US"),
    ([8, 8, 4, 0], 24, "US"),
    ([1, 1, 1, 0], 24, "AU"),
    ([9, 9, 9, 0], 24, "US"),
    ([192, 0, 2, 0], 24, "DE"),    // TEST-NET-1
    ([198, 51, 100, 0], 24, "GB"), // TEST-NET-2
    ([203, 0, 113, 0], 24, "JP"),  // TEST-NET-3
    // Representative ARIN / RIPE / APNIC / LACNIC allocations
    ([104, 16, 0, 0], 12, "US"),
    ([172, 64, 0, 0], 13, "US"),
    ([142, 0, 0, 0], 8, "CA"),
    ([185, 0, 0, 0], 10, "DE"),
    ([185, 64, 0, 0], 10, "GB"),
    ([193, 0, 0, 0], 9, "NL"),
    ([194, 0, 0, 0], 9, "FR"),
    ([133, 0, 0, 0], 8, "JP"),
    ([210, 0, 0, 0], 8, "JP"),
    ([211, 0, 0, 0], 8, "KR"),
    ([202, 0, 0, 0], 8, "SG"),
    ([203, 0, 0, 0], 8, "AU"),
    ([177, 0, 0, 0], 8, "BR"),
    ([103, 0, 0, 0], 8, "IN"),
];

fn lookup_region(code: &str) -> Option<&'static StaticRegion> {
    let norm = match code.trim().to_ascii_uppercase().as_str() {
        "UK" => "GB".to_string(),
        other => other.to_string(),
    };
    REGIONS.iter().find(|r| r.code == norm)
}

fn is_non_public_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_unspecified()
}

fn cidr_lookup_ipv4(ip: Ipv4Addr) -> Option<&'static str> {
    let ip_u32 = u32::from(ip);
    let mut best: Option<(u8, &'static str)> = None;
    for &(net_octets, prefix_len, region) in CIDR_TABLE {
        let mask = if prefix_len == 0 {
            0
        } else {
            u32::MAX << (32 - prefix_len)
        };
        let net_u32 = u32::from(Ipv4Addr::from(net_octets));
        if (ip_u32 & mask) == (net_u32 & mask)
            && best.is_none_or(|(best_len, _)| prefix_len > best_len)
        {
            best = Some((prefix_len, region));
        }
    }
    best.map(|(_, r)| r)
}

/// Normalize a BCP-47 locale and derive coherent `languages` and `accept_language` fields.
pub fn derive_locale_fields(locale: &str) -> Result<(String, Vec<String>, String)> {
    let trimmed = locale.trim().replace('_', "-");
    if trimmed.is_empty() {
        return Err(MimikyuError::InvalidConfig(
            "locale cannot be empty".to_string(),
        ));
    }
    let parts: Vec<&str> = trimmed.split('-').collect();
    if parts.iter().any(|p| p.is_empty())
        || !(2..=3).contains(&parts[0].len())
        || !parts[0].chars().all(|c| c.is_ascii_alphabetic())
    {
        return Err(MimikyuError::InvalidConfig(format!(
            "invalid BCP-47 locale '{locale}'"
        )));
    }

    let primary = parts[0].to_ascii_lowercase();
    let mut normalized_parts = vec![primary.clone()];
    for sub in &parts[1..] {
        if !sub.chars().all(|c| c.is_ascii_alphanumeric()) || sub.len() > 8 {
            return Err(MimikyuError::InvalidConfig(format!(
                "invalid BCP-47 subtag '{sub}' in '{locale}'"
            )));
        }
        if sub.len() == 2 && sub.chars().all(|c| c.is_ascii_alphabetic()) {
            normalized_parts.push(sub.to_ascii_uppercase());
        } else if sub.len() == 4 && sub.chars().all(|c| c.is_ascii_alphabetic()) {
            let mut chars = sub.chars();
            let mut title = String::new();
            if let Some(first) = chars.next() {
                title.push(first.to_ascii_uppercase());
                for ch in chars {
                    title.push(ch.to_ascii_lowercase());
                }
            }
            normalized_parts.push(title);
        } else {
            normalized_parts.push(sub.to_ascii_lowercase());
        }
    }
    let normalized_locale = normalized_parts.join("-");

    let mut languages = vec![normalized_locale.clone()];
    if normalized_locale != primary {
        languages.push(primary.clone());
    }
    if primary != "en" {
        languages.push("en-US".to_string());
        languages.push("en".to_string());
    } else if normalized_locale != "en-US" {
        languages.push("en-US".to_string());
    }
    languages.dedup();

    let mut al_parts = Vec::with_capacity(languages.len());
    for (idx, lang) in languages.iter().enumerate() {
        if idx == 0 {
            al_parts.push(lang.clone());
        } else {
            let q = (1.0 - (idx as f64) * 0.1).max(0.1);
            al_parts.push(format!("{lang};q={q:.1}"));
        }
    }
    let accept_language = al_parts.join(",");

    Ok((normalized_locale, languages, accept_language))
}

/// Offline zero-network GeoIP/region lookup accepting a region code (`"DE"`, `"US-CA"`, `"JP"`) or IPv4 address.
pub fn lookup_geoip(query: &str) -> Result<GeoLocationProfile> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(MimikyuError::InvalidConfig(
            "geoip query cannot be empty".to_string(),
        ));
    }

    let region = if let Ok(ip) = trimmed.parse::<Ipv4Addr>() {
        if is_non_public_ipv4(ip) {
            return Err(MimikyuError::InvalidConfig(format!(
                "cannot resolve GeoIP for non-public IPv4 address '{ip}'"
            )));
        }
        let region_code = cidr_lookup_ipv4(ip).unwrap_or_else(|| {
            const FALLBACK_CODES: &[&str] = &["US", "GB", "DE", "FR", "JP", "CA", "AU", "SG"];
            FALLBACK_CODES[(ip.octets()[0] as usize) % FALLBACK_CODES.len()]
        });
        lookup_region(region_code).expect("static region code exists")
    } else {
        lookup_region(trimmed).ok_or_else(|| {
            MimikyuError::InvalidConfig(format!(
                "unknown GeoIP region or invalid IPv4 address '{trimmed}'"
            ))
        })?
    };

    let (locale, languages, accept_language) = derive_locale_fields(region.locale)?;
    Ok(GeoLocationProfile {
        region_code: region.code.to_string(),
        country_name: region.country.to_string(),
        city: region.city.to_string(),
        timezone: region.timezone.to_string(),
        locale,
        accept_language,
        languages,
        latitude: region.latitude,
        longitude: region.longitude,
        accuracy: 100.0,
    })
}

pub fn apply_geoip_to_profile(profile: &mut EnvProfile, query: &str) -> Result<GeoLocationProfile> {
    let geo = lookup_geoip(query)?;
    profile.timezone = geo.timezone.clone();
    profile.locale = geo.locale.clone();
    profile.languages = geo.languages.clone();
    profile.accept_language = geo.accept_language.clone();
    profile.geolocation = Some(GeolocationConfig {
        latitude: geo.latitude,
        longitude: geo.longitude,
        accuracy: geo.accuracy,
    });
    Ok(geo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_geoip_handles_cidr_and_region_codes() {
        let de = lookup_geoip("DE").unwrap();
        assert_eq!(de.timezone, "Europe/Berlin");
        assert_eq!(de.locale, "de-DE");
        assert_eq!(de.languages[0], "de-DE");

        let jp = lookup_geoip("203.0.113.5").unwrap();
        assert_eq!(jp.region_code, "JP");
        assert_eq!(jp.timezone, "Asia/Tokyo");
        assert_eq!(jp.locale, "ja-JP");

        let gb = lookup_geoip("198.51.100.42").unwrap();
        assert_eq!(gb.region_code, "GB");
        assert_eq!(gb.timezone, "Europe/London");

        let us = lookup_geoip("8.8.8.8").unwrap();
        assert_eq!(us.region_code, "US");
        assert_eq!(us.timezone, "America/New_York");

        let au = lookup_geoip("1.1.1.1").unwrap();
        assert_eq!(au.region_code, "AU");
        assert_eq!(au.timezone, "Australia/Sydney");

        assert!(lookup_geoip("127.0.0.1").is_err());
        assert!(lookup_geoip("192.168.1.10").is_err());
        assert!(lookup_geoip("ZZ").is_err());
    }

    #[test]
    fn derive_locale_fields_formats_languages_and_accept_language() {
        let (loc, langs, al) = derive_locale_fields("ja_jp").unwrap();
        assert_eq!(loc, "ja-JP");
        assert_eq!(langs, vec!["ja-JP", "ja", "en-US", "en"]);
        assert_eq!(al, "ja-JP,ja;q=0.9,en-US;q=0.8,en;q=0.7");

        let (loc2, langs2, al2) = derive_locale_fields("en-gb").unwrap();
        assert_eq!(loc2, "en-GB");
        assert_eq!(langs2, vec!["en-GB", "en", "en-US"]);
        assert_eq!(al2, "en-GB,en;q=0.9,en-US;q=0.8");
    }
}

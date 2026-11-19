use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanizeConfig {
    pub enabled: bool,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default = "default_overshoot_prob")]
    pub overshoot_prob: f64,
    #[serde(default = "default_key_delay_mean")]
    pub key_delay_mean_ms: f64,
    #[serde(default = "default_key_delay_std")]
    pub key_delay_std_ms: f64,
    #[serde(default)]
    pub seed: Option<u64>,
}

fn default_overshoot_prob() -> f64 {
    0.25
}

fn default_key_delay_mean() -> f64 {
    55.0
}

fn default_key_delay_std() -> f64 {
    22.0
}

impl Default for HumanizeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            duration_ms: None,
            overshoot_prob: default_overshoot_prob(),
            key_delay_mean_ms: default_key_delay_mean(),
            key_delay_std_ms: default_key_delay_std(),
            seed: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TrajectoryPoint {
    pub x: f64,
    pub y: f64,
    pub delay_ms: u64,
}

pub fn clamp_point_to_viewport(x: f64, y: f64, vp_w: f64, vp_h: f64) -> (f64, f64) {
    let max_x = if vp_w.is_finite() {
        (vp_w - 1.0).max(0.0)
    } else {
        0.0
    };
    let max_y = if vp_h.is_finite() {
        (vp_h - 1.0).max(0.0)
    } else {
        0.0
    };
    let cx = if x.is_finite() {
        x.clamp(0.0, max_x)
    } else {
        0.0
    };
    let cy = if y.is_finite() {
        y.clamp(0.0, max_y)
    } else {
        0.0
    };
    (cx, cy)
}

pub(crate) fn make_rng(seed: Option<u64>) -> StdRng {
    match seed {
        Some(s) => StdRng::seed_from_u64(s),
        None => StdRng::from_rng(&mut rand::rng()),
    }
}

pub fn generate_trajectory(
    start: Option<(f64, f64)>,
    target: (f64, f64),
    target_size: Option<(f64, f64)>,
    vp_w: f64,
    vp_h: f64,
    cfg: &HumanizeConfig,
) -> Vec<TrajectoryPoint> {
    let mut rng = make_rng(cfg.seed);
    let safe_vp_w = if vp_w.is_finite() && vp_w > 0.0 {
        vp_w
    } else {
        1.0
    };
    let safe_vp_h = if vp_h.is_finite() && vp_h > 0.0 {
        vp_h
    } else {
        1.0
    };
    let (tx, ty) = clamp_point_to_viewport(target.0, target.1, safe_vp_w, safe_vp_h);

    let (sx, sy) = match start {
        Some((x, y)) => {
            let clamped = clamp_point_to_viewport(x, y, safe_vp_w, safe_vp_h);
            if ((clamped.0 - tx).powi(2) + (clamped.1 - ty).powi(2)).sqrt() < 8.0 {
                let alt_x = if tx > safe_vp_w * 0.5 {
                    tx - 140.0
                } else {
                    tx + 140.0
                };
                let alt_y = if ty > safe_vp_h * 0.5 {
                    ty - 90.0
                } else {
                    ty + 90.0
                };
                clamp_point_to_viewport(alt_x, alt_y, safe_vp_w, safe_vp_h)
            } else {
                clamped
            }
        }
        None => {
            let def_x = safe_vp_w * 0.15;
            let def_y = safe_vp_h * 0.15;
            let d = ((def_x - tx).powi(2) + (def_y - ty).powi(2)).sqrt();
            if d < 40.0 {
                clamp_point_to_viewport(tx - 120.0, ty - 80.0, safe_vp_w, safe_vp_h)
            } else {
                clamp_point_to_viewport(def_x, def_y, safe_vp_w, safe_vp_h)
            }
        }
    };

    let dx = tx - sx;
    let dy = ty - sy;
    let dist = (dx * dx + dy * dy).sqrt().max(1.0);

    let w_eff = target_size
        .and_then(|(w, h)| {
            if w.is_finite() && h.is_finite() {
                Some(w.min(h).max(12.0))
            } else {
                None
            }
        })
        .unwrap_or(40.0);
    let fitts_id = (dist / w_eff + 1.0).log2();
    let total_ms = cfg
        .duration_ms
        .map(|m| (m as f64).max(30.0))
        .unwrap_or_else(|| (80.0 + 75.0 * fitts_id).clamp(90.0, 650.0));

    let steps = (total_ms / 14.0).round().clamp(6.0, 45.0) as usize;

    // Unit tangent and normal vectors
    let ux = dx / dist;
    let uy = dy / dist;
    let nx = -uy;
    let ny = ux;

    // Guaranteed non-collinear lateral arc when dist >= 5px
    let sign = if rng.random_bool(0.5) { 1.0 } else { -1.0 };
    let mag = (dist * rng.random_range(0.08..0.22)).clamp(8.0, 95.0);
    let arc1 = sign * mag;
    let arc2 = sign * mag * rng.random_range(0.35..0.85);

    let c1x = sx + dx * rng.random_range(0.22..0.38) + nx * arc1;
    let c1y = sy + dy * rng.random_range(0.22..0.38) + ny * arc1;
    let c2x = sx + dx * rng.random_range(0.65..0.82) + nx * arc2;
    let c2y = sy + dy * rng.random_range(0.65..0.82) + ny * arc2;

    let overshoot_prob = if cfg.overshoot_prob.is_finite() {
        cfg.overshoot_prob.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let do_overshoot = dist > 40.0 && rng.random_bool(overshoot_prob);
    let over_px = if do_overshoot {
        rng.random_range(4.0..14.0)
    } else {
        0.0
    };

    // Weight delays so start & end are slower than mid-flight (bell-modulated minimum-jerk)
    let mut raw_weights = Vec::with_capacity(steps);
    for i in 1..=steps {
        let u = i as f64 / steps as f64;
        // Higher weight near ends -> slower speed at start and target approach
        let end_emphasis = 0.65 + 1.15 * (2.0 * (u - 0.5)).powi(2);
        let jitter = rng.random_range(0.85..1.15);
        raw_weights.push(end_emphasis * jitter);
    }
    let w_sum: f64 = raw_weights.iter().sum::<f64>().max(1e-6);

    let mut pts = Vec::with_capacity(steps);
    for (idx, w) in raw_weights.into_iter().enumerate() {
        let i = idx + 1;
        let u = i as f64 / steps as f64;
        // Minimum-jerk S-curve reparameterization
        let s = 10.0 * u.powi(3) - 15.0 * u.powi(4) + 6.0 * u.powi(5);
        let delay_ms = ((total_ms * w / w_sum).round() as u64).max(3);

        if i == steps {
            pts.push(TrajectoryPoint {
                x: tx,
                y: ty,
                delay_ms,
            });
            break;
        }

        let omt = 1.0 - s;
        let mut bx = omt.powi(3) * sx
            + 3.0 * omt.powi(2) * s * c1x
            + 3.0 * omt * s.powi(2) * c2x
            + s.powi(3) * tx;
        let mut by = omt.powi(3) * sy
            + 3.0 * omt.powi(2) * s * c1y
            + 3.0 * omt * s.powi(2) * c2y
            + s.powi(3) * ty;

        if do_overshoot {
            let bump = (std::f64::consts::PI * u.powf(1.6)).sin().max(0.0);
            bx += ux * over_px * bump;
            by += uy * over_px * bump;
        }

        let micro_jitter = (1.0 - u) * 0.9;
        bx += rng.random_range(-micro_jitter..=micro_jitter);
        by += rng.random_range(-micro_jitter..=micro_jitter);

        let (cx, cy) = clamp_point_to_viewport(bx, by, safe_vp_w, safe_vp_h);
        pts.push(TrajectoryPoint {
            x: cx,
            y: cy,
            delay_ms,
        });
    }

    pts
}

pub fn sample_key_delay_ms(
    prev_char: Option<char>,
    cfg: &HumanizeConfig,
    rng: &mut impl Rng,
) -> u64 {
    let u1: f64 = rng.random_range(1e-6..1.0);
    let u2: f64 = rng.random_range(0.0..1.0);
    let z0 = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
    let mean = if cfg.key_delay_mean_ms.is_finite() {
        cfg.key_delay_mean_ms.max(0.0)
    } else {
        default_key_delay_mean()
    };
    let std_dev = if cfg.key_delay_std_ms.is_finite() {
        cfg.key_delay_std_ms.max(0.0)
    } else {
        default_key_delay_std()
    };
    let mut ms = mean + z0 * std_dev;
    if let Some(ch) = prev_char
        && (ch == ' ' || ch == '.' || ch == ',' || ch == '\n')
    {
        ms *= 1.25;
    }
    if let Some(dur) = cfg.duration_ms {
        let scale = (dur as f64 / 200.0).clamp(0.3, 2.5);
        ms *= scale;
    }
    let safe_ms = if ms.is_finite() {
        ms
    } else {
        default_key_delay_mean()
    };
    safe_ms.round().clamp(12.0, 220.0) as u64
}

pub fn split_wheel_notches(dx: f64, dy: f64, rng: &mut impl Rng) -> Vec<(f64, f64, u64)> {
    let dx = if dx.is_finite() { dx } else { 0.0 };
    let dy = if dy.is_finite() { dy } else { 0.0 };
    let total_mag = dx.abs().max(dy.abs());
    if total_mag < 1e-3 {
        return vec![(dx, dy, 0)];
    }
    let steps = (total_mag / 95.0).ceil().clamp(2.0, 12.0) as usize;
    let mut weights = Vec::with_capacity(steps);
    for _ in 0..steps {
        weights.push(rng.random_range(0.7..1.3));
    }
    let sum: f64 = weights.iter().sum::<f64>().max(1e-6);

    let mut out = Vec::with_capacity(steps);
    let mut acc_x = 0.0;
    let mut acc_y = 0.0;
    for (idx, w) in weights.into_iter().enumerate() {
        let delay = rng.random_range(15..=45);
        if idx + 1 == steps {
            let rx = dx - acc_x;
            let ry = dy - acc_y;
            out.push((
                if rx.is_finite() { rx } else { 0.0 },
                if ry.is_finite() { ry } else { 0.0 },
                delay,
            ));
        } else {
            let frac = w / sum;
            let mut sx = (dx * frac).round();
            let mut sy = (dy * frac).round();
            if !sx.is_finite() || !(acc_x + sx).is_finite() {
                sx = 0.0;
            }
            if !sy.is_finite() || !(acc_y + sy).is_finite() {
                sy = 0.0;
            }
            acc_x += sx;
            acc_y += sy;
            out.push((sx, sy, delay));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_point_to_viewport_prevents_negative_or_overflow() {
        assert_eq!(clamp_point_to_viewport(-5.0, -1.0, 1280.0, 800.0), (0.0, 0.0));
        assert_eq!(
            clamp_point_to_viewport(1280.0, 800.0, 1280.0, 800.0),
            (1279.0, 799.0)
        );
    }

    #[test]
    fn trajectory_is_non_collinear_bounded_and_non_constant_velocity() {
        let cfg = HumanizeConfig {
            enabled: true,
            seed: Some(42),
            ..Default::default()
        };
        let pts = generate_trajectory(
            Some((100.0, 100.0)),
            (600.0, 420.0),
            Some((120.0, 40.0)),
            1280.0,
            800.0,
            &cfg,
        );
        assert!(pts.len() > 2);
        assert_eq!((pts.last().unwrap().x, pts.last().unwrap().y), (600.0, 420.0));

        // Check viewport clamping & non-collinearity against start->end line
        let (x0, y0) = (100.0_f64, 100.0_f64);
        let (x1, y1) = (600.0_f64, 420.0_f64);
        let line_len = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
        let mut max_perp = 0.0f64;
        let mut speeds = Vec::new();
        let mut prev = (x0, y0);
        for p in &pts {
            assert!(p.x >= 0.0 && p.x <= 1279.0);
            assert!(p.y >= 0.0 && p.y <= 799.0);
            let area2 = ((x1 - x0) * (p.y - y0) - (y1 - y0) * (p.x - x0)).abs();
            max_perp = max_perp.max(area2 / line_len);
            let seg = ((p.x - prev.0).powi(2) + (p.y - prev.1).powi(2)).sqrt();
            speeds.push(seg / (p.delay_ms as f64));
            prev = (p.x, p.y);
        }
        assert!(max_perp > 2.0, "trajectory must be curved, max_perp={max_perp}");
        let min_v = speeds.iter().copied().fold(f64::INFINITY, f64::min);
        let max_v = speeds.iter().copied().fold(0.0_f64, f64::max);
        assert!(max_v > min_v * 1.5, "expected non-constant velocity: min={min_v}, max={max_v}");
    }

    #[test]
    fn wheel_notches_sum_exactly_to_requested_deltas() {
        let mut rng = make_rng(Some(7));
        let notches = split_wheel_notches(37.5, -420.0, &mut rng);
        assert!(notches.len() >= 2);
        let sum_x: f64 = notches.iter().map(|n| n.0).sum();
        let sum_y: f64 = notches.iter().map(|n| n.1).sum();
        assert!((sum_x - 37.5).abs() < 1e-6);
        assert!((sum_y - (-420.0)).abs() < 1e-6);
    }

    #[test]
    fn generate_trajectory_sanitizes_non_finite_overshoot_prob_and_inputs() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let cfg = HumanizeConfig {
                enabled: true,
                overshoot_prob: bad,
                seed: Some(11),
                ..Default::default()
            };
            let pts = generate_trajectory(
                Some((10.0, 10.0)),
                (300.0, 300.0),
                Some((f64::NAN, 24.0)),
                1280.0,
                800.0,
                &cfg,
            );
            assert!(!pts.is_empty());
            for p in &pts {
                assert!(p.x.is_finite() && p.x >= 0.0 && p.x <= 1279.0);
                assert!(p.y.is_finite() && p.y >= 0.0 && p.y <= 799.0);
                assert!(p.delay_ms >= 3);
            }
            assert_eq!((pts.last().unwrap().x, pts.last().unwrap().y), (300.0, 300.0));
        }

        let nan_vp_pts = generate_trajectory(
            Some((f64::NAN, f64::INFINITY)),
            (f64::NAN, f64::NEG_INFINITY),
            None,
            f64::NAN,
            f64::INFINITY,
            &HumanizeConfig {
                enabled: true,
                overshoot_prob: f64::NAN,
                seed: Some(13),
                ..Default::default()
            },
        );
        assert!(!nan_vp_pts.is_empty());
        for p in &nan_vp_pts {
            assert!(p.x.is_finite() && p.y.is_finite());
        }
    }

    #[test]
    fn key_delay_and_wheel_notches_sanitize_non_finite_inputs() {
        let mut rng = make_rng(Some(19));
        let cfg = HumanizeConfig {
            enabled: true,
            key_delay_mean_ms: f64::NAN,
            key_delay_std_ms: f64::INFINITY,
            ..Default::default()
        };
        let d = sample_key_delay_ms(Some(' '), &cfg, &mut rng);
        assert!((12..=220).contains(&d), "unexpected delay {d}");

        let notches = split_wheel_notches(f64::NAN, f64::INFINITY, &mut rng);
        assert_eq!(notches.len(), 1);
        assert_eq!(notches[0], (0.0, 0.0, 0));
    }

    #[test]
    fn wheel_notches_remain_finite_for_extreme_finite_deltas() {
        for seed in 0..16u64 {
            let mut rng = make_rng(Some(seed));
            for (dx, dy) in [
                (f64::MAX, -f64::MAX),
                (-f64::MAX, f64::MAX),
                (1.5e308, 0.0),
                (0.0, -1.5e308),
            ] {
                let notches = split_wheel_notches(dx, dy, &mut rng);
                assert!(notches.len() >= 2);
                for (nx, ny, _) in &notches {
                    assert!(
                        nx.is_finite() && ny.is_finite(),
                        "non-finite notch ({nx}, {ny}) for ({dx}, {dy})"
                    );
                }
            }
        }
    }
}

//! Seeded model of hand-driven pointer movement and typing for `--human`.
//!
//! Everything here is pure planning: given a seed and a profile it returns
//! the timed points or keystrokes to send, so the same seed always produces
//! the same input and the model can be tested without a browser. The daemon
//! owns dispatch; see `move_mouse_human` in `actions.rs` and
//! `interaction::run_typing_plan`.
//!
//! The model is about how input looks and paces on the page. It deliberately
//! does nothing about browser fingerprints, automation flags, or any specific
//! detector.

/// Raises the OS timer resolution while human input is being paced. Windows
/// otherwise rounds sleeps up to about 15 ms, which would flatten the planned
/// 7 to 13 ms step spacing. Dropping the guard restores the default.
pub struct PreciseTimerGuard;

impl PreciseTimerGuard {
    pub fn new() -> Self {
        #[cfg(windows)]
        // SAFETY: timeBeginPeriod has no pointer arguments; it is balanced by
        // timeEndPeriod in Drop.
        unsafe {
            windows_sys::Win32::Media::timeBeginPeriod(1);
        }
        Self
    }
}

impl Default for PreciseTimerGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PreciseTimerGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: balances the timeBeginPeriod call in new().
        unsafe {
            windows_sys::Win32::Media::timeEndPeriod(1);
        }
    }
}

/// Motion and typing parameters for one `--human` style.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HumanProfile {
    pub name: &'static str,
    /// Multiplier on stroke duration.
    pub pace: f64,
    /// Probability of overshooting on a long move.
    pub overshoot: f64,
    /// Probability of an extra small correction before the final one.
    pub micro_correction: f64,
    /// Centre of the log-normal inter-key interval.
    pub key_ms: f64,
    /// Log-normal spread of the inter-key interval.
    pub key_spread: f64,
    /// Probability of a typing mistake per character.
    pub typo: f64,
    /// Probability of a thinking pause before a character.
    pub think: f64,
    /// Upper bound of the extra pause after a word or clause boundary.
    pub word_gap_ms: f64,
    /// Centre of the log-normal button hold.
    pub hold_ms: f64,
}

pub const NATURAL: HumanProfile = HumanProfile {
    name: "natural",
    pace: 1.0,
    overshoot: 0.45,
    micro_correction: 0.35,
    key_ms: 125.0,
    key_spread: 0.42,
    typo: 0.05,
    think: 0.03,
    word_gap_ms: 130.0,
    hold_ms: 85.0,
};

pub const CAREFUL: HumanProfile = HumanProfile {
    name: "careful",
    pace: 1.55,
    overshoot: 0.7,
    micro_correction: 0.8,
    key_ms: 210.0,
    key_spread: 0.5,
    typo: 0.09,
    think: 0.07,
    word_gap_ms: 260.0,
    hold_ms: 115.0,
};

/// Look up a profile by its CLI name. An absent name means `natural`.
pub fn profile_named(name: Option<&str>) -> Option<&'static HumanProfile> {
    match name {
        None | Some("natural") => Some(&NATURAL),
        Some("careful") => Some(&CAREFUL),
        Some(_) => None,
    }
}

/// Small deterministic generator (xorshift64*), seeded through splitmix64 so
/// nearby seeds diverge immediately.
#[derive(Debug, Clone)]
pub struct HumanRng {
    state: u64,
}

impl HumanRng {
    pub fn new(seed: u64) -> Self {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Self {
            state: if z == 0 { 0x9E37_79B9_7F4A_7C15 } else { z },
        }
    }

    /// A seed for runs that did not ask for a reproducible one.
    pub fn fresh_seed() -> u64 {
        let mut bytes = [0u8; 8];
        if getrandom::getrandom(&mut bytes).is_ok() {
            return u64::from_le_bytes(bytes);
        }
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5EED)
    }

    /// Uniform value in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        let value = self.state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (value >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Log-normal-looking factor centred on 1, using a sum of three uniforms
    /// as the normal approximation.
    fn spread(&mut self, spread: f64) -> f64 {
        let normal = self.next_f64() + self.next_f64() + self.next_f64() - 1.5;
        (normal * spread).exp()
    }
}

/// One planned pointer position, `t_ms` after the movement starts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PathPoint {
    pub x: f64,
    pub y: f64,
    pub t_ms: f64,
}

/// Target width used by Fitts' law when the caller has no element.
pub const DEFAULT_TARGET_SIZE: f64 = 36.0;

/// Clamp a measured element size to the range Fitts' law is meaningful in, so
/// a giant container does not turn the movement into a flick.
fn fitts_width(target_size: f64) -> f64 {
    if target_size.is_finite() && target_size > 0.0 {
        target_size.clamp(8.0, 120.0)
    } else {
        DEFAULT_TARGET_SIZE
    }
}

/// Minimum-jerk position profile.
fn minimum_jerk(t: f64) -> f64 {
    t * t * t * (10.0 + t * (-15.0 + 6.0 * t))
}

struct Stroke {
    from: (f64, f64),
    to: (f64, f64),
    pace: f64,
    start_ms: f64,
    correction: bool,
    width: f64,
}

/// Append one ballistic stroke and return its end time. Fast start and slow
/// finish come from warping time before the minimum-jerk profile; the bow and
/// tremor both shrink as the stroke approaches its end.
fn plan_stroke(stroke: &Stroke, rng: &mut HumanRng, out: &mut Vec<PathPoint>) -> f64 {
    let (fx, fy) = stroke.from;
    let (tx, ty) = stroke.to;
    let dx = tx - fx;
    let dy = ty - fy;
    let distance = dx.hypot(dy).max(1.0);
    let duration = stroke.pace
        * (90.0 + 135.0 * (distance / stroke.width + 1.0).log2())
        * (0.9 + rng.next_f64() * 0.25);
    let (nx, ny) = (-dy / distance, dx / distance);
    let bow_scale = if stroke.correction {
        0.03
    } else {
        0.05 + rng.next_f64() * 0.11
    };
    let bow = bow_scale * distance * if rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
    let phase_a = rng.next_f64() * std::f64::consts::TAU;
    let phase_b = rng.next_f64() * std::f64::consts::TAU;
    let amplitude = (distance * 0.006).min(2.4);

    let mut t = 7.0 + rng.next_f64() * 6.0;
    while t < duration {
        let u = t / duration;
        let s = minimum_jerk(u.powf(0.88));
        let tremor = (1.0 - s) * ((u * 9.0 + phase_a).sin() + 0.6 * (u * 23.0 + phase_b).sin());
        let offset = 4.0 * s * (1.0 - s) * bow + tremor * amplitude;
        out.push(PathPoint {
            x: (fx + dx * s + nx * offset).round(),
            y: (fy + dy * s + ny * offset).round(),
            t_ms: stroke.start_ms + t,
        });
        t += 7.0 + rng.next_f64() * 6.0;
    }
    let end = stroke.start_ms + duration;
    out.push(PathPoint {
        x: tx,
        y: ty,
        t_ms: end,
    });
    end
}

/// Plan a full move from `from` to `to`: a main stroke that lands slightly
/// short or long, optionally a small correction, then a short final
/// correction that ends exactly on `to`. `target_size` is the width term of
/// Fitts' law, in CSS pixels.
pub fn plan_mouse_path(
    from: (f64, f64),
    to: (f64, f64),
    target_size: f64,
    profile: &HumanProfile,
    rng: &mut HumanRng,
) -> Vec<PathPoint> {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let distance = dx.hypot(dy);
    if distance < 1.0 {
        return vec![PathPoint {
            x: to.0,
            y: to.1,
            t_ms: 0.0,
        }];
    }
    let (ux, uy) = (dx / distance, dy / distance);
    let width = fitts_width(target_size);

    let overshoot = distance > 260.0 && rng.next_f64() < profile.overshoot;
    let miss = if overshoot {
        0.02 + rng.next_f64() * 0.035
    } else {
        -(0.008 + rng.next_f64() * 0.02)
    };
    let sideways = (rng.next_f64() - 0.5) * 14.0;
    let aim = (
        (to.0 + ux * miss * distance - uy * sideways).round(),
        (to.1 + uy * miss * distance + ux * sideways).round(),
    );

    let mut points = Vec::new();
    let mut last = aim;
    let mut clock = plan_stroke(
        &Stroke {
            from,
            to: aim,
            pace: profile.pace,
            start_ms: 0.0,
            correction: false,
            width,
        },
        rng,
        &mut points,
    );
    if rng.next_f64() < profile.micro_correction {
        clock += 40.0 + rng.next_f64() * 90.0;
        let middle = (
            (to.0 + (rng.next_f64() - 0.5) * 7.0).round(),
            (to.1 + (rng.next_f64() - 0.5) * 7.0).round(),
        );
        clock = plan_stroke(
            &Stroke {
                from: last,
                to: middle,
                pace: profile.pace * 0.8,
                start_ms: clock,
                correction: true,
                width,
            },
            rng,
            &mut points,
        );
        last = middle;
    }
    clock += 40.0 + rng.next_f64() * 90.0;
    plan_stroke(
        &Stroke {
            from: last,
            to,
            pace: profile.pace * 0.7,
            start_ms: clock,
            correction: true,
            width,
        },
        rng,
        &mut points,
    );
    points
}

/// Short settle between arriving and pressing the button.
pub fn plan_pre_press_ms(profile: &HumanProfile, rng: &mut HumanRng) -> u64 {
    ((40.0 + rng.next_f64() * 90.0) * profile.pace) as u64
}

/// How long the button stays down.
pub fn plan_hold_ms(profile: &HumanProfile, rng: &mut HumanRng) -> u64 {
    (profile.hold_ms * rng.spread(0.35)).clamp(40.0, 260.0) as u64
}

/// Fractions in `[-1, 1]` used to pick a landing point away from the exact
/// centre of the visible target. The page-side resolver scales them to the
/// target and verifies the result still hits it.
pub fn plan_landing_jitter(rng: &mut HumanRng) -> (f64, f64) {
    (rng.next_f64() * 2.0 - 1.0, rng.next_f64() * 2.0 - 1.0)
}

/// One typing action, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypingStep {
    Wait(u64),
    Type(char),
    Backspace,
}

/// Neighbouring keys on a US layout, for plausible slips.
fn adjacent_keys(lower: char) -> &'static str {
    match lower {
        'a' => "sq",
        'b' => "vn",
        'c' => "xv",
        'd' => "sf",
        'e' => "wr",
        'f' => "dg",
        'g' => "fh",
        'h' => "gj",
        'i' => "uo",
        'j' => "hk",
        'k' => "jl",
        'l' => "k",
        'm' => "n",
        'n' => "bm",
        'o' => "ip",
        'p' => "o",
        'q' => "w",
        'r' => "et",
        's' => "ad",
        't' => "ry",
        'u' => "yi",
        'v' => "cb",
        'w' => "qe",
        'x' => "zc",
        'y' => "tu",
        'z' => "x",
        _ => "",
    }
}

/// A character that a pinyin input method could offer in place of `ch`.
fn same_sound_character(ch: char) -> Option<char> {
    Some(match ch {
        '中' => '钟',
        '文' => '闻',
        '也' => '野',
        '一' => '已',
        '个' => '各',
        '字' => '自',
        '出' => '初',
        '现' => '线',
        '的' => '得',
        '是' => '事',
        '在' => '再',
        '有' => '又',
        '和' => '河',
        '不' => '布',
        '人' => '仁',
        '大' => '打',
        '上' => '尚',
        '来' => '莱',
        '他' => '她',
        '时' => '十',
        '要' => '药',
        '就' => '旧',
        '到' => '道',
        '好' => '号',
        '号' => '好',
        '我' => '握',
        '们' => '门',
        '地' => '第',
        '下' => '夏',
        '天' => '田',
        '年' => '念',
        '学' => '雪',
        '生' => '声',
        '会' => '回',
        '分' => '份',
        '工' => '公',
        '作' => '做',
        '电' => '点',
        '话' => '化',
        '手' => '首',
        '机' => '记',
        '邮' => '油',
        '箱' => '香',
        '名' => '明',
        '址' => '只',
        '码' => '马',
        '密' => '米',
        _ => return None,
    })
}

fn is_cjk(ch: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&ch)
}

fn is_word_boundary(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, ',' | '.' | '，' | '。')
}

fn wrong_character(ch: char, rng: &mut HumanRng) -> Option<char> {
    if is_cjk(ch) {
        return same_sound_character(ch);
    }
    let lower = ch.to_ascii_lowercase();
    let neighbours = adjacent_keys(lower).as_bytes();
    if neighbours.is_empty() {
        return None;
    }
    let pick = neighbours[(rng.next_f64() * neighbours.len() as f64) as usize] as char;
    Some(if ch == lower {
        pick
    } else {
        pick.to_ascii_uppercase()
    })
}

/// Plan the keystrokes for `text`. With `allow_typos` false (passwords, card
/// and one-time-code fields, or a field that cannot be inspected) no wrong
/// character is ever produced. Applying the plan to an empty buffer always
/// yields exactly `text`.
pub fn plan_typing(
    text: &str,
    profile: &HumanProfile,
    allow_typos: bool,
    rng: &mut HumanRng,
) -> Vec<TypingStep> {
    let chars: Vec<char> = text.chars().collect();
    let mut steps = Vec::with_capacity(chars.len() * 2);
    let key_interval = |rng: &mut HumanRng| profile.key_ms * rng.spread(profile.key_spread);

    for (index, &ch) in chars.iter().enumerate() {
        let cjk = is_cjk(ch);
        let mut wait = key_interval(rng) * if cjk { 1.9 } else { 1.0 };
        if index > 0 && is_word_boundary(chars[index - 1]) {
            wait += profile.word_gap_ms * rng.next_f64();
        }
        if rng.next_f64() < profile.think {
            wait += 300.0 + rng.next_f64() * 600.0;
        }
        steps.push(TypingStep::Wait(wait as u64));

        let wrong = if allow_typos && rng.next_f64() < profile.typo {
            wrong_character(ch, rng)
        } else {
            None
        };
        let Some(wrong) = wrong else {
            steps.push(TypingStep::Type(ch));
            continue;
        };

        steps.push(TypingStep::Type(wrong));
        // A wrong Latin key is noticed one or two characters later; a wrong
        // Chinese candidate is corrected right away.
        let carried = if cjk {
            0
        } else {
            (1 + (rng.next_f64() * 2.0) as usize).min(chars.len() - index - 1)
        };
        for &next in &chars[index + 1..index + 1 + carried] {
            steps.push(TypingStep::Wait(key_interval(rng) as u64));
            steps.push(TypingStep::Type(next));
        }
        steps.push(TypingStep::Wait((220.0 + rng.next_f64() * 380.0) as u64));
        for _ in 0..=carried {
            steps.push(TypingStep::Wait(
                (profile.key_ms * 0.55 * (0.8 + rng.next_f64() * 0.5)) as u64,
            ));
            steps.push(TypingStep::Backspace);
        }
        steps.push(TypingStep::Wait((120.0 + rng.next_f64() * 160.0) as u64));
        steps.push(TypingStep::Type(ch));
    }
    steps
}

/// Total wall-clock time a plan will take, used to extend the CLI read timeout.
pub fn typing_budget_ms(char_count: usize, profile: &HumanProfile) -> u64 {
    // A generous upper bound: three plain intervals per character, plus a
    // thinking pause for every ten characters.
    (char_count as f64 * (profile.key_ms * 3.0 + profile.word_gap_ms + 90.0)) as u64 + 2_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(steps: &[TypingStep]) -> String {
        let mut buffer = String::new();
        for step in steps {
            match step {
                TypingStep::Type(c) => buffer.push(*c),
                TypingStep::Backspace => {
                    buffer.pop();
                }
                TypingStep::Wait(_) => {}
            }
        }
        buffer
    }

    fn typed_chars(steps: &[TypingStep]) -> Vec<char> {
        steps
            .iter()
            .filter_map(|s| match s {
                TypingStep::Type(c) => Some(*c),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn same_seed_gives_same_plan_and_different_seed_differs() {
        let plan = |seed| {
            let mut rng = HumanRng::new(seed);
            plan_mouse_path((5.0, 5.0), (700.0, 400.0), 36.0, &NATURAL, &mut rng)
        };
        assert_eq!(plan(7), plan(7));
        assert_ne!(plan(7), plan(8));
    }

    #[test]
    fn mouse_path_is_integer_steps_with_7_to_13_ms_spacing_inside_strokes() {
        for seed in 0..200 {
            let mut rng = HumanRng::new(seed);
            let path = plan_mouse_path((10.0, 20.0), (900.0, 500.0), 40.0, &NATURAL, &mut rng);
            let mut previous = 0.0;
            for point in &path[..path.len() - 1] {
                assert_eq!(point.x, point.x.round(), "x must be a whole pixel");
                assert_eq!(point.y, point.y.round(), "y must be a whole pixel");
                assert!(point.t_ms > previous, "time must move forward");
                previous = point.t_ms;
            }
            let gaps: Vec<f64> = path.windows(2).map(|w| w[1].t_ms - w[0].t_ms).collect();
            let inner = gaps.iter().filter(|g| **g >= 7.0 && **g <= 13.0).count();
            assert!(
                inner * 10 >= gaps.len() * 8,
                "most steps must sit in the 7 to 13 ms band: {inner}/{}",
                gaps.len()
            );
            assert!(
                gaps.iter().any(|g| (g - gaps[0]).abs() > 0.5),
                "spacing must be uneven"
            );
        }
    }

    #[test]
    fn mouse_path_ends_exactly_on_the_target() {
        for seed in 0..200 {
            let mut rng = HumanRng::new(seed);
            let path = plan_mouse_path((3.0, 9.0), (640.5, 311.25), 30.0, &CAREFUL, &mut rng);
            let last = path.last().unwrap();
            assert_eq!((last.x, last.y), (640.5, 311.25));
        }
    }

    #[test]
    fn mouse_speed_starts_fast_and_finishes_slow() {
        // Over the first stroke, half the distance is covered well before
        // half the time has passed.
        let mut rng = HumanRng::new(3);
        let from = (0.0, 0.0);
        let to = (1000.0, 0.0);
        let path = plan_mouse_path(from, to, 36.0, &NATURAL, &mut rng);
        let first_correction = path.windows(2).position(|w| w[1].t_ms - w[0].t_ms > 30.0);
        let stroke = &path[..first_correction.unwrap_or(path.len())];
        let end_time = stroke.last().unwrap().t_ms;
        let half_way = stroke.iter().find(|p| p.x >= 500.0).unwrap();
        assert!(
            half_way.t_ms < end_time * 0.5,
            "reached the midpoint at {} of {}",
            half_way.t_ms,
            end_time
        );
    }

    #[test]
    fn mouse_path_curves_away_from_the_straight_line() {
        let mut curved = 0;
        for seed in 0..50 {
            let mut rng = HumanRng::new(seed);
            let path = plan_mouse_path((0.0, 0.0), (800.0, 0.0), 36.0, &NATURAL, &mut rng);
            if path.iter().any(|p| p.y.abs() >= 10.0) {
                curved += 1;
            }
        }
        assert!(curved >= 45, "only {curved}/50 paths bowed");
    }

    #[test]
    fn mouse_path_tremor_fades_near_the_target() {
        // Final correction stays within a few pixels of the line to the target.
        for seed in 0..200 {
            let mut rng = HumanRng::new(seed);
            let path = plan_mouse_path((0.0, 0.0), (800.0, 400.0), 36.0, &NATURAL, &mut rng);
            let tail: Vec<_> = path.iter().rev().take(4).collect();
            for point in tail {
                let d = (point.x - 800.0).hypot(point.y - 400.0);
                assert!(d < 24.0, "seed {seed}: point {d} px from target near end");
            }
        }
    }

    #[test]
    fn long_moves_sometimes_overshoot_and_short_moves_do_not() {
        let overshoots = |to: (f64, f64)| {
            (0..300)
                .filter(|seed| {
                    let mut rng = HumanRng::new(*seed);
                    plan_mouse_path((0.0, 0.0), to, 36.0, &NATURAL, &mut rng)
                        .iter()
                        .any(|p| p.x > to.0 + 5.0)
                })
                .count()
        };
        let long = overshoots((900.0, 0.0));
        assert!(long > 60 && long < 240, "long move overshot {long}/300");
        assert_eq!(overshoots((200.0, 0.0)), 0, "short moves never overshoot");
    }

    #[test]
    fn longer_moves_take_longer_following_fitts_law() {
        let mean_ms = |to: f64| {
            (0..200)
                .map(|seed| {
                    let mut rng = HumanRng::new(seed);
                    plan_mouse_path((0.0, 0.0), (to, 0.0), 36.0, &NATURAL, &mut rng)
                        .last()
                        .unwrap()
                        .t_ms
                })
                .sum::<f64>()
                / 200.0
        };
        assert!(mean_ms(100.0) < mean_ms(400.0));
        assert!(mean_ms(400.0) < mean_ms(1600.0));
        let narrow = |size: f64| {
            let mut rng = HumanRng::new(11);
            plan_mouse_path((0.0, 0.0), (500.0, 0.0), size, &NATURAL, &mut rng)
                .last()
                .unwrap()
                .t_ms
        };
        assert!(narrow(10.0) > narrow(100.0), "smaller targets take longer");
    }

    #[test]
    fn careful_profile_moves_slower_than_natural() {
        let total = |profile: &HumanProfile| {
            (0..100)
                .map(|seed| {
                    let mut rng = HumanRng::new(seed);
                    plan_mouse_path((0.0, 0.0), (600.0, 300.0), 36.0, profile, &mut rng)
                        .last()
                        .unwrap()
                        .t_ms
                })
                .sum::<f64>()
        };
        assert!(total(&CAREFUL) > total(&NATURAL) * 1.2);
    }

    #[test]
    fn press_timing_varies_within_bounds() {
        let mut rng = HumanRng::new(1);
        let holds: Vec<u64> = (0..200).map(|_| plan_hold_ms(&NATURAL, &mut rng)).collect();
        assert!(holds.iter().all(|h| (40..=260).contains(h)));
        assert!(holds.iter().min() != holds.iter().max(), "hold must vary");
        let pause = plan_pre_press_ms(&NATURAL, &mut rng);
        assert!((40..=130).contains(&pause));
    }

    #[test]
    fn typing_plan_always_reproduces_the_text() {
        let text = "Hello, this is typed by hand. 中文也一个字一个字出现。";
        for seed in 0..500 {
            for profile in [&NATURAL, &CAREFUL] {
                let mut rng = HumanRng::new(seed);
                let plan = plan_typing(text, profile, true, &mut rng);
                assert_eq!(apply(&plan), text, "seed {seed} {}", profile.name);
            }
        }
    }

    #[test]
    fn typing_makes_occasional_mistakes_when_allowed() {
        let text = "the quick brown fox jumps over the lazy dog";
        let typed: usize = (0..300)
            .filter(|seed| {
                let mut rng = HumanRng::new(*seed);
                let plan = plan_typing(text, &NATURAL, true, &mut rng);
                typed_chars(&plan).iter().collect::<String>() != text
            })
            .count();
        assert!(typed > 200, "mistakes in {typed}/300 runs");
    }

    #[test]
    fn typing_never_makes_mistakes_in_sensitive_fields() {
        let text = "SuperSecret-1234 中文密码";
        for seed in 0..1000 {
            let mut rng = HumanRng::new(seed);
            let plan = plan_typing(text, &CAREFUL, false, &mut rng);
            assert_eq!(typed_chars(&plan).iter().collect::<String>(), text);
            assert!(!plan.contains(&TypingStep::Backspace));
        }
    }

    #[test]
    fn wrong_latin_key_is_adjacent_and_noticed_within_two_characters() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        for seed in 0..500 {
            let mut rng = HumanRng::new(seed);
            let plan = plan_typing(text, &CAREFUL, true, &mut rng);
            let actions: Vec<_> = plan
                .iter()
                .filter(|s| !matches!(s, TypingStep::Wait(_)))
                .collect();
            let mut position = 0usize;
            let mut i = 0;
            while i < actions.len() {
                if let TypingStep::Type(c) = actions[i] {
                    let expected = text.chars().nth(position);
                    if Some(*c) == expected {
                        position += 1;
                        i += 1;
                        continue;
                    }
                    let want = expected.expect("typed past the end of the text");
                    assert!(adjacent_keys(want).contains(*c));
                    let mut carried = 0;
                    i += 1;
                    while matches!(actions[i], TypingStep::Type(_)) {
                        carried += 1;
                        i += 1;
                    }
                    let mut erased = 0;
                    while matches!(actions.get(i), Some(TypingStep::Backspace)) {
                        erased += 1;
                        i += 1;
                    }
                    assert!(carried >= 1 || position == text.len() - 1);
                    assert!(carried <= 2, "noticed {carried} characters later");
                    assert_eq!(erased, carried + 1, "must erase everything after the slip");
                    continue;
                }
                i += 1;
            }
            assert_eq!(position, text.chars().count());
        }
    }

    #[test]
    fn wrong_chinese_character_is_same_sound_and_fixed_immediately() {
        let text = "中文也一个字出现";
        let mut seen = false;
        for seed in 0..500 {
            let mut rng = HumanRng::new(seed);
            let plan = plan_typing(text, &CAREFUL, true, &mut rng);
            let actions: Vec<_> = plan
                .iter()
                .filter(|s| !matches!(s, TypingStep::Wait(_)))
                .collect();
            for window in actions.windows(3) {
                if let [TypingStep::Type(wrong), TypingStep::Backspace, TypingStep::Type(right)] =
                    window
                {
                    assert_eq!(same_sound_character(*right), Some(*wrong));
                    seen = true;
                }
            }
            let distinct_wrong = actions
                .iter()
                .filter(|a| matches!(a, TypingStep::Backspace))
                .count();
            let typed = actions
                .iter()
                .filter(|a| matches!(a, TypingStep::Type(_)))
                .count();
            assert_eq!(typed, text.chars().count() + distinct_wrong);
        }
        assert!(seen, "no same-sound correction in 500 runs");
    }

    #[test]
    fn typing_intervals_centre_on_the_profile() {
        let text = "a".repeat(4000);
        let mean = |profile: &HumanProfile| {
            let mut rng = HumanRng::new(9);
            let plan = plan_typing(&text, profile, false, &mut rng);
            let waits: Vec<u64> = plan
                .iter()
                .filter_map(|s| match s {
                    TypingStep::Wait(ms) if *ms < 400 => Some(*ms),
                    _ => None,
                })
                .collect();
            waits.iter().sum::<u64>() as f64 / waits.len() as f64
        };
        let natural = mean(&NATURAL);
        let careful = mean(&CAREFUL);
        assert!((natural - 125.0).abs() < 25.0, "natural mean {natural}");
        assert!((careful - 210.0).abs() < 45.0, "careful mean {careful}");
    }

    #[test]
    fn typing_pauses_after_word_boundaries_and_thinks_occasionally() {
        let text = "aaaa bbbb cccc dddd eeee ffff gggg hhhh ".repeat(25);
        let mut rng = HumanRng::new(21);
        let plan = plan_typing(&text, &NATURAL, false, &mut rng);
        let waits: Vec<u64> = plan
            .iter()
            .filter_map(|s| match s {
                TypingStep::Wait(ms) => Some(*ms),
                _ => None,
            })
            .collect();
        assert!(waits.iter().any(|w| *w >= 300), "no thinking pause");
        let chars: Vec<char> = text.chars().collect();
        let (mut boundary, mut inner) = (Vec::new(), Vec::new());
        for (i, wait) in waits.iter().enumerate().skip(1) {
            if *wait >= 300 {
                continue;
            }
            if chars[i - 1] == ' ' {
                boundary.push(*wait);
            } else {
                inner.push(*wait);
            }
        }
        let average = |v: &[u64]| v.iter().sum::<u64>() as f64 / v.len() as f64;
        assert!(average(&boundary) > average(&inner) + 25.0);
    }

    #[test]
    fn profile_lookup_defaults_to_natural_and_rejects_unknown_names() {
        assert_eq!(profile_named(None).unwrap().name, "natural");
        assert_eq!(profile_named(Some("careful")).unwrap().name, "careful");
        assert!(profile_named(Some("sloppy")).is_none());
    }
}

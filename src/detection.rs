const CHEST_LABELS: &[&str] = &[
    "CHEST",
    "CHEST OPENED",
    "CHEST SEARCHED",
    "RARE CHEST",
    "RARE CHEST OPENED",
    "RARE CHEST SEARCHED",
    "AMMO BOX",
    "RARE AMMO BOX",
    "AMMO CRATE",
];

pub struct Cues {
    pub chest: bool,
    pub battle_bus: bool,
    pub pickup: bool,
    pub xp_notice: bool,
    pub chest_label: bool,
    pub combat: bool,
}

impl Cues {
    pub fn read(text: &str) -> Self {
        let lines: Vec<_> = text
            .lines()
            .map(|line| {
                line.split(|c: char| !c.is_ascii_alphanumeric() && c != ',')
                    .filter(|word| !word.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_ascii_uppercase()
            })
            .filter(|line| !line.is_empty())
            .collect();

        let chest = lines.iter().enumerate().any(|(index, line)| {
            if CHEST_LABELS.contains(&line.as_str()) {
                return index.checked_sub(1).is_some_and(|i| xp_only(&lines[i]))
                    || lines.get(index + 1).is_some_and(|next| xp_only(next));
            }

            CHEST_LABELS.iter().any(|label| {
                line.strip_prefix(label)
                    .and_then(|rest| rest.strip_prefix(' '))
                    .or_else(|| {
                        line.strip_suffix(label)
                            .and_then(|rest| rest.strip_suffix(' '))
                    })
                    .is_some_and(xp_only)
            })
        });
        let battle_bus = lines.iter().any(|line| {
            line == "TO JUMP" || line.ends_with(" TO JUMP") || line.starts_with("LAST STOP IN ")
        });
        let pickup = lines.iter().any(|line| {
            line == "PICKUP"
                || line == "PICK UP"
                || line.starts_with("PICK UP ")
                || line.ends_with(" PICK UP")
                || line.contains(" PICK UP ")
                || line.contains("BACKPACK FULL")
                || line == "SWAP"
        });
        let combat = lines.iter().any(|line| {
            line.starts_with("ELIMINAT")
                || line.starts_with("KNOCKED")
                || line.contains("SHIELD CRACKED")
                || line == "HEADSHOT"
        });
        let xp_notice = lines.iter().any(|line| xp_only(line));
        let chest_label = lines
            .iter()
            .any(|line| CHEST_LABELS.contains(&line.as_str()));
        Self {
            chest: chest && !combat,
            battle_bus,
            pickup,
            xp_notice,
            chest_label,
            combat,
        }
    }
}

#[derive(Default)]
pub struct BreakCue {
    marker_at: Option<u64>,
    hit_at: Option<u64>,
    burst_at: Option<u64>,
    burst_visible: bool,
    pending_at: Option<u64>,
    last_count: Option<u64>,
    combat_at: Option<u64>,
}

impl BreakCue {
    pub fn observe(
        &mut self,
        now: u64,
        marker: bool,
        impact: bool,
        burst: bool,
        cues: &Cues,
    ) -> bool {
        if cues.combat {
            self.combat_at = Some(now);
            self.marker_at = None;
            self.hit_at = None;
            self.burst_at = None;
            self.pending_at = None;
        }
        let blocked = self.combat_at.is_some_and(|at| now - at <= 850);
        // The elimination notice can arrive a frame or two after the hit.
        if self.pending_at.is_some_and(|at| now - at >= 220) {
            self.pending_at = None;
            self.last_count = Some(now);
            return true;
        }
        if marker {
            self.marker_at = Some(now);
        }
        let nearby = self.marker_at.is_some_and(|at| now - at <= 1100);
        if !blocked && impact && nearby {
            self.hit_at = Some(now);
        }
        if !blocked && nearby && burst && !self.burst_visible {
            self.burst_at = Some(now);
        }
        self.burst_visible = burst;
        if !blocked
            && self.pending_at.is_none()
            && self.last_count.is_none_or(|at| now - at > 1400)
            && self.burst_at.is_some_and(|burst| {
                now - burst <= 1300
                    && (cues.pickup
                        || self
                            .hit_at
                            .is_some_and(|hit| hit < burst && burst - hit <= 1100))
            })
        {
            self.hit_at = None;
            self.burst_at = None;
            self.pending_at = Some(now);
        }
        false
    }
}

pub struct HudFrame<'a> {
    pub pixels: &'a [u8],
    pub width: usize,
    pub height: usize,
}

impl HudFrame<'_> {
    fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.width + x) * 4;
        [self.pixels[i + 2], self.pixels[i + 1], self.pixels[i]]
    }

    pub fn enlarged_xp(&self) -> (Vec<u8>, i32, i32) {
        let (left, right) = (self.width * 62 / 100, self.width * 98 / 100);
        let (top, bottom) = (self.height * 30 / 100, self.height * 49 / 100);
        let scale = if self.height < 850 { 3 } else { 2 };
        let (width, height) = ((right - left) * scale, (bottom - top) * scale);
        let mut pixels = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let [r, g, b] = self.pixel(left + x / scale, top + y / scale);
                let purple = b >= 155 && b >= r && b.saturating_sub(g) >= 15;
                let light = r >= 235 && g >= 220 && b >= 235;
                let value = if purple || light { 255 } else { 0 };
                pixels.extend_from_slice(&[value, value, value, 0]);
            }
        }
        (pixels, width as i32, height as i32)
    }

    pub fn chest_reward_label(&self) -> bool {
        // The italic nine-pixel HUD label is often too small for OCR to
        // recognize, even when the larger XP amount reads clearly.
        const CHEST: [&[u8]; 9] = [
            b"...###.......#..####...###..#####...",
            b"..#####..##..#.#####..#####.#####...",
            b".##..##..#..##.##....##..##...#.....",
            b".##...#..#..##.##....##......##.....",
            b".#......##..##.####...###....##.....",
            b"##......######.##......###...##.....",
            b"##...##.##..#..#....##..##...##.....",
            b"###.##..#...#..##...##..##...#......",
            b".####...#..##..####..####....#......",
        ];
        const AMMO: [&[u8]; 9] = [
            b".....##........##........##....##........###.....##.......##...",
            b"....###...###..###..##...##...#####.....##.##..#####..##..##...",
            b"....###...###.#.#..###..#.#..##..##.....#..##.##..##..##.##....",
            b"...##.#..####.#.#..###..#.#..#...##....##..##.##..##...###.....",
            b"...#..#..##.###.#..#.###.##..#...##....#####..##..##...###.....",
            b"...#..#..#..##.##..#.###.##.##...##....##.##..#...##..####.....",
            b"..#####..#..##.##..#..##.##.##...#.....#...####...##..##.#.....",
            b"..##.###.#.....##.##.....#..##..##.....#..##..##.##..##..#.....",
            b"..#...##.#.....##.##.....#...####......####...####...#...##....",
        ];
        self.reward_matches(
            &CHEST,
            &[(4, 1), (11, 1), (18, 1), (24, 1), (31, 1), (11, 5)],
        ) || self.reward_matches(
            &AMMO,
            &[(5, 1), (12, 1), (23, 1), (31, 1), (41, 1), (48, 1), (56, 1)],
        )
    }

    fn reward_matches(&self, label: &[&[u8]; 9], anchors: &[(usize, usize)]) -> bool {
        let light = |x, y| {
            let [r, g, b] = self.pixel(x, y);
            r >= 210 && g >= 155 && b >= 218 && b.saturating_sub(r) < 60
        };
        let columns = label[0].len();
        for scale in [100, 85, 115, 70, 135] {
            let width = (columns * self.height * scale / 72_000).max(12);
            let height = (9 * self.height * scale / 72_000).max(4);
            if width >= self.width {
                continue;
            }
            let right_min = (self.width * 82 / 100).saturating_sub(width);
            let right_max = (self.width * 89 / 100).saturating_sub(width);
            for y in self.height * 38 / 100..self.height * 47 / 100 {
                for x in right_min..right_max {
                    let matches = |col, row| light(x + col * width / columns, y + row * height / 9);
                    if anchors
                        .iter()
                        .filter(|&&(col, row)| matches(col, row))
                        .count()
                        < 4
                    {
                        continue;
                    }
                    let (mut overlap, mut union) = (0, 0);
                    for (row, pattern) in label.iter().enumerate() {
                        for (col, &expected) in pattern.iter().enumerate() {
                            let expected = expected == b'#';
                            let actual = matches(col, row);
                            overlap += usize::from(expected && actual);
                            union += usize::from(expected || actual);
                        }
                    }
                    let prior = (self.height / 90).max(2);
                    let attached = (x.saturating_sub(prior)..x.saturating_sub(prior / 4))
                        .flat_map(|x| (y..y + height).map(move |y| (x, y)))
                        .filter(|&(x, y)| light(x, y))
                        .count();
                    if overlap * 100 >= union * 56 && attached * 7 < prior * height {
                        return true;
                    }
                }
            }
        }
        false
    }

    pub fn container_burst(&self) -> bool {
        let core = |x, y| {
            let [r, g, b] = self.pixel(x, y);
            r >= 245 && g >= 228 && b >= 186 && r.saturating_sub(b) >= 7
        };
        let halo = |x, y| {
            let [r, g, b] = self.pixel(x, y);
            r >= 140 && r.saturating_sub(g) >= 8 && g.saturating_sub(b) >= 10
        };
        let dark = |x, y| {
            let [_, g, b] = self.pixel(x, y);
            g <= 190 || b <= 155
        };
        let (inner, middle, outer) = (
            (self.height / 90).max(2),
            (self.height / 23).max(4),
            (self.height / 10).max(8),
        );
        let ring = |x: usize, y: usize, distance: usize| {
            let diagonal = distance * 7 / 10;
            [
                (x - distance, y),
                (x + distance, y),
                (x, y - distance),
                (x, y + distance),
                (x - diagonal, y - diagonal),
                (x + diagonal, y - diagonal),
                (x - diagonal, y + diagonal),
                (x + diagonal, y + diagonal),
            ]
        };
        let stride = (self.height / 170).max(2);
        let mut bright_centers = 0;
        for y in ((self.height / 5).max(outer)..(self.height - outer)).step_by(stride) {
            for x in ((self.width / 5).max(outer)
                ..(self.width * 4 / 5).min(self.width.saturating_sub(outer)))
                .step_by(stride)
            {
                if core(x, y)
                    && ring(x, y, inner)
                        .iter()
                        .filter(|&&(x, y)| core(x, y))
                        .count()
                        >= 5
                    && ring(x, y, middle)
                        .iter()
                        .filter(|&&(x, y)| halo(x, y))
                        .count()
                        >= 4
                    && ring(x, y, outer)
                        .iter()
                        .filter(|&&(x, y)| dark(x, y))
                        .count()
                        >= 6
                {
                    // A glint on the intact chest can pass at a few pixels.
                    // The opening burst is a larger patch of matching light.
                    bright_centers += 1;
                    if bright_centers >= 8 {
                        return true;
                    }
                }
            }
        }
        false
    }

    pub fn pickaxe_impact(&self) -> bool {
        // The reticle sits 69% down the captured rectangle. A pickaxe impact
        // fills this area with a brief blue-white flash.
        let (cx, cy) = (self.width / 2, self.height * 69 / 100);
        let radius = (self.height * 11 / 100).max(3);
        let mut bright = 0;
        for y in ((cy - radius)..(cy + radius)).step_by(3) {
            for x in (cx.saturating_sub(radius)..(cx + radius).min(self.width)).step_by(3) {
                let [r, g, b] = self.pixel(x, y);
                if b >= 205 && g >= 155 && u16::from(b) >= u16::from(r) + 18 {
                    bright += 1;
                }
            }
        }
        bright * 200 > radius * radius
    }

    pub fn chest_marker(&self) -> bool {
        let white = |x, y| {
            let [r, g, b] = self.pixel(x, y);
            r >= 235 && g >= 220 && b >= 190
        };
        // The visualized chest sound has a white lid with a keyhole and
        // a separate white base. Sample that silhouette at neighboring HUD sizes.
        let stride = (self.height / 300).max(1);
        let sizes = [30, 35, 40, 45, 50].map(|ratio| (self.height * ratio / 1000).max(8));
        for y in (self.height / 9..self.height * 8 / 9).step_by(stride) {
            for x in (self.width / 12..self.width * 11 / 12).step_by(stride) {
                if !white(x, y) {
                    continue;
                }
                for w in sizes {
                    let h = w * 85 / 100;
                    let left = x.saturating_sub(w / 5);
                    let top = y.saturating_sub(h / 5);
                    let at = |dx, dy| white(left + w * dx / 100, top + h * dy / 100);
                    if left + w + w / 6 >= self.width || top + h >= self.height {
                        continue;
                    }
                    if at(78, 21)
                        && at(20, 47)
                        && at(78, 47)
                        && at(20, 85)
                        && at(50, 85)
                        && at(78, 85)
                        && !at(50, 31)
                        && !at(50, 40)
                        && !at(20, 68)
                        && !at(50, 68)
                        && !at(78, 68)
                        && !white(left.saturating_sub(w / 6), top + h / 3)
                        && !white(left + w + w / 6, top + h / 3)
                    {
                        return true;
                    }
                }
            }
        }
        false
    }
}

fn xp_only(line: &str) -> bool {
    line.strip_suffix("XP").is_some_and(|amount| {
        let amount = amount.trim_end();
        amount.starts_with(|c: char| c.is_ascii_digit())
            && amount.chars().all(|c| c.is_ascii_digit() || c == ',')
    })
}

// Require several clear frames before treating the same HUD text as a new event.
pub struct CueLatch {
    clear_frames: u8,
    rearm_after: u8,
}

impl CueLatch {
    pub fn new(rearm_after: u8) -> Self {
        Self {
            clear_frames: rearm_after,
            rearm_after,
        }
    }

    pub fn observe(&mut self, present: bool) -> bool {
        if present {
            let appeared = self.clear_frames >= self.rearm_after;
            self.clear_frames = 0;
            appeared
        } else {
            self.clear_frames = self.clear_frames.saturating_add(1);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_chest_xp_without_counting_prompts_or_quests() {
        for text in [
            "+100 XP\nCHEST",
            "RARE CHEST +200XP",
            "+1,000 XP CHEST OPENED",
            "+300 XP\nAMMO BOX",
        ] {
            assert!(Cues::read(text).chest, "{text}");
        }
        for text in [
            "E Search",
            "CHEST",
            "+1,700 XP\nHEADSHOT\nELIMINATION",
            "+100 XP\nFIRST CHEST SEARCHED",
            "Search chests (3/5) +100 XP",
        ] {
            assert!(!Cues::read(text).chest, "{text}");
        }
    }

    #[test]
    fn distinguishes_battle_bus_from_gliding() {
        assert!(Cues::read("[Space] TO JUMP").battle_bus);
        assert!(Cues::read("LAST STOP IN 24s").battle_bus);
        assert!(!Cues::read("Space TO DEPLOY GLIDER").battle_bus);
    }

    #[test]
    fn a_flickering_cue_only_counts_once_until_it_clears() {
        let mut latch = CueLatch::new(3);
        let frames = [true, true, false, true, false, false, false, true];
        let events: Vec<_> = frames.map(|present| latch.observe(present)).into();
        assert_eq!(
            events,
            [true, false, false, false, false, false, false, true]
        );
    }

    #[test]
    fn a_break_counts_once_even_if_the_marker_stays_visible() {
        let mut detector = BreakCue::default();
        let neutral = Cues::read("");
        assert!(!detector.observe(100, true, true, false, &neutral));
        assert!(!detector.observe(450, true, false, true, &neutral));
        assert!(detector.observe(700, true, false, true, &neutral));
        assert!(!detector.observe(900, true, true, true, &neutral));
    }

    #[test]
    fn a_combat_notice_cancels_a_candidate_break() {
        let mut detector = BreakCue::default();
        let loot = Cues::read("PICK UP");
        let elimination = Cues::read("+1,700 XP\nSHIELD CRACKED\nELIMINATION\nPICK UP");
        assert!(elimination.combat);
        assert!(!detector.observe(100, true, true, false, &loot));
        assert!(!detector.observe(250, true, false, true, &loot));
        assert!(!detector.observe(450, true, true, true, &elimination));
        assert!(!detector.observe(900, false, false, false, &loot));
    }
}

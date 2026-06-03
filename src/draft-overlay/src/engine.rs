//! Pure recommender engine (no hudhook / no imgui) — shared by the overlay DLL
//! (`lib.rs`) and the offline self-test (`../../overlay-selftest`, via #[path]).
//!
//! Two regimes, blended by data confidence:
//!   - DATA term: champ winrate delta + synergy + counter + banrate (from matches).
//!     Sample-shrunk, so it fades to ~0 when there are few matches.
//!   - THEORY term: tag-based comp-need + synergy + counter (from taxonomy only).
//!     Always available; weighted by (1 - confidence) so it DRIVES the ranking at
//!     cold-start (empty save) and recedes as real stats accumulate.
//! An optional available-champion POOL restricts candidates (the game can release
//! champions gradually).

use std::collections::HashSet;

pub const WEIGHTS_TSV: &str = include_str!("draft_weights.tsv");
pub const STATE_PATH: &str = "C:\\tmp\\tfm2_draft_state.txt";
pub const TOP_N: usize = 8;

// champion id -> Korean display name (all 60; banpick-data.js champions[]).
pub const CHAMPS: &[(&str, &str)] = &[
    ("vampire", "흡혈귀"), ("monk", "몽크"), ("circus_blade", "곡예사"), ("android", "안드로이드"),
    ("inquisitor", "이단심문관"), ("lancer", "창술사"), ("ninja", "닌자"), ("fighter", "격투가"),
    ("ice_mage", "얼음술사"), ("wind_mage", "바람술사"), ("exorcist", "엑소시스트"), ("ghost", "유령"),
    ("pole_warrior", "봉술사"), ("dual_blader", "듀얼 블레이더"), ("demon", "악마"), ("swordman", "검사"),
    ("white_mage", "백마술사"), ("shadowmancer", "그림자술사"), ("pythoness", "무녀"), ("hunter", "사냥꾼"),
    ("jiangshi", "강시"), ("guardian_spirit", "수호령"), ("druid", "드루이드"), ("siege_breaker", "공성병"),
    ("knight", "기사"), ("werewolf", "늑대인간"), ("dark_mage", "흑마술사"), ("magic_knight", "마검사"),
    ("illusionist", "환영술사"), ("berserker", "광전사"), ("pyromancer", "화염술사"), ("bomber", "폭탄병"),
    ("necromancer", "네크로맨서"), ("priest", "성직자"), ("lightning_mage", "번개술사"), ("prisoner", "죄수"),
    ("dokkaebi", "도깨비"), ("executioner", "처형인"), ("barrier_magician", "결계사"), ("shield_bearer", "방패병"),
    ("archer", "궁수"), ("gambler", "도박사"), ("whip_master", "채찍술사"), ("hitman", "히트맨"),
    ("chef", "요리사"), ("spirit_caller", "정령사"), ("taoist", "도사"), ("gunner", "총잡이"),
    ("hammerer", "중보병"), ("clown", "광대"), ("cavalry_knight", "기병"), ("enchanter", "인챈터"),
    ("soldier", "소총수"), ("plague_doctor", "역병의사"), ("dancer", "무희"), ("boomerang_hunter", "부메랑 헌터"),
    ("voodoo_shaman", "부두술사"), ("poison_dart_hunter", "독침술사"), ("bard", "음유시인"), ("ogre", "오우거"),
];

pub fn ko_name(id: &str) -> String {
    CHAMPS.iter().find(|(i, _)| *i == id).map(|(_, k)| k.to_string()).unwrap_or_else(|| id.to_string())
}

// tunable weights. NOTE (2026-06-01 validation): a non-circular holdout showed the
// DATA synergy/counter terms add no measurable predictive power at this career's
// sample size (68 team matches: synergy +0.003 AUC ≈ noise, counter −0.004 ≈ harmful),
// while champ-winrate (C) is the real signal (AUC 0.574). So synergy/counter are now
// (a) DEMOTED below C, (b) faded by data confidence (×self.confidence(), so they grow
// back as matches accumulate), and (c) the counter no longer double-counts. See VALIDATION.md.
const W_SYN: f32 = 0.7; // was 1.2 — demoted to a secondary tie-breaker behind C
const W_CNT: f32 = 0.5; // was 1.0 — halved (the +cnt − cnt_rev pair was ~2× on an anti-symmetric table)
const W_WEAK: f32 = 0.5; // was 1.0 — "
const W_COMP: f32 = 0.6;
const W_DUP: f32 = 0.4;
const W_BAN_CNT: f32 = 1.0;
const W_BAN_SYN: f32 = 0.8;
const W_BAN_META: f32 = 0.5;
const COMP_UNIT: f32 = 0.12;
// early-pick flexibility bonus, per extra lane a champ can play (capped at +2 lanes).
// First picks are "blind" → favor flex champs that hide your lane assignment.
const W_FLEX: f32 = 0.01;
// confidence: full trust in DATA terms at this many matches (theory fully faded).
const FULL_CONF_MATCHES: f32 = 200.0;
// a champ with fewer games than this behind its winrate is flagged low-confidence
// in the UI (its `C` is too thin to trust). Unknown game count = not flagged.
const LOW_CONF_GAMES: u32 = 15;
// DATA-driven role thresholds on the per-game ratios in `RP` rows (tanking/dealing,
// healing/dealing). Calibrated from real patch stats — clean gaps separate the roles:
//   healers (guardian 4.8, monk 4.7, pythoness 2.7) ≫ bruisers (vampire 0.9, fighter 0.7)
//   tanks (android 2.4, ogre 1.8) ≫ carries (archer 0.6, soldier 0.4)
const HEAL_RATIO: f32 = 1.5; // healRatio ≥ → dedicated healer/enchanter
const TANK_RATIO: f32 = 1.6; // tankRatio ≥ (and not a healer) → hard frontline/tank
const FRONT_RATIO: f32 = 1.25; // tankRatio ≥ (and not a healer) → frontline (incl. bruiser)

/// Lane (position) display names, index 0..5. Matches the launcher's POS rows
/// (0=Top 1=Jungle 2=Mid 3=Bottom 4=Support, from champion_patch_statistics).
pub const LANES: [&str; 5] = ["탑", "정글", "미드", "바텀", "서폿"];

#[derive(Default)]
struct Taxon {
    category: String,
    damage: String,
    tags: Vec<String>,
}

pub struct Tables {
    champ_delta: std::collections::HashMap<String, f32>,
    /// games behind each champ's delta (optional 4th field of a `C` row); used to
    /// flag thin-sample recommendations as low-confidence in the UI.
    champ_games: std::collections::HashMap<String, u32>,
    /// per-(champ, lane index) winrate delta + games behind it (`CP` rows). Lane-
    /// SPECIFIC — so a champ strong in its main lane isn't credited that winrate in
    /// an off-lane. Falls back to the champ-level `champ_delta` when a cell is absent.
    champ_pos_delta: std::collections::HashMap<(String, u8), (f32, u32)>,
    synergy: std::collections::HashMap<(String, String), f32>,
    counter: std::collections::HashMap<(String, String), f32>,
    taxon: std::collections::HashMap<String, Taxon>,
    banrate: std::collections::HashMap<String, f32>,
    /// total matches behind these stats (from an `M\t<n>` row); 0 = cold-start.
    pub matches: usize,
    /// which save/career these stats came from (from a `SAVE\t<name>` row).
    pub save_name: String,
    /// available champion pool for this save (from a `POOL\t<csv ids>` row);
    /// empty = unknown → no restriction. The launcher derives it from which
    /// champions appear in the save (gradual champion release).
    pub pool: HashSet<String>,
    /// champion -> primary lane index (most-played position), from `POS` rows.
    pos_primary: std::collections::HashMap<String, u8>,
    /// champion -> all lanes it plays meaningfully (flex), from `POS` rows.
    /// Empty overall = no position data → the lane board falls back to a list.
    pos_play: std::collections::HashMap<String, Vec<u8>>,
    /// champion -> (tankRatio, healRatio) = tanking/dealing, healing/dealing per
    /// game (from `RP` rows). DATA-driven role signal: thresholded to detect
    /// frontline/healer for comp balance, catching champs the static tags miss.
    role: std::collections::HashMap<String, (f32, f32)>,
}

/// Baked fallback table (compiled-in draft_weights.tsv).
pub fn parse_tables() -> Tables {
    parse_tables_from_str(WEIGHTS_TSV)
}

/// Parse tables from TSV text — used both for the baked fallback and for a
/// refreshed draft_weights.tsv loaded from disk at runtime.
pub fn parse_tables_from_str(tsv: &str) -> Tables {
    use std::collections::HashMap;
    let mut t = Tables {
        champ_delta: HashMap::new(),
        champ_games: HashMap::new(),
        champ_pos_delta: HashMap::new(),
        synergy: HashMap::new(),
        counter: HashMap::new(),
        taxon: HashMap::new(),
        banrate: HashMap::new(),
        matches: 0,
        save_name: String::new(),
        pool: HashSet::new(),
        pos_primary: HashMap::new(),
        pos_play: HashMap::new(),
        role: HashMap::new(),
    };
    for line in tsv.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        match f[0] {
            "C" if f.len() >= 3 => {
                t.champ_delta.insert(f[1].into(), f[2].parse().unwrap_or(0.0));
                // optional 4th field = games behind this delta (low-confidence flag)
                if let Some(g) = f.get(3).and_then(|x| x.parse::<u32>().ok()) {
                    t.champ_games.insert(f[1].into(), g);
                }
            }
            // CP<TAB><champ><TAB><laneIdx><TAB><delta><TAB><games> — per-lane winrate.
            "CP" if f.len() >= 5 => {
                if let Ok(li) = f[2].parse::<u8>() {
                    let d = f[3].parse().unwrap_or(0.0);
                    let g = f[4].parse().unwrap_or(0);
                    t.champ_pos_delta.insert((f[1].into(), li.min(4)), (d, g));
                }
            }
            "S" if f.len() >= 4 => {
                t.synergy.insert((f[1].into(), f[2].into()), f[3].parse().unwrap_or(0.0));
            }
            "K" if f.len() >= 4 => {
                t.counter.insert((f[1].into(), f[2].into()), f[3].parse().unwrap_or(0.0));
            }
            "T" if f.len() >= 4 => {
                let tags = if f.len() >= 5 {
                    f[4].split(',').map(|s| s.to_string()).collect()
                } else {
                    Vec::new()
                };
                t.taxon.insert(f[1].into(), Taxon { category: f[2].into(), damage: f[3].into(), tags });
            }
            "B" if f.len() >= 3 => {
                t.banrate.insert(f[1].into(), f[2].parse().unwrap_or(0.0));
            }
            // M<TAB><total_matches> — drives the data-confidence blend.
            "M" if f.len() >= 2 => {
                t.matches = f[1].parse().unwrap_or(0);
            }
            // SAVE<TAB><name> — which save/career these stats came from.
            "SAVE" if f.len() >= 2 => {
                t.save_name = f[1].to_string();
            }
            // POOL<TAB><id1,id2,...> — available champions for this save.
            "POOL" if f.len() >= 2 => {
                t.pool = f[1].split(',').filter(|x| !x.is_empty()).map(|x| x.to_string()).collect();
            }
            // POS<TAB><champ><TAB><primaryIdx><TAB><idx,idx,...> — lane positions.
            "POS" if f.len() >= 4 => {
                let prim: u8 = f[2].parse().unwrap_or(0);
                let mut plays: Vec<u8> =
                    f[3].split(',').filter_map(|x| x.parse().ok()).filter(|l| *l < 5).collect();
                if plays.is_empty() {
                    plays.push(prim);
                }
                t.pos_primary.insert(f[1].into(), prim.min(4));
                t.pos_play.insert(f[1].into(), plays);
            }
            // RP<TAB><champ><TAB><tankRatio><TAB><healRatio> — data role signal.
            "RP" if f.len() >= 4 => {
                let tr = f[2].parse().unwrap_or(0.0);
                let hr = f[3].parse().unwrap_or(0.0);
                t.role.insert(f[1].into(), (tr, hr));
            }
            _ => {}
        }
    }
    t
}

impl Tables {
    pub fn len(&self) -> usize { self.champ_delta.len() }
    /// 0.0 (no data → pure theory) .. 1.0 (rich data → pure stats).
    pub fn confidence(&self) -> f32 { (self.matches as f32 / FULL_CONF_MATCHES).min(1.0) }
    fn delta(&self, c: &str) -> f32 { *self.champ_delta.get(c).unwrap_or(&0.0) }
    /// games behind a champ's winrate; u32::MAX if unknown (→ never flagged).
    fn games(&self, c: &str) -> u32 { *self.champ_games.get(c).unwrap_or(&u32::MAX) }
    fn low_conf(&self, c: &str) -> bool { self.games(c) < LOW_CONF_GAMES }
    /// Lane-specific winrate delta: the (champ, lane) cell if present, else the
    /// champ-level delta. So recommending a champ for a lane it rarely plays uses
    /// THAT lane's (shrunk-to-neutral) winrate, not its main-lane strength.
    fn delta_for(&self, c: &str, lane: Option<u8>) -> f32 {
        if let Some(l) = lane {
            if let Some((d, _)) = self.champ_pos_delta.get(&(c.into(), l)) {
                return *d;
            }
        }
        self.delta(c)
    }
    /// games behind the lane-specific delta (for the low-confidence "?" flag).
    fn games_for(&self, c: &str, lane: Option<u8>) -> u32 {
        if let Some(l) = lane {
            if let Some((_, g)) = self.champ_pos_delta.get(&(c.into(), l)) {
                return *g;
            }
        }
        self.games(c)
    }
    fn low_conf_for(&self, c: &str, lane: Option<u8>) -> bool {
        self.games_for(c, lane) < LOW_CONF_GAMES
    }
    fn syn(&self, a: &str, b: &str) -> f32 { *self.synergy.get(&(a.into(), b.into())).unwrap_or(&0.0) }
    fn cnt(&self, a: &str, b: &str) -> f32 { *self.counter.get(&(a.into(), b.into())).unwrap_or(&0.0) }
    fn ban(&self, c: &str) -> f32 { *self.banrate.get(c).unwrap_or(&0.0) }
    fn has_tag(&self, c: &str, tag: &str) -> bool {
        self.taxon.get(c).map(|t| t.tags.iter().any(|x| x == tag)).unwrap_or(false)
    }
    fn damage(&self, c: &str) -> &str { self.taxon.get(c).map(|t| t.damage.as_str()).unwrap_or("AD") }
    fn category(&self, c: &str) -> &str { self.taxon.get(c).map(|t| t.category.as_str()).unwrap_or("") }

    // ---- DATA-driven role classification (RP rows) — augments the static tags ----
    /// (tankRatio, healRatio) for a champ; (0,0) if no role data.
    fn role(&self, c: &str) -> (f32, f32) { self.role.get(c).copied().unwrap_or((0.0, 0.0)) }
    /// dedicated healer/enchanter by data (heals far more than it deals).
    fn is_healer_data(&self, c: &str) -> bool { self.role(c).1 >= HEAL_RATIO }
    /// hard frontline/tank by data (soaks much more than it deals, and isn't a healer).
    fn is_tank_data(&self, c: &str) -> bool {
        let (t, h) = self.role(c);
        t >= TANK_RATIO && h < 1.0
    }
    /// frontline (tank OR bruiser) by data — not a healer.
    fn is_frontline_data(&self, c: &str) -> bool {
        let (t, h) = self.role(c);
        t >= FRONT_RATIO && h < 1.2
    }
    /// Meta TIER letter (S/A/B/C/D) from the champ's winrate delta — per-lane if a
    /// lane is given, else the champ's overall (blended) winrate. "" when there is no
    /// winrate data (cold start), so the grid stays clean until stats exist. Same
    /// thresholds as the HTML report so the two views agree.
    pub fn tier_of(&self, c: &str, lane: Option<u8>) -> &'static str {
        let has = match lane {
            Some(l) => self.champ_pos_delta.contains_key(&(c.to_string(), l)),
            None => self.champ_delta.contains_key(c),
        };
        if !has {
            return "";
        }
        let d = self.delta_for(c, lane);
        if d >= 0.06 {
            "S"
        } else if d >= 0.02 {
            "A"
        } else if d >= -0.02 {
            "B"
        } else if d >= -0.06 {
            "C"
        } else {
            "D"
        }
    }

    /// Short role glyph for the UI: 힐(healer)/탱(tank)/브(bruiser·frontline)/딜(carry).
    /// "" when there's no role data yet (cold start / <5 games) — don't guess.
    pub fn role_tag(&self, c: &str) -> &'static str {
        if !self.role.contains_key(c) {
            ""
        } else if self.is_healer_data(c) {
            "힐"
        } else if self.is_tank_data(c) {
            "탱"
        } else if self.is_frontline_data(c) {
            "브"
        } else {
            "딜"
        }
    }
    fn category_team(&self, allies: &[&str], cat: &str) -> bool {
        allies.iter().any(|a| self.category(a) == cat)
    }

    // ---- lane / position helpers (POS rows) ----
    fn has_pos(&self) -> bool { !self.pos_play.is_empty() }
    fn primary(&self, c: &str) -> Option<u8> { self.pos_primary.get(c).copied() }
    fn plays(&self, c: &str, lane: u8) -> bool {
        self.pos_play.get(c).map(|v| v.contains(&lane)).unwrap_or(false)
    }
    /// number of lanes this champ plays (1 if unknown → no flex bonus).
    fn flex(&self, c: &str) -> u8 { self.pos_play.get(c).map(|v| v.len() as u8).unwrap_or(1) }
    /// counter advantage of `cand` over `enemy`: data counter diff + a faded
    /// tag-based term (so the lane counter-flag still fires at cold-start).
    fn counters(&self, cand: &str, enemy: &str) -> f32 {
        let data = self.cnt(cand, enemy) - self.cnt(enemy, cand);
        let theory = (1.0 - self.confidence()) * self.tag_counter(cand, &[enemy]);
        data + theory
    }

    fn comp_need(&self, cand: &str, allies: &[&str]) -> f32 {
        // First pick (no allies yet): there is no identifiable comp GAP, so this
        // term must be 0 — otherwise an empty team "needs everything" and champs
        // with the most utility tags (CC/Heal/Shield/Backline = mostly SUPPORTS)
        // wrongly top the first pick. Ramp the signal up as the team forms.
        if allies.is_empty() {
            return 0.0;
        }
        let team_has = |tag: &str| allies.iter().any(|a| self.has_tag(a, tag));
        // role need is satisfied by the static TAG or the DATA signal (RP) — so a
        // champ the tags miss but that empirically tanks/heals still fills the gap.
        let is_tank = |c: &str| self.has_tag(c, "Tank") || self.is_tank_data(c);
        let is_front = |c: &str| self.has_tag(c, "Frontline") || self.is_frontline_data(c);
        let is_heal = |c: &str| self.has_tag(c, "Heal") || self.has_tag(c, "Shield") || self.is_healer_data(c);
        let mut s = 0.0;
        if !allies.iter().any(|a| is_tank(a)) && is_tank(cand) {
            s += COMP_UNIT;
        } else if !allies.iter().any(|a| is_front(a)) && is_front(cand) {
            s += COMP_UNIT * 0.5;
        }
        if !team_has("CC") && self.has_tag(cand, "CC") {
            s += COMP_UNIT * 0.7;
        }
        if !allies.iter().any(|a| is_heal(a)) && is_heal(cand) {
            s += COMP_UNIT * 0.6;
        }
        if !(team_has("Backline") || self.category_team(allies, "Range"))
            && (self.has_tag(cand, "Backline") || self.category(cand) == "Range")
        {
            s += COMP_UNIT * 0.6;
        }
        let ad = allies.iter().filter(|a| self.damage(a).contains("AD")).count();
        let ap = allies.iter().filter(|a| self.damage(a).contains("AP")).count();
        if !allies.is_empty() {
            if ap == 0 && self.damage(cand).contains("AP") {
                s += COMP_UNIT * 0.5;
            }
            if ad == 0 && self.damage(cand).contains("AD") {
                s += COMP_UNIT * 0.5;
            }
        }
        // signal strength grows with team size (1 pick = weak gap evidence, 4+ = full)
        s * (allies.len() as f32 / 4.0).min(1.0)
    }

    fn overlap(&self, cand: &str, allies: &[&str]) -> f32 {
        let cat = self.category(cand);
        if cat.is_empty() {
            return 0.0;
        }
        let n = allies.iter().filter(|a| self.category(a) == cat).count();
        n as f32 * COMP_UNIT * 0.5
    }

    /// Tag-based SYNERGY (no match data): engage→follow-up, peel-for-carry.
    /// Cold-start proxy for the data-driven `syn`; faded out by confidence.
    fn tag_synergy(&self, cand: &str, allies: &[&str]) -> f32 {
        let team = |tag: &str| allies.iter().any(|a| self.has_tag(a, tag));
        let mut s = 0.0;
        // engage (CC) + AOE/DOT follow-up
        if team("CC") && (self.has_tag(cand, "AOE") || self.has_tag(cand, "DOT")) {
            s += 0.5;
        }
        if team("AOE") && self.has_tag(cand, "CC") {
            s += 0.5;
        }
        // peel for a squishy backline carry
        if (team("Backline") || self.category_team(allies, "Range"))
            && (self.has_tag(cand, "Heal") || self.has_tag(cand, "Shield") || self.has_tag(cand, "CC"))
        {
            s += 0.5;
        }
        s * COMP_UNIT
    }

    /// Tag-based COUNTER vs the enemy comp (no match data). Cold-start proxy for
    /// the data-driven `cnt`; faded out by confidence.
    fn tag_counter(&self, cand: &str, enemies: &[&str]) -> f32 {
        let cnt = |tag: &str| enemies.iter().filter(|e| self.has_tag(e, tag)).count() as f32;
        let cat = |c: &str| enemies.iter().filter(|e| self.category(e) == c).count() as f32;
        let mut s = 0.0;
        // enemy squishy backline -> dive it (mobility / assassin)
        let backline = cnt("Backline") + cat("Range");
        if backline >= 2.0 && (self.has_tag(cand, "Mobility") || self.category(cand) == "Assassin") {
            s += 0.4 * (backline - 1.0).min(3.0);
        }
        // enemy tanky frontline -> whittle (poke / dot)
        let front = cnt("Tank") + cnt("Frontline");
        if front >= 2.0 && (self.has_tag(cand, "Poke") || self.has_tag(cand, "DOT")) {
            s += 0.4 * (front - 1.0).min(3.0);
        }
        // enemy poke -> engage (frontline / mobility to close)
        if cnt("Poke") >= 2.0 && (self.has_tag(cand, "Frontline") || self.has_tag(cand, "Mobility")) {
            s += 0.3;
        }
        // enemy heavy CC -> mitigate (mobility / shield)
        if cnt("CC") >= 3.0 && (self.has_tag(cand, "Mobility") || self.has_tag(cand, "Shield")) {
            s += 0.3;
        }
        s * COMP_UNIT
    }

    /// PICK desirability. DATA terms mirror tfm2_draft_ai::score_pick; the tag
    /// THEORY terms (tag_synergy/tag_counter) are added, scaled by (1-confidence),
    /// so they fold into the synergy/counter buckets and drive cold-start ranking.
    pub fn score_pick(&self, cand: &str, allies: &[&str], enemies: &[&str]) -> (f32, &'static str) {
        self.score_pick_in(cand, allies, enemies, None)
    }

    /// Like `score_pick`, but for a SPECIFIC lane: the champ-power term uses that
    /// lane's winrate (`delta_for`) instead of the champ's overall winrate. Pass
    /// `None` for a lane-agnostic score (flat list / ban context).
    pub fn score_pick_in(
        &self,
        cand: &str,
        allies: &[&str],
        enemies: &[&str],
        lane: Option<u8>,
    ) -> (f32, &'static str) {
        let picks_made = allies.len() as f32;
        let late = (picks_made / 4.0).min(1.0);
        let cnt_ramp = 0.5 + 0.5 * late;
        let power_ramp = 1.0 - 0.4 * late;
        let data = self.confidence(); // trust pairwise DATA proportional to match volume
        let theory = 1.0 - data;

        let base = power_ramp * self.delta_for(cand, lane);
        let mut syn = 0.0;
        for a in allies {
            syn += data * W_SYN * self.syn(cand, a);
        }
        syn += theory * self.tag_synergy(cand, allies);
        let mut cnt = 0.0;
        for e in enemies {
            cnt += data * W_CNT * cnt_ramp * self.cnt(cand, e);
            cnt -= data * W_WEAK * cnt_ramp * self.cnt(e, cand);
        }
        cnt += theory * self.tag_counter(cand, enemies);
        let comp = W_COMP * self.comp_need(cand, allies);
        let dup = W_DUP * self.overlap(cand, allies);
        let total = base + syn + cnt + comp - dup;
        (total, dominant(&[("강챔", base), ("시너지", syn), ("카운터", cnt), ("조합", comp)]))
    }

    /// BAN desirability. DATA terms mirror tfm2_draft_ai::score_ban; at cold-start
    /// add a faded tag term for champs that counter our picks / fit the meta.
    pub fn score_ban(&self, cand: &str, allies: &[&str], enemies: &[&str]) -> (f32, &'static str) {
        let data = self.confidence();
        let theory = 1.0 - data;
        let base = self.delta(cand);
        let mut cnt = 0.0;
        for a in allies {
            cnt += data * W_BAN_CNT * self.cnt(cand, a);
        }
        // cold-start: ban champs whose tags counter OUR picks (allies)
        cnt += theory * self.tag_counter(cand, allies);
        let mut syn = 0.0;
        for e in enemies {
            syn += data * W_BAN_SYN * self.syn(cand, e);
        }
        let meta = W_BAN_META * self.ban(cand);
        let total = base + cnt + syn + meta;
        (total, dominant(&[("강챔", base), ("우리킬", cnt), ("적시너지", syn), ("밴율", meta)]))
    }
}

fn dominant(parts: &[(&'static str, f32)]) -> &'static str {
    parts
        .iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(l, _)| *l)
        .unwrap_or("")
}

// ---- IPC draft state (kept for the offline self-test / legacy IPC format) ----
#[derive(Default, Clone)]
pub struct DraftState {
    pub ok: bool,
    pub ts: u64,
    pub phase: String, // "BAN" | "PICK"
    pub explore: bool,
    pub ally_pick: Vec<String>,
    pub enemy_pick: Vec<String>,
    pub ally_ban: Vec<String>,
    pub enemy_ban: Vec<String>,
    pub avail: usize,
}

fn split_csv(v: &str) -> Vec<String> {
    v.split(',').filter(|x| !x.is_empty()).map(|x| x.to_string()).collect()
}

pub fn parse_state(txt: &str) -> DraftState {
    let mut s = DraftState::default();
    for line in txt.lines() {
        let (k, v) = match line.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        match k {
            "ts" => s.ts = v.parse().unwrap_or(0),
            "phase" => s.phase = v.to_string(),
            "explore" => s.explore = v == "1",
            "ally_pick" => s.ally_pick = split_csv(v),
            "enemy_pick" => s.enemy_pick = split_csv(v),
            "ally_ban" => s.ally_ban = split_csv(v),
            "enemy_ban" => s.enemy_ban = split_csv(v),
            "avail" => s.avail = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    s.ok = !s.phase.is_empty();
    s
}

pub fn read_state() -> Option<DraftState> {
    let txt = std::fs::read_to_string(STATE_PATH).ok()?;
    Some(parse_state(&txt))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct Row {
    pub ko: String,
    pub id: String,
    pub score: f32,
    pub label: &'static str,
    /// true if this champ's winrate is based on too few games to trust (<15).
    pub low_conf: bool,
    /// data-driven role glyph (힐/탱/브/딜), "" if unknown.
    pub role: &'static str,
    /// meta tier letter (S/A/B/C/D), "" if no winrate data.
    pub tier: &'static str,
}

pub struct View {
    pub phase_ban: bool,
    pub explore: bool,
    pub age_ms: u64,
    pub my_pick: Vec<String>,  // ko names
    pub opp_pick: Vec<String>, // ko names
    pub my_ban: Vec<String>,
    pub opp_ban: Vec<String>,
    pub rows: Vec<Row>,
    pub matches: usize, // data volume behind the stats (for the confidence label)
    pub pool_size: usize, // number of available champions considered (0 = all)
}

/// Rank not-yet-taken champions for the current phase, from the human's side.
/// `pool` (if Some) restricts candidates to currently-available champions.
pub fn recommend(
    tables: &Tables,
    s: &DraftState,
    human_is_enemy: bool,
    pool: Option<&HashSet<String>>,
) -> View {
    let (my_pick, opp_pick, my_ban, opp_ban) = if human_is_enemy {
        (&s.enemy_pick, &s.ally_pick, &s.enemy_ban, &s.ally_ban)
    } else {
        (&s.ally_pick, &s.enemy_pick, &s.ally_ban, &s.enemy_ban)
    };
    let phase_ban = s.phase == "BAN";

    let mut taken: HashSet<&str> = HashSet::new();
    for v in [&s.ally_pick, &s.enemy_pick, &s.ally_ban, &s.enemy_ban] {
        for id in v {
            taken.insert(id.as_str());
        }
    }

    let allies: Vec<&str> = my_pick.iter().map(|x| x.as_str()).collect();
    let enemies: Vec<&str> = opp_pick.iter().map(|x| x.as_str()).collect();

    let mut rows: Vec<Row> = Vec::new();
    for (id, _ko) in CHAMPS {
        if taken.contains(id) {
            continue;
        }
        if let Some(p) = pool {
            if !p.contains(*id) {
                continue; // not yet released / not available
            }
        }
        let (score, label) = if phase_ban {
            tables.score_ban(id, &allies, &enemies)
        } else {
            tables.score_pick(id, &allies, &enemies)
        };
        rows.push(Row {
            ko: ko_name(id),
            id: id.to_string(),
            score,
            label,
            low_conf: tables.low_conf(id),
            role: tables.role_tag(id),
            tier: tables.tier_of(id, None),
        });
    }
    rows.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    rows.truncate(TOP_N);

    let ko_list = |ids: &[String]| ids.iter().map(|id| ko_name(id)).collect::<Vec<_>>();
    View {
        phase_ban,
        explore: s.explore,
        age_ms: now_ms().saturating_sub(s.ts),
        my_pick: ko_list(my_pick),
        opp_pick: ko_list(opp_pick),
        my_ban: ko_list(my_ban),
        opp_ban: ko_list(opp_ban),
        rows,
        matches: tables.matches,
        pool_size: pool.map(|p| p.len()).unwrap_or(0),
    }
}

// ---------------------------------------------------------------------------
// Lane-aware PICK board: one slot per position (탑/정글/미드/바텀/서폿). Fills
// the lanes my picks cover (by primary→flex→any), then for each OPEN lane ranks
// the best champs that can play it. Pick timing (선픽/후픽) tunes the ranking:
// early picks get a flexibility bonus; late picks lean on counters (cnt_ramp in
// score_pick) and surface a counter flag vs the enemy's revealed lane champ.
// ---------------------------------------------------------------------------
pub struct LaneCand {
    pub ko: String,
    pub id: String,
    pub score: f32,
    pub label: &'static str,
    pub low_conf: bool,
    /// data-driven role glyph (힐/탱/브/딜), "" if unknown.
    pub role: &'static str,
    /// meta tier letter (S/A/B/C/D) for THIS lane, "" if no winrate data.
    pub tier: &'static str,
}

pub struct Lane {
    pub name: &'static str,
    /// ko name of my pick assigned here; `None` = open (use `cands`).
    pub filled: Option<String>,
    /// top candidates that can play this lane (open lanes only).
    pub cands: Vec<LaneCand>,
    /// ko of an enemy champ that can play here and our top candidate counters.
    pub counter_of: Option<String>,
    /// the recommended lane to pick this turn (★).
    pub star: bool,
}

pub struct LaneView {
    pub explore: bool,
    pub age_ms: u64,
    pub my_pick: Vec<String>,
    pub opp_pick: Vec<String>,
    pub my_ban: Vec<String>,
    pub opp_ban: Vec<String>,
    pub lanes: Vec<Lane>, // always 5
    pub picks_made: usize,
    pub timing: &'static str, // 선픽 / 중반 / 후픽
    pub tip: &'static str,
    pub matches: usize,
    pub pool_size: usize,
    /// false when no POS data is loaded → caller should fall back to a flat list.
    pub has_pos: bool,
}

pub fn recommend_lanes(
    tables: &Tables,
    s: &DraftState,
    human_is_enemy: bool,
    pool: Option<&HashSet<String>>,
) -> LaneView {
    let (my_pick, opp_pick, my_ban, opp_ban) = if human_is_enemy {
        (&s.enemy_pick, &s.ally_pick, &s.enemy_ban, &s.ally_ban)
    } else {
        (&s.ally_pick, &s.enemy_pick, &s.ally_ban, &s.enemy_ban)
    };

    let mut taken: HashSet<&str> = HashSet::new();
    for v in [&s.ally_pick, &s.enemy_pick, &s.ally_ban, &s.enemy_ban] {
        for id in v {
            taken.insert(id.as_str());
        }
    }
    let allies: Vec<&str> = my_pick.iter().map(|x| x.as_str()).collect();
    let enemies: Vec<&str> = opp_pick.iter().map(|x| x.as_str()).collect();

    // assign my picks to lanes: primary lane if open, else any playable, else any.
    let mut filled: [Option<&str>; 5] = [None; 5];
    for pick in &allies {
        let mut placed = false;
        if let Some(p) = tables.primary(pick) {
            if filled[p as usize].is_none() {
                filled[p as usize] = Some(pick);
                placed = true;
            }
        }
        if !placed {
            if let Some(ls) = tables.pos_play.get(*pick) {
                for &l in ls {
                    if filled[l as usize].is_none() {
                        filled[l as usize] = Some(pick);
                        placed = true;
                        break;
                    }
                }
            }
        }
        if !placed {
            for slot in filled.iter_mut() {
                if slot.is_none() {
                    *slot = Some(pick);
                    break;
                }
            }
        }
    }

    let picks_made = allies.len();
    let early = picks_made <= 1;
    let late = picks_made >= 3;
    let (timing, tip) = if early {
        ("선픽", "플렉스·강픽 우선 (라인 안 들키게)")
    } else if late {
        ("후픽", "카운터 우선 (상대 드러난 라인 받아치기)")
    } else {
        ("중반", "시너지·조합 채우기")
    };

    let mut lanes: Vec<Lane> = Vec::with_capacity(5);
    let mut best_open: Option<(usize, f32)> = None;
    for lane in 0u8..5 {
        if let Some(p) = filled[lane as usize] {
            lanes.push(Lane {
                name: LANES[lane as usize],
                filled: Some(ko_name(p)),
                cands: vec![],
                counter_of: None,
                star: false,
            });
            continue;
        }
        // open lane → rank champs that can play it
        let mut cands: Vec<LaneCand> = Vec::new();
        for (id, _ko) in CHAMPS {
            if taken.contains(id) {
                continue;
            }
            if let Some(pl) = pool {
                if !pl.contains(*id) {
                    continue;
                }
            }
            if !tables.plays(id, lane) {
                continue;
            }
            let (mut score, label) = tables.score_pick_in(id, &allies, &enemies, Some(lane));
            if early {
                score += W_FLEX * (tables.flex(id).saturating_sub(1).min(2) as f32);
            }
            cands.push(LaneCand {
                ko: ko_name(id),
                id: id.to_string(),
                score,
                label,
                // low-confidence flag is now per-LANE: a champ with few games IN THIS
                // lane is flagged "?", even if it has many games in its main lane.
                low_conf: tables.low_conf_for(id, Some(lane)),
                role: tables.role_tag(id),
                tier: tables.tier_of(id, Some(lane)),
            });
        }
        cands.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        cands.truncate(3);

        // counter flag: our top cand vs an enemy that can also play this lane
        let mut counter_of = None;
        if let Some(top) = cands.first() {
            let mut best = 0.03f32;
            for e in &enemies {
                if tables.plays(e, lane) {
                    let adv = tables.counters(&top.id, e);
                    if adv > best {
                        best = adv;
                        counter_of = Some(ko_name(e));
                    }
                }
            }
        }

        if let Some(top) = cands.first() {
            let better = best_open.map(|(_, bs)| top.score > bs).unwrap_or(true);
            if better {
                best_open = Some((lanes.len(), top.score));
            }
        }
        lanes.push(Lane { name: LANES[lane as usize], filled: None, cands, counter_of, star: false });
    }
    if let Some((vi, _)) = best_open {
        lanes[vi].star = true;
    }

    let ko_list = |ids: &[String]| ids.iter().map(|id| ko_name(id)).collect::<Vec<_>>();
    LaneView {
        explore: s.explore,
        age_ms: now_ms().saturating_sub(s.ts),
        my_pick: ko_list(my_pick),
        opp_pick: ko_list(opp_pick),
        my_ban: ko_list(my_ban),
        opp_ban: ko_list(opp_ban),
        lanes,
        picks_made,
        timing,
        tip,
        matches: tables.matches,
        pool_size: pool.map(|p| p.len()).unwrap_or(0),
        has_pos: tables.has_pos(),
    }
}

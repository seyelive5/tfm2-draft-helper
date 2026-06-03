//! draft-overlay — injected OpenGL overlay: a manual-input ban/pick RECOMMENDER for TFM2.
//!
//! No game mod. Click the 60-champ grid (or the recommendation rows) to mark
//! my-pick / enemy-pick / my-ban / enemy-ban; it instantly ranks the rest.
//! Stats + the per-save champion pool come from C:\tmp\tfm2-overlay\draft_weights.tsv
//! (written by inject-overlay.exe, auto-reloaded when it changes). Ctrl+F10 toggles.

mod engine;

use engine::{recommend, recommend_lanes, DraftState, Tables, CHAMPS};
use hudhook::hooks::opengl3::ImguiOpenGl3Hooks;
use hudhook::imgui::*;
use hudhook::{hudhook, ImguiRenderLoop, MessageFilter, RenderContext};
use std::collections::HashSet;
use std::time::SystemTime;

const FONT_PATH: &str = "C:\\Windows\\Fonts\\malgun.ttf";
const COLS: usize = 6;

/// Shared data dir = %LOCALAPPDATA%\tfm2-overlay — user-agnostic; the mod, the launcher,
/// and this injected overlay all read/write here. Falls back to C:\tmp if env missing.
fn shared(name: &str) -> std::path::PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| "C:\\tmp".into());
    std::path::PathBuf::from(base).join("tfm2-overlay").join(name)
}
const BTN_W: f32 = 94.0;
const RELOAD_EVERY_FRAMES: u64 = 120;

// palette
const ACCENT: [f32; 4] = [0.32, 0.84, 0.80, 1.0]; // teal
const GOLD: [f32; 4] = [0.99, 0.83, 0.38, 1.0];
const GREEN: [f32; 4] = [0.46, 0.92, 0.56, 1.0];
const RED: [f32; 4] = [0.98, 0.46, 0.46, 1.0];
const WHITE: [f32; 4] = [0.88, 0.90, 0.93, 1.0];
const GREY: [f32; 4] = [0.58, 0.62, 0.68, 1.0];

/// Meta-tier color (matches report.html): S red · A orange · B yellow · C green · D grey.
fn tier_color(t: &str) -> [f32; 4] {
    match t {
        "S" => [1.0, 0.36, 0.36, 1.0],
        "A" => [1.0, 0.62, 0.26, 1.0],
        "B" => [1.0, 0.79, 0.34, 1.0],
        "C" => [0.56, 0.82, 0.31, 1.0],
        "D" => [0.55, 0.60, 0.68, 1.0],
        _ => WHITE,
    }
}

/// Marking-mode label + a distinct color (so the current click target is obvious).
fn mode_badge(m: Mode) -> (&'static str, [f32; 4]) {
    match m {
        Mode::MyPick => ("내 픽", [0.36, 0.85, 0.50, 1.0]),
        Mode::EnemyPick => ("상대 픽", [0.95, 0.45, 0.45, 1.0]),
        Mode::MyBan => ("내 밴", [0.92, 0.80, 0.38, 1.0]),
        Mode::EnemyBan => ("상대 밴", [0.82, 0.50, 0.86, 1.0]),
        Mode::Clear => ("지움", GREY),
        Mode::Pool => ("풀 편집", ACCENT),
    }
}

#[link(name = "user32")]
extern "system" {
    fn GetAsyncKeyState(v_key: i32) -> i16;
}
const VK_CONTROL: i32 = 0x11;
const VK_F10: i32 = 0x79;
fn key_down(vk: i32) -> bool {
    (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
}

fn tsv_mtime() -> Option<SystemTime> {
    std::fs::metadata(shared("draft_weights.tsv")).and_then(|m| m.modified()).ok()
}

/// (ui_scale, btn_w, bg_alpha) — appearance settings, persisted to settings.txt.
fn load_settings() -> (f32, f32, f32) {
    let (mut us, mut bw, mut ba) = (1.0f32, BTN_W, 0.97f32);
    for line in std::fs::read_to_string(shared("settings.txt")).unwrap_or_default().lines() {
        if let Some((k, v)) = line.split_once('=') {
            let val: f32 = v.trim().parse().unwrap_or(f32::NAN);
            if val.is_finite() {
                match k.trim() {
                    "ui_scale" => us = val,
                    "btn_w" => bw = val,
                    "bg_alpha" => ba = val,
                    _ => {}
                }
            }
        }
    }
    (us.clamp(0.6, 2.5), bw.clamp(50.0, 200.0), ba.clamp(0.25, 1.0))
}

fn save_settings(us: f32, bw: f32, ba: f32) {
    let _ = std::fs::write(shared("settings.txt"), format!("ui_scale={}\nbtn_w={}\nbg_alpha={}\n", us, bw, ba));
}


/// "save_20260528_004140.data" -> "05-28 00:41"
fn fmt_date(filename: &str) -> String {
    let s = filename.strip_prefix("save_").unwrap_or(filename);
    let s = s.strip_suffix(".data").unwrap_or(s);
    if let Some((date, time)) = s.split_once('_') {
        if date.len() == 8 && time.len() >= 4 {
            return format!("{}-{} {}:{}", &date[4..6], &date[6..8], &time[0..2], &time[2..4]);
        }
    }
    s.to_string()
}

/// saves.tsv (filename<TAB>count<TAB>sizeMB<TAB>team<TAB>manager) → [(filename, label)].
/// Display leads with the player's MANAGER name + team (from the save header) so the
/// user instantly tells careers apart: "asdf (Gen.G) · 05-31 20:35 · 25MB · 8판".
fn load_saves() -> Vec<(String, String)> {
    std::fs::read_to_string(shared("saves.tsv"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split('\t');
            let f = it.next()?.to_string();
            if f.is_empty() {
                return None;
            }
            let count = it.next().unwrap_or("?");
            let mb = it.next().unwrap_or("?");
            let team = it.next().unwrap_or("").trim();
            let manager = it.next().unwrap_or("").trim();
            let cnt = if count == "?" { String::new() } else { format!("  ·  {}판", count) };
            let who = if !manager.is_empty() {
                if team.is_empty() { manager.to_string() } else { format!("{} ({})", manager, team) }
            } else {
                team.to_string()
            };
            let who_p = if who.is_empty() { String::new() } else { format!("{}  ·  ", who) };
            Some((f.clone(), format!("{}{}  ·  {}MB{}", who_p, fmt_date(&f), mb, cnt)))
        })
        .collect()
}

fn load_pin() -> String {
    std::fs::read_to_string(shared("pin.txt")).unwrap_or_default().trim().to_string()
}

fn save_pin(name: &str) {
    let _ = std::fs::write(shared("pin.txt"), name);
}

/// combo index for the current pin (0 = auto/newest).
fn sel_from_pin(saves: &[(String, String)], pin: &str) -> usize {
    if pin.is_empty() {
        return 0;
    }
    saves.iter().position(|(f, _)| f == pin).map(|i| i + 1).unwrap_or(0)
}

fn load_tables() -> (Tables, String) {
    match std::fs::read_to_string(shared("draft_weights.tsv")) {
        // Use the launcher-written stats whenever they belong to a real SAVE — even a
        // brand-new game with ZERO champ-winrate (C) rows (= cold-start). Keying on a
        // `C` row was the bug: a fresh game has no C rows, so the overlay fell back to
        // the BAKED table and wrongly showed its old 901-match stats for a new career.
        Ok(s) if s.lines().any(|l| l.starts_with("SAVE\t")) => {
            let champs = s.lines().filter(|l| l.starts_with("C\t")).count();
            let src = if champs == 0 {
                "새 세이브 · 데이터 없음(콜드스타트)".to_string()
            } else {
                format!("{}챔프", champs)
            };
            (engine::parse_tables_from_str(&s), src)
        }
        _ => (engine::parse_tables(), "내장 기본값".to_string()),
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    MyPick,
    EnemyPick,
    MyBan,
    EnemyBan,
    Clear,
    Pool,
}

#[derive(PartialEq, Clone, Copy)]
enum Slot {
    Avail,
    MyPick,
    EnemyPick,
    MyBan,
    EnemyBan,
}

/// Series ban/pick rule (TFM2 banpick_style). Champions PICKED in earlier sets
/// get locked out of later sets:
///   Off    = classic (no carry-over).
///   Normal = 피어리스: each team can't repeat ITS OWN prior picks (per-team).
///   Hard   = 하드 피어리스: neither team can pick anything EITHER team used.
#[derive(PartialEq, Clone, Copy)]
enum Fearless {
    Off,
    Normal,
    Hard,
}

fn slot_colors(s: Slot) -> [[f32; 4]; 3] {
    match s {
        Slot::Avail => [[0.16, 0.18, 0.23, 1.0], [0.27, 0.33, 0.42, 1.0], [0.32, 0.40, 0.50, 1.0]],
        Slot::MyPick => [[0.14, 0.45, 0.24, 1.0], [0.19, 0.58, 0.30, 1.0], [0.23, 0.66, 0.34, 1.0]],
        Slot::EnemyPick => [[0.52, 0.17, 0.19, 1.0], [0.66, 0.23, 0.25, 1.0], [0.74, 0.27, 0.29, 1.0]],
        Slot::MyBan => [[0.30, 0.27, 0.16, 1.0], [0.40, 0.36, 0.22, 1.0], [0.46, 0.41, 0.26, 1.0]],
        Slot::EnemyBan => [[0.34, 0.20, 0.30, 1.0], [0.44, 0.26, 0.40, 1.0], [0.50, 0.30, 0.46, 1.0]],
    }
}

// Korean initial-consonant (초성) quick-filter — lets the user find champs by
// CLICKING instead of typing Korean (the injected overlay can't take Korean IME
// input). 14 base groups; the 5 tense consonants fold into their plain group.
const CHOSUNG: [&str; 14] =
    ["ㄱ", "ㄴ", "ㄷ", "ㄹ", "ㅁ", "ㅂ", "ㅅ", "ㅇ", "ㅈ", "ㅊ", "ㅋ", "ㅌ", "ㅍ", "ㅎ"];

/// 초성 group (0..14) of a Korean name's first syllable; None if not Hangul.
fn chosung_group(name: &str) -> Option<u8> {
    let s = name.chars().next()? as u32;
    if !(0xAC00..=0xD7A3).contains(&s) {
        return None;
    }
    // 19 leading-jamo index → 14 base groups (ㄲ→ㄱ, ㄸ→ㄷ, ㅃ→ㅂ, ㅆ→ㅅ, ㅉ→ㅈ)
    const MAP: [u8; 19] = [0, 0, 1, 2, 2, 3, 4, 5, 5, 6, 6, 7, 8, 8, 9, 10, 11, 12, 13];
    Some(MAP[((s - 0xAC00) / 588) as usize])
}

fn apply_mode(cur: Slot, mode: Mode) -> Slot {
    let target = match mode {
        Mode::MyPick => Slot::MyPick,
        Mode::EnemyPick => Slot::EnemyPick,
        Mode::MyBan => Slot::MyBan,
        Mode::EnemyBan => Slot::EnemyBan,
        Mode::Clear => return Slot::Avail,
        Mode::Pool => return cur,
    };
    if cur == target {
        Slot::Avail
    } else {
        target
    }
}

struct Overlay {
    tables: Tables,
    tsv_source: String,
    tsv_mtime: Option<SystemTime>,
    pool: HashSet<String>,
    slots: Vec<Slot>,
    mode: Mode,
    rec_ban: bool,
    search: String,
    saves: Vec<(String, String)>,
    sel_idx: usize,
    ui_scale: f32,
    btn_w: f32,
    bg_alpha: f32,
    show_settings: bool,
    visible: bool,
    prev_combo: bool,
    font_loaded: bool,
    frames: u64,
    last_reload_frame: u64,
    // fearless series tracking
    fearless: Fearless,
    series_my: HashSet<String>,    // champs MY team picked in earlier sets
    series_enemy: HashSet<String>, // champs ENEMY picked in earlier sets
    series_game: u32,              // current set number (1-based)
    chosung: Option<u8>,           // active 초성 grid filter (None = all)
}

impl Overlay {
    fn new() -> Self {
        let (tables, tsv_source) = load_tables();
        let pool = tables.pool.clone();
        let (ui_scale, btn_w, bg_alpha) = load_settings();
        let saves = load_saves();
        let sel_idx = sel_from_pin(&saves, &load_pin());
        Overlay {
            tables,
            tsv_source,
            tsv_mtime: tsv_mtime(),
            pool,
            slots: vec![Slot::Avail; CHAMPS.len()],
            mode: Mode::MyBan,   // draft starts with bans
            rec_ban: true,        // …so recommend bans first
            search: String::new(),
            saves,
            sel_idx,
            ui_scale,
            btn_w,
            bg_alpha,
            show_settings: false,
            visible: true,
            prev_combo: false,
            font_loaded: false,
            frames: 0,
            last_reload_frame: 0,
            fearless: Fearless::Off,
            series_my: HashSet::new(),
            series_enemy: HashSet::new(),
            series_game: 1,
            chosung: None,
        }
    }

    fn current_state(&self) -> DraftState {
        let mut st = DraftState::default();
        st.ok = true;
        st.phase = if self.rec_ban { "BAN".into() } else { "PICK".into() };
        for (i, (id, _ko)) in CHAMPS.iter().enumerate() {
            match self.slots[i] {
                Slot::MyPick => st.ally_pick.push(id.to_string()),
                Slot::EnemyPick => st.enemy_pick.push(id.to_string()),
                Slot::MyBan => st.ally_ban.push(id.to_string()),
                Slot::EnemyBan => st.enemy_ban.push(id.to_string()),
                Slot::Avail => {}
            }
        }
        st
    }
}

/// Apply a click on champion index `i` according to the current mode.
fn click_champ(i: usize, mode: Mode, slots: &mut [Slot], pool: &mut HashSet<String>) {
    let id = CHAMPS[i].0;
    if mode == Mode::Pool {
        if pool.contains(id) {
            pool.remove(id);
        } else {
            pool.insert(id.to_string());
        }
    } else {
        slots[i] = apply_mode(slots[i], mode);
    }
}

fn set_theme(ctx: &mut Context) {
    let s = ctx.style_mut();
    s.window_rounding = 8.0;
    s.child_rounding = 6.0;
    s.frame_rounding = 6.0;
    s.grab_rounding = 5.0;
    s.popup_rounding = 6.0;
    s.window_padding = [14.0, 12.0];
    s.frame_padding = [9.0, 5.0];
    s.item_spacing = [8.0, 7.0];
    s.item_inner_spacing = [7.0, 5.0];
    s.window_border_size = 0.0;
    s.frame_border_size = 0.0;
    s.scrollbar_rounding = 8.0;
    let c = &mut s.colors;
    c[StyleColor::WindowBg as usize] = [0.065, 0.075, 0.10, 0.97];
    c[StyleColor::ChildBg as usize] = [0.09, 0.10, 0.135, 0.6];
    c[StyleColor::Text as usize] = [0.88, 0.90, 0.93, 1.0];
    c[StyleColor::TextDisabled as usize] = [0.50, 0.54, 0.60, 1.0];
    c[StyleColor::TitleBg as usize] = [0.065, 0.075, 0.10, 1.0];
    c[StyleColor::TitleBgActive as usize] = [0.10, 0.30, 0.31, 1.0];
    c[StyleColor::FrameBg as usize] = [0.14, 0.16, 0.21, 1.0];
    c[StyleColor::FrameBgHovered as usize] = [0.18, 0.28, 0.34, 1.0];
    c[StyleColor::FrameBgActive as usize] = [0.20, 0.40, 0.44, 1.0];
    c[StyleColor::Button as usize] = [0.16, 0.18, 0.23, 1.0];
    c[StyleColor::ButtonHovered as usize] = [0.20, 0.42, 0.46, 1.0];
    c[StyleColor::ButtonActive as usize] = [0.24, 0.52, 0.56, 1.0];
    c[StyleColor::Header as usize] = [0.16, 0.30, 0.33, 0.55];
    c[StyleColor::HeaderHovered as usize] = [0.20, 0.44, 0.48, 0.85];
    c[StyleColor::HeaderActive as usize] = [0.24, 0.54, 0.58, 1.0];
    c[StyleColor::CheckMark as usize] = ACCENT;
    c[StyleColor::SliderGrab as usize] = ACCENT;
    c[StyleColor::Separator as usize] = [0.20, 0.24, 0.30, 1.0];
    c[StyleColor::ScrollbarBg as usize] = [0.065, 0.075, 0.10, 0.6];
    c[StyleColor::ScrollbarGrab as usize] = [0.22, 0.26, 0.32, 1.0];
}

impl ImguiRenderLoop for Overlay {
    fn initialize<'a>(&'a mut self, ctx: &mut Context, _render_context: &'a mut dyn RenderContext) {
        if let Ok(ttf) = std::fs::read(FONT_PATH) {
            let cfg = FontConfig {
                size_pixels: 19.0,
                glyph_ranges: FontGlyphRanges::korean(),
                oversample_h: 1,
                oversample_v: 1,
                ..FontConfig::default()
            };
            ctx.fonts().add_font(&[FontSource::TtfData {
                data: &ttf,
                size_pixels: 19.0,
                config: Some(cfg),
            }]);
            self.font_loaded = true;
        }
        set_theme(ctx);
    }

    fn message_filter(&self, io: &Io) -> MessageFilter {
        if !self.visible {
            return MessageFilter::empty();
        }
        let mut f = MessageFilter::empty();
        if io.want_capture_mouse {
            f |= MessageFilter::InputMouse;
        }
        if io.want_capture_keyboard {
            f |= MessageFilter::InputKeyboard;
        }
        f
    }

    fn render(&mut self, ui: &mut Ui) {
        let combo = key_down(VK_CONTROL) && key_down(VK_F10);
        if combo && !self.prev_combo {
            self.visible = !self.visible;
        }
        self.prev_combo = combo;
        if !self.visible {
            return;
        }

        self.frames += 1;
        if self.frames - self.last_reload_frame >= RELOAD_EVERY_FRAMES {
            self.last_reload_frame = self.frames;
            // refresh the save list + which one is selected (pin)
            self.saves = load_saves();
            self.sel_idx = sel_from_pin(&self.saves, &load_pin());
            let m = tsv_mtime();
            if m != self.tsv_mtime {
                self.tsv_mtime = m;
                let (t, src) = load_tables();
                self.tables = t;
                self.tsv_source = src;
                self.pool = self.tables.pool.clone();
            }
        }

        // fearless: champions I may NOT pick this set (Normal=my prior picks; Hard=either
        // team's). Used for the grid tint + the lock label.
        let my_locked: HashSet<String> = match self.fearless {
            Fearless::Off => HashSet::new(),
            Fearless::Normal => self.series_my.clone(),
            Fearless::Hard => self.series_my.iter().chain(self.series_enemy.iter()).cloned().collect(),
        };
        // champions the ENEMY may NOT pick this set. Banning one is pointless (they can't
        // pick it anyway), so BAN recommendations exclude this set instead of mine.
        let enemy_locked: HashSet<String> = match self.fearless {
            Fearless::Off => HashSet::new(),
            Fearless::Normal => self.series_enemy.clone(),
            Fearless::Hard => self.series_my.iter().chain(self.series_enemy.iter()).cloned().collect(),
        };
        // PHASE-aware: pick recs drop what I can't pick; ban recs drop what the enemy
        // can't pick (so we never suggest banning an already-used, unpickable champ — and
        // in Normal fearless we DO still suggest banning MY used champs to deny the enemy).
        let rec_lock: &HashSet<String> = if self.rec_ban { &enemy_locked } else { &my_locked };
        let restrict = !self.pool.is_empty() || !rec_lock.is_empty();
        let effective: HashSet<String> = if !restrict {
            HashSet::new()
        } else if self.pool.is_empty() {
            CHAMPS.iter().map(|(id, _)| id.to_string()).filter(|id| !rec_lock.contains(id)).collect()
        } else {
            self.pool.iter().filter(|id| !rec_lock.contains(*id)).cloned().collect()
        };
        let pool_opt = if restrict { Some(&effective) } else { None };
        let view = recommend(&self.tables, &self.current_state(), false, pool_opt);
        // PICK phase → lane board (one slot per position); BAN phase stays a list.
        let lane_view = if self.rec_ban {
            None
        } else {
            Some(recommend_lanes(&self.tables, &self.current_state(), false, pool_opt))
        };
        let font_loaded = self.font_loaded;
        let tsv_source = self.tsv_source.clone();
        let conf = self.tables.confidence();
        let matches = self.tables.matches;
        let save_name = self.tables.save_name.clone();
        let pool_n = self.pool.len();
        let auto_pool = self.tables.pool.clone();
        // per-champ meta tier (overall), index-aligned with CHAMPS — shown on every
        // grid button so the draft grid doubles as a live tier table.
        let grid_tiers: Vec<&'static str> = CHAMPS.iter().map(|(id, _)| self.tables.tier_of(id, None)).collect();
        let mut mode = self.mode;
        let mut rec_ban = self.rec_ban;
        let mut slots = self.slots.clone();
        let mut pool = self.pool.clone();
        let mut search = self.search.clone();
        let mut chosung = self.chosung;
        let saves = self.saves.clone();
        let mut sel_idx = self.sel_idx;
        let mut new_pin: Option<String> = None;
        let mut ui_scale = self.ui_scale;
        let mut btn_w = self.btn_w;
        let mut bg_alpha = self.bg_alpha;
        let mut show_settings = self.show_settings;
        let mut reset = false;
        let mut do_refresh = false;
        // fearless UI state + display
        let mut fearless = self.fearless;
        let mut next_set = false;
        let mut clear_series = false;
        let locked_me = my_locked.clone(); // champs I can't pick this set (grid tint)
        let my_locked_n = my_locked.len();
        let enemy_locked_n = match self.fearless {
            Fearless::Off => 0,
            Fearless::Normal => self.series_enemy.len(),
            Fearless::Hard => my_locked.len(),
        };
        let series_game = self.series_game;

        ui.window("TFM2 드래프트 추천기")
            .size([600.0, 880.0], Condition::FirstUseEver)
            .position([24.0, 24.0], Condition::FirstUseEver)
            .bg_alpha(bg_alpha)
            .build(|| {
                ui.set_window_font_scale(ui_scale);
                if !font_loaded {
                    ui.text_colored(RED, "malgun.ttf load failed");
                }

                // ── header: SAVE PICKER (newest is not always the one you loaded) ──
                ui.text_colored(GREY, "세이브");
                ui.same_line();
                ui.set_next_item_width(248.0);
                let mut items: Vec<String> = Vec::with_capacity(saves.len() + 1);
                items.push("자동 (최신 세이브)".to_string());
                for (_f, disp) in &saves {
                    items.push(disp.clone());
                }
                if ui.combo("##save", &mut sel_idx, &items, |s| std::borrow::Cow::Borrowed(s.as_str())) {
                    new_pin = Some(if sel_idx == 0 {
                        String::new()
                    } else {
                        saves.get(sel_idx - 1).map(|(f, _)| f.clone()).unwrap_or_default()
                    });
                }
                let conf_label = if matches == 0 {
                    "데이터 없음 · 이론 추천".to_string()
                } else if conf < 1.0 {
                    format!("현재패치 {}판 · 이론 {}%", matches, ((1.0 - conf) * 100.0) as i32)
                } else {
                    format!("현재패치 {}판 · 통계", matches)
                };
                let pool_label = if pool_n == 0 { "풀 전체".into() } else { format!("풀 {}/60", pool_n) };
                let loaded = if save_name.is_empty() { tsv_source.clone() } else { save_name.clone() };
                ui.text_colored(GREY, format!("로드됨 {}", loaded));
                ui.text_colored(GREY, format!("{}  ·  {}  ·  Ctrl+F10", conf_label, pool_label));
                ui.separator();

                // ── phase (big) + mode ───────────────────────────────────────
                ui.text_colored(WHITE, "단계");
                ui.same_line();
                ui.radio_button("밴", &mut rec_ban, true);
                ui.same_line();
                ui.radio_button("픽", &mut rec_ban, false);
                // marking options follow the PHASE: ban phase shows only ban-marks, pick
                // phase only pick-marks (+ 지움/풀 always). Keep the mode valid for the phase.
                if rec_ban && (mode == Mode::MyPick || mode == Mode::EnemyPick) {
                    mode = Mode::MyBan;
                } else if !rec_ban && (mode == Mode::MyBan || mode == Mode::EnemyBan) {
                    mode = Mode::MyPick;
                }
                ui.text_colored(GREY, "마킹");
                ui.same_line();
                if rec_ban {
                    ui.radio_button("내 밴", &mut mode, Mode::MyBan);
                    ui.same_line();
                    ui.radio_button("상대 밴", &mut mode, Mode::EnemyBan);
                } else {
                    ui.radio_button("내 픽", &mut mode, Mode::MyPick);
                    ui.same_line();
                    ui.radio_button("상대 픽", &mut mode, Mode::EnemyPick);
                }
                ui.same_line();
                ui.radio_button("지움", &mut mode, Mode::Clear);
                ui.same_line();
                ui.radio_button("풀", &mut mode, Mode::Pool);
                // make the current click target unmistakable (prevents mis-marking)
                ui.same_line();
                let (mlabel, mcol) = mode_badge(mode);
                ui.text_colored(mcol, format!("« 클릭 = {}", mlabel));

                if ui.button("픽/밴 초기화") {
                    reset = true;
                }
                ui.same_line();
                if ui.button("통계 새로고침") {
                    do_refresh = true;
                }
                ui.same_line();
                if ui.button("설정") {
                    show_settings = !show_settings;
                }
                if mode == Mode::Pool {
                    ui.same_line();
                    if ui.button("풀 전체") {
                        pool = CHAMPS.iter().map(|(id, _)| id.to_string()).collect();
                    }
                    ui.same_line();
                    if ui.button("자동") {
                        pool = auto_pool.clone();
                    }
                }

                // ── fearless (series) rule ───────────────────────────────────
                ui.text_colored(GREY, "피어리스");
                ui.same_line();
                ui.radio_button("없음", &mut fearless, Fearless::Off);
                ui.same_line();
                ui.radio_button("피어리스", &mut fearless, Fearless::Normal);
                ui.same_line();
                ui.radio_button("하드", &mut fearless, Fearless::Hard);
                if fearless != Fearless::Off {
                    ui.same_line();
                    ui.text_colored(ACCENT, format!("{}세트", series_game));
                    ui.same_line();
                    if ui.button("다음 세트 »") {
                        next_set = true;
                    }
                    ui.same_line();
                    if ui.button("시리즈 초기화") {
                        clear_series = true;
                    }
                    let lock_label = if fearless == Fearless::Hard {
                        format!("금지(공통) {}", my_locked_n)
                    } else {
                        format!("금지 내 {}  ·  상대 {}", my_locked_n, enemy_locked_n)
                    };
                    ui.text_colored(GREY, format!("이전 세트 사용 챔프 » 추천·그리드 제외  ·  {}", lock_label));
                }
                ui.separator();

                // ── current board ────────────────────────────────────────────
                let join = |v: &[String]| if v.is_empty() { "-".to_string() } else { v.join(", ") };
                // compact board: parallel 픽 / 밴 rows, each 내 · 상대 (was 3 lines → 2)
                ui.text_colored(GREY, "픽  내 ");
                ui.same_line();
                ui.text_colored(GREEN, join(&view.my_pick));
                ui.same_line();
                ui.text_colored(GREY, " · 상대 ");
                ui.same_line();
                ui.text_colored(RED, join(&view.opp_pick));
                ui.text_colored(GREY, "밴  내 ");
                ui.same_line();
                ui.text_colored(WHITE, join(&view.my_ban));
                ui.same_line();
                ui.text_colored(GREY, " · 상대 ");
                ui.same_line();
                ui.text_colored(WHITE, join(&view.opp_ban));
                ui.separator();

                // ── recommendations ──────────────────────────────────────────
                if let Some(lv) = lane_view.as_ref().filter(|lv| lv.has_pos) {
                    // PICK: lane board — one slot per position, click a champ = my pick
                    // ▶ prominent #1 recommendation = the starred lane's top candidate
                    if let Some((tier, role, name, lane_name, label)) = lv
                        .lanes
                        .iter()
                        .find(|l| l.star)
                        .and_then(|l| l.cands.first().map(|c| (c.tier, c.role, c.ko.clone(), l.name, c.label)))
                    {
                        let badge = if tier.is_empty() { String::new() } else { format!("[{}] ", tier) };
                        let rsp = if role.is_empty() { String::new() } else { format!("{} ", role) };
                        ui.text_colored(tier_color(tier), format!("» 지금 추천: {}{}{}  ·  {} 라인  ({})", badge, rsp, name, lane_name, label));
                        ui.spacing();
                    }
                    ui.text_colored(ACCENT, "추천 픽 (라인별)");
                    ui.same_line();
                    ui.text_colored(GREY, format!("{} (내픽 {}/5) · {}", lv.timing, lv.picks_made, lv.tip));
                    ui.same_line();
                    ui.text_colored(GREY, "?=표본부족 · 힐·탱·브·딜=데이터 역할 · »=추천 라인");
                    ui.spacing();
                    for lane in &lv.lanes {
                        if let Some(champ) = &lane.filled {
                            ui.text_colored(GREY, lane.name);
                            ui.same_line();
                            ui.text_colored(GREEN, format!("[내픽] {}", champ));
                            continue;
                        }
                        let lane_col = if lane.star { ACCENT } else { WHITE };
                        ui.text_colored(lane_col, format!("{}{}", if lane.star { "» " } else { "   " }, lane.name));
                        for (ci, c) in lane.cands.iter().enumerate() {
                            ui.same_line();
                            // button text colored by META TIER (S red…D grey); order still = rank
                            let _tc = ui.push_style_color(StyleColor::Text, tier_color(c.tier));
                            let rp = if c.role.is_empty() { String::new() } else { format!("{} ", c.role) };
                            if ui.button(format!("{}{} {:+.0}{}##L{}_{}", rp, c.ko, c.score * 100.0, if c.low_conf { "?" } else { "" }, lane.name, c.id)) {
                                if let Some(idx) = CHAMPS.iter().position(|(cid, _)| *cid == c.id) {
                                    slots[idx] = apply_mode(slots[idx], Mode::MyPick);
                                }
                            }
                        }
                        if lane.cands.is_empty() {
                            ui.same_line();
                            ui.text_colored(GREY, "후보 없음");
                        }
                        if let Some(cof) = &lane.counter_of {
                            ui.same_line();
                            ui.text_colored(GOLD, format!("[카운터:{}]", cof));
                        }
                    }
                } else {
                    // BAN (or no position data): flat ranked list — click a row to mark
                    // ▶ prominent #1 recommendation
                    if let Some(r) = view.rows.first() {
                        let badge = if r.tier.is_empty() { String::new() } else { format!("[{}] ", r.tier) };
                        let rsp = if r.role.is_empty() { String::new() } else { format!("{} ", r.role) };
                        let what = if view.phase_ban { "추천 밴" } else { "추천 픽" };
                        ui.text_colored(tier_color(r.tier), format!("» 지금 {}: {}{}{}  ({})", what, badge, rsp, r.ko, r.label));
                        ui.spacing();
                    }
                    let head = if view.phase_ban { "추천 밴" } else { "추천 픽" };
                    ui.text_colored(ACCENT, format!("{}  ", head));
                    ui.same_line();
                    ui.text_colored(GREY, "행 클릭=마킹 · ?=표본부족 · 힐·탱·브·딜=데이터 역할");
                    ui.spacing();
                    for (i, r) in view.rows.iter().enumerate() {
                        // each row colored by its META TIER (S red … D grey)
                        let _t = ui.push_style_color(StyleColor::Text, tier_color(r.tier));
                        let mark = if r.low_conf { " ?" } else { "" };
                        let rp = if r.role.is_empty() { "  ".to_string() } else { format!("{} ", r.role) };
                        let label = format!("{:>2}. {}{:<11}{:+5.0}   {}{}", i + 1, rp, r.ko, r.score * 100.0, r.label, mark);
                        if ui.selectable(label) {
                            if let Some(idx) = CHAMPS.iter().position(|(cid, _)| *cid == r.id) {
                                click_champ(idx, mode, &mut slots, &mut pool);
                            }
                        }
                    }
                }
                ui.separator();

                // ── champion grid (search: 초성 click + English id) ──────────
                // 초성 quick-filter — click instead of typing Korean (the overlay
                // can't take Korean IME input). The text box matches the English id.
                ui.text_colored(GREY, "초성");
                for gi in 0..CHOSUNG.len() {
                    if gi != 7 {
                        ui.same_line(); // wrap to a 2nd row at the halfway point
                    }
                    let active = chosung == Some(gi as u8);
                    let _c = active.then(|| ui.push_style_color(StyleColor::Button, ACCENT));
                    let _t = active.then(|| ui.push_style_color(StyleColor::Text, [0.06, 0.08, 0.10, 1.0]));
                    if ui.button_with_size(format!("{}##cs{}", CHOSUNG[gi], gi), [34.0, 0.0]) {
                        chosung = if active { None } else { Some(gi as u8) };
                    }
                }
                ui.same_line();
                if ui.button("전체##csall") {
                    chosung = None;
                }
                ui.text_colored(GREY, "검색(영문)");
                ui.same_line();
                ui.set_next_item_width(170.0);
                ui.input_text("##q", &mut search).build();
                ui.same_line();
                ui.text_colored(GREY, "예: monk, archer · 한글은 초성 · 챔프앞 S~D=현 패치 티어");
                if !search.is_empty() {
                    ui.same_line();
                    if ui.button("×") {
                        search.clear();
                    }
                }
                let q = search.to_lowercase();
                let mut shown = 0usize;
                for i in 0..CHAMPS.len() {
                    let (id, ko) = CHAMPS[i];
                    if !q.is_empty() && !ko.contains(search.as_str()) && !id.to_lowercase().contains(&q) {
                        continue;
                    }
                    if let Some(cs) = chosung {
                        if chosung_group(ko) != Some(cs) {
                            continue;
                        }
                    }
                    if shown % COLS != 0 {
                        ui.same_line();
                    }
                    shown += 1;
                    let out_of_pool = !pool.is_empty() && !pool.contains(id);
                    let mut c = slot_colors(slots[i]);
                    if out_of_pool {
                        for col in c.iter_mut() {
                            col[0] *= 0.35;
                            col[1] *= 0.35;
                            col[2] *= 0.35;
                        }
                    }
                    // fearless: champion used in an earlier set → I can't pick it (magenta)
                    if slots[i] == Slot::Avail && locked_me.contains(id) {
                        c = [[0.30, 0.13, 0.36, 1.0], [0.40, 0.18, 0.46, 1.0], [0.46, 0.22, 0.52, 1.0]];
                    }
                    let _b = ui.push_style_color(StyleColor::Button, c[0]);
                    let _h = ui.push_style_color(StyleColor::ButtonHovered, c[1]);
                    let _a = ui.push_style_color(StyleColor::ButtonActive, c[2]);
                    // tier prefix (S/A/B/C/D) → the grid reads as a live tier table
                    let tp = if grid_tiers[i].is_empty() { String::new() } else { format!("{}·", grid_tiers[i]) };
                    if ui.button_with_size(format!("{}{}##{}", tp, ko, i), [btn_w * ui_scale, 0.0]) {
                        click_champ(i, mode, &mut slots, &mut pool);
                    }
                }
            });

        // ── separate settings window ─────────────────────────────────────────
        if show_settings {
            ui.window("설정")
                .size([330.0, 0.0], Condition::FirstUseEver)
                .position([650.0, 40.0], Condition::FirstUseEver)
                .bg_alpha(bg_alpha)
                .build(|| {
                    ui.set_window_font_scale(ui_scale);
                    ui.text_colored(GREY, "모양 설정 (자동 저장)");
                    ui.spacing();
                    ui.text_colored(WHITE, "글씨 크기");
                    ui.set_next_item_width(230.0);
                    ui.slider("##fs", 0.7f32, 2.2f32, &mut ui_scale);
                    ui.text_colored(WHITE, "버튼 너비");
                    ui.set_next_item_width(230.0);
                    ui.slider("##bw", 60.0f32, 170.0f32, &mut btn_w);
                    ui.text_colored(WHITE, "배경 진하기");
                    ui.set_next_item_width(230.0);
                    ui.slider("##ba", 0.35f32, 1.0f32, &mut bg_alpha);
                    ui.spacing();
                    if ui.button("기본값으로") {
                        ui_scale = 1.0;
                        btn_w = BTN_W;
                        bg_alpha = 0.97;
                    }
                    ui.same_line();
                    if ui.button("닫기") {
                        show_settings = false;
                    }
                });
        }

        if reset {
            for s in &mut slots {
                *s = Slot::Avail;
            }
        }
        if next_set {
            // bank this set's PICKS into the series history, then clear the board.
            for (i, (id, _)) in CHAMPS.iter().enumerate() {
                match slots[i] {
                    Slot::MyPick => {
                        self.series_my.insert(id.to_string());
                    }
                    Slot::EnemyPick => {
                        self.series_enemy.insert(id.to_string());
                    }
                    _ => {}
                }
            }
            for s in &mut slots {
                *s = Slot::Avail;
            }
            self.series_game += 1;
        }
        if clear_series {
            self.series_my.clear();
            self.series_enemy.clear();
            self.series_game = 1;
        }
        self.fearless = fearless;
        self.mode = mode;
        self.rec_ban = rec_ban;
        self.slots = slots;
        self.pool = pool;
        self.search = search;
        self.chosung = chosung;
        self.sel_idx = sel_idx;
        if let Some(p) = new_pin {
            save_pin(&p); // launcher picks this up → regen for the chosen save
        }
        if (ui_scale, btn_w, bg_alpha) != (self.ui_scale, self.btn_w, self.bg_alpha) {
            save_settings(ui_scale, btn_w, bg_alpha);
        }
        self.ui_scale = ui_scale;
        self.btn_w = btn_w;
        self.bg_alpha = bg_alpha;
        self.show_settings = show_settings;
        if do_refresh {
            let (t, src) = load_tables();
            self.tables = t;
            self.tsv_source = src;
            self.tsv_mtime = tsv_mtime();
            self.pool = self.tables.pool.clone();
        }
    }
}

hudhook!(ImguiOpenGl3Hooks, Overlay::new());

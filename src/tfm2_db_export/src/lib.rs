//! tfm2_db_export — PRODUCTION stats exporter for the draft recommender.
//!
//! Replaces `tfm2_save_probe.exe` (a standalone save-FILE decoder whose source is
//! lost and which BROKE when game 0.4.7 changed the save format). Instead of
//! decoding the save off-line, this mod reads the LIVE `ClientDatabase` from
//! inside the running game and writes the exact files the launcher +
//! `draft-weights-gen` already consume. Robust to save-format changes — only needs
//! a rebuild when the mod API itself changes (rare).
//!
//! Writes (atomically) into the launcher's probe dir whenever match / patch / pool
//! counts change (≈ once per new match, patch update, or champion release):
//!   champion_patch_statistics.tsv → drives C(winrate)/POS(lanes)/B(banrate)  [CRITICAL]
//!   solo_rank_matches.debug.txt   → gen synergy/counter (+position)          [Rust-Debug]
//!   match_replays.debug.txt       → gen synergy/counter                       [Rust-Debug]
//!   pool.txt                      → overlay POOL (db.available_champions)
//!   _stamp.txt                    → "<solo>,<replay>,<patchTotal>,<pool>" (launcher regen trigger)
//! Also one-time auto-launches the overlay injector, so enabling this single mod
//! makes the whole recommender run. Read-only w.r.t. the game.

use mod_api::*;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// Shared dir = %LOCALAPPDATA%/tfm2-overlay (user-agnostic; the launcher + overlay use
/// the same). Falls back to C:/tmp if the env var is missing.
fn base() -> String {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| "C:/tmp".to_string());
    format!("{}/tfm2-overlay", local.replace('\\', "/"))
}

/// Sum of growing counts; a change means new data → re-export. usize::MAX = never written.
static LAST_SIG: AtomicUsize = AtomicUsize::new(usize::MAX);
/// Accumulated in-game ms since the last write, to throttle big dumps during fast sim.
static ACC_MS: AtomicU32 = AtomicU32::new(0);
static SPAWNED: AtomicBool = AtomicBool::new(false);
const THROTTLE_MS: u32 = 8000;

fn atomic_write(path: &str, body: &str) {
    let tmp = format!("{}.tmp", path);
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// `db.champion_patch_statistics` → the TSV the launcher (patch_current / compute_pos)
/// and the gen (banrate) parse. Column order MUST stay exactly as below; `{:?}` on the
/// position enum renders `Top`/`Jungle`/`Mid`/`Bottom`/`Support` (what compute_pos expects).
fn patch_tsv(db: &ClientDatabase) -> String {
    let mut s = String::with_capacity(64 * 1024);
    s.push_str("version\ttotal_match\tchampion\tposition\tbans\twins\tmatches\tdealing\ttanking\thealing\tkills\tdeaths\tcs\tgold\tdealing_line_phase\ttanking_line_phase\thealing_line_phase\tgold_line_phase\tcs_line_phase\n");
    for (ver, cps) in &db.champion_patch_statistics {
        for (champ, css) in &cps.data {
            for (pos, st) in &css.by_position {
                s.push_str(&format!(
                    "{}\t{}\t{}\t{:?}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    ver,
                    cps.total_match,
                    champ,
                    pos,
                    css.bans,
                    st.wins,
                    st.matches,
                    st.dealing,
                    st.tanking,
                    st.healing,
                    st.kills,
                    st.deaths,
                    st.cs,
                    st.gold,
                    st.dealing_line_phase,
                    st.tanking_line_phase,
                    st.healing_line_phase,
                    st.gold_line_phase,
                    st.cs_line_phase,
                ));
            }
        }
    }
    s
}

struct E;
impl ModExtension for E {
    fn pre_update(&self, scene: &mut Scene, _ui: &mut GameUI, _a: &mut Assets, dt: f32) {
        let Scene::InGame { data } = scene else {
            return;
        };

        let db = data.db();
        // early InGame frames have an empty db (still loading) — wait for it.
        if db.available_champions.is_empty() && db.athletes.is_empty() {
            return;
        }

        // cheap per-frame change signature (all O(1) / tiny). Counts only grow, so the
        // sum changes iff new data arrived (or a different save loaded).
        let solo = db.solo_rank_matches.len();
        let replay = db.match_replays.len();
        let pool_n = db.available_champions.len();
        let patch_total: usize = db.champion_patch_statistics.values().map(|c| c.total_match).sum();
        let sig = solo
            .wrapping_add(replay)
            .wrapping_add(patch_total)
            .wrapping_add(pool_n);

        let first = LAST_SIG.load(Ordering::Relaxed) == usize::MAX;
        if !first {
            if LAST_SIG.load(Ordering::Relaxed) == sig {
                return; // nothing changed
            }
            // changed — but throttle the (multi-MB) dump during fast season sim
            let ms = (dt * 1000.0) as u32;
            let acc = ACC_MS.fetch_add(ms, Ordering::Relaxed).saturating_add(ms);
            if acc < THROTTLE_MS {
                return;
            }
        }
        ACC_MS.store(0, Ordering::Relaxed);
        LAST_SIG.store(sig, Ordering::Relaxed);

        let b = base();
        let out = format!("{}/probe", b);
        let _ = std::fs::create_dir_all(&out);
        atomic_write(&format!("{}/champion_patch_statistics.tsv", out), &patch_tsv(&db));
        atomic_write(&format!("{}/solo_rank_matches.debug.txt", out), &format!("{:#?}", db.solo_rank_matches));
        atomic_write(&format!("{}/match_replays.debug.txt", out), &format!("{:#?}", db.match_replays));
        let mut pool: Vec<&str> = db.available_champions.iter().map(|s| s.as_str()).collect();
        pool.sort_unstable();
        atomic_write(&format!("{}/pool.txt", b), &pool.join(","));
        atomic_write(
            &format!("{}/_stamp.txt", out),
            &format!("{},{},{},{}", solo, replay, patch_total, pool_n),
        );

        // one-time: auto-launch the overlay injector AFTER probe/ is populated (so the
        // launcher never reads stale data on startup). It self-guards against doubles.
        if !SPAWNED.swap(true, Ordering::Relaxed) {
            let _ = std::process::Command::new(format!("{}/inject-overlay.exe", b)).spawn();
        }
    }
}

fn init(_c: &GameCtx) -> ModRegistration {
    let mut r = ModRegistration::new("tfm2_db_export");
    r.set_extension(E);
    r
}

declare_mod!(init);

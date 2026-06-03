//! inject-overlay — ONE-CLICK launcher for the TFM2 draft recommender.
//! No PowerShell, no game mod. Double-click (with the game open) and it:
//!   1) tracks the SAVE you're currently playing (newest, or pin.txt override) and
//!      keeps PER-SAVE stats — each save/career is a different environment (long-
//!      played+patched vs fresh), managed separately, never mixed;
//!   2) regenerates that save's stats (save-probe + draft-weights-gen, patch-CURRENT
//!      winrate) and loads them for the overlay;
//!   3) injects the overlay DLL and keeps watching for new saves.
//! Leave this window open for live updates; closing it leaves the overlay running.
//!
//! Tools live next to this exe. Per-save caches: .\stats\<save>.tsv. Active stats:
//! .\draft_weights.tsv. Manual save lock: put a save filename in .\pin.txt.

use hudhook::inject::Process;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

const TARGET: &str = "TeamfightManager2.exe";
const DLL: &str = "draft_overlay.dll";
const PROBE: &str = "tfm2_save_probe.exe";
const GEN: &str = "draft-weights-gen.exe";
const REPORT: &str = "meta-report.exe";
const BANPICK: &str = "banpick-data.js";
const OUT_TSV: &str = "draft_weights.tsv";
const KEEP_CACHES: usize = 40;

// All 60 champion ids — used to derive this save's available POOL (a champion
// appears in the save's decoded data only if it is released in that save).
const CHAMPS: &[&str] = &[
    "vampire", "monk", "circus_blade", "android", "inquisitor", "lancer", "ninja", "fighter",
    "ice_mage", "wind_mage", "exorcist", "ghost", "pole_warrior", "dual_blader", "demon", "swordman",
    "white_mage", "shadowmancer", "pythoness", "hunter", "jiangshi", "guardian_spirit", "druid",
    "siege_breaker", "knight", "werewolf", "dark_mage", "magic_knight", "illusionist", "berserker",
    "pyromancer", "bomber", "necromancer", "priest", "lightning_mage", "prisoner", "dokkaebi",
    "executioner", "barrier_magician", "shield_bearer", "archer", "gambler", "whip_master", "hitman",
    "chef", "spirit_caller", "taoist", "gunner", "hammerer", "clown", "cavalry_knight", "enchanter",
    "soldier", "plague_doctor", "dancer", "boomerang_hunter", "voodoo_shaman", "poison_dart_hunter",
    "bard", "ogre",
];

fn here() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn save_dir() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    Path::new(&appdata).join("TeamSamoyed").join("TeamfightManager2").join("data")
}

fn is_save(p: &Path) -> bool {
    p.file_name()
        .map(|n| n.to_string_lossy())
        .map(|n| n.starts_with("save_2") && n.ends_with(".data"))
        .unwrap_or(false)
}

fn newest_save() -> Option<(PathBuf, SystemTime)> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(save_dir()).ok()?.flatten() {
        let p = e.path();
        if !is_save(&p) {
            continue;
        }
        if let Ok(mt) = e.metadata().and_then(|m| m.modified()) {
            if best.as_ref().map_or(true, |(bt, _)| mt > *bt) {
                best = Some((mt, p));
            }
        }
    }
    best.map(|(mt, p)| (p, mt))
}

/// The save to use: a `pin.txt` filename if present+valid, else the newest save
/// (= the career you're actively playing, since the game writes it).
fn active_save() -> Option<(PathBuf, SystemTime)> {
    if let Ok(name) = std::fs::read_to_string(here().join("pin.txt")) {
        let name = name.trim();
        if !name.is_empty() {
            let p = save_dir().join(name);
            if let Ok(mt) = std::fs::metadata(&p).and_then(|m| m.modified()) {
                return Some((p, mt));
            }
        }
    }
    newest_save()
}

fn stem(save: &Path) -> String {
    save.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "save".into())
}

fn cache_path(save: &Path) -> PathBuf {
    here().join("stats").join(format!("{}.tsv", stem(save)))
}

/// Patch-CURRENT per-champ winrate deltas + total matches from
/// champion_patch_statistics.tsv (latest version, k=8 shrinkage toward 0.5).
// A fresh patch has few matches → thin per-(champ,lane) samples → noisy winrates.
// The PREVIOUS patch's balance mostly carries over (a patch tweaks only some champs),
// so blend it in at a discount to add sample size without over-trusting stale data.
const PREV_PATCH_WEIGHT: f64 = 0.35;

fn patch_current(patch: &str) -> (Vec<String>, Vec<String>, u32) {
    let lane_idx = |p: &str| match p {
        "Top" => Some(0u8),
        "Jungle" => Some(1),
        "Mid" => Some(2),
        "Bottom" => Some(3),
        "Support" => Some(4),
        _ => None,
    };
    let rows: Vec<Vec<&str>> = patch
        .lines()
        .skip(1)
        .map(|l| l.split('\t').collect::<Vec<&str>>())
        .filter(|f| f.len() >= 7)
        .collect();
    if rows.is_empty() {
        return (vec![], vec![], 0);
    }
    // latest + previous patch versions (sorted version strings)
    let mut vers: Vec<&str> = rows.iter().map(|f| f[0]).collect();
    vers.sort_unstable();
    vers.dedup();
    let latest = *vers.last().unwrap();
    let prev = if vers.len() >= 2 { Some(vers[vers.len() - 2]) } else { None };
    // M / "current patch N판" = the latest patch's match count (honest current-patch volume)
    let total: u32 = rows.iter().find(|f| f[0] == latest).and_then(|f| f[1].parse().ok()).unwrap_or(0);
    // blended (wins, matches): current patch ×1 + previous patch ×α (fractional ok)
    let mut by: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    let mut byp: BTreeMap<(String, u8), (f64, f64)> = BTreeMap::new();
    for f in &rows {
        let wgt = if f[0] == latest {
            1.0
        } else if Some(f[0]) == prev {
            PREV_PATCH_WEIGHT
        } else {
            continue; // older patches: drop (too stale)
        };
        let w: f64 = f[5].parse().unwrap_or(0.0);
        let m: f64 = f[6].parse().unwrap_or(0.0);
        let e = by.entry(f[2].to_string()).or_insert((0.0, 0.0));
        e.0 += wgt * w;
        e.1 += wgt * m;
        if let Some(li) = lane_idx(f[3]) {
            let pe = byp.entry((f[2].to_string(), li)).or_insert((0.0, 0.0));
            pe.0 += wgt * w;
            pe.1 += wgt * m;
        }
    }
    // shrink the (blended) winrate toward 0.5: adj = (w+4)/(m+8). 4th field = effective
    // games → overlay flags <15 as low-confidence.
    let shrink = |w: f64, m: f64| (w + 4.0) / (m + 8.0) - 0.5;
    let c_rows = by
        .iter()
        .map(|(c, (w, m))| format!("C\t{}\t{:.3}\t{}", c, shrink(*w, *m), m.round() as u32))
        .collect();
    let cp_rows = byp
        .iter()
        .filter(|(_, (_, m))| *m > 0.0)
        .map(|((c, li), (w, m))| format!("CP\t{}\t{}\t{:.3}\t{}", c, li, shrink(*w, *m), m.round() as u32))
        .collect();
    (c_rows, cp_rows, total)
}

/// The save's available champion POOL = ids that appear (quoted) anywhere in the
/// decoded save data. A champion not yet released in this save appears NOWHERE,
/// so this tracks gradual champion release per-save with no game mod.
fn compute_pool(probe_out: &Path) -> Vec<String> {
    let mut content = String::new();
    for f in ["athletes.debug.txt", "teams.debug.txt", "match_replays.debug.txt", "solo_rank_matches.debug.txt"] {
        if let Ok(s) = std::fs::read_to_string(probe_out.join(f)) {
            content.push_str(&s);
            content.push('\n');
        }
    }
    CHAMPS
        .iter()
        .filter(|id| content.contains(&format!("\"{}\"", id)))
        .map(|s| s.to_string())
        .collect()
}

/// The TRUE champion pool from the `tfm2_pool_reader` mod (game's actual
/// `db.available_champions`, custom settings included), written to `.\pool.txt`.
/// Preferred over the appearance-based `compute_pool` heuristic when present.
fn read_pool_txt() -> Option<Vec<String>> {
    let s = std::fs::read_to_string(here().join("pool.txt")).ok()?;
    let v: Vec<String> = s.trim().split(',').filter(|x| !x.is_empty()).map(|x| x.to_string()).collect();
    if v.is_empty() { None } else { Some(v) }
}

/// Patch the POOL row of draft_weights.tsv to match the mod's pool.txt, cheaply,
/// without a full regen. Lets the real (custom) pool reach the overlay live while
/// you sit in a banpick (where the save mtime doesn't change to trigger regen).
fn sync_pool() {
    let pool = match read_pool_txt() {
        Some(p) => p,
        None => return,
    };
    let new_row = format!("POOL\t{}", pool.join(","));
    let path = here().join(OUT_TSV);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
    if lines.iter().any(|l| l == &new_row) {
        return; // already in sync — don't touch the file (avoids needless reloads)
    }
    match lines.iter().position(|l| l.starts_with("POOL\t")) {
        Some(i) => lines[i] = new_row,
        None => lines.push(new_row),
    }
    let _ = std::fs::write(&path, lines.join("\n") + "\n");
}

/// champion -> lane positions, aggregated over ALL patch versions. primary =
/// most-played lane; playable = lanes with >=20% of the champ's matches (most
/// champs flex 2-3 lanes here). Emits `POS\t<champ>\t<primaryIdx>\t<idx,...>`
/// (0=Top 1=Jungle 2=Mid 3=Bottom 4=Support), primary listed first.
fn compute_pos(patch: &str) -> Vec<String> {
    let lane_idx = |p: &str| match p {
        "Top" => Some(0usize),
        "Jungle" => Some(1),
        "Mid" => Some(2),
        "Bottom" => Some(3),
        "Support" => Some(4),
        _ => None,
    };
    let mut by: BTreeMap<String, [u32; 5]> = BTreeMap::new();
    for line in patch.lines().skip(1) {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 7 {
            continue;
        }
        if let Some(li) = lane_idx(f[3]) {
            by.entry(f[2].to_string()).or_insert([0; 5])[li] += f[6].parse().unwrap_or(0);
        }
    }
    by.iter()
        .filter_map(|(c, m)| {
            let total: u32 = m.iter().sum();
            if total == 0 {
                return None;
            }
            let primary = (0..5).max_by_key(|&i| m[i]).unwrap() as u8;
            let mut plays: Vec<u8> =
                (0..5).filter(|&i| m[i] as f32 / total as f32 >= 0.20).map(|i| i as u8).collect();
            plays.retain(|&l| l != primary);
            plays.insert(0, primary);
            let list = plays.iter().map(|l| l.to_string()).collect::<Vec<_>>().join(",");
            Some(format!("POS\t{}\t{}\t{}", c, primary, list))
        })
        .collect()
}

/// Per-champ ROLE signal from the current patch's per-position stats:
///   tankRatio = tanking/dealing, healRatio = healing/dealing  (per-game averages,
///   aggregated over positions, matches-weighted). Emits `RP\t<champ>\t<tankRatio>\t<healRatio>`.
/// The overlay thresholds these to detect frontline/healer for comp balance — DATA-
/// driven, so it works even for champs the static banpick-data.js tags miss/mis-tag.
fn compute_roles(patch: &str) -> Vec<String> {
    let rows: Vec<Vec<&str>> = patch
        .lines()
        .skip(1)
        .map(|l| l.split('\t').collect::<Vec<&str>>())
        .filter(|f| f.len() >= 10)
        .collect();
    if rows.is_empty() {
        return vec![];
    }
    // role profile (deal/tank/heal) is intrinsic to a champ's kit and stable across
    // patches → aggregate ALL versions for the widest, most stable coverage.
    let mut agg: BTreeMap<String, (f64, f64, f64, u32)> = BTreeMap::new();
    for f in &rows {
        let e = agg.entry(f[2].to_string()).or_insert((0.0, 0.0, 0.0, 0));
        e.0 += f[7].parse().unwrap_or(0.0);
        e.1 += f[8].parse().unwrap_or(0.0);
        e.2 += f[9].parse().unwrap_or(0.0);
        e.3 += f[6].parse().unwrap_or(0u32);
    }
    agg.iter()
        .filter(|(_, (_, _, _, m))| *m >= 5) // need a few games for a stable role
        .filter_map(|(c, (d, t, h, _))| {
            if *d <= 0.0 {
                return None;
            }
            Some(format!("RP\t{}\t{:.2}\t{:.2}", c, t / d, h / d))
        })
        .collect()
}

/// Probe + generate this save's stats into its per-save cache file. Returns
/// (ok, total_matches). The cache embeds a `SAVE\t<stem>` line for the overlay.
fn regen(save: &Path, cache: &Path) -> (bool, u32) {
    let d = here();
    let probe_out = d.join("probe");
    let _ = std::fs::create_dir_all(&probe_out);
    let _ = std::fs::create_dir_all(d.join("stats"));

    // The tfm2_db_export mod writes the LIVE game DB straight into probe/ while you
    // play. Prefer it — the standalone save-probe is broken on game 0.4.7 (save
    // format changed, source lost). Fall back to save-probe only if the mod output
    // is absent (e.g. an old 0.4.6 game without the mod).
    let mod_tsv = probe_out.join("champion_patch_statistics.tsv");
    let have_mod_export = std::fs::metadata(&mod_tsv).map(|m| m.len() > 0).unwrap_or(false);
    if have_mod_export {
        println!("  [i] db_export 모드의 라이브 DB 통계 사용 (save-probe 생략)");
    } else {
        let ok1 = Command::new(d.join(PROBE))
            .arg(save)
            .arg(&probe_out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok1 {
            eprintln!("  [!] 통계 생성 실패: db_export 모드를 켜고 게임 안(인게임)으로 들어가세요.");
            return (false, 0);
        }
    }
    let ok2 = Command::new(d.join(GEN))
        .arg(&probe_out)
        .arg("--out")
        .arg(probe_out.join("draft_weights.json"))
        .arg("--banpick")
        .arg(d.join(BANPICK))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok2 {
        eprintln!("  [!] draft-weights-gen 실패");
        return (false, 0);
    }
    let gen = std::fs::read_to_string(probe_out.join("draft_weights.tsv")).unwrap_or_default();
    let patch = std::fs::read_to_string(probe_out.join("champion_patch_statistics.tsv")).unwrap_or_default();
    let mut out: Vec<String> = gen
        .lines()
        .filter(|l| {
            !l.starts_with("C\t") && !l.starts_with("M\t") && !l.starts_with("SAVE\t") && !l.starts_with("POOL\t")
        })
        .map(|s| s.to_string())
        .collect();
    let (c_rows, cp_rows, total) = patch_current(&patch);
    // prefer the mod-provided exact pool (db.available_champions); fall back to the
    // appearance heuristic if the pool_reader mod isn't running.
    let pool = read_pool_txt().unwrap_or_else(|| compute_pool(&probe_out));
    out.push(format!("SAVE\t{}", stem(save)));
    if !pool.is_empty() {
        out.push(format!("POOL\t{}", pool.join(",")));
    }
    out.extend(compute_pos(&patch));
    out.extend(compute_roles(&patch));
    out.extend(c_rows);
    out.extend(cp_rows);
    out.push(format!("M\t{}", total));
    // record which db_export stamp this cache was built from, so the loop can
    // detect new in-game data (new match) and regenerate even between save writes.
    out.push(format!("STAMP\t{}", export_stamp()));
    let _ = std::fs::write(cache, out.join("\n") + "\n");

    // (re)generate the HTML meta·champion analysis report next to the tools.
    let _ = Command::new(d.join(REPORT))
        .arg(probe_out.join("champion_patch_statistics.tsv"))
        .arg(d.join("report.html"))
        .arg(stem(save))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    (true, total)
}

/// Keep only the newest KEEP_CACHES per-save stat files.
fn prune_caches() {
    let dir = here().join("stats");
    let mut files: Vec<(SystemTime, PathBuf)> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension().map(|x| x == "tsv").unwrap_or(false) {
                    e.metadata().and_then(|m| m.modified()).ok().map(|mt| (mt, p))
                } else {
                    None
                }
            })
            .collect(),
        Err(_) => return,
    };
    if files.len() <= KEEP_CACHES {
        return;
    }
    files.sort_by_key(|(mt, _)| *mt);
    for (_, p) in files.iter().take(files.len() - KEEP_CACHES) {
        let _ = std::fs::remove_file(p);
    }
}

fn clean_str(b: &[u8]) -> Option<String> {
    std::str::from_utf8(b).ok().filter(|s| !s.is_empty() && !s.chars().any(|c| c.is_control())).map(|s| s.to_string())
}

/// (team, manager) read from the UNCOMPRESSED save header — the same names the
/// in-game Load menu shows, no decode needed. Layout near the start:
///   ...[flag][len]<TEAM>[len]"custom:custom_team_logo/<id>"[len]<MANAGER>...
/// TEAM = the length-prefixed string just BEFORE the logo path; MANAGER (the name
/// the player chose) = the length-prefixed string just AFTER it. Both differ per
/// career, so they identify the save.
fn header_info(save: &Path) -> (String, String) {
    let mut buf = vec![0u8; 512];
    let n = match std::fs::File::open(save).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => return (String::new(), String::new()),
    };
    buf.truncate(n);
    let needle = b"custom:custom_team_logo";
    let o = match buf.windows(needle.len()).position(|w| w == needle) {
        Some(o) => o,
        None => return (String::new(), String::new()),
    };
    // TEAM: length-prefixed string immediately before the logo path
    let mut team = String::new();
    for tl in 1..=40usize {
        if o < 2 + tl {
            break;
        }
        let lenpos = o - 2 - tl;
        if buf[lenpos] as usize == tl {
            if let Some(s) = clean_str(&buf[lenpos + 1..lenpos + 1 + tl]) {
                team = s;
                break;
            }
        }
    }
    // MANAGER: length-prefixed string immediately after the logo path
    let mut manager = String::new();
    let logo_len = buf[o - 1] as usize;
    let after = o + logo_len;
    if after < buf.len() {
        let mlen = buf[after] as usize;
        if (1..=40).contains(&mlen) && after + 1 + mlen <= buf.len() {
            if let Some(s) = clean_str(&buf[after + 1..after + 1 + mlen]) {
                manager = s;
            }
        }
    }
    (team, manager)
}

fn read_cached_count(cache: &Path) -> u32 {
    std::fs::read_to_string(cache)
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("M\t").and_then(|n| n.parse::<u32>().ok())))
        .unwrap_or(0)
}

/// The tfm2_db_export mod's current live-DB export stamp (probe/_stamp.txt). It
/// changes whenever new match/patch/pool data appears in-game, so the launcher can
/// regenerate promptly — even while you sit in a banpick between save writes.
fn export_stamp() -> String {
    std::fs::read_to_string(here().join("probe").join("_stamp.txt"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The export stamp a cache was built from (its STAMP row), "" if none.
fn read_cache_stamp(cache: &Path) -> String {
    std::fs::read_to_string(cache)
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("STAMP\t").map(|x| x.trim().to_string())))
        .unwrap_or_default()
}

/// Write the list of saves (newest first) + cached match count to saves.tsv, so
/// the overlay can offer a SAVE PICKER (newest-mtime is not reliably the save the
/// user loaded — they may load an old career; the picker lets them choose).
fn write_saves_list() {
    let d = here();
    let mut entries: Vec<(SystemTime, String, String, u64, String, String)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(save_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if !is_save(&p) {
                continue;
            }
            let fname = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let md = e.metadata();
            let mt = md.as_ref().ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
            let mb = md.as_ref().map(|m| m.len() / (1024 * 1024)).unwrap_or(0);
            let cache = d.join("stats").join(format!("{}.tsv", stem(&p)));
            let count = if cache.exists() { read_cached_count(&cache).to_string() } else { "?".into() };
            let (team, manager) = header_info(&p);
            entries.push((mt, fname, count, mb, team, manager));
        }
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    // filename <TAB> count <TAB> size(MB) <TAB> team <TAB> manager  (team+manager from the save header)
    let body: String = entries
        .iter()
        .map(|(_, f, c, mb, t, m)| format!("{}\t{}\t{}\t{}\t{}\n", f, c, mb, t, m))
        .collect();
    let _ = std::fs::write(d.join("saves.tsv"), body);
}

fn try_inject() -> bool {
    let dll = here().join(DLL);
    if !dll.exists() {
        eprintln!("  [!] {} 없음", dll.display());
        return false;
    }
    let dll = dll.canonicalize().unwrap_or(dll);
    match Process::by_name(TARGET) {
        Ok(p) => match p.inject(dll) {
            Ok(()) => {
                println!("  오버레이 주입 완료.");
                true
            }
            Err(e) => {
                eprintln!("  주입 실패: {e:?}");
                false
            }
        },
        Err(_) => false,
    }
}

fn main() {
    // single-instance guard (std-only): the pool_reader mod auto-spawns this on game
    // start, so guard against a second copy (e.g. a manual double-click) double-injecting.
    let _lock = match std::net::TcpListener::bind("127.0.0.1:48619") {
        Ok(l) => l,
        Err(_) => {
            println!("오버레이 런처가 이미 실행 중입니다. (중복 실행 방지)");
            return;
        }
    };
    println!("TFM2 드래프트 추천기 런처 (세이브별 통계 관리)");
    let d = here();
    for t in [DLL, PROBE, GEN, BANPICK] {
        if !d.join(t).exists() {
            eprintln!("  [!] 누락: {}", d.join(t).display());
        }
    }

    let mut injected = false;
    let mut last_key = String::new();
    let mut first = true;
    let mut misses = 0u32;

    loop {
        // refresh the save list for the overlay's picker
        write_saves_list();

        // 1) per-save stats for the ACTIVE save (pin.txt selection, else newest)
        if let Some((save, mt)) = active_save() {
            // include the mod's live-DB export stamp so a freshly-played match (new
            // in-game data) refreshes the overlay even if the save mtime hasn't moved.
            let stamp = export_stamp();
            let key = format!("{:?}|{:?}|{}", save, mt, stamp);
            if key != last_key {
                last_key = key;
                let name = stem(&save);
                let cache = cache_path(&save);
                // cache is valid only if it is at least as new as the save file AND was
                // built from the current export stamp (new matches/patch invalidate it;
                // loading+playing an old save also bumps its mtime → its cache goes stale).
                let cache_fresh = std::fs::metadata(&cache)
                    .and_then(|m| m.modified())
                    .map(|cmt| cmt >= mt)
                    .unwrap_or(false)
                    && (stamp.is_empty() || read_cache_stamp(&cache) == stamp);
                let total = if cache_fresh {
                    println!("세이브 [{}] 캐시 사용.", name);
                    read_cached_count(&cache)
                } else {
                    println!("세이브 [{}] 통계 생성 중...", name);
                    let (ok, t) = regen(&save, &cache);
                    if ok {
                        t
                    } else {
                        0
                    }
                };
                let _ = std::fs::copy(&cache, d.join(OUT_TSV));
                prune_caches();
                write_saves_list(); // count for this save is now known
                let mode = if total < 50 { "콜드스타트(이론 위주)" } else { "통계 위주" };
                println!("  활성 세이브: {}  ·  현재 패치 {}판 → {}", name, total, mode);
            }
        } else if first {
            eprintln!("  [!] 세이브를 찾지 못함: {}", save_dir().display());
        }

        // 1b) keep the live POOL in sync with the mod's pool.txt EVEN WHEN the save
        // doesn't change (e.g. sitting in a banpick) — a full regen only fires on a
        // save-mtime change, so without this the real pool never reaches the overlay.
        sync_pool();

        // 2) inject once the game is running; auto-close when the game exits
        if !injected {
            if try_inject() {
                injected = true;
                println!("이 창은 게임이 켜져 있는 동안 통계를 자동 갱신합니다. (게임 종료 시 자동으로 닫힘)");
                println!("Ctrl+F10 으로 오버레이 토글.");
            } else if first {
                println!("게임을 켜고 밴픽 화면으로 가면 자동 주입됩니다. 대기 중...");
            }
        } else if Process::by_name(TARGET).is_err() {
            // game was running and is now gone → close the launcher (2× to avoid a transient miss)
            misses += 1;
            if misses >= 2 {
                println!("게임이 종료되어 런처를 닫습니다.");
                std::process::exit(0);
            }
        } else {
            misses = 0;
        }

        first = false;
        std::thread::sleep(std::time::Duration::from_secs(8));
    }
}

//! draft-weights-gen
//!
//! Converts TFM2 `save_probe` output (the `solo_rank_matches.debug.txt` Rust-Debug
//! dump, plus optionally `match_replays.debug.txt`) into `draft_weights.json` — a
//! score table consumed by the `tfm2_draft_ai` mod's `ModDraftScoreHook`.
//!
//! Statistics mirrored from the dashboard's build_meta_data.py:
//!   - per-(champion, position) wins/games  (add_player_stat)
//!   - synergy pairs (co-pick winrate)      (RelationAccumulator._record_team)
//!   - counter pairs (vs-enemy winrate)     (RelationAccumulator._record_opponents)
//!
//! Each winrate is shrunk toward 0.5 (neutral) by sample size so a 1-game 100%
//! does not produce a huge bonus:  adj = (wins + k*0.5) / (games + k).
//! The stored `delta` is (adj - 0.5), roughly in [-0.5, 0.5]; the mod multiplies
//! by a tunable SCALE.
//!
//! Champion identity: champions are recorded as NAME STRINGS in the save. The
//! mod's `candidate: usize` indexes the game's champion list, which we assume
//! equals the order in banpick-data.js (CHAMPION_ORDER below). Verify in-game once.
//!
//! Usage:
//!   draft-weights-gen <probe_dir> [--out draft_weights.json]
//!   (probe_dir holds solo_rank_matches.debug.txt and/or match_replays.debug.txt)

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

/// Champion order = index basis for the mod's `candidate: usize`.
/// Extracted from banpick-data.js `champions[]` (base_unpacked champion_info order).
const CHAMPION_ORDER: &[&str] = &[
    "vampire", "monk", "circus_blade", "android", "inquisitor", "lancer", "ninja",
    "fighter", "ice_mage", "wind_mage", "exorcist", "ghost", "pole_warrior",
    "dual_blader", "demon", "swordman", "white_mage", "shadowmancer", "pythoness",
    "hunter", "jiangshi", "guardian_spirit", "druid", "siege_breaker", "knight",
    "werewolf", "dark_mage", "magic_knight", "illusionist", "berserker", "pyromancer",
    "bomber", "necromancer", "priest", "lightning_mage", "prisoner", "dokkaebi",
    "executioner", "barrier_magician", "shield_bearer", "archer", "gambler",
    "whip_master", "hitman", "chef", "spirit_caller", "taoist", "gunner", "hammerer",
    "clown", "cavalry_knight", "enchanter", "soldier", "plague_doctor", "dancer",
    "boomerang_hunter", "voodoo_shaman", "poison_dart_hunter", "bard", "ogre",
];

// Shrinkage constants (pseudo-count of neutral 0.5 games added).
const K_POSITION: f64 = 8.0; // per (champion, position)
const K_OVERALL: f64 = 10.0; // per champion overall
const K_PAIR: f64 = 12.0; // synergy / counter pairs (thinner samples → more conservative)

const POSITIONS: &[&str] = &["top", "jungle", "mid", "bottom", "support"];

#[derive(Default, Clone)]
struct WL {
    games: u32,
    wins: u32,
}
impl WL {
    fn record(&mut self, won: bool) {
        self.games += 1;
        if won {
            self.wins += 1;
        }
    }
    /// Shrunk winrate toward 0.5.
    fn adj_winrate(&self, k: f64) -> f64 {
        (self.wins as f64 + k * 0.5) / (self.games as f64 + k)
    }
    fn raw_winrate(&self) -> f64 {
        if self.games == 0 {
            0.0
        } else {
            self.wins as f64 / self.games as f64
        }
    }
}

#[derive(Default)]
struct ChampAgg {
    overall: WL,
    by_pos: BTreeMap<String, WL>,
}

#[derive(Default)]
struct Aggregator {
    champs: BTreeMap<String, ChampAgg>,
    // synergy[a][b] = WL of a when teamed with b
    synergy: BTreeMap<String, BTreeMap<String, WL>>,
    // counter[a][b] = WL of a when facing b
    counter: BTreeMap<String, BTreeMap<String, WL>>,
    matches: u32,
}

impl Aggregator {
    fn record_player(&mut self, champ: &str, pos: Option<&str>, won: bool) {
        let c = self.champs.entry(champ.to_string()).or_default();
        c.overall.record(won);
        if let Some(p) = pos {
            c.by_pos.entry(p.to_string()).or_default().record(won);
        }
    }

    fn record_team_synergy(&mut self, team: &[String], won: bool) {
        for i in 0..team.len() {
            for j in 0..team.len() {
                if i == j {
                    continue;
                }
                self.synergy
                    .entry(team[i].clone())
                    .or_default()
                    .entry(team[j].clone())
                    .or_default()
                    .record(won);
            }
        }
    }

    fn record_counters(&mut self, team: &[String], enemies: &[String], won: bool) {
        for c in team {
            for e in enemies {
                self.counter
                    .entry(c.clone())
                    .or_default()
                    .entry(e.clone())
                    .or_default()
                    .record(won);
            }
        }
    }

    fn record_match(&mut self, blue: &[(String, Option<String>)], red: &[(String, Option<String>)], blue_win: bool) {
        self.matches += 1;
        for (c, p) in blue {
            self.record_player(c, p.as_deref(), blue_win);
        }
        for (c, p) in red {
            self.record_player(c, p.as_deref(), !blue_win);
        }
        let blue_names: Vec<String> = blue.iter().map(|(c, _)| c.clone()).collect();
        let red_names: Vec<String> = red.iter().map(|(c, _)| c.clone()).collect();
        self.record_team_synergy(&blue_names, blue_win);
        self.record_team_synergy(&red_names, !blue_win);
        self.record_counters(&blue_names, &red_names, blue_win);
        self.record_counters(&red_names, &blue_names, !blue_win);
    }
}

// ---------- balanced-delimiter scanning over the Rust-Debug text ----------

/// Given text and the index of an opening delimiter, return (slice, end_index)
/// for the balanced region including both delimiters.
fn read_balanced(bytes: &[u8], open_idx: usize, open: u8, close: u8) -> Option<(usize, usize)> {
    if open_idx >= bytes.len() || bytes[open_idx] != open {
        return None;
    }
    let mut depth = 0i32;
    let mut i = open_idx;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some((open_idx, i)); // inclusive
            }
        }
        i += 1;
    }
    None
}

/// Find `needle` starting at/after `from`; return index of the byte right at the
/// match start, or None.
fn find_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    let last = hay.len().checked_sub(needle.len())?;
    let mut i = from;
    while i <= last {
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parse a boolean field `name: true|false` within `block`.
fn parse_bool(block: &str, name: &str) -> Option<bool> {
    let key = format!("{}: ", name);
    let idx = block.find(&key)?;
    let rest = &block[idx + key.len()..];
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// Extract a `name: [ ... ]` array slice (balanced brackets) from block.
fn extract_array<'a>(block: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{}: [", name);
    let kidx = block.find(&key)?;
    let bracket = kidx + key.len() - 1; // index of '['
    let b = block.as_bytes();
    let (s, e) = read_balanced(b, bracket, b'[', b']')?;
    Some(&block[s..=e])
}

/// First quoted string for `field: "..."` inside block.
fn parse_quoted_field(block: &str, field: &str) -> Option<String> {
    let key = format!("{}: \"", field);
    let idx = block.find(&key)?;
    let rest = &block[idx + key.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Position from AthleteStat role weights: max of top/jungle/mid/bottom/support.
fn parse_position_from_stats(block: &str) -> Option<String> {
    let mut best: Option<(String, i64)> = None;
    for &pos in POSITIONS {
        let key = format!("{}: ", pos);
        // there may be several; the role weights live in AthleteStat. Take the
        // first occurrence of each key (role weights appear once per athlete).
        if let Some(idx) = block.find(&key) {
            let rest = &block[idx + key.len()..];
            let numend = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
            if let Ok(v) = rest[..numend].parse::<i64>() {
                let norm = if pos == "bottom" { "bottom" } else { pos };
                match &best {
                    Some((_, bv)) if *bv >= v => {}
                    _ => best = Some((norm.to_string(), v)),
                }
            }
        }
    }
    match best {
        Some((p, v)) if v > 0 => Some(p),
        _ => None,
    }
}

/// Split a team-array slice into individual struct blocks named `struct_name`.
fn split_structs(team_text: &str, struct_name: &str) -> Vec<String> {
    let b = team_text.as_bytes();
    let needle = format!("{} {{", struct_name);
    let nb = needle.as_bytes();
    let mut out = Vec::new();
    let mut off = 0usize;
    while let Some(start) = find_from(b, nb, off) {
        let brace = start + needle.len() - 1; // index of '{'
        if let Some((s, e)) = read_balanced(b, brace, b'{', b'}') {
            out.push(team_text[s..=e].to_string());
            off = e + 1;
        } else {
            break;
        }
    }
    out
}

/// Parse a team array into (champion, position) for members in the champ set.
fn parse_team(team_text: &str, struct_name: &str, with_pos: bool, set: &std::collections::HashSet<String>) -> Vec<(String, Option<String>)> {
    let mut players = Vec::new();
    for blk in split_structs(team_text, struct_name) {
        let champ = match parse_quoted_field(&blk, "champion") {
            Some(c) => c,
            None => continue,
        };
        if !set.contains(&champ) {
            continue;
        }
        let pos = if with_pos { parse_position_from_stats(&blk) } else { None };
        players.push((champ, pos));
    }
    players
}

fn parse_matches_file(
    agg: &mut Aggregator,
    text: &str,
    match_struct: &str,
    athlete_struct: &str,
    require_played: bool,
    with_pos: bool,
    set: &std::collections::HashSet<String>,
) -> u32 {
    let b = text.as_bytes();
    let marker = format!("{} {{", match_struct);
    let mb = marker.as_bytes();
    let mut off = 0usize;
    let mut count = 0u32;
    while let Some(start) = find_from(b, mb, off) {
        let brace = start + marker.len() - 1;
        let (s, e) = match read_balanced(b, brace, b'{', b'}') {
            Some(v) => v,
            None => break,
        };
        off = e + 1;
        let block = &text[s..=e];
        if require_played && !block.contains("played: true") {
            continue;
        }
        let blue_win = match parse_bool(block, "blue_team_win") {
            Some(v) => v,
            None => continue,
        };
        let blue_arr = match extract_array(block, "blue_team") {
            Some(a) => a,
            None => continue,
        };
        let red_arr = match extract_array(block, "red_team") {
            Some(a) => a,
            None => continue,
        };
        let blue = parse_team(blue_arr, athlete_struct, with_pos, set);
        let red = parse_team(red_arr, athlete_struct, with_pos, set);
        if blue.is_empty() || red.is_empty() {
            continue;
        }
        agg.record_match(&blue, &red, blue_win);
        count += 1;
    }
    count
}

// ---------- JSON output (hand-written, std-only) ----------

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[derive(Default, Clone)]
struct Taxon {
    category: String,
    damage: String, // "AD" / "AP" / "AD|AP"
    tags: Vec<String>,
}

/// Parse banpick-data.js `champions[]`: each is a bare `{ ... }` object with
/// "id","category","tags":[...],"rawTags":[...]. Damage type = AD/AP in rawTags.
fn parse_taxonomy(text: &str) -> BTreeMap<String, Taxon> {
    let mut out = BTreeMap::new();
    let b = text.as_bytes();
    // locate `"champions": [`
    let arr_key = "\"champions\": [";
    let astart = match text.find(arr_key) { Some(v) => v, None => return out };
    let bracket = astart + arr_key.len() - 1;
    let (s, e) = match read_balanced(b, bracket, b'[', b']') { Some(v) => v, None => return out };
    let arr = &text[s..=e];
    // iterate top-level `{ ... }` objects within the array
    let ab = arr.as_bytes();
    let mut off = 0usize;
    while let Some(brace) = ab[off..].iter().position(|&c| c == b'{').map(|p| off + p) {
        let (os, oe) = match read_balanced(ab, brace, b'{', b'}') { Some(v) => v, None => break };
        let obj = &arr[os..=oe];
        off = oe + 1;
        let id = match json_string_field(obj, "id") { Some(v) => v, None => continue };
        let category = json_string_field(obj, "category").unwrap_or_default();
        let tags = parse_quoted_array(obj, "tags");
        let raw = parse_quoted_array(obj, "rawTags");
        let has_ad = raw.iter().any(|t| t == "AD");
        let has_ap = raw.iter().any(|t| t == "AP");
        let damage = match (has_ad, has_ap) {
            (true, true) => "AD|AP",
            (true, false) => "AD",
            (false, true) => "AP",
            _ => "AD",
        }.to_string();
        out.insert(id, Taxon { category, damage, tags });
    }
    out
}

/// JSON string field: `"field": "value"` (quoted key, unlike the Debug-format helper).
fn json_string_field(block: &str, field: &str) -> Option<String> {
    let key = format!("\"{}\": \"", field);
    let idx = block.find(&key)?;
    let rest = &block[idx + key.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// All quoted strings inside a `field: [ ... ]` array.
fn parse_quoted_array(block: &str, field: &str) -> Vec<String> {
    let key = format!("\"{}\": [", field);
    let kidx = match block.find(&key) { Some(v) => v, None => return Vec::new() };
    let bracket = kidx + key.len() - 1;
    let b = block.as_bytes();
    let (s, e) = match read_balanced(b, bracket, b'[', b']') { Some(v) => v, None => return Vec::new() };
    let inner = &block[s + 1..e];
    let mut out = Vec::new();
    let mut rest = inner;
    while let Some(q) = rest.find('"') {
        let after = &rest[q + 1..];
        if let Some(q2) = after.find('"') {
            out.push(after[..q2].to_string());
            rest = &after[q2 + 1..];
        } else { break; }
    }
    out
}

/// champion_patch_statistics.tsv → per-champ ban rate (bans / total_match).
/// `bans` is season-level (repeats per position row); total_match is col 2.
fn parse_banrate(tsv: &str) -> BTreeMap<String, f64> {
    let mut bans: BTreeMap<String, u32> = BTreeMap::new();
    let mut total = 0u32;
    for (li, line) in tsv.lines().enumerate() {
        if li == 0 { continue; }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 5 { continue; }
        if total == 0 { total = f[1].parse().unwrap_or(0); }
        let champ = f[2].to_string();
        let b: u32 = f[4].parse().unwrap_or(0);
        let e = bans.entry(champ).or_insert(0);
        *e = (*e).max(b); // season-level: take max across position rows
    }
    let mut out = BTreeMap::new();
    if total > 0 {
        for (c, b) in bans { out.insert(c, r3(b as f64 / total as f64)); }
    }
    out
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: draft-weights-gen <probe_dir> [--out <file>] [--min-pair <n>]");
        process::exit(2);
    }
    let probe_dir = PathBuf::from(&args[1]);
    let mut out_path = probe_dir.join("draft_weights.json");
    let mut min_pair = 4u32; // omit pairs with fewer than this many games
    let mut banpick_path: Option<PathBuf> = None;
    let mut patch_path: Option<PathBuf> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => { i += 1; out_path = PathBuf::from(&args[i]); }
            "--min-pair" => { i += 1; min_pair = args[i].parse().unwrap_or(4); }
            "--banpick" => { i += 1; banpick_path = Some(PathBuf::from(&args[i])); }
            "--patch" => { i += 1; patch_path = Some(PathBuf::from(&args[i])); }
            other => { eprintln!("unknown arg: {}", other); process::exit(2); }
        }
        i += 1;
    }
    // Defaults: patch stats live next to the probe output.
    let patch_path = patch_path
        .unwrap_or_else(|| probe_dir.join("champion_patch_statistics.tsv"));

    // Taxonomy (category, damage type, structural tags) from banpick-data.js.
    let taxonomy: BTreeMap<String, Taxon> = match &banpick_path {
        Some(p) => parse_taxonomy(&fs::read_to_string(p).unwrap_or_default()),
        None => BTreeMap::new(),
    };
    eprintln!("taxonomy: {} champions tagged", taxonomy.len());
    // Ban rate from champion_patch_statistics.tsv.
    let banrate: BTreeMap<String, f64> = if patch_path.exists() {
        parse_banrate(&fs::read_to_string(&patch_path).unwrap_or_default())
    } else { BTreeMap::new() };
    eprintln!("banrate: {} champions", banrate.len());

    let set: std::collections::HashSet<String> =
        CHAMPION_ORDER.iter().map(|s| s.to_string()).collect();

    let mut agg = Aggregator::default();

    // Solo rank = the big sample (with position info from AthleteStat).
    let solo = probe_dir.join("solo_rank_matches.debug.txt");
    let mut solo_count = 0;
    if solo.exists() {
        let text = fs::read_to_string(&solo).unwrap_or_default();
        solo_count = parse_matches_file(
            &mut agg, &text, "SoloRankMatch", "SoloRankAthlete", true, true, &set,
        );
        eprintln!("solo_rank_matches: parsed {} played matches", solo_count);
    } else {
        eprintln!("note: {} not found", solo.display());
    }

    // Match replays = additional games (position via enum, but we only use names).
    let replays = probe_dir.join("match_replays.debug.txt");
    let mut replay_count = 0;
    if replays.exists() {
        let text = fs::read_to_string(&replays).unwrap_or_default();
        replay_count = parse_matches_file(
            &mut agg, &text, "MatchReplayData", "MatchReplayAthlete", false, false, &set,
        );
        eprintln!("match_replays: parsed {} matches", replay_count);
    }

    if agg.matches == 0 {
        eprintln!("ERROR: no matches parsed; nothing to write");
        process::exit(1);
    }

    // ---- build JSON ----
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str(&format!(
        "  \"meta\": {{ \"source\": \"save_probe\", \"matches\": {}, \"soloMatches\": {}, \"replayMatches\": {}, \"kPos\": {}, \"kOverall\": {}, \"kPair\": {}, \"minPair\": {} }},\n",
        agg.matches, solo_count, replay_count, K_POSITION, K_OVERALL, K_PAIR, min_pair
    ));

    // champion_order
    s.push_str("  \"championOrder\": [");
    for (idx, c) in CHAMPION_ORDER.iter().enumerate() {
        if idx > 0 {
            s.push_str(", ");
        }
        s.push_str(&format!("\"{}\"", json_escape(c)));
    }
    s.push_str("],\n");

    // champions: delta from PRIMARY position (most games) shrunk winrate; also overall.
    s.push_str("  \"champions\": {\n");
    let mut first = true;
    for (champ, c) in &agg.champs {
        // primary position = most games
        let primary = c
            .by_pos
            .iter()
            .max_by_key(|(_, wl)| wl.games)
            .map(|(p, wl)| (p.clone(), wl.clone()));
        // delta basis: primary position shrunk winrate if it has enough games,
        // else overall.
        let (basis_wr, basis_label) = match &primary {
            Some((p, wl)) if wl.games >= 3 => (wl.adj_winrate(K_POSITION), p.clone()),
            _ => (c.overall.adj_winrate(K_OVERALL), "overall".to_string()),
        };
        let delta = basis_wr - 0.5;
        if !first {
            s.push_str(",\n");
        }
        first = false;
        s.push_str(&format!(
            "    \"{}\": {{ \"games\": {}, \"wins\": {}, \"winrate\": {}, \"delta\": {}, \"primaryPos\": \"{}\", \"basis\": \"{}\"",
            json_escape(champ),
            c.overall.games,
            c.overall.wins,
            r3(c.overall.raw_winrate()),
            r3(delta),
            primary.as_ref().map(|(p, _)| p.as_str()).unwrap_or("none"),
            basis_label,
        ));
        // positions sub-map
        s.push_str(", \"positions\": {");
        let mut pf = true;
        for (p, wl) in &c.by_pos {
            if !pf {
                s.push_str(", ");
            }
            pf = false;
            s.push_str(&format!(
                "\"{}\": {{ \"games\": {}, \"wins\": {}, \"delta\": {} }}",
                p,
                wl.games,
                wl.wins,
                r3(wl.adj_winrate(K_POSITION) - 0.5)
            ));
        }
        s.push_str("} }");
    }
    s.push_str("\n  },\n");

    // synergy & counter
    let write_relation = |s: &mut String, name: &str, table: &BTreeMap<String, BTreeMap<String, WL>>, last: bool| {
        s.push_str(&format!("  \"{}\": {{\n", name));
        let mut cf = true;
        for (a, inner) in table {
            // keep only pairs with enough games
            let entries: Vec<(&String, &WL)> =
                inner.iter().filter(|(_, wl)| wl.games >= min_pair).collect();
            if entries.is_empty() {
                continue;
            }
            if !cf {
                s.push_str(",\n");
            }
            cf = false;
            s.push_str(&format!("    \"{}\": {{", json_escape(a)));
            let mut ef = true;
            for (b, wl) in entries {
                if !ef {
                    s.push_str(", ");
                }
                ef = false;
                s.push_str(&format!(
                    "\"{}\": {{ \"games\": {}, \"delta\": {} }}",
                    json_escape(b),
                    wl.games,
                    r3(wl.adj_winrate(K_PAIR) - 0.5)
                ));
            }
            s.push('}');
        }
        if last {
            s.push_str("\n  }\n");
        } else {
            s.push_str("\n  },\n");
        }
    };
    write_relation(&mut s, "synergy", &agg.synergy, false);
    write_relation(&mut s, "counter", &agg.counter, true);

    s.push_str("}\n");

    fs::write(&out_path, &s).unwrap_or_else(|e| {
        eprintln!("failed to write {}: {}", out_path.display(), e);
        process::exit(1);
    });
    eprintln!(
        "wrote {} ({} champions, {} bytes)",
        out_path.display(),
        agg.champs.len(),
        s.len()
    );

    // ---- also emit a flat TSV the mod parses at runtime (no JSON parser needed) ----
    // Format (tab-separated), one record per line:
    //   C \t champ \t delta                 (per-champion delta, primary basis)
    //   S \t a \t b \t delta                (synergy: champ a teamed with b)
    //   K \t a \t b \t delta                (counter: champ a vs enemy b)
    let tsv_path = out_path.with_extension("tsv");
    let mut t = String::new();
    t.push_str("# draft_weights TSV  C=champ S=synergy K=counter\n");
    for (champ, c) in &agg.champs {
        let primary = c.by_pos.iter().max_by_key(|(_, wl)| wl.games);
        let (basis_wr, _) = match primary {
            Some((p, wl)) if wl.games >= 3 => (wl.adj_winrate(K_POSITION), p.clone()),
            _ => (c.overall.adj_winrate(K_OVERALL), "overall".to_string()),
        };
        t.push_str(&format!("C\t{}\t{}\n", champ, r3(basis_wr - 0.5)));
    }
    for (a, inner) in &agg.synergy {
        for (b, wl) in inner {
            if wl.games >= min_pair {
                t.push_str(&format!("S\t{}\t{}\t{}\n", a, b, r3(wl.adj_winrate(K_PAIR) - 0.5)));
            }
        }
    }
    for (a, inner) in &agg.counter {
        for (b, wl) in inner {
            if wl.games >= min_pair {
                t.push_str(&format!("K\t{}\t{}\t{}\n", a, b, r3(wl.adj_winrate(K_PAIR) - 0.5)));
            }
        }
    }
    // T = taxonomy: champ \t category \t damage \t tag1,tag2,...
    for (champ, tx) in &taxonomy {
        t.push_str(&format!("T\t{}\t{}\t{}\t{}\n", champ, tx.category, tx.damage, tx.tags.join(",")));
    }
    // B = ban rate
    for (champ, br) in &banrate {
        t.push_str(&format!("B\t{}\t{}\n", champ, br));
    }
    fs::write(&tsv_path, &t).unwrap_or_else(|e| {
        eprintln!("failed to write {}: {}", tsv_path.display(), e);
        process::exit(1);
    });
    eprintln!("wrote {} ({} bytes)", tsv_path.display(), t.len());
    let _ = Path::new(&out_path);
}

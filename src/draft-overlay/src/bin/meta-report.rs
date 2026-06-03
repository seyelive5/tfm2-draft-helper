//! meta-report — generates a self-contained HTML "메타·챔피언 분석" report from the
//! live-DB `champion_patch_statistics.tsv` (written by the tfm2_db_export mod). The
//! launcher runs this each regen; the user opens `report.html` in a browser. No deps.
//!
//! Usage: meta-report <champion_patch_statistics.tsv> <out.html> [save_label]

#[path = "../engine.rs"]
mod engine;
use engine::ko_name;

use std::collections::BTreeMap;

const POS_KO: &[(&str, &str)] =
    &[("Top", "탑"), ("Jungle", "정글"), ("Mid", "미드"), ("Bottom", "바텀"), ("Support", "서폿")];
const MIN_POS: u32 = 8; // min games in a position to list it
const MIN_TOP: u32 = 15; // min total games to enter the OP top-list

#[derive(Default, Clone)]
struct Cell {
    wins: u32,
    matches: u32,
    bans: u32,
    deal: f64,
    tank: f64,
    heal: f64,
    kills: f64,
    deaths: f64,
    cs: f64,
    gold: f64,
}

/// shrunk winrate toward 0.5 (k=8), as in the recommender.
fn wr(w: u32, m: u32) -> f64 {
    (w as f64 + 4.0) / (m as f64 + 8.0)
}

/// tier label + color from the winrate delta (wr - 0.5).
fn tier(d: f64) -> (&'static str, &'static str) {
    if d >= 0.06 {
        ("S", "#ff5d5d")
    } else if d >= 0.02 {
        ("A", "#ff9f43")
    } else if d >= -0.02 {
        ("B", "#feca57")
    } else if d >= -0.06 {
        ("C", "#8fd14f")
    } else {
        ("D", "#7a8aa0")
    }
}

/// data role glyph (same thresholds as the overlay).
fn role(deal: f64, tank: f64, heal: f64) -> &'static str {
    if deal <= 0.0 {
        return "";
    }
    let (tr, hr) = (tank / deal, heal / deal);
    if hr >= 1.5 {
        "힐"
    } else if tr >= 1.6 && hr < 1.0 {
        "탱"
    } else if tr >= 1.25 && hr < 1.2 {
        "브"
    } else {
        "딜"
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn pg(v: f64, m: u32) -> f64 {
    if m == 0 {
        0.0
    } else {
        v / m as f64
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: meta-report <patch.tsv> <out.html> [save_label]");
        std::process::exit(2);
    }
    let save_label = args.get(3).cloned().unwrap_or_default();
    let tsv = std::fs::read_to_string(&args[1]).unwrap_or_default();
    let rows: Vec<Vec<&str>> =
        tsv.lines().skip(1).map(|l| l.split('\t').collect::<Vec<_>>()).filter(|f| f.len() >= 14).collect();
    if rows.is_empty() {
        let _ = std::fs::write(&args[2], "<!doctype html><meta charset=utf-8><body style='background:#11151c;color:#ccc;font-family:sans-serif;padding:40px'>아직 데이터가 없습니다. 게임을 진행하면 통계가 쌓입니다.</body>");
        return;
    }
    let latest = rows.iter().map(|f| f[0]).max().unwrap_or("").to_string();
    let cur: Vec<&Vec<&str>> = rows.iter().filter(|f| f[0] == latest).collect();
    let total: u32 = cur.first().and_then(|f| f[1].parse().ok()).unwrap_or(0);

    // per (champ, pos) cells + per-champ totals
    let mut cells: BTreeMap<(String, String), Cell> = BTreeMap::new();
    let mut champ_tot: BTreeMap<String, Cell> = BTreeMap::new();
    for f in &cur {
        let champ = f[2].to_string();
        let pos = f[3].to_string();
        let m: u32 = f[6].parse().unwrap_or(0);
        let upd = |c: &mut Cell| {
            c.wins += f[5].parse().unwrap_or(0);
            c.matches += m;
            c.bans = c.bans.max(f[4].parse().unwrap_or(0));
            c.deal += f[7].parse().unwrap_or(0.0);
            c.tank += f[8].parse().unwrap_or(0.0);
            c.heal += f[9].parse().unwrap_or(0.0);
            c.kills += f[10].parse().unwrap_or(0.0);
            c.deaths += f[11].parse().unwrap_or(0.0);
            c.cs += f[12].parse().unwrap_or(0.0);
            c.gold += f[13].parse().unwrap_or(0.0);
        };
        upd(cells.entry((champ.clone(), pos)).or_default());
        upd(champ_tot.entry(champ).or_default());
    }

    let mut h = String::with_capacity(64 * 1024);
    h.push_str("<!doctype html><html lang=ko><head><meta charset=utf-8>");
    h.push_str("<meta name=viewport content='width=device-width,initial-scale=1'>");
    h.push_str("<title>TFM2 메타 분석</title><style>");
    h.push_str(
        "body{background:#11151c;color:#dfe6f0;font-family:'Malgun Gothic',sans-serif;margin:0;padding:24px 28px;}\
         h1{font-size:22px;margin:0 0 2px} h2{font-size:17px;margin:26px 0 8px;color:#7fb2ff;border-bottom:1px solid #2a3340;padding-bottom:4px}\
         h3{font-size:14px;margin:16px 0 4px;color:#9fb0c8}\
         .sub{color:#8ea0b8;font-size:13px;margin:0 0 4px} .legend{color:#6f8097;font-size:12px;margin:2px 0 0}\
         table{border-collapse:collapse;width:100%;font-size:12.5px;margin:4px 0 6px} \
         th,td{padding:3px 8px;text-align:right;white-space:nowrap} th{color:#8ea0b8;font-weight:600;border-bottom:1px solid #2a3340;text-align:right}\
         td.l,th.l{text-align:left} tr:nth-child(even){background:#161b24} tr:hover{background:#1d2533}\
         .tier{font-weight:800;border-radius:4px;padding:1px 7px;color:#11151c}\
         .role{display:inline-block;min-width:16px;text-align:center;border-radius:3px;padding:0 4px;font-size:11px;font-weight:700;color:#11151c}\
         .r힐{background:#5ad17a}.r탱{background:#6db1ff}.r브{background:#46c7c7}.r딜{background:#ff7a7a}\
         .wr{font-weight:700} .grey{color:#6f8097} .cols{column-width:340px;column-gap:24px}",
    );
    h.push_str("</style></head><body>");
    h.push_str("<h1>TFM2 메타·챔피언 분석</h1>");
    h.push_str(&format!(
        "<p class=sub>패치 <b>{}</b> · 매치 <b>{}</b>판{}</p>",
        esc(&latest),
        total,
        if save_label.is_empty() { String::new() } else { format!(" · 세이브 <b>{}</b>", esc(&save_label)) }
    ));
    h.push_str("<p class=legend>티어 S~D = 현 패치 보정 승률(표본 적으면 50%로 수축) · 역할 힐/탱/브/딜 = 딜·탱·힐 데이터 · 픽률·밴률 = 전체 매치 대비 · 표본 부족 제외</p>");

    // ---- OP top list (overall, by champ-total shrunk winrate, games>=MIN_TOP) ----
    let mut tops: Vec<(&String, &Cell)> = champ_tot.iter().filter(|(_, c)| c.matches >= MIN_TOP).collect();
    tops.sort_by(|a, b| wr(b.1.wins, b.1.matches).partial_cmp(&wr(a.1.wins, a.1.matches)).unwrap());
    h.push_str("<h2>이번 패치 강캐 TOP</h2><table>");
    h.push_str("<tr><th class=l>티어</th><th class=l>챔프</th><th>역할</th><th>승률</th><th>판수</th><th>픽률</th><th>밴률</th><th>딜/판</th><th>탱/판</th><th>힐/판</th></tr>");
    for (champ, c) in tops.iter().take(15) {
        let d = wr(c.wins, c.matches) - 0.5;
        let (tl, tc) = tier(d);
        let rl = role(c.deal, c.tank, c.heal);
        h.push_str(&format!(
            "<tr><td class=l><span class=tier style='background:{tc}'>{tl}</span></td>\
             <td class=l>{name}</td><td>{role}</td>\
             <td class=wr style='color:{tc}'>{wrp:.1}%</td><td>{g}</td><td>{pk:.0}%</td><td>{bn:.0}%</td>\
             <td>{dl:.0}</td><td>{tk:.0}</td><td>{hl:.0}</td></tr>",
            tc = tc, tl = tl, name = esc(&ko_name(champ)),
            role = if rl.is_empty() { String::new() } else { format!("<span class='role r{rl}'>{rl}</span>", rl = rl) },
            wrp = wr(c.wins, c.matches) * 100.0, g = c.matches,
            pk = c.matches as f64 / total.max(1) as f64 * 100.0,
            bn = c.bans as f64 / total.max(1) as f64 * 100.0,
            dl = pg(c.deal, c.matches), tk = pg(c.tank, c.matches), hl = pg(c.heal, c.matches),
        ));
    }
    h.push_str("</table>");

    // ---- per-position tier tables ----
    h.push_str("<h2>라인별 티어</h2><div class=cols>");
    for (pos_en, pos_ko) in POS_KO {
        let mut list: Vec<(&String, &Cell)> = cells
            .iter()
            .filter(|((_, p), c)| p == pos_en && c.matches >= MIN_POS)
            .map(|((ch, _), c)| (ch, c))
            .collect();
        if list.is_empty() {
            continue;
        }
        list.sort_by(|a, b| wr(b.1.wins, b.1.matches).partial_cmp(&wr(a.1.wins, a.1.matches)).unwrap());
        h.push_str(&format!("<h3>{}</h3><table>", pos_ko));
        h.push_str("<tr><th class=l>티어</th><th class=l>챔프</th><th>역할</th><th>승률</th><th>판</th><th>픽률</th><th>딜/판</th><th>탱/판</th><th>힐/판</th><th>CS</th><th>골드</th></tr>");
        for (champ, c) in &list {
            let d = wr(c.wins, c.matches) - 0.5;
            let (tl, tc) = tier(d);
            let rl = role(c.deal, c.tank, c.heal);
            h.push_str(&format!(
                "<tr><td class=l><span class=tier style='background:{tc}'>{tl}</span></td>\
                 <td class=l>{name}</td><td>{role}</td>\
                 <td class=wr style='color:{tc}'>{wrp:.1}%</td><td>{g}</td><td>{pk:.0}%</td>\
                 <td>{dl:.0}</td><td>{tk:.0}</td><td>{hl:.0}</td><td>{cs:.0}</td><td>{gold:.0}</td></tr>",
                tc = tc, tl = tl, name = esc(&ko_name(champ)),
                role = if rl.is_empty() { String::new() } else { format!("<span class='role r{rl}'>{rl}</span>", rl = rl) },
                wrp = wr(c.wins, c.matches) * 100.0, g = c.matches,
                pk = c.matches as f64 / total.max(1) as f64 * 100.0,
                dl = pg(c.deal, c.matches), tk = pg(c.tank, c.matches), hl = pg(c.heal, c.matches),
                cs = pg(c.cs, c.matches), gold = pg(c.gold, c.matches),
            ));
        }
        h.push_str("</table>");
    }
    h.push_str("</div>");
    h.push_str("<p class=legend style='margin-top:18px'>tfm2_db_export 모드가 읽은 라이브 게임 DB 기반 · 게임 진행할수록 표본이 늘어 정확해집니다.</p>");
    h.push_str("</body></html>");

    if let Err(e) = std::fs::write(&args[2], &h) {
        eprintln!("write {} failed: {}", args[2], e);
        std::process::exit(1);
    }
    eprintln!("wrote {} ({} bytes, {} champ-pos cells)", args[2], h.len(), cells.len());
}

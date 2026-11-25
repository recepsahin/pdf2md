//! Konum tabanlı çıkarım:
//!   * karakter koordinatlarından satırlar (içerik akışı sırasıyla),
//!   * hücre çizgilerinden (ince dikdörtgenler / çizgiler) tablo ızgarası -> Markdown tablo,
//!   * üst/alt bilgi: sayfa kenar şeridi + sayfalar arası tekrar.
//!
//! Algoritma gerçek bir datasheet (bq24296M) üzerinde Python'da doğrulandıktan sonra
//! buraya taşındı.

use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

/// Bir sayfanın çıktı parçaları: düz metin (başlık/liste sezgisine girer) veya hazır Markdown tablo.
pub enum PageItem {
    Text(String),
    Table(String),
}

type OResult = std::result::Result<(), pdf_extract::OutputError>;

const THIN: f64 = 2.0; // bundan ince dikdörtgen = çizgi (pt)
const TOL: f64 = 1.5; // koordinat birleştirme toleransı (pt)
const MAX_CELL_H: f64 = 70.0; // ızgara hücreleri bundan yüksekse şekil ızgarasıdır, tablo değil
const FILL_MIN: f64 = 0.45; // dolu hücre oranı alt sınırı
const BAND: f64 = 0.10; // sayfa yüksekliğinin üst/alt %10'u: üst/alt bilgi şeridi

// ---------------------------------------------------------------------------
// Veri yapıları
// ---------------------------------------------------------------------------

struct Ch {
    x: f64,    // başlangıç x
    y: f64,    // taban çizgisi (aşağı doğru artar)
    w: f64,    // genişlik (pt)
    size: f64, // punto
    s: String,
    ws: bool,   // bir kelime/dize diliminin ilk karakteri mi (begin_word sonrası)
    vdir: i8,   // 0: yatay metin; -1/+1: dikey metin (y'de azalan/artan yönde ilerler)
    form: bool, // Form XObject içinden mi (pdf-extract bunların konumunu güvenilir vermiyor)
}

impl Ch {
    /// Karakterin yaklaşık merkezi (sayfa koordinatları, y aşağı doğru artar).
    fn center(&self) -> (f64, f64) {
        if self.vdir == 0 {
            (self.x + self.w / 2.0, self.y - 0.3 * self.size)
        } else {
            let d = self.vdir as f64;
            (self.x + d * 0.3 * self.size, self.y + d * self.w / 2.0)
        }
    }
}

#[derive(Clone, Copy)]
struct Rect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
} // y aşağı doğru artar

struct PageData {
    width: f64,
    height: f64,
    chars: Vec<Ch>,
    rects: Vec<Rect>,
}

struct Collector {
    pages: Vec<PageData>,
    next_word: bool,
}

impl pdf_extract::OutputDev for Collector {
    fn begin_page(
        &mut self,
        _page_num: u32,
        media_box: &pdf_extract::MediaBox,
        _art_box: Option<(f64, f64, f64, f64)>,
    ) -> OResult {
        self.pages.push(PageData {
            width: media_box.urx - media_box.llx,
            height: media_box.ury - media_box.lly,
            chars: Vec::new(),
            rects: Vec::new(),
        });
        Ok(())
    }

    fn end_page(&mut self) -> OResult {
        Ok(())
    }

    fn output_character(
        &mut self,
        trm: &pdf_extract::Transform,
        width: f64,
        _spacing: f64,
        font_size: f64,
        char: &str,
    ) -> OResult {
        if char.trim().is_empty() {
            // açık boşluk karakteri: konumu önemsiz, sonraki karakter yeni kelime başlar
            self.next_word = true;
            return Ok(());
        }
        let ws = self.next_word;
        self.next_word = false;
        if let Some(p) = self.pages.last_mut() {
            // pdf-extract'ın PlainTextOutput'uyla aynı hesap: dönüşümlü punto.
            let fx = font_size * (trm.m11 + trm.m21);
            let fy = font_size * (trm.m12 + trm.m22);
            let size = (fx * fy).abs().sqrt();
            if size > 0.0 {
                // Dönük metin: yazı yönü y eksenine daha yakınsa
                let vdir: i8 = if trm.m12.abs() > trm.m11.abs() {
                    if trm.m12 > 0.0 {
                        -1
                    } else {
                        1
                    }
                } else {
                    0
                };
                p.chars.push(Ch {
                    x: trm.m31,
                    y: p.height - trm.m32,
                    w: width * size,
                    size,
                    s: char.to_string(),
                    ws,
                    vdir,
                    form: false,
                });
            }
        }
        Ok(())
    }

    fn begin_word(&mut self) -> OResult {
        self.next_word = true;
        Ok(())
    }
    fn end_word(&mut self) -> OResult {
        Ok(())
    }
    fn end_line(&mut self) -> OResult {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// İçerik akışından dikdörtgen / çizgi çıkarımı (hücre kenarlıkları)
// ---------------------------------------------------------------------------

fn num(o: &lopdf::Object) -> f64 {
    match o {
        lopdf::Object::Integer(i) => *i as f64,
        lopdf::Object::Real(r) => *r as f64,
        _ => 0.0,
    }
}

/// m ile c matrislerinin çarpımı (PDF satır-vektör kuralı: önce m, sonra c).
fn mul(m: [f64; 6], c: [f64; 6]) -> [f64; 6] {
    [
        m[0] * c[0] + m[1] * c[2],
        m[0] * c[1] + m[1] * c[3],
        m[2] * c[0] + m[3] * c[2],
        m[2] * c[1] + m[3] * c[3],
        m[4] * c[0] + m[5] * c[2] + c[4],
        m[4] * c[1] + m[5] * c[3] + c[5],
    ]
}

fn apply(c: &[f64; 6], x: f64, y: f64) -> (f64, f64) {
    (c[0] * x + c[2] * y + c[4], c[1] * x + c[3] * y + c[5])
}

fn commit(
    out: &mut Vec<Rect>,
    rects: &mut Vec<Rect>,
    segs: &mut Vec<(f64, f64, f64, f64)>,
    take_rects: bool,
    take_segs: bool,
) {
    if take_rects {
        out.append(rects);
    } else {
        rects.clear();
    }
    if take_segs {
        for (x0, y0, x1, y1) in segs.drain(..) {
            if (x1 - x0).abs() < 0.5 || (y1 - y0).abs() < 0.5 {
                out.push(Rect {
                    x0: x0.min(x1),
                    y0: y0.min(y1),
                    x1: x0.max(x1),
                    y1: y0.max(y1),
                });
            }
        }
    } else {
        segs.clear();
    }
}

fn page_rects(doc: &lopdf::Document, id: lopdf::ObjectId, page_h: f64) -> Vec<Rect> {
    let data = match doc.get_page_content(id) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let content = match lopdf::content::Content::decode(&data) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut ctm: [f64; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mut stack: Vec<[f64; 6]> = Vec::new();
    let mut pend_rects: Vec<Rect> = Vec::new();
    let mut pend_segs: Vec<(f64, f64, f64, f64)> = Vec::new();
    let mut cur: Option<(f64, f64)> = None;
    let mut out: Vec<Rect> = Vec::new();

    for op in &content.operations {
        let o = &op.operands;
        match op.operator.as_str() {
            "q" => stack.push(ctm),
            "Q" => {
                if let Some(m) = stack.pop() {
                    ctm = m;
                }
            }
            "cm" if o.len() >= 6 => {
                let m = [num(&o[0]), num(&o[1]), num(&o[2]), num(&o[3]), num(&o[4]), num(&o[5])];
                ctm = mul(m, ctm);
            }
            "re" if o.len() >= 4 => {
                let (x, y, w, h) = (num(&o[0]), num(&o[1]), num(&o[2]), num(&o[3]));
                let (ax, ay) = apply(&ctm, x, y);
                let (bx, by) = apply(&ctm, x + w, y + h);
                pend_rects.push(Rect {
                    x0: ax.min(bx),
                    y0: ay.min(by),
                    x1: ax.max(bx),
                    y1: ay.max(by),
                });
            }
            "m" if o.len() >= 2 => {
                cur = Some(apply(&ctm, num(&o[0]), num(&o[1])));
            }
            "l" if o.len() >= 2 => {
                let p = apply(&ctm, num(&o[0]), num(&o[1]));
                if let Some(c) = cur {
                    pend_segs.push((c.0, c.1, p.0, p.1));
                }
                cur = Some(p);
            }
            "f" | "F" | "f*" => {
                commit(&mut out, &mut pend_rects, &mut pend_segs, true, false);
                cur = None;
            }
            "S" | "s" | "B" | "B*" | "b" | "b*" => {
                commit(&mut out, &mut pend_rects, &mut pend_segs, true, true);
                cur = None;
            }
            "n" => {
                commit(&mut out, &mut pend_rects, &mut pend_segs, false, false);
                cur = None;
            }
            _ => {}
        }
    }

    // PDF'te y yukarı doğru artar; aşağı doğru artan düzene çevir.
    out.into_iter()
        .map(|r| Rect {
            x0: r.x0,
            x1: r.x1,
            y0: page_h - r.y1,
            y1: page_h - r.y0,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Geometri yardımcıları
// ---------------------------------------------------------------------------

struct Dsu {
    p: Vec<usize>,
}

impl Dsu {
    fn new(n: usize) -> Self {
        Dsu { p: (0..n).collect() }
    }
    fn find(&mut self, mut a: usize) -> usize {
        while self.p[a] != a {
            let g = self.p[self.p[a]];
            self.p[a] = g;
            a = g;
        }
        a
    }
    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.p[rb] = ra;
        }
    }
}

#[derive(Clone, Copy)]
struct Edge {
    c: f64, // sabit koordinat (yatay kenar için y, dikey için x)
    a: f64, // aralık başı
    b: f64, // aralık sonu
}

fn cluster(vals: &[f64], tol: f64) -> Vec<f64> {
    let mut v = vals.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let mut groups: Vec<Vec<f64>> = Vec::new();
    for x in v {
        let join = match groups.last() {
            Some(g) => x - g[g.len() - 1] <= tol,
            None => false,
        };
        if join {
            groups.last_mut().unwrap().push(x);
        } else {
            groups.push(vec![x]);
        }
    }
    groups
        .iter()
        .map(|g| g.iter().sum::<f64>() / g.len() as f64)
        .collect()
}

fn nearest(grid: &[f64], v: f64) -> usize {
    let mut best = 0;
    let mut bd = f64::MAX;
    for (i, g) in grid.iter().enumerate() {
        let d = (g - v).abs();
        if d < bd {
            bd = d;
            best = i;
        }
    }
    best
}

fn merge_intervals(mut iv: Vec<(f64, f64)>, tol: f64) -> Vec<(f64, f64)> {
    iv.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, f64)> = Vec::new();
    for (a, b) in iv {
        let join = match out.last() {
            Some(l) => a <= l.1 + tol,
            None => false,
        };
        if join {
            let l = out.last_mut().unwrap();
            if b > l.1 {
                l.1 = b;
            }
        } else {
            out.push((a, b));
        }
    }
    out
}

fn covers(iv: &[(f64, f64)], a: f64, b: f64, tol: f64) -> bool {
    iv.iter().any(|&(s, e)| s <= a + tol && e >= b - tol)
}

fn edges_from_rects(rects: &[Rect], pw: f64, ph: f64) -> (Vec<Edge>, Vec<Edge>) {
    let mut hs = Vec::new();
    let mut vs = Vec::new();
    for r in rects {
        let w = r.x1 - r.x0;
        let h = r.y1 - r.y0;
        if w > 0.9 * pw && h > 0.9 * ph {
            continue; // sayfa arka planı
        }
        if h <= THIN && w > THIN {
            hs.push(Edge { c: (r.y0 + r.y1) / 2.0, a: r.x0, b: r.x1 });
        } else if w <= THIN && h > THIN {
            vs.push(Edge { c: (r.x0 + r.x1) / 2.0, a: r.y0, b: r.y1 });
        } else if w <= THIN && h <= THIN {
            continue; // nokta
        } else {
            hs.push(Edge { c: r.y0, a: r.x0, b: r.x1 });
            hs.push(Edge { c: r.y1, a: r.x0, b: r.x1 });
            vs.push(Edge { c: r.x0, a: r.y0, b: r.y1 });
            vs.push(Edge { c: r.x1, a: r.y0, b: r.y1 });
        }
    }
    (hs, vs)
}

/// Aynı doğru üzerindeki (TOL içinde) kenarları birleştirir; parçalı çizgiler tek çizgi olur.
fn normalize_edges(e: &[Edge]) -> Vec<Edge> {
    let coords = cluster(&e.iter().map(|x| x.c).collect::<Vec<f64>>(), TOL);
    let mut by: Vec<Vec<(f64, f64)>> = vec![Vec::new(); coords.len()];
    for x in e {
        by[nearest(&coords, x.c)].push((x.a, x.b));
    }
    let mut out = Vec::new();
    for (k, iv) in by.into_iter().enumerate() {
        for (a, b) in merge_intervals(iv, 2.0 * TOL) {
            out.push(Edge { c: coords[k], a, b });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tablolar
// ---------------------------------------------------------------------------

struct C<'a> {
    x0: f64,
    x1: f64,
    cx: f64,
    cy: f64,
    size: f64,
    s: &'a str,
    ws: bool,
}

struct Table {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    md: String,
}

fn cell_text(mut cs: Vec<&C<'_>>) -> String {
    if cs.is_empty() {
        return String::new();
    }
    cs.sort_by(|a, b| a.cy.total_cmp(&b.cy).then(a.x0.total_cmp(&b.x0)));
    let mut lines: Vec<Vec<&C<'_>>> = Vec::new();
    for c in cs {
        let same = match lines.last() {
            Some(l) => (c.cy - l[l.len() - 1].cy).abs() <= 0.5 * c.size,
            None => false,
        };
        if same {
            lines.last_mut().unwrap().push(c);
        } else {
            lines.push(vec![c]);
        }
    }
    let mut parts: Vec<String> = Vec::new();
    for mut ln in lines {
        ln.sort_by(|a, b| a.x0.total_cmp(&b.x0));
        let mut s = String::new();
        let mut last: Option<f64> = None;
        for c in ln {
            if let Some(le) = last {
                if c.ws && c.x0 - le > 0.12 * c.size && !s.ends_with(' ') {
                    s.push(' ');
                }
            }
            s.push_str(c.s);
            last = Some(c.x1);
        }
        parts.push(s.trim().to_string());
    }
    let mut res = String::new();
    for l in parts {
        if res.is_empty() {
            res = l;
        } else if res.ends_with('-') && l.chars().next().map_or(false, |c| c.is_lowercase()) {
            res.pop();
            res.push_str(&l);
        } else {
            res.push(' ');
            res.push_str(&l);
        }
    }
    res.split_whitespace().collect::<Vec<&str>>().join(" ")
}

fn esc(s: &str) -> String {
    s.replace('|', "\\|")
}

fn build_table(hs: &[Edge], vs: &[Edge], cc: &[C<'_>]) -> Option<Table> {
    let xs = cluster(&vs.iter().map(|e| e.c).collect::<Vec<f64>>(), TOL);
    let ys = cluster(&hs.iter().map(|e| e.c).collect::<Vec<f64>>(), TOL);
    if xs.len() < 2 || ys.len() < 2 {
        return None;
    }
    let ncol = xs.len() - 1;
    let nrow = ys.len() - 1;

    // Şekil ızgaralarını (grafik çerçeveleri vb.) ele: hücreler çok yüksek.
    let mut hts: Vec<f64> = (0..nrow).map(|j| ys[j + 1] - ys[j]).collect();
    hts.sort_by(|a, b| a.total_cmp(b));
    let median = hts[hts.len() / 2];
    let maxh = hts[hts.len() - 1];
    if median > MAX_CELL_H || maxh > 2.0 * MAX_CELL_H {
        return None;
    }

    // Her ızgara çizgisi için birleştirilmiş kapsama aralıkları
    let mut hcov: Vec<Vec<(f64, f64)>> = vec![Vec::new(); ys.len()];
    for e in hs {
        hcov[nearest(&ys, e.c)].push((e.a, e.b));
    }
    let mut vcov: Vec<Vec<(f64, f64)>> = vec![Vec::new(); xs.len()];
    for e in vs {
        vcov[nearest(&xs, e.c)].push((e.a, e.b));
    }
    let hcov: Vec<Vec<(f64, f64)>> = hcov.into_iter().map(|iv| merge_intervals(iv, TOL)).collect();
    let vcov: Vec<Vec<(f64, f64)>> = vcov.into_iter().map(|iv| merge_intervals(iv, TOL)).collect();

    let locate = |cx: f64, cy: f64| -> Option<(usize, usize)> {
        if cx < xs[0] || cx > xs[xs.len() - 1] || cy < ys[0] || cy > ys[ys.len() - 1] {
            return None;
        }
        let i = xs.partition_point(|&x| x <= cx).saturating_sub(1).min(ncol - 1);
        let j = ys.partition_point(|&y| y <= cy).saturating_sub(1).min(nrow - 1);
        Some((i, j))
    };

    // Izgara satırlarına göre karakterler (metin bir sınırı kesiyor mu testi için)
    let mut inrow: Vec<Vec<usize>> = vec![Vec::new(); nrow];
    for (idx, c) in cc.iter().enumerate() {
        if let Some((_, j)) = locate(c.cx, c.cy) {
            inrow[j].push(idx);
        }
    }
    let crosses = |j: usize, x: f64| -> bool {
        let mut idx = inrow[j].clone();
        idx.sort_by(|&a, &b| {
            let ka = (cc[a].cy / 3.0).round() as i64;
            let kb = (cc[b].cy / 3.0).round() as i64;
            ka.cmp(&kb).then(cc[a].x0.total_cmp(&cc[b].x0))
        });
        for w in idx.windows(2) {
            let a = &cc[w[0]];
            let b = &cc[w[1]];
            if (a.cy - b.cy).abs() <= 0.5 * a.size
                && b.x0 - a.x1 <= 0.6 * a.size
                && a.cx < x
                && x <= b.cx
            {
                return true;
            }
        }
        false
    };

    // Hücre birleştirme:
    //  - üst kenarlık eksikse alttaki hücre üsttekiyle birleşir (dikey birleşik hücre),
    //  - sol kenarlık eksik VE metin o sınırı kesiyorsa yatay birleşir.
    //    (Veri satırlarında MIN|TYP|MAX arasında çizgi olmaması birleşme anlamına gelmez.)
    let mut dsu = Dsu::new(ncol * nrow);
    for j in 0..nrow {
        for i in 0..ncol {
            let top_ok = covers(&hcov[j], xs[i], xs[i + 1], TOL);
            let left_ok = covers(&vcov[i], ys[j], ys[j + 1], TOL);
            if j > 0 && !top_ok {
                dsu.union((j - 1) * ncol + i, j * ncol + i);
            }
            if i > 0 && !left_ok && crosses(j, xs[i]) {
                dsu.union(j * ncol + i - 1, j * ncol + i);
            }
        }
    }
    let root_of: Vec<usize> = (0..ncol * nrow).map(|k| dsu.find(k)).collect();

    // Hücre metinleri
    let mut cell_chars: HashMap<usize, Vec<&C<'_>>> = HashMap::new();
    for c in cc {
        if let Some((i, j)) = locate(c.cx, c.cy) {
            cell_chars.entry(root_of[j * ncol + i]).or_default().push(c);
        }
    }
    let mut texts: HashMap<usize, String> = HashMap::new();
    for (r, v) in cell_chars {
        texts.insert(r, cell_text(v));
    }

    let roots: HashSet<usize> = root_of.iter().cloned().collect();
    let nonempty = roots
        .iter()
        .filter(|r| texts.get(*r).map_or(false, |t| !t.is_empty()))
        .count();
    let fill = nonempty as f64 / roots.len().max(1) as f64;
    if nonempty < 4 || fill < FILL_MIN {
        return None;
    }
    // Pin numarası diyagramı gibi: tüm dolu hücreler 1-2 haneli tam sayı
    let all_small_ints = texts
        .values()
        .filter(|t| !t.is_empty())
        .all(|t| t.len() <= 2 && t.chars().all(|c| c.is_ascii_digit()));
    if all_small_ints {
        return None;
    }

    // Birleşik hücre geometrisi: [ilk sütun, son sütun, ilk satır, son satır]
    let mut span: HashMap<usize, [usize; 4]> = HashMap::new();
    for j in 0..nrow {
        for i in 0..ncol {
            let r = root_of[j * ncol + i];
            let a = span.entry(r).or_insert([i, i, j, j]);
            a[0] = a[0].min(i);
            a[1] = a[1].max(i);
            a[2] = a[2].min(j);
            a[3] = a[3].max(j);
        }
    }

    // Başlık bloğu: 0. satır + 0. satırdan başlayıp aşağı uzanan dikey birleşik hücreler
    let mut h = 1usize;
    loop {
        let mut changed = false;
        for (r, a) in &span {
            let has_text = texts.get(r).map_or(false, |t| !t.is_empty());
            if a[2] == 0 && a[3] >= h && has_text {
                h = a[3] + 1;
                changed = true;
            }
        }
        if !changed || h >= nrow {
            break;
        }
    }
    let h = h.min(nrow.saturating_sub(1).max(1));

    let mut header: Vec<String> = Vec::new();
    for i in 0..ncol {
        let mut parts: Vec<&str> = Vec::new();
        let mut seen: HashSet<usize> = HashSet::new();
        for j in 0..h {
            let r = root_of[j * ncol + i];
            if seen.insert(r) {
                if let Some(t) = texts.get(&r) {
                    if !t.is_empty() {
                        parts.push(t.as_str());
                    }
                }
            }
        }
        header.push(parts.join(" "));
    }

    let mut body: Vec<Vec<String>> = Vec::new();
    for j in h..nrow {
        let mut row: Vec<String> = Vec::with_capacity(ncol);
        for i in 0..ncol {
            let r = root_of[j * ncol + i];
            let t = texts.get(&r).cloned().unwrap_or_default();
            let a = span[&r];
            if i != a[0] {
                row.push(String::new()); // yatay birleşik: metin yalnızca ilk sütunda
            } else if j == a[2].max(h) || (a[3] > a[2] && t.chars().count() <= 60) {
                row.push(t); // dikey birleşik: kısa metni her satırda tekrarla
            } else {
                row.push(String::new());
            }
        }
        if row.iter().any(|x| !x.is_empty()) {
            body.push(row);
        }
    }

    let mut rows: Vec<Vec<String>> = vec![header];
    rows.extend(body);
    let keep: Vec<usize> = (0..ncol)
        .filter(|&i| rows.iter().any(|r| !r[i].is_empty()))
        .collect();
    if rows.len() < 2 || keep.len() < 2 {
        return None;
    }
    let rows: Vec<Vec<String>> = rows
        .into_iter()
        .map(|r| keep.iter().map(|&i| r[i].clone()).collect())
        .collect();

    let mut md = String::new();
    for (n, r) in rows.iter().enumerate() {
        let cells: Vec<String> = r.iter().map(|x| esc(x)).collect();
        md.push_str(&format!("| {} |\n", cells.join(" | ")));
        if n == 0 {
            md.push_str(&format!("|{}|\n", vec![" --- "; keep.len()].join("|")));
        }
    }

    Some(Table {
        x0: xs[0],
        y0: ys[0],
        x1: xs[xs.len() - 1],
        y1: ys[ys.len() - 1],
        md: md.trim_end().to_string(),
    })
}

fn detect_tables(p: &PageData) -> Vec<Table> {
    let (hs, vs) = edges_from_rects(&p.rects, p.width, p.height);
    let hs = normalize_edges(&hs);
    let vs = normalize_edges(&vs);
    let nh = hs.len();
    if nh < 2 || vs.len() < 2 {
        return Vec::new();
    }

    // Form XObject karakterlerinin konumu güvenilmez: tablo hücrelerine alınmaz.
    let cc: Vec<C<'_>> = p
        .chars
        .iter()
        .filter(|c| !c.form)
        .map(|c| {
            let (cx, cy) = c.center();
            C {
                x0: if c.vdir == 0 { c.x } else { cx - c.size / 2.0 },
                x1: if c.vdir == 0 { c.x + c.w } else { cx + c.size / 2.0 },
                cx,
                cy,
                size: c.size,
                s: c.s.as_str(),
                ws: c.ws,
            }
        })
        .collect();

    // Kenarları kesişimlerine göre bağlı bileşenlere ayır
    let mut dsu = Dsu::new(nh + vs.len());
    for (i, e) in hs.iter().enumerate() {
        for (j, f) in vs.iter().enumerate() {
            if e.a - TOL <= f.c && f.c <= e.b + TOL && f.a - TOL <= e.c && e.c <= f.b + TOL {
                dsu.union(i, nh + j);
            }
        }
    }
    let mut group_of: HashMap<usize, usize> = HashMap::new();
    let mut groups: Vec<(Vec<Edge>, Vec<Edge>)> = Vec::new();
    for k in 0..nh + vs.len() {
        let root = dsu.find(k);
        let g = *group_of.entry(root).or_insert_with(|| {
            groups.push((Vec::new(), Vec::new()));
            groups.len() - 1
        });
        if k < nh {
            groups[g].0.push(hs[k]);
        } else {
            groups[g].1.push(vs[k - nh]);
        }
    }

    let mut tables = Vec::new();
    for (g_h, g_v) in &groups {
        if g_h.len() >= 2 && g_v.len() >= 2 {
            if let Some(t) = build_table(g_h, g_v, &cc) {
                tables.push(t);
            }
        }
    }
    tables
}

// ---------------------------------------------------------------------------
// Satırlar (içerik akışı sırası) ve üst/alt bilgi
// ---------------------------------------------------------------------------

struct Line {
    text: String,
    y: f64,
    size: f64,
    form: bool, // Form XObject metni: konumu güvenilmez (üst/alt bilgi sayılmaz, tabloya girmez)
}

enum Seg {
    Line(Line),
    Table(usize),
}

struct LineB {
    text: String,
    y: f64,
    size: f64,
    last_end: f64,
    form: bool,
}

struct VLine {
    text: String,
    x: f64,
    y: f64,
    size: f64,
    dir: f64,
    last_end: f64,
    form: bool,
}

fn flush(cur: &mut Option<LineB>, segs: &mut Vec<Seg>) {
    if let Some(l) = cur.take() {
        let t = l.text.trim().to_string();
        if !t.is_empty() {
            segs.push(Seg::Line(Line { text: t, y: l.y, size: l.size, form: l.form }));
        }
    }
}

fn flush_v(cur: &mut Option<VLine>, segs: &mut Vec<Seg>) {
    if let Some(l) = cur.take() {
        let t = l.text.trim().to_string();
        if !t.is_empty() {
            segs.push(Seg::Line(Line { text: t, y: l.y, size: l.size, form: l.form }));
        }
    }
}

fn build_segs(p: &PageData, tables: &[Table]) -> Vec<Seg> {
    let mut segs: Vec<Seg> = Vec::new();
    let mut emitted = vec![false; tables.len()];
    let mut cur: Option<LineB> = None;
    let mut vcur: Option<VLine> = None;

    for ch in &p.chars {
        let (cx, cy) = ch.center();

        // Tablo içindeki (güvenilir konumlu) karakterler: tablo ilk görüldüğünde bir kez yazılır
        if !ch.form {
            let in_table = tables.iter().position(|t| {
                cx >= t.x0 - 1.0 && cx <= t.x1 + 1.0 && cy >= t.y0 - 1.0 && cy <= t.y1 + 1.0
            });
            if let Some(k) = in_table {
                if !emitted[k] {
                    flush(&mut cur, &mut segs);
                    flush_v(&mut vcur, &mut segs);
                    segs.push(Seg::Table(k));
                    emitted[k] = true;
                }
                continue;
            }
        }

        if ch.vdir != 0 {
            // Dikey metin (eksen başlıkları, dönük pin etiketleri): tek satırda birleştir
            flush(&mut cur, &mut segs);
            let dir = ch.vdir as f64;
            let same = match &vcur {
                Some(v) => {
                    let along = (ch.y - v.last_end) * v.dir;
                    v.form == ch.form
                        && v.dir == dir
                        && (ch.x - v.x).abs() <= 0.6 * v.size.max(ch.size)
                        && along > -1.0 * ch.size
                        && along < 2.5 * ch.size
                }
                None => false,
            };
            if same {
                let v = vcur.as_mut().unwrap();
                let along = (ch.y - v.last_end) * v.dir;
                if ch.ws && along > 0.1 * ch.size {
                    v.text.push(' ');
                }
                v.text.push_str(&ch.s);
                v.last_end = ch.y + dir * ch.w;
            } else {
                flush_v(&mut vcur, &mut segs);
                vcur = Some(VLine {
                    text: ch.s.clone(),
                    x: ch.x,
                    y: ch.y,
                    size: ch.size,
                    dir,
                    last_end: ch.y + dir * ch.w,
                    form: ch.form,
                });
            }
            continue;
        }

        flush_v(&mut vcur, &mut segs);
        let same_line = match &cur {
            Some(l) => l.form == ch.form && (ch.y - l.y).abs() <= 0.5 * l.size.max(ch.size),
            None => false,
        };
        if same_line {
            let l = cur.as_mut().unwrap();
            let gap = ch.x - l.last_end;
            // boşluk: ileri doğru küçük bir aralık ya da geriye büyük sıçrama (başka parça)
            if ch.ws && (gap > 0.1 * ch.size || gap < -1.0 * ch.size) {
                l.text.push(' ');
            }
            l.text.push_str(&ch.s);
            l.last_end = ch.x + ch.w;
        } else {
            flush(&mut cur, &mut segs);
            cur = Some(LineB {
                text: ch.s.clone(),
                y: ch.y,
                size: ch.size,
                last_end: ch.x + ch.w,
                form: ch.form,
            });
        }
    }
    flush(&mut cur, &mut segs);
    flush_v(&mut vcur, &mut segs);
    segs
}

/// Sıralı karakter kümesi: rakamlar ve boşluklar yok sayılır, kelime sırasından bağımsız.
fn sig(s: &str) -> String {
    let mut v: Vec<char> = s
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_ascii_digit())
        .collect();
    v.sort_unstable();
    v.into_iter().collect()
}

fn in_band(y: f64, h: f64) -> bool {
    y < BAND * h || y > (1.0 - BAND) * h
}

// ---------------------------------------------------------------------------
// Dış arayüz
// ---------------------------------------------------------------------------

/// PDF'i okur ve her sayfa için parça listesi döndürür.
/// `detect_tables`: false ise tablo tespiti yapılmaz.
fn collect(doc: &lopdf::Document) -> Result<Vec<PageData>> {
    let mut col = Collector { pages: Vec::new(), next_word: true };
    let res = catch_unwind(AssertUnwindSafe(|| pdf_extract::output_doc(doc, &mut col)));
    match res {
        Ok(Ok(())) => Ok(col.pages),
        Ok(Err(e)) => Err(anyhow!("PDF okunamadı: {e}")),
        Err(_) => Err(anyhow!("PDF ayrıştırıcı çöktü")),
    }
}

/// Sayfa içerik akışlarından `Do` (XObject çağrısı) operatörlerini çıkarılmış bir kopya yükler.
/// Hiçbir sayfa değişmediyse None döner.
fn load_without_do(path: &Path) -> Option<lopdf::Document> {
    let mut doc = lopdf::Document::load(path).ok()?;
    let ids: Vec<lopdf::ObjectId> = doc.get_pages().values().cloned().collect();
    let mut changed = false;
    for id in ids {
        let data = match doc.get_page_content(id) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let mut content = match lopdf::content::Content::decode(&data) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let before = content.operations.len();
        content.operations.retain(|op| op.operator != "Do");
        if content.operations.len() == before {
            continue;
        }
        if let Ok(bytes) = content.encode() {
            if doc.change_page_content(id, bytes).is_ok() {
                changed = true;
            }
        }
    }
    if changed {
        Some(doc)
    } else {
        None
    }
}

fn same_char(a: &Ch, b: &Ch) -> bool {
    a.s == b.s && (a.x - b.x).abs() < 1e-6 && (a.y - b.y).abs() < 1e-6
}

/// `Do`'suz çıkarımda bulunmayan karakterler Form XObject içindendir: işaretle.
fn mark_form_chars(pages: &mut [PageData], stripped: &[PageData]) {
    if pages.len() != stripped.len() {
        return;
    }
    for (p, s) in pages.iter_mut().zip(stripped.iter()) {
        if p.chars.len() == s.chars.len() {
            continue;
        }
        let mut flags = vec![false; p.chars.len()];
        let mut ptr = 0usize;
        for (i, c) in p.chars.iter().enumerate() {
            if ptr < s.chars.len() && same_char(c, &s.chars[ptr]) {
                ptr += 1;
            } else {
                flags[i] = true;
            }
        }
        if ptr != s.chars.len() {
            continue; // eşleşme tutarsız: dokunma
        }
        for (c, f) in p.chars.iter_mut().zip(flags) {
            c.form = f;
        }
    }
}

pub fn extract(path: &Path, detect: bool) -> Result<Vec<Vec<PageItem>>> {
    let doc = lopdf::Document::load(path).map_err(|e| anyhow!("PDF yüklenemedi: {e}"))?;

    let mut pages = collect(&doc)?;
    if let Some(stripped_doc) = load_without_do(path) {
        if let Ok(stripped) = collect(&stripped_doc) {
            mark_form_chars(&mut pages, &stripped);
        }
    }

    if detect {
        let ids: Vec<lopdf::ObjectId> = doc.get_pages().values().cloned().collect();
        if ids.len() == pages.len() {
            for (p, id) in pages.iter_mut().zip(ids.iter()) {
                p.rects = page_rects(&doc, *id, p.height);
            }
        }
    }

    let tables: Vec<Vec<Table>> = pages
        .iter()
        .map(|p| if detect { detect_tables(p) } else { Vec::new() })
        .collect();
    let all_segs: Vec<Vec<Seg>> = pages
        .iter()
        .zip(tables.iter())
        .map(|(p, t)| build_segs(p, t))
        .collect();

    // Üst/alt bilgi: kenar şeridindeki satırların imzası kaç sayfada geçiyor?
    let n = pages.len();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (pi, segs) in all_segs.iter().enumerate() {
        let mut seen: HashSet<String> = HashSet::new();
        for seg in segs {
            if let Seg::Line(l) = seg {
                if !l.form && in_band(l.y, pages[pi].height) {
                    seen.insert(sig(&l.text));
                }
            }
        }
        for s in seen {
            *counts.entry(s).or_insert(0) += 1;
        }
    }
    let is_header_footer = |text: &str, y: f64, h: f64, form: bool| -> bool {
        if form || !in_band(y, h) {
            return false;
        }
        let sg = sig(text);
        let low = sg.to_lowercase();
        // yalnızca sayfa numarası: "12", "Page 3", "3 of 10", "Sayfa 4"
        if low.is_empty() || low == "page" || low == "sayfa" || low == "pageof" || low == "of" {
            return true;
        }
        if sg.chars().count() < 3 {
            return false;
        }
        let c = counts.get(&sg).copied().unwrap_or(0);
        c >= 3 || (n >= 2 && c == n)
    };

    // Parçaları birleştir
    let mut result: Vec<Vec<PageItem>> = Vec::with_capacity(n);
    for (pi, segs) in all_segs.into_iter().enumerate() {
        let h = pages[pi].height;
        let mut items: Vec<PageItem> = Vec::new();
        let mut buf = String::new();
        let mut prev: Option<(f64, f64, bool)> = None;
        for seg in segs {
            match seg {
                Seg::Line(l) => {
                    if is_header_footer(&l.text, l.y, h, l.form) {
                        continue;
                    }
                    if let Some((py, ps, pf)) = prev {
                        let dy = l.y - py;
                        // Form metinleri (şema/grafik etiketleri) tek paragrafta toplanır
                        let brk = if pf && l.form {
                            false
                        } else {
                            pf != l.form || dy > 1.5 * ps || dy < -0.5 * ps
                        };
                        if brk {
                            buf.push('\n'); // paragraf boşluğu
                        }
                    }
                    buf.push_str(&l.text);
                    buf.push('\n');
                    prev = Some((l.y, l.size, l.form));
                }
                Seg::Table(k) => {
                    if !buf.trim().is_empty() {
                        items.push(PageItem::Text(std::mem::take(&mut buf)));
                    } else {
                        buf.clear();
                    }
                    items.push(PageItem::Table(tables[pi][k].md.clone()));
                    prev = None;
                }
            }
        }
        if !buf.trim().is_empty() {
            items.push(PageItem::Text(buf));
        }
        result.push(items);
    }
    Ok(result)
}

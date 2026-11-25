//! pdf2md: PDF dosyasındaki metni çıkarıp Markdown'a çeviren yerel konsol aracı.
//!
//! Kullanım:
//!     pdf2md girdi.pdf -o cikti.md
//!
//! Not: Yalnızca metin tabanlı PDF'lerle çalışır (OCR yok). Başlık, liste ve
//! paragraf tespiti sezgiseldir. Hücre çizgileri olan tablolar Markdown tabloya çevrilir
//! (layout özelliği, varsayılan açık); görseller ve denklemler dönüştürülmez.

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

#[cfg(feature = "layout")]
mod layout;

#[derive(Parser)]
#[command(name = "pdf2md", version, about = "PDF dosyasını Markdown'a çevirir (yerel, OCR yok)")]
struct Args {
    /// Dönüştürülecek PDF dosyası
    input: PathBuf,

    /// Üretilecek Markdown dosyasının adı (verilmezse girdi adı + .md)
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    output: Option<PathBuf>,

    /// Sayfa işaretçilerini (<!-- page N -->) ekleme
    #[arg(long)]
    no_page_markers: bool,

    /// TAMAMEN BÜYÜK HARFLİ kısa satırları da başlık say
    /// (datasheet'lerde pin/tablo etiketlerinde yanlış pozitif verir, varsayılan kapalı)
    #[arg(long)]
    caps_headings: bool,

    /// Tablo tespitini kapat (konum tabanlı çıkarım ve üst/alt bilgi temizliği yine çalışır)
    #[arg(long)]
    no_tables: bool,

    /// Konum tabanlı çıkarımı kullanma; eski, yalnızca metin tabanlı yolu kullan
    #[arg(long)]
    simple: bool,
}

enum Block {
    Heading(String),
    Para(String),
    Item(String),
}

fn main() -> Result<()> {
    let args = Args::parse();

    if !args.input.is_file() {
        bail!("Girdi dosyası bulunamadı: {}", args.input.display());
    }
    let output = args
        .output
        .clone()
        .unwrap_or_else(|| args.input.with_extension("md"));

    // 1) Konum tabanlı yol (tablolar + üst/alt bilgi temizliği); olmazsa 2) basit metin yolu
    let (bodies, tables) = match try_layout(&args) {
        Some(r) => r,
        None => (convert_simple(&args)?, 0),
    };

    if bodies.iter().all(|b| b.trim().is_empty()) {
        eprintln!(
            "Uyarı: Hiç metin çıkarılamadı. PDF taranmış (görüntü) olabilir; OCR gerekir."
        );
    }

    let mut md = String::new();
    for (i, body) in bodies.iter().enumerate() {
        if body.trim().is_empty() {
            continue;
        }
        if !args.no_page_markers {
            md.push_str(&format!("<!-- page {} -->\n\n", i + 1));
        }
        md.push_str(body);
        md.push_str("\n\n");
    }

    let md = md
        .replace("I 2C", "I²C")
        .replace('\u{fb00}', "ff")
        .replace('\u{fb01}', "fi")
        .replace('\u{fb02}', "fl")
        .replace('\u{fb03}', "ffi")
        .replace('\u{fb04}', "ffl");
    fs::write(&output, md.trim_end().to_string() + "\n")
        .with_context(|| format!("Yazılamadı: {}", output.display()))?;

    eprintln!(
        "Tamam: {} sayfa, {} tablo -> {}",
        bodies.len(),
        tables,
        output.display()
    );
    Ok(())
}

/// Eski yol: pdf-extract'ın düz metni + metin tabanlı üst/alt bilgi tespiti.
fn convert_simple(args: &Args) -> Result<Vec<String>> {
    // Bazı bozuk PDF'lerde ayrıştırıcı panic edebilir; bunu düzgün hataya çevir.
    let pages = catch_unwind(AssertUnwindSafe(|| {
        pdf_extract::extract_text_by_pages(&args.input)
    }))
    .map_err(|_| anyhow!("PDF ayrıştırıcı çöktü (bozuk veya desteklenmeyen PDF olabilir)"))?
    .map_err(|e| anyhow!("PDF okunamadı: {e}"))?;

    let repeated = find_repeated_lines(&pages);
    Ok(pages
        .iter()
        .map(|p| convert_page(p, &repeated, args.caps_headings))
        .collect())
}

#[cfg(feature = "layout")]
fn try_layout(args: &Args) -> Option<(Vec<String>, usize)> {
    if args.simple {
        return None;
    }
    let pages = match layout::extract(&args.input, !args.no_tables) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Uyarı: konum tabanlı çıkarım başarısız ({e}); basit moda geçiliyor.");
            return None;
        }
    };
    let none = HashSet::new();
    let mut table_count = 0usize;
    let mut bodies: Vec<String> = Vec::with_capacity(pages.len());
    for items in pages {
        let mut parts: Vec<String> = Vec::new();
        for item in items {
            match item {
                layout::PageItem::Text(t) => {
                    let b = convert_page(&t, &none, args.caps_headings);
                    if !b.trim().is_empty() {
                        parts.push(b);
                    }
                }
                layout::PageItem::Table(md) => {
                    table_count += 1;
                    parts.push(md);
                }
            }
        }
        bodies.push(parts.join("\n\n"));
    }
    Some((bodies, table_count))
}

#[cfg(not(feature = "layout"))]
fn try_layout(_args: &Args) -> Option<(Vec<String>, usize)> {
    None
}

/// Rakamları '#' ile değiştirerek satırı normalleştirir ("Sayfa 3" ~ "Sayfa 4").
fn normalize(line: &str) -> String {
    // Boşluk ve rakamları atıp karakterleri sıralar: "www.ti.com SLUSBU3B ..." ile
    // "SLUSBU3B ... www.ti.com" (veya yapışık "FeedbackCopyright") aynı anahtarı verir.
    let mut v: Vec<char> = line
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_ascii_digit())
        .collect();
    v.sort_unstable();
    v.into_iter().collect()
}

/// Sayfaların yarısından fazlasında tekrar eden kısa satırlar (üst/alt bilgi).
fn find_repeated_lines(pages: &[String]) -> HashSet<String> {
    if pages.len() < 3 {
        return HashSet::new();
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    for p in pages {
        let mut seen = HashSet::new();
        for l in p.lines() {
            let t = l.trim();
            if t.is_empty() || t.chars().count() > 160 {
                continue;
            }
            if t.chars().filter(|c| c.is_alphabetic()).count() < 6 {
                continue;
            }
            seen.insert(normalize(t));
        }
        for s in seen {
            *counts.entry(s).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .filter(|(_, c)| c * 2 > pages.len())
        .map(|(s, _)| s)
        .collect()
}

fn is_noise(line: &str, repeated: &HashSet<String>) -> bool {
    // Tek başına sayfa numarası
    if line.chars().count() <= 4 && line.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    repeated.contains(&normalize(line))
}

fn flush(current: &mut Option<Block>, blocks: &mut Vec<Block>) {
    if let Some(b) = current.take() {
        blocks.push(b);
    }
}

fn append_line(text: &mut String, line: &str) {
    // Satır sonu tire birleştirme: "mikro-" + "denetleyici" -> "mikrodenetleyici"
    let hyphen_join = text.ends_with('-')
        && text
            .chars()
            .rev()
            .nth(1)
            .map_or(false, |c| c.is_alphabetic())
        && line.chars().next().map_or(false, |c| c.is_lowercase());
    if hyphen_join {
        text.pop();
    } else {
        text.push(' ');
    }
    text.push_str(line);
}

/// İçindekiler satırı: "7.5 Electrical Characteristics ........ 7" -> "- 7.5 Electrical Characteristics (p. 7)"
fn detect_toc_line(line: &str) -> Option<String> {
    let strip = |t: &str| -> String {
        t.trim()
            .trim_start_matches(|c: char| "•●▪◦·–-*".contains(c) || c.is_whitespace())
            .to_string()
    };
    // Noktasız içindekiler satırı: "12.2 Receiving Notification of Updates 44"
    if !line.contains("...") {
        let (head, tail) = line.rsplit_once(' ')?;
        let first = head.split_whitespace().next()?;
        let numbered = first.chars().next().map_or(false, |c| c.is_ascii_digit())
            && first.chars().all(|c| c.is_ascii_digit() || c == '.');
        if numbered
            && tail.len() <= 3
            && !tail.is_empty()
            && tail.chars().all(|c| c.is_ascii_digit())
            && head.split_whitespace().count() >= 4
            && head.chars().filter(|c| c.is_alphabetic()).count() >= 10
        {
            return Some(format!("- {} (p. {})", head.trim(), tail));
        }
        return None;
    }
    let idx = line.find("...")?;
    let title_owned = strip(&line[..idx]);
    let title = title_owned.as_str();
    let page = line[idx..].trim_start_matches(|c: char| c == '.' || c.is_whitespace());
    if title.is_empty() || page.is_empty() || page.len() > 4 {
        return None;
    }
    if !page.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("- {} (p. {})", title, page))
}

fn convert_page(text: &str, repeated: &HashSet<String>, caps_headings: bool) -> String {
    let mut blocks: Vec<Block> = Vec::new();
    let mut current: Option<Block> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            flush(&mut current, &mut blocks);
            continue;
        }
        if is_noise(line, repeated) {
            continue;
        }
        if line.chars().filter(|c| "æ÷ø´".contains(*c)).count() >= 3 {
            flush(&mut current, &mut blocks);
            blocks.push(Block::Para(
                "> ⚠ Formül/sembol bozuk çıktı (Symbol font); PDF'teki sayfaya bakın.".to_string(),
            ));
            continue;
        }
        if let Some(toc) = detect_toc_line(line) {
            flush(&mut current, &mut blocks);
            // İçindekilerde satır kırılmış: "13 Mechanical, ... Orderable" + "Information ... 44"
            let wrapped = match (blocks.last(), toc.strip_prefix("- ")) {
                (Some(Block::Heading(h)), Some(t)) => {
                    let first_is_digit = t.chars().next().map_or(false, |c| c.is_ascii_digit());
                    if first_is_digit {
                        None
                    } else {
                        Some(format!("- {} {}", h.trim_start_matches('#').trim(), t))
                    }
                }
                _ => None,
            };
            match wrapped {
                Some(merged) => {
                    blocks.pop();
                    blocks.push(Block::Item(merged));
                }
                None => blocks.push(Block::Item(toc)),
            }
            continue;
        }
        if let Some(h) = detect_heading(line, caps_headings) {
            flush(&mut current, &mut blocks);
            blocks.push(Block::Heading(h));
            continue;
        }
        if let Some(item) = detect_list_item(line) {
            flush(&mut current, &mut blocks);
            current = Some(Block::Item(item));
            continue;
        }
        match current.as_mut() {
            Some(Block::Para(t)) | Some(Block::Item(t)) => append_line(t, line),
            _ => current = Some(Block::Para(line.to_string())),
        }
    }
    flush(&mut current, &mut blocks);

    let mut out = String::new();
    let mut prev_item = false;
    for (i, b) in blocks.iter().enumerate() {
        let (s, is_item) = match b {
            Block::Heading(s) | Block::Para(s) => (s, false),
            Block::Item(s) => (s, true),
        };
        if i > 0 {
            out.push_str(if prev_item && is_item { "\n" } else { "\n\n" });
        }
        out.push_str(s);
        prev_item = is_item;
    }
    out
}

fn detect_heading(line: &str, caps_headings: bool) -> Option<String> {
    let len = line.chars().count();
    if len > 120 || line.ends_with(['.', ',', ';']) {
        return None;
    }

    // 1) Numaralı başlık: "2 Giriş", "2.3 Saat Ayarları"
    if let Some((token, rest)) = line.split_once(char::is_whitespace) {
        let rest = rest.trim();
        let starts_digit = token.chars().next().map_or(false, |c| c.is_ascii_digit());
        let valid_chars = token.chars().all(|c| c.is_ascii_digit() || c == '.');
        let segments_ok = token.split('.').all(|s| s.len() <= 3);
        let words: Vec<&str> = rest.split_whitespace().collect();
        if starts_digit
            && valid_chars
            && segments_ok
            && !token.ends_with('.')
            && !words.is_empty()
            && words.len() <= 12
            && words[0].chars().filter(|c| c.is_alphabetic()).count() >= 3
            && rest.chars().any(|c| c.is_lowercase())
            && rest.chars().next().map_or(false, |c| c.is_uppercase())
        {
            let dots = token.matches('.').count();
            let level = (dots + 2).min(4);
            return Some(format!("{} {}", "#".repeat(level), line));
        }
    }

    // 2) TAMAMEN BÜYÜK HARFLİ kısa satır
    let letters = line.chars().filter(|c| c.is_alphabetic()).count();
    let has_lower = line.chars().any(|c| c.is_lowercase());
    let words = line.split_whitespace().count();
    if caps_headings && letters >= 3 && !has_lower && len <= 60 && words <= 8 && letters * 10 >= len * 6 {
        return Some(format!("## {}", line));
    }

    None
}

fn detect_list_item(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("– ") {
        let rest = rest.trim();
        if !rest.is_empty() {
            return Some(format!("  - {}", rest));
        }
    }
    for p in ["• ", "● ", "▪ ", "◦ ", "· ", "- ", "* "] {
        if let Some(rest) = line.strip_prefix(p) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some(format!("- {}", rest));
            }
        }
    }
    // Numaralı liste: "1. metin" veya "1) metin"
    let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() && digits.len() <= 3 {
        let rest = &line[digits.len()..];
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            let r = r.trim();
            if !r.is_empty() {
                return Some(format!("{}. {}", digits, r));
            }
        }
    }
    None
}

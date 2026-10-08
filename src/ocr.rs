//! OCR with Tesseract, used to give the PDF an invisible text layer so the Kindle
//! can select words and look them up in the dictionary.

use crate::detect::{Rect, Shape};
use crate::layout::OutPage;
use anyhow::{Context, Result, bail};
use image::{DynamicImage, GrayImage, imageops::FilterType};
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::{Command, Stdio};

/// ISO 639-1 code, Tesseract language(s), display name.
const LANGUAGES: &[(&str, &str, &str)] = &[
    ("en", "eng", "English"),
    ("fr", "fra", "French"),
    ("de", "deu", "German"),
    ("es", "spa", "Spanish"),
    ("it", "ita", "Italian"),
    ("pt", "por", "Portuguese"),
    ("nl", "nld", "Dutch"),
    ("sv", "swe", "Swedish"),
    ("da", "dan", "Danish"),
    ("no", "nor", "Norwegian"),
    ("fi", "fin", "Finnish"),
    ("pl", "pol", "Polish"),
    ("cs", "ces", "Czech"),
    ("hu", "hun", "Hungarian"),
    ("ro", "ron", "Romanian"),
    ("el", "ell", "Greek"),
    ("tr", "tur", "Turkish"),
    ("ru", "rus", "Russian"),
    ("uk", "ukr", "Ukrainian"),
    ("fa", "fas", "Persian"),
    ("ar", "ara", "Arabic"),
    ("he", "heb", "Hebrew"),
    ("hi", "hin", "Hindi"),
    ("id", "ind", "Indonesian"),
    ("vi", "vie", "Vietnamese"),
    ("ja", "jpn_vert+jpn", "Japanese"),
    ("zh", "chi_sim+chi_sim_vert", "Chinese (simplified)"),
    ("zh-tw", "chi_tra+chi_tra_vert", "Chinese (traditional)"),
    ("ko", "kor", "Korean"),
];

/// A recognised word in source-page pixel coordinates.
#[derive(Clone, Debug)]
pub struct Word {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub text: String,
}

/// A word positioned on a Kindle page (pixels, y down).
#[derive(Clone, Debug)]
pub struct PlacedWord {
    /// Start of the baseline.
    pub x: f32,
    pub y: f32,
    /// Length along the baseline and font size, in pixels.
    pub advance: f32,
    pub size: f32,
    /// Baseline runs downwards (panel rotated to landscape) instead of to the right.
    pub down: bool,
    pub text: String,
}

/// Map a language code (`en`, `fa`, or a Tesseract code like `eng`) to Tesseract's
/// `-l` argument and a display name, and check the language data is installed.
pub fn resolve(code: &str) -> Result<(String, String)> {
    let code = code.trim().to_ascii_lowercase();
    let (tess, name) = LANGUAGES
        .iter()
        .find(|(iso, tess, name)| *iso == code || *tess == code || name.to_ascii_lowercase() == code)
        .map(|&(_, t, n)| (t.to_string(), n.to_string()))
        .unwrap_or_else(|| (code.clone(), code.clone()));

    let out = Command::new("tesseract")
        .arg("--list-langs")
        .output()
        .context("text layer needs Tesseract OCR (`brew install tesseract`), or pass --no-ocr")?;
    let listing = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    let installed: Vec<&str> = listing.lines().skip(1).map(str::trim).collect();
    let missing: Vec<&str> = tess.split('+').filter(|l| !installed.contains(l)).collect();
    if !missing.is_empty() {
        bail!(
            "Tesseract has no data for {name} ({}); install it with `brew install tesseract-lang`",
            missing.join(", ")
        );
    }
    Ok((tess, name))
}

/// Ask which language the comic is in. Empty answer = English.
pub fn prompt() -> Result<String> {
    if !io::stdin().is_terminal() {
        return Ok("en".into());
    }
    println!("Which language is the comic in? (used for the dictionary text layer)");
    let shown: Vec<String> = LANGUAGES.iter().map(|(iso, _, name)| format!("{iso} {name}")).collect();
    for chunk in shown.chunks(6) {
        println!("  {}", chunk.join(", "));
    }
    let stdin = io::stdin();
    loop {
        print!("Language [en]: ");
        io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok("en".into());
        }
        let line = line.trim();
        if line.is_empty() {
            return Ok("en".into());
        }
        let known = LANGUAGES.iter().any(|(iso, tess, name)| {
            *iso == line.to_ascii_lowercase() || *tess == line || name.eq_ignore_ascii_case(line)
        });
        // Unknown codes are passed through as Tesseract language names.
        if known || line.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '+') {
            return Ok(line.to_string());
        }
        println!("Unknown language '{line}'.");
    }
}

/// OCR one source page. Returns words in reading order of Tesseract's layout.
pub fn recognize(img: &DynamicImage, lang: &str) -> Result<Vec<Word>> {
    // Comic lettering is small; Tesseract works best around 30 px letters.
    let gray = img.to_luma8();
    let scale = (2800.0 / gray.height() as f32).clamp(1.0, 3.0);
    let gray = if scale > 1.0 {
        let w = (gray.width() as f32 * scale).round() as u32;
        let h = (gray.height() as f32 * scale).round() as u32;
        image::imageops::resize(&gray, w, h, FilterType::CatmullRom)
    } else {
        gray
    };
    let mut png = Vec::new();
    gray.write_to(&mut io::Cursor::new(&mut png), image::ImageFormat::Png)?;

    let mut child = Command::new("tesseract")
        // Sparse text mode finds balloons scattered over the page.
        .args(["stdin", "stdout", "-l", lang, "--psm", "11", "tsv"])
        // Pages already run in parallel; keep each Tesseract single-threaded.
        .env("OMP_THREAD_LIMIT", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("running tesseract")?;
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(&png));
    let out = child.wait_with_output()?;
    let _ = writer.join();
    if !out.status.success() {
        bail!("tesseract failed on a page");
    }
    let words = parse_tsv(&String::from_utf8_lossy(&out.stdout), &gray);
    Ok(words
        .into_iter()
        .map(|w| Word { x: w.x / scale, y: w.y / scale, w: w.w / scale, h: w.h / scale, ..w })
        .collect())
}

struct Raw {
    line: (u32, u32, u32),
    word: Word,
}

fn parse_tsv(tsv: &str, gray: &GrayImage) -> Vec<Word> {
    let mut raws: Vec<Raw> = Vec::new();
    for row in tsv.lines().skip(1) {
        let f: Vec<&str> = row.split('\t').collect();
        if f.len() < 12 || f[0] != "5" {
            continue;
        }
        let num = |i: usize| f[i].parse::<f32>().unwrap_or(0.0);
        let (x, y, w, h, conf) = (num(6), num(7), num(8), num(9), num(10));
        let text = f[11].trim();
        let letters = text.chars().filter(|c| c.is_alphabetic()).count();
        let len = text.chars().count();
        // Balloon and caption lettering sits on light paper; noise from the art does not.
        // Short tokens are where noise lives, so they need high confidence.
        let keep = conf >= 45.0
            && letters >= 1
            && letters as f32 >= 0.6 * len as f32
            && (letters >= 3 || conf >= 80.0)
            && light_fraction(gray, x, y, w, h) >= 0.4;
        if keep {
            let line = (num(2) as u32, num(3) as u32, num(4) as u32);
            raws.push(Raw { line, word: Word { x, y, w, h, text: text.to_string() } });
        }
    }

    // Join words hyphenated across lines ("LEAV-" + "ING") so both halves look up "leaving".
    for i in 0..raws.len().saturating_sub(1) {
        let (a, b) = (&raws[i], &raws[i + 1]);
        let same_par = a.line.0 == b.line.0 && a.line.1 == b.line.1 && b.line.2 == a.line.2 + 1;
        let t = &a.word.text;
        if same_par && t.ends_with('-') && !t.ends_with("--") && t.chars().filter(|c| c.is_alphabetic()).count() >= 2 {
            let joined = format!("{}{}", t.trim_end_matches('-'), b.word.text);
            raws[i].word.text = joined.clone();
            raws[i + 1].word.text = joined;
        }
    }

    raws.into_iter().flat_map(|r| clean(r.word)).collect()
}

fn light_fraction(g: &GrayImage, x: f32, y: f32, w: f32, h: f32) -> f32 {
    let (x0, y0) = (x.max(0.0) as u32, y.max(0.0) as u32);
    let (x1, y1) = (((x + w) as u32).min(g.width()), ((y + h) as u32).min(g.height()));
    let (mut light, mut total) = (0u32, 0u32);
    for yy in y0..y1 {
        for xx in x0..x1 {
            total += 1;
            light += (g.get_pixel(xx, yy)[0] > 140) as u32;
        }
    }
    if total == 0 { 0.0 } else { light as f32 / total as f32 }
}

/// Split runs like "LOOK---YOU" into words, trim punctuation, lowercase ALL-CAPS
/// lettering, and split tall boxes of vertical CJK text into one box per character.
fn clean(w: Word) -> Vec<Word> {
    let chars: Vec<char> = w.text.chars().collect();
    let n = chars.len().max(1) as f32;
    let is_sep = |i: usize| {
        let c = chars[i];
        c == '/' || c == '…' || c == '—' || (c == '-' && (chars.get(i + 1) == Some(&'-') || i > 0 && chars[i - 1] == '-'))
    };
    let mut out = Vec::new();
    let mut start = 0;
    for i in 0..=chars.len() {
        if i < chars.len() && !is_sep(i) {
            continue;
        }
        if i > start {
            let piece: String = chars[start..i].iter().collect();
            // Keep the piece's share of the box so selection lines up with the lettering.
            let (px, pw) = (w.x + w.w * start as f32 / n, w.w * (i - start) as f32 / n);
            let text = piece.trim_matches(|c: char| !c.is_alphanumeric()).to_string();
            if text.chars().any(|c| c.is_alphabetic()) {
                let letters = text.chars().filter(|c| c.is_alphabetic());
                let all_caps = letters.clone().count() >= 2 && letters.clone().all(|c| c.is_uppercase());
                let text = if all_caps { text.to_lowercase() } else { text };
                out.push(Word { x: px, y: w.y, w: pw, h: w.h, text });
            }
        }
        start = i + 1;
    }
    out.into_iter()
        .flat_map(|w| {
            let n = w.text.chars().count();
            if n >= 2 && w.h > w.w * 1.5 {
                let ch = w.h / n as f32;
                w.text
                    .chars()
                    .enumerate()
                    .map(|(i, c)| Word { x: w.x, y: w.y + ch * i as f32, w: w.w, h: ch, text: c.to_string() })
                    .collect()
            } else {
                vec![w]
            }
        })
        .collect()
}

/// Position the words of every panel on a Kindle page, following the panel's crop,
/// scale and rotation. Words outside a panel's outline are dropped.
pub fn place_words(page: &OutPage, words: &[Vec<Word>], shapes: &[Vec<Option<Shape>>]) -> Vec<PlacedWord> {
    let mut out = Vec::new();
    for it in &page.items {
        let src: Rect = it.src;
        let shape = &shapes[it.page][it.panel];
        let (rw, rh) = if it.rotated { (it.h as f32, it.w as f32) } else { (it.w as f32, it.h as f32) };
        let (sx, sy) = (rw / src.w as f32, rh / src.h as f32);
        // Source point -> Kindle page point.
        let map = |px: f32, py: f32| {
            let (tx, ty) = ((px - src.x as f32) * sx, (py - src.y as f32) * sy);
            if it.rotated {
                (it.x as f32 + rh - ty, it.y as f32 + tx)
            } else {
                (it.x as f32 + tx, it.y as f32 + ty)
            }
        };
        for w in &words[it.page] {
            let (cx, cy) = (w.x + w.w / 2.0, w.y + w.h / 2.0);
            let inside = cx >= src.x as f32
                && cy >= src.y as f32
                && cx < (src.x + src.w) as f32
                && cy < src.bottom() as f32
                && shape.as_ref().is_none_or(|s| s.alpha(cx, cy) > 0.5);
            if !inside {
                continue;
            }
            let (x, y) = map(w.x, w.y + w.h);
            out.push(PlacedWord {
                x,
                y,
                advance: w.w * sx,
                size: w.h * sy,
                down: it.rotated,
                text: w.text.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, w: f32) -> Word {
        Word { x: 0.0, y: 0.0, w, h: 10.0, text: text.into() }
    }

    #[test]
    fn cleans_comic_lettering() {
        let t = |w: Word| clean(w).into_iter().map(|w| w.text).collect::<Vec<_>>();
        assert_eq!(t(word("LOOK---YOU", 100.0)), ["look", "you"]);
        assert_eq!(t(word("ARTILLERY!", 100.0)), ["artillery"]);
        assert_eq!(t(word("THEY'LL", 70.0)), ["they'll"]);
        assert_eq!(t(word("I", 10.0)), ["I"]);
        assert_eq!(t(word("Paris,", 60.0)), ["Paris"]);
        let parts = clean(word("LOOK---YOU", 100.0));
        assert!(parts[1].x > 60.0, "second piece keeps its position in the box");
    }

    #[test]
    fn joins_hyphenated_lines() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   5\t1\t1\t1\t1\t1\t10\t10\t50\t12\t90\tLEAV-\n\
                   5\t1\t1\t1\t2\t1\t10\t30\t30\t12\t90\tING\n";
        let g = GrayImage::from_pixel(100, 100, image::Luma([255]));
        let words: Vec<String> = parse_tsv(tsv, &g).into_iter().map(|w| w.text).collect();
        assert_eq!(words, ["leaving", "leaving"]);
    }
}

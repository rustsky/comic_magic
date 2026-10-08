//! Render packed pages and write PDF / CBZ / image folders.

use crate::detect::Shape;
use crate::layout::OutPage;
use crate::ocr::PlacedWord;
use anyhow::{Context, Result};
use image::{DynamicImage, ImageReader, Rgb, RgbImage, imageops, imageops::FilterType};
use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct RenderOptions {
    pub width: u32,
    pub height: u32,
    pub color: bool,
    pub gamma: f32,
    pub quality: u8,
}

pub fn load_image(path: &Path) -> Result<DynamicImage> {
    ImageReader::open(path)?
        .with_guessed_format()?
        .decode()
        .with_context(|| format!("decoding {}", path.display()))
}

/// Compose one Kindle page and return it JPEG-encoded.
/// `shapes[page][panel]` is the exact outline of a non-rectangular panel.
pub fn render_page(
    page: &OutPage,
    sources: &[PathBuf],
    shapes: &[Vec<Option<Shape>>],
    o: &RenderOptions,
) -> Result<Vec<u8>> {
    let mut canvas = RgbImage::from_pixel(o.width, o.height, Rgb([255, 255, 255]));
    let mut cache: HashMap<usize, DynamicImage> = HashMap::new();
    for it in &page.items {
        if !cache.contains_key(&it.page) {
            cache.insert(it.page, load_image(&sources[it.page])?);
        }
        let img = &cache[&it.page];
        let crop = img.crop_imm(it.src.x, it.src.y, it.src.w, it.src.h);
        let (rw, rh) = if it.rotated { (it.h, it.w) } else { (it.w, it.h) };
        let mut tile = crop.resize_exact(rw, rh, FilterType::Lanczos3).to_rgb8();
        if let Some(shape) = &shapes[it.page][it.panel] {
            // Paint everything outside the panel's outline white.
            let (sx, sy) = (it.src.w as f32 / rw as f32, it.src.h as f32 / rh as f32);
            for (x, y, p) in tile.enumerate_pixels_mut() {
                let a = shape.alpha(it.src.x as f32 + (x as f32 + 0.5) * sx, it.src.y as f32 + (y as f32 + 0.5) * sy);
                if a < 1.0 {
                    p.0 = p.0.map(|c| (c as f32 * a + 255.0 * (1.0 - a)).round() as u8);
                }
            }
        }
        if it.rotated {
            tile = imageops::rotate90(&tile);
        }
        imageops::overlay(&mut canvas, &tile, it.x as i64, it.y as i64);
    }

    if (o.gamma - 1.0).abs() > f32::EPSILON {
        let lut: Vec<u8> = (0..256)
            .map(|v| ((v as f32 / 255.0).powf(o.gamma) * 255.0).round() as u8)
            .collect();
        for p in canvas.pixels_mut() {
            p.0 = p.0.map(|c| lut[c as usize]);
        }
    }

    let mut buf = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, o.quality);
    if o.color {
        enc.encode_image(&canvas)?;
    } else {
        enc.encode_image(&DynamicImage::ImageRgb8(canvas).to_luma8())?;
    }
    Ok(buf)
}

/// Minimal PDF: one full-bleed JPEG per page, page size matching the screen at its ppi.
/// `texts[i]` is an optional invisible text layer for page `i` (empty = none), which
/// makes words selectable for the Kindle dictionary.
pub fn write_pdf(
    path: &Path,
    pages: &[Vec<u8>],
    texts: &[Vec<PlacedWord>],
    o: &RenderOptions,
    ppi: u32,
    title: &str,
) -> Result<()> {
    let mut w = Counter { inner: BufWriter::new(fs::File::create(path)?), pos: 0 };
    let mut offsets: Vec<u64> = Vec::new();
    let pt = 72.0 / ppi as f32;
    let pw = o.width as f32 * pt;
    let ph = o.height as f32 * pt;
    let cs = if o.color { "DeviceRGB" } else { "DeviceGray" };
    let n = pages.len();
    // Object ids: 1 catalog, 2 pages, 3 info, then (page, contents, image) per page,
    // then the text-layer font objects.
    let page_id = |i: usize| 4 + 3 * i;
    let font_id = 4 + 3 * n;
    let has_text = texts.iter().any(|t| !t.is_empty());

    w.write_all(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n")?;
    offsets.push(w.pos);
    write!(w, "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n")?;
    offsets.push(w.pos);
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", page_id(i))).collect();
    write!(w, "2 0 obj\n<< /Type /Pages /Count {n} /Kids [{}] >>\nendobj\n", kids.join(" "))?;
    offsets.push(w.pos);
    write!(w, "3 0 obj\n<< /Title ({}) /Producer (comic-magic) >>\nendobj\n", pdf_escape(title))?;

    for (i, jpeg) in pages.iter().enumerate() {
        let id = page_id(i);
        let words = texts.get(i).map(Vec::as_slice).unwrap_or_default();
        let font = if words.is_empty() { String::new() } else { format!("/Font << /F1 {font_id} 0 R >> ") };
        offsets.push(w.pos);
        write!(
            w,
            "{id} 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {pw:.2} {ph:.2}] \
             /Resources << {font}/XObject << /Im0 {} 0 R >> >> /Contents {} 0 R >>\nendobj\n",
            id + 2,
            id + 1
        )?;
        let mut content = format!("q {pw:.2} 0 0 {ph:.2} 0 0 cm /Im0 Do Q");
        if !words.is_empty() {
            content.push_str("\nBT 3 Tr");
            for word in words {
                content.push_str(&text_op(word, pt, ph));
            }
            content.push_str("\nET");
        }
        offsets.push(w.pos);
        write!(w, "{} 0 obj\n<< /Length {} >>\nstream\n{content}\nendstream\nendobj\n", id + 1, content.len())?;
        offsets.push(w.pos);
        write!(
            w,
            "{} 0 obj\n<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /{cs} \
             /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
            id + 2,
            o.width,
            o.height,
            jpeg.len()
        )?;
        w.write_all(jpeg)?;
        w.write_all(b"\nendstream\nendobj\n")?;
    }

    if has_text {
        write_glyphless_font(&mut w, &mut offsets, font_id)?;
    }

    let xref = w.pos;
    write!(w, "xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1)?;
    for off in &offsets {
        write!(w, "{off:010} 00000 n \n")?;
    }
    write!(w, "trailer\n<< /Size {} /Root 1 0 R /Info 3 0 R >>\nstartxref\n{xref}\n%%EOF\n", offsets.len() + 1)?;
    w.inner.flush()?;
    Ok(())
}

/// One invisible word. The glyphless font advances every character by half the font
/// size, so horizontal scaling (Tz) stretches the word over its box on the page.
fn text_op(word: &PlacedWord, pt: f32, page_h: f32) -> String {
    let units: Vec<u16> = word.text.chars().filter_map(|c| u16::try_from(c as u32).ok()).collect();
    if units.is_empty() || word.size < 1.0 {
        return String::new();
    }
    let size = word.size * pt;
    let tz = 100.0 * word.advance / (units.len() as f32 * 0.5 * word.size);
    let (x, y) = (word.x * pt, page_h - word.y * pt);
    // Text matrix: baseline to the right, or downwards for panels rotated to landscape.
    let m = if word.down { "0 -1 1 0" } else { "1 0 0 1" };
    let hex: String = units.iter().map(|u| format!("{u:04X}")).collect();
    format!("\n/F1 {size:.2} Tf {tz:.1} Tz {m} {x:.2} {y:.2} Tm <{hex}> Tj")
}

/// Type0 font over Tesseract's glyphless TrueType font: text is written as UTF-16
/// code units (Identity-H), every code maps to the same invisible glyph, and a
/// ToUnicode CMap maps codes back to Unicode so selection and lookup work in any script.
fn write_glyphless_font<W: Write>(w: &mut Counter<W>, offsets: &mut Vec<u64>, id: usize) -> Result<()> {
    const TTF: &[u8] = include_bytes!("../assets/glyphless.ttf");
    let (cid_font, descriptor, file, to_unicode, gid_map) = (id + 1, id + 2, id + 3, id + 4, id + 5);

    offsets.push(w.pos);
    write!(
        w,
        "{id} 0 obj\n<< /Type /Font /Subtype /Type0 /BaseFont /GlyphLessFont /Encoding /Identity-H \
         /DescendantFonts [{cid_font} 0 R] /ToUnicode {to_unicode} 0 R >>\nendobj\n"
    )?;
    offsets.push(w.pos);
    write!(
        w,
        "{cid_font} 0 obj\n<< /Type /Font /Subtype /CIDFontType2 /BaseFont /GlyphLessFont \
         /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
         /FontDescriptor {descriptor} 0 R /DW 500 /CIDToGIDMap {gid_map} 0 R >>\nendobj\n"
    )?;
    offsets.push(w.pos);
    write!(
        w,
        "{descriptor} 0 obj\n<< /Type /FontDescriptor /FontName /GlyphLessFont /Flags 5 \
         /FontBBox [0 0 500 1000] /ItalicAngle 0 /Ascent 1000 /Descent 0 /CapHeight 1000 /StemV 80 \
         /FontFile2 {file} 0 R >>\nendobj\n"
    )?;
    offsets.push(w.pos);
    write!(w, "{file} 0 obj\n<< /Length {} /Length1 {} >>\nstream\n", TTF.len(), TTF.len())?;
    w.write_all(TTF)?;
    w.write_all(b"\nendstream\nendobj\n")?;

    let cmap = "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
        /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
        /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
        1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n\
        1 beginbfrange\n<0000> <FFFF> <0000>\nendbfrange\n\
        endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend";
    offsets.push(w.pos);
    write!(w, "{to_unicode} 0 obj\n<< /Length {} >>\nstream\n{cmap}\nendstream\nendobj\n", cmap.len())?;

    // Every CID -> glyph 1.
    let raw: Vec<u8> = std::iter::repeat_n([0u8, 1u8], 65536).flatten().collect();
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&raw)?;
    let packed = z.finish()?;
    offsets.push(w.pos);
    write!(w, "{gid_map} 0 obj\n<< /Length {} /Filter /FlateDecode >>\nstream\n", packed.len())?;
    w.write_all(&packed)?;
    w.write_all(b"\nendstream\nendobj\n")?;
    Ok(())
}

fn pdf_escape(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control())
        .flat_map(|c| match c {
            '(' | ')' | '\\' => vec!['\\', c],
            _ => vec![c],
        })
        .collect()
}

struct Counter<W: Write> {
    inner: W,
    pos: u64,
}

impl<W: Write> Write for Counter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

pub fn write_cbz(path: &Path, pages: &[Vec<u8>]) -> Result<()> {
    let mut zip = zip::ZipWriter::new(fs::File::create(path)?);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (i, jpeg) in pages.iter().enumerate() {
        zip.start_file(format!("{:05}.jpg", i + 1), opts)?;
        zip.write_all(jpeg)?;
    }
    zip.finish()?;
    Ok(())
}

pub fn write_images(dir: &Path, pages: &[Vec<u8>]) -> Result<()> {
    fs::create_dir_all(dir)?;
    for (i, jpeg) in pages.iter().enumerate() {
        fs::write(dir.join(format!("{:05}.jpg", i + 1)), jpeg)?;
    }
    Ok(())
}

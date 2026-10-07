//! Turn any supported input into an ordered list of page image files.

use crate::progress;
use anyhow::{Context, Result, bail};
use indicatif::ProgressBar;
use std::cmp::Ordering;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff"];

fn ext_of(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

pub fn is_image(p: &Path) -> bool {
    IMAGE_EXTS.contains(&ext_of(p).as_str())
}

/// Returns page image paths in reading order. Extracted/rendered files go in `tmp`.
/// `pb` starts as a spinner and becomes a bar when the page count is known.
pub fn collect_pages(input: &Path, tmp: &Path, pdf_dpi: u32, pb: &ProgressBar) -> Result<Vec<PathBuf>> {
    if input.is_dir() {
        return images_in_dir(input);
    }
    if !input.exists() {
        bail!("input not found: {}", input.display());
    }
    match ext_of(input).as_str() {
        "cbz" | "zip" => extract_zip(input, tmp, pb),
        "cbr" | "rar" | "cb7" | "7z" | "cbt" | "tar" => extract_with_tool(input, tmp, pb),
        "pdf" => render_pdf(input, tmp, pdf_dpi, pb),
        _ if is_image(input) => Ok(vec![input.to_path_buf()]),
        e => bail!("unsupported input type '.{e}'"),
    }
}

fn images_in_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let p = entry?.path();
            let hidden = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.'));
            if hidden {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if is_image(&p) {
                out.push(p);
            }
        }
    }
    out.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    if out.is_empty() {
        bail!("no images found in {}", dir.display());
    }
    Ok(out)
}

fn extract_zip(path: &Path, tmp: &Path, pb: &ProgressBar) -> Result<Vec<PathBuf>> {
    let file = fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(file).context("opening zip/cbz")?;
    let mut names: Vec<(usize, String)> = (0..zip.len())
        .filter_map(|i| {
            let f = zip.by_index(i).ok()?;
            let name = f.name().to_string();
            let skip = f.is_dir() || name.starts_with("__MACOSX") || name.rsplit('/').next()?.starts_with('.');
            (!skip && is_image(Path::new(&name))).then_some((i, name))
        })
        .collect();
    names.sort_by(|a, b| natural_cmp(&a.1, &b.1));
    if names.is_empty() {
        bail!("no images inside {}", path.display());
    }
    progress::to_bar(pb, names.len());
    let mut out = Vec::new();
    for (n, (i, name)) in names.iter().enumerate() {
        let mut f = zip.by_index(*i)?;
        let dest = tmp.join(format!("{n:05}.{}", ext_of(Path::new(name))));
        let mut w = fs::File::create(&dest)?;
        std::io::copy(&mut f, &mut w)?;
        out.push(dest);
        pb.inc(1);
    }
    Ok(out)
}

/// CBR/CB7/CBT: delegate to bsdtar (ships with macOS, handles rar/7z via libarchive), unar or 7z.
fn extract_with_tool(path: &Path, tmp: &Path, pb: &ProgressBar) -> Result<Vec<PathBuf>> {
    let dir = tmp.join("archive");
    fs::create_dir_all(&dir)?;
    let attempts: [(&str, Vec<&std::ffi::OsStr>); 3] = [
        ("bsdtar", vec!["-xf".as_ref(), path.as_os_str(), "-C".as_ref(), dir.as_os_str()]),
        ("unar", vec!["-q".as_ref(), "-f".as_ref(), "-o".as_ref(), dir.as_os_str(), path.as_os_str()]),
        ("7z", vec!["x".as_ref(), "-y".as_ref(), path.as_os_str()]),
    ];
    for (tool, args) in attempts {
        let mut cmd = Command::new(tool);
        cmd.args(&args);
        if tool == "7z" {
            cmd.arg(format!("-o{}", dir.display()));
        }
        if let Ok((st, _)) = run_watching(cmd, &dir, None, pb)
            && st.success()
        {
            return images_in_dir(&dir);
        }
    }
    bail!("could not extract {} (install bsdtar, unar or 7z)", path.display())
}

/// Render PDF pages with MuPDF (`mutool`) or Poppler (`pdftoppm`).
fn render_pdf(path: &Path, tmp: &Path, dpi: u32, pb: &ProgressBar) -> Result<Vec<PathBuf>> {
    let dir = tmp.join("pdf");
    fs::create_dir_all(&dir)?;
    let total = pdf_page_count(path);
    let dpi = dpi.to_string();
    let mut mutool = Command::new("mutool");
    mutool.args(["draw", "-q", "-r", &dpi, "-o"]).arg(dir.join("p%05d.png")).arg(path);
    let ok = matches!(run_watching(mutool, &dir, total, pb), Ok((st, _)) if st.success());
    if !ok {
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        pb.set_position(0);
        let mut poppler = Command::new("pdftoppm");
        poppler.args(["-r", &dpi, "-png"]).arg(path).arg(dir.join("p"));
        match run_watching(poppler, &dir, total, pb) {
            Ok((st, _)) if st.success() => {}
            Ok((_, err)) => bail!("pdftoppm failed: {err}"),
            Err(_) => bail!("PDF input needs `mutool` (MuPDF) or `pdftoppm` (Poppler) on PATH"),
        }
    }
    images_in_dir(&dir)
}

/// Page count from `mutool show` or `pdfinfo`; `None` if neither works.
fn pdf_page_count(path: &Path) -> Option<usize> {
    let out = Command::new("mutool").args(["show", "-q"]).arg(path).arg("trailer/Root/Pages/Count").output();
    if let Ok(o) = out
        && let Ok(n) = String::from_utf8_lossy(&o.stdout).trim().parse()
    {
        return Some(n);
    }
    let o = Command::new("pdfinfo").arg(path).output().ok()?;
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("Pages:"))
        .and_then(|n| n.trim().parse().ok())
}

/// Run an external tool, reporting progress by counting the files it writes into `dir`.
/// Returns its exit status and stderr.
fn run_watching(mut cmd: Command, dir: &Path, total: Option<usize>, pb: &ProgressBar) -> Result<(ExitStatus, String)> {
    if let Some(n) = total {
        progress::to_bar(pb, n);
    }
    let mut child = cmd.stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
    // Drain stderr on a thread so a chatty tool can't block on a full pipe.
    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        let done = count_files(dir);
        pb.set_position(done as u64);
        if total.is_none() {
            pb.set_message(format!("{done} page(s)"));
        }
        std::thread::sleep(Duration::from_millis(150));
    };
    Ok((status, reader.join().unwrap_or_default()))
}

fn count_files(dir: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                n += 1;
            }
        }
    }
    n
}

/// Compare strings treating digit runs as numbers ("page2" < "page10").
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let nb = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let da = trim_zeros(&a[..na]);
                let db = trim_zeros(&b[..nb]);
                let ord = da.len().cmp(&db.len()).then_with(|| da.cmp(db));
                if ord != Ordering::Equal {
                    return ord;
                }
                a = &a[na..];
                b = &b[nb..];
            }
            (Some(x), Some(y)) => {
                let ord = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

fn trim_zeros(d: &[u8]) -> &[u8] {
    let i = d.iter().position(|&c| c != b'0').unwrap_or(d.len());
    &d[i..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut v = vec!["p10.jpg", "p2.jpg", "p1.jpg", "P03.jpg"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["p1.jpg", "p2.jpg", "P03.jpg", "p10.jpg"]);
    }
}

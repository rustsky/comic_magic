mod detect;
mod devices;
mod input;
mod layout;
mod output;
mod progress;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use indicatif::ParallelProgressIterator;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq)]
enum Format {
    /// PDF sized to the Kindle screen (copy over USB; works on every Kindle).
    Pdf,
    /// Comic book zip of JPEG pages (KOReader, Kindle Comic Converter, etc.).
    Cbz,
    /// A folder of JPEG pages.
    Images,
}

/// Split comic pages into panels and repack them so they are readable on a Kindle without zooming.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Comic to convert: .pdf, .cbz/.zip, .cbr/.cb7, an image, or a folder of images.
    input: Option<PathBuf>,

    /// Output path (default: <input>_<device>.<ext>).
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Kindle model (see --list-devices). Asked interactively when omitted.
    #[arg(short, long)]
    device: Option<String>,

    /// List known Kindle models and exit.
    #[arg(long)]
    list_devices: bool,

    /// Custom screen width in pixels (with --height and --ppi instead of --device).
    #[arg(long, requires_all = ["height", "ppi"])]
    width: Option<u32>,
    #[arg(long, requires_all = ["width", "ppi"])]
    height: Option<u32>,
    #[arg(long, requires_all = ["width", "height"])]
    ppi: Option<u32>,
    /// Keep colour (default: only for colour Kindles).
    #[arg(long)]
    color: bool,

    #[arg(short, long, value_enum, default_value_t = Format::Pdf)]
    format: Format,

    /// Manga mode: read panels right to left.
    #[arg(long)]
    rtl: bool,

    /// Minimum on-screen panel size relative to the printed comic (1.0 = print size).
    /// Higher = bigger text, fewer panels per page.
    #[arg(short = 's', long, default_value_t = 0.8)]
    readable_scale: f32,

    /// Height of the printed source page in inches (US comic 10.2, manga ~7.2, bande dessinée ~11.7).
    #[arg(long, default_value_t = 10.1875)]
    print_height: f32,

    /// Rotate wide panels to landscape when that makes them notably larger.
    #[arg(long)]
    rotate: bool,

    /// Never mix panels from different source pages on one Kindle page.
    #[arg(long)]
    page_break: bool,

    /// Screen margin in pixels.
    #[arg(long, default_value_t = 8)]
    margin: u32,

    /// Space between panels in pixels.
    #[arg(long, default_value_t = 14)]
    gap: u32,

    /// Maximum factor panels are enlarged to fill leftover page space.
    #[arg(long, default_value_t = 2.0)]
    max_upscale: f32,

    /// Gamma applied to output (>1 darkens; 1.0 = unchanged). E-ink often benefits from ~1.4.
    #[arg(long, default_value_t = 1.0)]
    gamma: f32,

    /// JPEG quality of output pages.
    #[arg(long, default_value_t = 88, value_parser = clap::value_parser!(u8).range(30..=100))]
    quality: u8,

    /// Ink detection threshold (luma difference from the paper colour).
    #[arg(long, default_value_t = 48)]
    threshold: u8,

    /// Split panels joined by balloons along lines covered at most this fraction (0-1).
    /// Higher splits more aggressively.
    #[arg(long, default_value_t = 0.35)]
    split_coverage: f32,

    /// Drop detected regions smaller than this fraction of the page area.
    #[arg(long, default_value_t = 0.01)]
    min_panel_area: f32,

    /// Resolution used to rasterise PDF pages.
    #[arg(long, default_value_t = 300)]
    pdf_dpi: u32,

    /// Write each source page with the detected panels outlined into this folder.
    #[arg(long)]
    debug_dir: Option<PathBuf>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    if args.list_devices {
        devices::print_list();
        return Ok(());
    }
    let Some(input) = args.input.clone() else {
        bail!("missing INPUT (see --help)");
    };
    if !(0.1..=4.0).contains(&args.readable_scale) {
        bail!("--readable-scale must be between 0.1 and 4.0");
    }

    let device = match (&args.device, args.width) {
        (Some(id), _) => devices::find(id)
            .with_context(|| format!("unknown device '{id}' (see --list-devices)"))?,
        (None, Some(width)) => devices::Device {
            id: format!("{width}x{}", args.height.unwrap()),
            name: "custom".into(),
            width,
            height: args.height.unwrap(),
            ppi: args.ppi.unwrap(),
            color: args.color,
        },
        (None, None) => devices::prompt()?,
    };
    let color = device.color || args.color;
    println!("Target: {} ({}x{} @ {} ppi)", device.name, device.width, device.height, device.ppi);

    let start = Instant::now();
    let tmp = tempfile::tempdir()?;
    let pb = progress::spinner("Loading", "");
    let pages = input::collect_pages(&input, tmp.path(), args.pdf_dpi, &pb)?;
    pb.finish_and_clear();
    println!("Loaded {} source page(s)", pages.len());

    // 1. Detect panels on every page in parallel.
    let det = detect::DetectOptions {
        threshold: args.threshold,
        split_coverage: args.split_coverage,
        min_panel_area: args.min_panel_area,
        padding: 0.004,
        rtl: args.rtl,
    };
    if let Some(d) = &args.debug_dir {
        std::fs::create_dir_all(d)?;
    }
    let pb = progress::bar("Detecting", pages.len(), "pages");
    let sources: Vec<layout::SourcePage> = pages
        .par_iter()
        .enumerate()
        .progress_with(pb.clone())
        .map(|(i, p)| -> Result<layout::SourcePage> {
            let img = output::load_image(p)?;
            let panels = detect::detect_panels(&img, &det);
            if let Some(d) = &args.debug_dir {
                detect::debug_image(&img, &panels).save(d.join(format!("page{:04}.jpg", i + 1)))?;
            }
            Ok(layout::SourcePage { height: img.height(), panels })
        })
        .collect::<Result<_>>()?;
    pb.finish_and_clear();
    let n_panels: usize = sources.iter().map(|s| s.panels.len()).sum();
    println!("Detected {n_panels} panel(s)");

    // 2. Pack panels into Kindle pages.
    let lo = layout::LayoutOptions {
        screen_w: device.width,
        screen_h: device.height,
        ppi: device.ppi,
        margin: args.margin,
        gap: args.gap,
        readable_scale: args.readable_scale,
        print_height_in: args.print_height,
        max_upscale: args.max_upscale.max(1.0),
        rotate: args.rotate,
        page_break: args.page_break,
        rtl: args.rtl,
    };
    let out_pages = layout::layout(&sources, &lo);

    // 3. Render.
    let ro = output::RenderOptions {
        width: device.width,
        height: device.height,
        color,
        gamma: args.gamma,
        quality: args.quality,
    };
    let pb = progress::bar("Rendering", out_pages.len(), "Kindle pages");
    let rendered: Vec<Vec<u8>> = out_pages
        .par_iter()
        .progress_with(pb.clone())
        .map(|p| output::render_page(p, &pages, &ro))
        .collect::<Result<_>>()?;
    pb.finish_and_clear();

    let out_path = args.output.clone().unwrap_or_else(|| default_output(&input, &device.id, args.format));
    let title = input.file_stem().and_then(|s| s.to_str()).unwrap_or("comic");
    let pb = progress::spinner("Writing", &out_path.display().to_string());
    match args.format {
        Format::Pdf => output::write_pdf(&out_path, &rendered, &ro, device.ppi, title)?,
        Format::Cbz => output::write_cbz(&out_path, &rendered)?,
        Format::Images => output::write_images(&out_path, &rendered)?,
    }
    pb.finish_and_clear();
    println!(
        "Wrote {} Kindle page(s) to {} in {:.1}s",
        rendered.len(),
        out_path.display(),
        start.elapsed().as_secs_f32()
    );
    Ok(())
}

fn default_output(input: &Path, device: &str, format: Format) -> PathBuf {
    let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("comic");
    let name = match format {
        Format::Pdf => format!("{stem}_{device}.pdf"),
        Format::Cbz => format!("{stem}_{device}.cbz"),
        Format::Images => format!("{stem}_{device}"),
    };
    input.parent().unwrap_or(Path::new(".")).join(name)
}

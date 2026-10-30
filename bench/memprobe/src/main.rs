//! Where a conversion's bytes go (`plans/performance.org` § The probe).
//!
//! Links geosetta as a library under a counting `#[global_allocator]` and
//! splits one conversion into its phases: the input bytes, the read
//! (`read_features`), the IR left resident once the input is dropped, and the
//! write (`write_features_with`), whose scratch is its peak less the IR and
//! the output. RSS says how much; this says where.
//!
//! ```sh
//! cargo run --release --manifest-path bench/memprobe/Cargo.toml -- in.geojson kml
//! cargo run --release --manifest-path bench/memprobe/Cargo.toml -- in.parquet geojson --compact
//! ```
//!
//! Arguments mirror the CLI's: `<input> <output format> [--from FMT]
//! [--compact] [--kml-version 2.2|2.3] [--sort-hilbert]`. Between read and
//! write it does what the CLI does that changes memory: the Hilbert sort when
//! asked, and releasing natives the target cannot replay.
use geosetta::{read_features, reorder_hilbert, write_features_with, FeatureCollection, Format, KmlVersion, WriteOptions};
use std::alloc::{GlobalAlloc, Layout, System};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grow(layout.size());
            ALLOCS.fetch_add(1, Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        LIVE.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, new) };
        if !q.is_null() {
            LIVE.fetch_sub(layout.size(), Relaxed);
            grow(new);
            ALLOCS.fetch_add(1, Relaxed);
        }
        q
    }
}

fn grow(by: usize) {
    let now = LIVE.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(now, Relaxed);
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// One phase's peak, its allocations and its time.
struct Phase {
    peak: usize,
    allocs: usize,
    ms: f64,
}

fn phase<T>(f: impl FnOnce() -> T) -> (T, Phase) {
    PEAK.store(LIVE.load(Relaxed), Relaxed);
    let allocs = ALLOCS.load(Relaxed);
    let t = Instant::now();
    let value = f();
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let p = Phase { peak: PEAK.load(Relaxed), allocs: ALLOCS.load(Relaxed) - allocs, ms };
    (value, p)
}

fn mb(bytes: usize) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// The CLI's `release_foreign_natives` (`src/main.rs`): natives the target
/// cannot replay are dropped before the write, which changes no output.
fn release_foreign_natives(fc: &mut FeatureCollection, target: Format) {
    let kept = |format: Format| {
        format == target || matches!((format, target), (Format::Kml, Format::Kmz) | (Format::Kmz, Format::Kml))
    };
    if fc.native.as_ref().is_some_and(|n| !kept(n.format)) {
        fc.native = None;
    }
    for f in &mut fc.features {
        if f.native.as_ref().is_some_and(|n| !kept(n.format)) {
            f.native = None;
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: memprobe <input> <output format> [--from FMT] [--compact] [--kml-version 2.2|2.3] [--sort-hilbert]";
    let (Some(path), Some(to)) = (args.first(), args.get(1)) else { return Err(usage.into()) };
    let to = Format::parse(to).map_err(|e| e.to_string())?;
    let mut from = Format::from_path(path);
    let mut options = WriteOptions::default();
    let mut sort = false;
    let mut rest = args[2..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--compact" => options.compact = true,
            "--sort-hilbert" => sort = true,
            "--from" => from = Some(Format::parse(rest.next().ok_or(usage)?).map_err(|e| e.to_string())?),
            "--kml-version" => {
                options.kml_version = Some(KmlVersion::parse(rest.next().ok_or(usage)?).map_err(|e| e.to_string())?)
            }
            other => return Err(format!("unknown option {other:?}\n{usage}")),
        }
    }
    let from = from.ok_or("cannot tell the input's format from its name; pass --from")?;

    let base = LIVE.load(Relaxed);
    let (input, read_in) = phase(|| std::fs::read(path).map_err(|e| format!("{path}: {e}")));
    let input = input?;
    let size = input.len();
    let mut warnings = Vec::new();
    let (fc, read) = phase(|| read_features(from, &input, &mut warnings));
    let mut fc = fc.map_err(|e| e.to_string())?;
    drop(input);
    if sort {
        reorder_hilbert(&mut fc);
    }
    release_foreign_natives(&mut fc, to);
    let ir = LIVE.load(Relaxed) - base;
    let (out, write) = phase(|| write_features_with(to, &fc, options));
    let out = out.map_err(|e| e.to_string())?;
    let rows = fc.features.len().max(1);

    println!("{path} -> {to:?}: {} features", fc.features.len());
    println!("  input           {:>10}", mb(size));
    println!(
        "  read            peak {:>10}  {:>8.1}M allocs  {:>7.0} ms   (reading the file: {:.0} ms)",
        mb(read.peak - base),
        read.allocs as f64 / 1e6,
        read.ms,
        read_in.ms
    );
    println!("  IR resident     {:>10}  ({:.0} B/feature)", mb(ir), ir as f64 / rows as f64);
    println!(
        "  write           peak {:>10}  {:>8.1}M allocs  {:>7.0} ms",
        mb(write.peak - base),
        write.allocs as f64 / 1e6,
        write.ms
    );
    println!("  write scratch   {:>10}  (peak less IR and output)", mb((write.peak - base).saturating_sub(ir + out.len())));
    println!("  output          {:>10}", mb(out.len()));
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("memprobe: {e}");
            ExitCode::FAILURE
        }
    }
}

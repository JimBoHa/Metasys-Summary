use std::{io::Cursor, process::Command};

use anyhow::{Context, Result, bail};
use image::{DynamicImage, GrayImage, ImageFormat, imageops::FilterType};
use uuid::Uuid;

use super::models::{NormalizedPoint, TraceFeature, TraceKind};

pub const MAX_PDF_BYTES: usize = 25 * 1024 * 1024;
const MAX_RENDER_DIMENSION: u32 = 1_800;
const MAX_TRACE_FEATURES: usize = 1_500;

pub struct ProcessedFloorPlan {
    pub image_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub trace: Vec<TraceFeature>,
}

pub fn process_pdf(pdf_data: &[u8]) -> Result<ProcessedFloorPlan> {
    if pdf_data.len() < 5 || !pdf_data.starts_with(b"%PDF-") {
        bail!("uploaded file is not a valid PDF");
    }
    if pdf_data.len() > MAX_PDF_BYTES {
        bail!("PDF exceeds the 25 MB upload limit");
    }

    let directory = tempfile::tempdir().context("create protected PDF render directory")?;
    let input_path = directory.path().join("floor-plan.pdf");
    let output_path = directory.path().join("floor-plan.png");
    std::fs::write(&input_path, pdf_data).context("stage uploaded floor-plan PDF")?;

    let output = Command::new("/usr/bin/sips")
        .args(["-s", "format", "png"])
        .arg(&input_path)
        .arg("--out")
        .arg(&output_path)
        .output()
        .context("start native macOS PDF renderer")?;
    if !output.status.success() {
        bail!("macOS could not render the first page of this PDF");
    }

    let rendered = std::fs::read(&output_path).context("read rendered floor-plan image")?;
    let source = image::load_from_memory_with_format(&rendered, ImageFormat::Png)
        .context("decode rendered floor-plan image")?;
    let image = resize_floor_plan(source);
    let width = image.width();
    let height = image.height();
    let trace = auto_trace(&image.to_luma8());
    let mut encoded = Cursor::new(Vec::new());
    image
        .write_to(&mut encoded, ImageFormat::Png)
        .context("encode cleaned floor-plan preview")?;

    Ok(ProcessedFloorPlan {
        image_data: encoded.into_inner(),
        width,
        height,
        trace,
    })
}

fn resize_floor_plan(image: DynamicImage) -> DynamicImage {
    if image.width() <= MAX_RENDER_DIMENSION && image.height() <= MAX_RENDER_DIMENSION {
        image
    } else {
        image.resize(
            MAX_RENDER_DIMENSION,
            MAX_RENDER_DIMENSION,
            FilterType::Triangle,
        )
    }
}

#[derive(Clone, Copy)]
struct CandidateLine {
    horizontal: bool,
    start: u32,
    end: u32,
    coordinate: u32,
}

#[derive(Clone, Copy)]
struct MergedLine {
    horizontal: bool,
    start: u32,
    end: u32,
    coordinate_sum: u64,
    samples: u32,
}

fn auto_trace(image: &GrayImage) -> Vec<TraceFeature> {
    let (width, height) = image.dimensions();
    if width < 20 || height < 20 {
        return Vec::new();
    }
    let min_run = (width.min(height) / 90).clamp(12, 28);
    let mut candidates = horizontal_candidates(image, min_run);
    candidates.extend(vertical_candidates(image, min_run));
    let mut merged: Vec<MergedLine> = Vec::new();

    for candidate in candidates {
        let existing = merged.iter_mut().find(|line| {
            line.horizontal == candidate.horizontal
                && coordinate_distance(**line, candidate) <= 3
                && overlap_ratio(line.start, line.end, candidate.start, candidate.end) >= 0.72
        });
        if let Some(line) = existing {
            line.start = line.start.min(candidate.start);
            line.end = line.end.max(candidate.end);
            line.coordinate_sum += u64::from(candidate.coordinate);
            line.samples += 1;
        } else {
            merged.push(MergedLine {
                horizontal: candidate.horizontal,
                start: candidate.start,
                end: candidate.end,
                coordinate_sum: u64::from(candidate.coordinate),
                samples: 1,
            });
        }
    }

    merged.sort_by_key(|line| std::cmp::Reverse(line.end.saturating_sub(line.start)));
    merged
        .into_iter()
        .take(MAX_TRACE_FEATURES)
        .map(|line| line_to_feature(line, width, height))
        .collect()
}

fn horizontal_candidates(image: &GrayImage, min_run: u32) -> Vec<CandidateLine> {
    let (width, height) = image.dimensions();
    let mut output = Vec::new();
    for y in 0..height {
        scan_line(width, min_run, |x| is_ink(image.get_pixel(x, y).0[0]))
            .into_iter()
            .for_each(|(start, end)| {
                output.push(CandidateLine {
                    horizontal: true,
                    start,
                    end,
                    coordinate: y,
                });
            });
    }
    output
}

fn vertical_candidates(image: &GrayImage, min_run: u32) -> Vec<CandidateLine> {
    let (width, height) = image.dimensions();
    let mut output = Vec::new();
    for x in 0..width {
        scan_line(height, min_run, |y| is_ink(image.get_pixel(x, y).0[0]))
            .into_iter()
            .for_each(|(start, end)| {
                output.push(CandidateLine {
                    horizontal: false,
                    start,
                    end,
                    coordinate: x,
                });
            });
    }
    output
}

fn scan_line(
    length: u32,
    minimum_run: u32,
    mut is_dark: impl FnMut(u32) -> bool,
) -> Vec<(u32, u32)> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < length {
        while index < length && !is_dark(index) {
            index += 1;
        }
        let start = index;
        let mut last_dark = index;
        let mut gaps = 0;
        while index < length {
            if is_dark(index) {
                last_dark = index;
                gaps = 0;
            } else {
                gaps += 1;
                if gaps > 1 {
                    break;
                }
            }
            index += 1;
        }
        if last_dark >= start && last_dark - start + 1 >= minimum_run {
            output.push((start, last_dark));
        }
        index = index.max(last_dark.saturating_add(1));
    }
    output
}

fn is_ink(luminance: u8) -> bool {
    luminance < 150
}

fn coordinate_distance(line: MergedLine, candidate: CandidateLine) -> u32 {
    let average = (line.coordinate_sum / u64::from(line.samples)) as u32;
    average.abs_diff(candidate.coordinate)
}

fn overlap_ratio(a_start: u32, a_end: u32, b_start: u32, b_end: u32) -> f32 {
    let overlap = a_end.min(b_end).saturating_sub(a_start.max(b_start));
    let shorter = (a_end - a_start).min(b_end - b_start).max(1);
    overlap as f32 / shorter as f32
}

fn line_to_feature(line: MergedLine, width: u32, height: u32) -> TraceFeature {
    let coordinate = line.coordinate_sum as f32 / line.samples as f32;
    let length = line.end.saturating_sub(line.start) as f32;
    let scale = width.max(height) as f32;
    let ratio = length / scale;
    let kind = if line.samples >= 3 || ratio >= 0.16 {
        TraceKind::Wall
    } else if ratio >= 0.075 {
        TraceKind::Cubicle
    } else if ratio >= 0.035 {
        TraceKind::Door
    } else {
        TraceKind::Furniture
    };
    let points = if line.horizontal {
        vec![
            NormalizedPoint {
                x: line.start as f32 / width as f32,
                y: coordinate / height as f32,
            },
            NormalizedPoint {
                x: line.end as f32 / width as f32,
                y: coordinate / height as f32,
            },
        ]
    } else {
        vec![
            NormalizedPoint {
                x: coordinate / width as f32,
                y: line.start as f32 / height as f32,
            },
            NormalizedPoint {
                x: coordinate / width as f32,
                y: line.end as f32 / height as f32,
            },
        ]
    };
    TraceFeature {
        id: Uuid::new_v4().to_string(),
        kind,
        points,
        thickness: line.samples.clamp(1, 8) as f32,
    }
}

#[cfg(test)]
mod tests {
    use image::{GrayImage, Luma};

    use super::auto_trace;
    use crate::portal::models::TraceKind;

    #[test]
    fn traces_long_wall_and_shorter_furniture_lines() {
        let mut image = GrayImage::from_pixel(400, 240, Luma([255]));
        for y in 30..34 {
            for x in 20..360 {
                image.put_pixel(x, y, Luma([0]));
            }
        }
        for x in 120..180 {
            image.put_pixel(x, 120, Luma([0]));
        }
        let trace = auto_trace(&image);
        assert!(trace.iter().any(|feature| feature.kind == TraceKind::Wall));
        assert!(trace.len() >= 2);
    }
}

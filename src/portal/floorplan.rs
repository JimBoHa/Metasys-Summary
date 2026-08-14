use std::{io::Cursor, process::Command};

use anyhow::{Context, Result, bail};
use image::{DynamicImage, ImageFormat, imageops::FilterType};

use super::models::TraceFeature;

pub const MAX_PDF_BYTES: usize = 25 * 1024 * 1024;
const MAX_RENDER_DIMENSION: u32 = 1_800;

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
    let mut encoded = Cursor::new(Vec::new());
    image
        .write_to(&mut encoded, ImageFormat::Png)
        .context("encode cleaned floor-plan preview")?;

    Ok(ProcessedFloorPlan {
        image_data: encoded.into_inner(),
        width,
        height,
        trace: Vec::new(),
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

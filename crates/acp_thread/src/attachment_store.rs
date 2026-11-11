use agent_client_protocol::v2::schema as acp;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

const MAX_BYTES: usize = 32 * 1024 * 1024;

pub fn root() -> PathBuf {
    paths::data_dir().join("agent-attachments")
}

pub fn save_data_url(directory: &Path, url: &str) -> Result<PathBuf> {
    let (header, data) = url.split_once(',').context("Missing embedded image data")?;
    let mime = header
        .strip_prefix("data:")
        .and_then(|s| s.strip_suffix(";base64"))
        .context("Only embedded base64 images can be restored automatically")?;
    save_image(directory, data, mime)
}

pub fn import_file(directory: &Path, source: &Path) -> Result<PathBuf> {
    let name = source.to_string_lossy();
    ensure!(
        source.is_absolute() && !name.starts_with("//") && !name.starts_with(r"\\"),
        "Only local attachment paths are supported"
    );
    let file = std::fs::File::open(source)?;
    ensure!(
        file.metadata()?.len() <= MAX_BYTES as u64,
        "Attachment exceeds 32 MiB"
    );
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(file, MAX_BYTES as u64 + 1),
        &mut bytes,
    )?;
    ensure!(bytes.len() <= MAX_BYTES, "Attachment exceeds 32 MiB");
    save_image(directory, &STANDARD.encode(bytes), "image/unknown")
}

pub fn save_image(directory: &Path, data: &str, mime: &str) -> Result<PathBuf> {
    ensure!(
        data.len() <= MAX_BYTES * 4 / 3 + 4,
        "Attachment exceeds 32 MiB"
    );
    ensure!(mime.starts_with("image/"), "Not an image attachment");
    let bytes = STANDARD
        .decode(data)
        .context("Invalid image attachment encoding")?;
    ensure!(bytes.len() <= MAX_BYTES, "Attachment exceeds 32 MiB");
    let extension = match image::guess_format(&bytes)? {
        image::ImageFormat::Png => "png",
        image::ImageFormat::Jpeg => "jpg",
        image::ImageFormat::Gif => "gif",
        image::ImageFormat::WebP => "webp",
        image::ImageFormat::Bmp => "bmp",
        _ => bail!("Unsupported image attachment format"),
    };
    let digest = format!("{:x}", Sha256::digest(&bytes));
    std::fs::create_dir_all(directory).context("Could not create permanent attachment storage")?;
    let path = directory.join(format!("{digest}.{extension}"));
    if path.exists() {
        ensure!(
            std::fs::read(&path)? == bytes,
            "Stored attachment is damaged"
        );
        return Ok(path);
    }
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(&path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                std::fs::read(&path)? == bytes,
                "Stored attachment is damaged"
            );
        }
        Err(error) => return Err(error.error.into()),
    }
    Ok(path)
}

pub fn prepare(directory: &Path, blocks: Vec<acp::ContentBlock>) -> Result<Vec<acp::ContentBlock>> {
    let existing_text = blocks
        .iter()
        .filter_map(|block| match block {
            acp::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut output = Vec::new();
    for mut block in blocks {
        if let acp::ContentBlock::Image(image) = &mut block {
            if image.data.is_empty()
                && image
                    .uri
                    .as_deref()
                    .is_some_and(|uri| uri.starts_with("https://") || uri.starts_with("http://"))
            {
                output.push(block);
                continue;
            }
            let path = save_image(directory, &image.data, image.mime_type.as_ref())?;
            let uri = url::Url::from_file_path(&path)
                .map_err(|_| anyhow::anyhow!("Invalid attachment path"))?
                .to_string();
            image.uri = Some(uri);
            // Keep a plain path in the agent's context as well as the visual input.
            let reference = format!(
                "\n\nOriginal image attachment (permanent local file): {}\n",
                path.display()
            );
            if !existing_text.contains(reference.trim()) {
                output.push(acp::ContentBlock::Text(acp::TextContent::new(reference)));
            }
        }
        output.push(block);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn png() -> String {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(4, 4)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("PNG");
        STANDARD.encode(bytes.into_inner())
    }
    #[test]
    fn attachments_are_durable_deduplicated_and_validated() {
        let directory = tempfile::tempdir().expect("temporary store");
        let encoded = png();
        let first = save_image(directory.path(), &encoded, "image/png").expect("save");
        assert_eq!(
            first,
            save_data_url(
                directory.path(),
                &format!("data:image/png;base64,{encoded}")
            )
            .expect("restore")
        );
        assert_eq!(
            std::fs::read_dir(directory.path()).expect("store").count(),
            1
        );
        assert_eq!(
            std::fs::read(first).expect("original"),
            STANDARD.decode(encoded).expect("decode")
        );
        assert!(save_image(directory.path(), "bad", "image/png").is_err());
        assert!(save_data_url(directory.path(), "https://example.org/private.png").is_err());
    }

    #[test]
    fn prompt_keeps_visual_input_and_adds_an_agent_readable_path() {
        let directory = tempfile::tempdir().expect("store");
        let data = png();
        let blocks = prepare(
            directory.path(),
            vec![acp::ContentBlock::Image(acp::ImageContent::new(
                data.clone(),
                "image/png",
            ))],
        )
        .expect("prepare");
        let acp::ContentBlock::Text(reference) = &blocks[0] else {
            panic!("file reference")
        };
        assert!(
            reference
                .text
                .contains(directory.path().to_string_lossy().as_ref())
        );
        let acp::ContentBlock::Image(image) = &blocks[1] else {
            panic!("visual input")
        };
        assert_eq!(image.data, data);
        let path = url::Url::parse(image.uri.as_deref().expect("uri"))
            .expect("URL")
            .to_file_path()
            .expect("file");
        assert!(path.is_file());
        assert_eq!(import_file(directory.path(), &path).expect("import"), path);
    }

    #[test]
    fn resending_does_not_duplicate_reference_and_corruption_is_not_overwritten() {
        let directory = tempfile::tempdir().expect("store");
        let blocks = prepare(
            directory.path(),
            vec![acp::ContentBlock::Image(acp::ImageContent::new(
                png(),
                "image/png",
            ))],
        )
        .expect("prepare");
        assert_eq!(prepare(directory.path(), blocks).expect("resend").len(), 2);
        let path = save_image(directory.path(), &png(), "image/png").expect("stored");
        std::fs::write(&path, b"damaged").expect("simulate damaged original");
        assert!(save_image(directory.path(), &png(), "image/png").is_err());
        assert_eq!(std::fs::read(path).expect("unchanged"), b"damaged");
    }
}

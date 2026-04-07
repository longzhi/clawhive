use anyhow::{Context as _, Result};
use base64::Engine as _;
use clawhive_provider::ContentBlock;
use clawhive_schema::{Attachment, AttachmentKind};

pub(super) const MAX_ATTACHMENT_TEXT_CHARS: usize = 12_000;
pub(super) const MAX_PDF_IMAGE_PAGES: usize = 8;

pub(super) fn is_text_mime(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime == "application/json"
        || mime == "application/xml"
        || mime == "application/javascript"
        || mime == "application/x-yaml"
        || mime == "application/yaml"
        || mime == "application/toml"
        || mime == "application/x-sh"
}

pub(super) fn build_user_content(
    text: String,
    attachment_blocks: Vec<ContentBlock>,
) -> Vec<ContentBlock> {
    let mut content = Vec::with_capacity(1 + attachment_blocks.len());
    if !text.is_empty() {
        content.push(ContentBlock::Text { text });
    }
    content.extend(attachment_blocks);
    content
}

pub(super) fn decode_attachment_bytes(attachment: &Attachment) -> Option<Vec<u8>> {
    match base64::engine::general_purpose::STANDARD.decode(&attachment.url) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            tracing::warn!(
                file_name = ?attachment.file_name,
                mime_type = ?attachment.mime_type,
                url_len = attachment.url.len(),
                error = %e,
                "failed to base64-decode attachment data"
            );
            None
        }
    }
}

pub(super) fn trim_attachment_text(text: &str) -> Option<String> {
    let trimmed = text.replace('\u{0000}', "").trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

pub(super) fn truncate_attachment_text(text: &str) -> String {
    if text.chars().count() <= MAX_ATTACHMENT_TEXT_CHARS {
        return text.to_string();
    }

    let end = text
        .char_indices()
        .nth(MAX_ATTACHMENT_TEXT_CHARS)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());
    format!(
        "{}\n\n[attachment text truncated after {} characters]",
        &text[..end],
        MAX_ATTACHMENT_TEXT_CHARS
    )
}

pub(super) fn extract_pdf_text(bytes: &[u8]) -> Result<String> {
    let document = lopdf::Document::load_mem(bytes).context("parse pdf attachment")?;
    let page_numbers: Vec<u32> = document.get_pages().keys().copied().collect();
    if page_numbers.is_empty() {
        return Ok(String::new());
    }

    document
        .extract_text(&page_numbers)
        .context("extract pdf attachment text")
}

pub(super) struct PdfPageImage {
    pub(super) data: Vec<u8>,
    pub(super) media_type: String,
}

pub(super) fn extract_pdf_page_images(bytes: &[u8]) -> Vec<PdfPageImage> {
    let document = match lopdf::Document::load_mem(bytes) {
        Ok(doc) => doc,
        Err(_) => return Vec::new(),
    };

    let pages = document.get_pages();
    let mut results = Vec::new();

    for (&_page_num, &page_id) in pages.iter().take(MAX_PDF_IMAGE_PAGES) {
        let images = match document.get_page_images(page_id) {
            Ok(imgs) => imgs,
            Err(_) => continue,
        };
        for img in images {
            let filters: Vec<&str> = img
                .filters
                .as_ref()
                .map(|f| f.iter().map(String::as_str).collect())
                .unwrap_or_default();

            if filters == ["DCTDecode"] {
                results.push(PdfPageImage {
                    data: img.content.to_vec(),
                    media_type: "image/jpeg".into(),
                });
            } else {
                tracing::debug!(
                    filters = ?filters,
                    width = img.width,
                    height = img.height,
                    color_space = ?img.color_space,
                    "skipping non-JPEG embedded image in scanned PDF"
                );
            }
        }
    }

    results
}

pub(super) fn attachment_prompt_fragment(attachment: &Attachment) -> Option<String> {
    let mime = attachment
        .mime_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    let label = attachment.file_name.as_deref().unwrap_or("attachment");

    let extracted_text = decode_attachment_bytes(attachment).and_then(|bytes| {
        let byte_len = bytes.len();
        if is_text_mime(mime) {
            match String::from_utf8(bytes) {
                Ok(text) => trim_attachment_text(&text).or_else(|| {
                    tracing::warn!(
                        file_name = label,
                        mime_type = mime,
                        byte_len,
                        "text attachment decoded but contained no usable text"
                    );
                    None
                }),
                Err(e) => {
                    tracing::warn!(
                        file_name = label,
                        mime_type = mime,
                        byte_len,
                        error = %e,
                        "text attachment is not valid UTF-8"
                    );
                    None
                }
            }
        } else if mime == "application/pdf" {
            match extract_pdf_text(&bytes) {
                Ok(text) => trim_attachment_text(&text).or_else(|| {
                    tracing::warn!(
                        file_name = label,
                        mime_type = mime,
                        byte_len,
                        "PDF parsed successfully but extracted text is empty — \
                         likely a scanned/image-only PDF without a text layer"
                    );
                    None
                }),
                Err(e) => {
                    tracing::warn!(
                        file_name = label,
                        mime_type = mime,
                        byte_len,
                        error = %e,
                        "failed to parse PDF attachment"
                    );
                    None
                }
            }
        } else {
            tracing::debug!(
                file_name = label,
                mime_type = mime,
                byte_len,
                "attachment MIME type not extractable, using binary placeholder"
            );
            None
        }
    });

    let body = match extracted_text {
        Some(text) => truncate_attachment_text(&text),
        None => "[binary attachment uploaded; automatic text extraction unavailable]".to_string(),
    };

    Some(format!(
        "<attachment name=\"{label}\" type=\"{mime}\">\n{body}\n</attachment>"
    ))
}

pub(super) fn build_attachment_blocks(attachments: &[Attachment]) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    for a in attachments {
        match a.kind {
            AttachmentKind::Image => {
                let media_type = a
                    .mime_type
                    .clone()
                    .or_else(|| sniff_image_mime_from_base64(&a.url))
                    .unwrap_or_else(|| "image/jpeg".to_string());
                blocks.push(ContentBlock::Image {
                    data: a.url.clone(),
                    media_type,
                });
            }
            _ => {
                let mime = a.mime_type.as_deref().unwrap_or("application/octet-stream");

                if mime == "application/pdf" {
                    if let Some(pdf_blocks) = build_pdf_content_blocks(a) {
                        blocks.extend(pdf_blocks);
                        continue;
                    }
                }

                if let Some(text) = attachment_prompt_fragment(a) {
                    blocks.push(ContentBlock::Text { text });
                }
            }
        }
    }
    blocks
}

pub(super) fn build_pdf_content_blocks(attachment: &Attachment) -> Option<Vec<ContentBlock>> {
    let label = attachment.file_name.as_deref().unwrap_or("attachment.pdf");
    let bytes = decode_attachment_bytes(attachment)?;
    let byte_len = bytes.len();

    match extract_pdf_text(&bytes) {
        Ok(ref text) => {
            if let Some(trimmed) = trim_attachment_text(text) {
                let body = truncate_attachment_text(&trimmed);
                let fragment = format!(
                    "<attachment name=\"{label}\" type=\"application/pdf\">\n{body}\n</attachment>"
                );
                return Some(vec![ContentBlock::Text { text: fragment }]);
            }

            tracing::info!(
                file_name = label,
                byte_len,
                "PDF text layer is empty, attempting image extraction for scanned PDF"
            );
            let images = extract_pdf_page_images(&bytes);
            if images.is_empty() {
                tracing::warn!(
                    file_name = label,
                    byte_len,
                    "scanned PDF fallback: no extractable images found"
                );
                return None;
            }
            let page_count = images.len();
            tracing::info!(
                file_name = label,
                page_count,
                "extracted scanned PDF page images for vision-based reading"
            );
            let mut blocks = Vec::with_capacity(1 + page_count);
            blocks.push(ContentBlock::Text {
                text: format!(
                    "<attachment name=\"{label}\" type=\"application/pdf\">\n\
                     [Scanned PDF — {page_count} page image(s) follow. \
                     Read and extract all text from these page images.]\n\
                     </attachment>"
                ),
            });
            for img in images {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&img.data);
                blocks.push(ContentBlock::Image {
                    data: b64,
                    media_type: img.media_type,
                });
            }
            Some(blocks)
        }
        Err(e) => {
            tracing::warn!(
                file_name = label,
                byte_len,
                error = %e,
                "failed to parse PDF, attempting raw image extraction"
            );
            let images = extract_pdf_page_images(&bytes);
            if images.is_empty() {
                return None;
            }
            let page_count = images.len();
            tracing::info!(
                file_name = label,
                page_count,
                "extracted images from unparseable PDF"
            );
            let mut blocks = Vec::with_capacity(1 + page_count);
            blocks.push(ContentBlock::Text {
                text: format!(
                    "<attachment name=\"{label}\" type=\"application/pdf\">\n\
                     [PDF text extraction failed — {page_count} page image(s) follow. \
                     Read and extract all text from these page images.]\n\
                     </attachment>"
                ),
            });
            for img in images {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&img.data);
                blocks.push(ContentBlock::Image {
                    data: b64,
                    media_type: img.media_type,
                });
            }
            Some(blocks)
        }
    }
}

pub(super) fn build_session_text(user_text: &str, attachments: &[Attachment]) -> String {
    let mut parts = vec![user_text.to_string()];
    for a in attachments {
        if matches!(a.kind, AttachmentKind::Image) {
            continue;
        }
        if let Some(fragment) = attachment_prompt_fragment(a) {
            parts.push(fragment);
        }
    }
    parts.join("\n\n")
}

/// Decode the first few bytes of a base64 image to detect its actual format.
fn sniff_image_mime_from_base64(b64: &str) -> Option<String> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    // Only need the first 8 bytes to check magic numbers.
    let prefix = &b64[..b64.len().min(16)];
    let bytes = STANDARD.decode(prefix).ok()?;

    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        Some("image/png".to_string())
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg".to_string())
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif".to_string())
    } else if bytes.starts_with(b"RIFF") && bytes.len() >= 8 {
        Some("image/webp".to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pdf_bytes(text: &str) -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        use lopdf::{dictionary, Document, Object, Stream};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! {
                "F1" => font_id,
            },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![72.into(), 720.into()]),
                Operation::new("Tj", vec![Object::string_literal(text)]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });

        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn build_attachment_blocks_extracts_pdf_text() {
        use base64::Engine;

        let attachment = Attachment {
            kind: AttachmentKind::Document,
            url: base64::engine::general_purpose::STANDARD
                .encode(sample_pdf_bytes("Lease says landlord pays")),
            mime_type: Some("application/pdf".to_string()),
            file_name: Some("lease.pdf".to_string()),
            size: None,
        };

        let blocks = build_attachment_blocks(&[attachment]);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("lease.pdf"));
                assert!(text.contains("Lease says landlord pays"));
            }
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[test]
    fn build_session_text_keeps_binary_attachment_placeholder() {
        let session_text = build_session_text(
            "请看合同",
            &[Attachment {
                kind: AttachmentKind::Document,
                url: "not-base64".to_string(),
                mime_type: Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                        .to_string(),
                ),
                file_name: Some("lease.docx".to_string()),
                size: None,
            }],
        );

        assert!(session_text.contains("lease.docx"));
        assert!(session_text.contains("binary attachment uploaded"));
    }
}

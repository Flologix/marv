//! `docs/SECURITY.md`, "Images (when vision is added)". Nothing reads images yet; each rule's test is written and
//! ignored until vision lands.

use marv_core::Provenance;

/// **Any image taints the session**, and its text reaches the model only fenced as `Provenance::Image`.
///
/// Proven: made `Provenance::taints` false for `Image` → failed. (The fence is covered by
/// `provenance.rs::outside_text_is_tagged_and_fenced_as_data`.)
#[test]
fn an_image_taints() {
    assert!(Provenance::Image.taints());
}

/// **Sources: only the user** (a paste, or a file they name); never forwarded from web, MCP or mesh content. An MCP
/// result with an image block reaches the model as text, not as an image part.
#[test]
#[ignore = "needs vision (images are not built yet)"]
fn images_come_only_from_the_user() {
    unimplemented!("an MCP image ContentBlock → format_mcp_result has no image part; a web page's <img> is never fetched");
}

/// **A quarantined reader**: a tools-less model call describes the image; the main model sees only that, fenced as
/// `Provenance::Image`, and it's shown in the transcript.
#[test]
#[ignore = "needs vision (images are not built yet)"]
fn a_quarantined_reader_describes_images() {
    unimplemented!("the reader's request has no tools; the main request has no image part, only the fenced description");
}

/// **Decoding happens in the sandbox**, with metadata stripped and size limits.
#[test]
#[ignore = "needs vision (images are not built yet)"]
fn images_are_decoded_in_the_sandbox() {
    unimplemented!("decode via bwrap; EXIF/text chunks gone; an oversize image is refused before decoding");
}

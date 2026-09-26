//! The `offline_api` Markdownize adapter — PaddleOCR-VL reached over loopback
//! (07 §5.2's document-processing category, 07 §3's D1 url restriction).
//!
//! # Why this does not talk `/v1/chat/completions`
//!
//! The plan originally described this adapter as posting a data-URI image to an
//! OpenAI-compatible `/v1/chat/completions`, the way a generative VLM would be
//! driven. **That endpoint cannot do the job**, and the reason is architectural
//! rather than a matter of request shape.
//!
//! PaddleOCR-VL is two models. `PP-DocLayoutV2` (an RT-DETR detector plus a
//! pointer network) finds the layout elements, classifies them, and predicts
//! reading order; the 0.9B VLM then recognizes the content of regions that have
//! *already been cropped for it*. **Bounding boxes come from the first model,
//! and the OpenAI-compatible `paddleocr genai_server` serves only the second.**
//! Calling it directly returns recognized text for a region nobody located,
//! with no layout, no reading order, no bbox and no Markdown assembly — and
//! Kio cannot make up the difference, because the missing stage is a separate
//! detection model. Upstream says so in as many words: "It is strongly
//! discouraged to directly call such services through plain HTTP requests or
//! OpenAI clients to process document images."
//!
//! So the target is `POST /layout-parsing`, the end-to-end pipeline service.
//! It is a PaddleX-shaped API, not a chat one, which places this adapter in
//! 07 §8's *document-processing* category alongside Mistral OCR rather than in
//! the generative-LLM category — the prompt contract does not apply, only
//! §8.1's acceptance checks and the I/O schema do. `mistral_ocr.rs` is the
//! shape to follow here, not `bbox_annotation.rs`.
//!
//! # Two things Kio gives up by not prompting the model
//!
//! `/layout-parsing` takes no `temperature` and no `seed`. Determinism is the
//! server operator's configuration, not this adapter's request — which matters
//! because 07 §9's first-instance-wins freezes whatever the first call returned,
//! permanently. The acceptance check therefore has to *prove* determinism by
//! sending the same input twice, rather than pinning a sampling parameter and
//! assuming it.
//!
//! For the same reason `prompt_template_id` / `prompt_template_hash` are absent
//! from this profile. Kio supplies no prompt, so there is no template to hash,
//! and recording one would make the identity describe an input this adapter
//! does not produce.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::bbox_annotation::{canonical_source_escape, validate_bbox as validate_annotation_bbox};
use crate::http_policy::{
    HttpPolicy, HttpResponse, authenticated_local_agent, read_json_bounded, require_success,
};
use crate::identity::tool_profile_hash;
use crate::local_peer::{AuthenticatedLocalEndpoint, is_local_peer_tls_error};
use crate::mistral_ocr::{OcrImage, image_hash};
use crate::traits::MarkdownizeAdapter;
use crate::types::{
    AdapterKind, AdapterProfile, ExecutionMode, MarkdownUnit, MarkdownizeRequest,
    MarkdownizeResponse, PreparedUnitHint, UnitKind,
};
use crate::{AdapterError, Result};

/// The `tool_id` the local OCR target is declared under in `tools.toml`.
pub const LOCAL_OCR_ADAPTER_ID: &str = "paddleocr_vl_local";

/// 03 §5.1's `model_or_tool_family`. Names the pipeline, not the serving
/// backend: D5 keeps the runtime out of identity, and the same weights behind
/// a different server must stay one profile.
pub const LOCAL_OCR_MODEL_FAMILY: &str = "paddleocr-vl";

/// The model id the pipeline service is expected to be running. Upstream has
/// moved this name at least once (`PaddleOCR-VL-0.9B` → `PaddleOCR-VL-1.6-0.9B`),
/// which is exactly why 03 §5.1 pins weights by digest rather than by name —
/// see [`LOCAL_OCR_MODEL_VERSION_PIN`].
pub const LOCAL_OCR_DEFAULT_MODEL: &str = "PaddleOCR-VL-0.9B";

/// Response ceiling. `/layout-parsing` inlines page images as base64 by
/// default, so a multi-page PDF's response is dominated by image bytes rather
/// than by text and needs a far larger bound than an embedding response.
pub const LAYOUT_PARSING_RESPONSE_MAX_BYTES: usize = 256 * 1024 * 1024;

/// Per-response cap on persisted image bytes, mirroring the online OCR policy.
pub const LOCAL_OCR_MAX_PERSISTED_IMAGE_BYTES: usize = 64 * 1024 * 1024;

// Bound bookkeeping even for empty images. Count source entries and Markdown
// references together; 16,384 leaves room for two figures per maximum-size page.
const LOCAL_OCR_MAX_IMAGE_ITEMS: usize = 16_384;

struct ImageAdmissionBudget {
    bytes_remaining: usize,
    items_remaining: usize,
}

impl ImageAdmissionBudget {
    fn charge(&mut self, bytes: usize, items: usize) -> Result<()> {
        let remaining_bytes = self
            .bytes_remaining
            .checked_sub(bytes)
            .ok_or_else(|| violation("layout-parsing decoded image byte limit exceeded"))?;
        let remaining_items = self
            .items_remaining
            .checked_sub(items)
            .ok_or_else(|| violation("layout-parsing image item limit exceeded"))?;
        self.bytes_remaining = remaining_bytes;
        self.items_remaining = remaining_items;
        Ok(())
    }
}

/// Upper bound on pages accepted from one response, so a malformed or hostile
/// reply cannot mint unbounded units.
pub const LOCAL_OCR_MAX_PAGES: usize = 4096;

/// Which local implementation runs. Same posture as
/// [`crate::local_embedding::LocalEmbeddingExecution`]: `Copy`, and the url /
/// model that only `Real` needs are read where the adapter is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalOcrExecution {
    #[cfg(debug_assertions)]
    Mock,
    Real,
}

/// What `fileType` the service should be told the bytes are (0 = PDF,
/// 1 = image, per the `/layout-parsing` request schema).
///
/// Kio always sends this explicitly. The field is optional upstream, where an
/// absent value makes the server infer the type *from the URL* — and Kio sends
/// base64, which has no URL to infer from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutFileType {
    Pdf,
    Image,
}

impl LayoutFileType {
    #[must_use]
    pub fn wire_value(self) -> u8 {
        match self {
            Self::Pdf => 0,
            Self::Image => 1,
        }
    }

    /// Media types Kio routes to this adapter. Anything else is a caller error
    /// rather than something to guess at: sending a PDF as `fileType: 1` makes
    /// the service parse the first page as an image and silently lose the rest.
    ///
    /// **Every type listed here must also be nameable by
    /// [`crate::mistral_ocr::discovered_unit_kind`]**, because routing a file
    /// to this adapter commits to minting its units — a type this answers `Ok`
    /// for and that one refuses is routed here only to die at hint time
    /// (S3-I). The reverse gap is fine and is what `image/gif` is today:
    /// nameable but not routed, so it stays on the online lane instead.
    ///
    /// `image/tiff` was listed until 2026-08-05. Nothing produced it — the
    /// scanner's extension table has no `tif` — so it was dead, and once
    /// discovery started asking the media type it was dead *and* self-
    /// contradictory. The rest of the codebase already treats `.tiff` as an
    /// unsupported input to disclose, so claiming it here was the outlier.
    pub fn from_media_type(media_type: &str) -> Result<Self> {
        match media_type {
            "application/pdf" => Ok(Self::Pdf),
            "image/png" | "image/jpeg" | "image/webp" => Ok(Self::Image),
            other => Err(AdapterError::ContractViolation(format!(
                "local OCR adapter cannot route media type {other}"
            ))),
        }
    }
}

/// One document in, one parsed document out.
///
/// There is no per-page method and that is deliberate: `/layout-parsing` takes
/// a whole PDF and returns one element per page, so a page-at-a-time signature
/// would invite re-uploading the same document once per page.
pub trait LocalOcrClient: Clone {
    /// `file_base64` is the base64 of the *verified* raw bytes. The return is
    /// the service's parsed JSON body, left as `Value` so that parsing and
    /// transport stay separable — [`parse_layout_parsing`] is what gives it
    /// meaning, and it is unit-testable without a server.
    fn layout_parse(&self, file_base64: &str, file_type: LayoutFileType) -> Result<Value>;
}

/// Talks `POST {base_url}/layout-parsing` to a loopback pipeline service.
#[derive(Debug, Clone)]
pub struct EnvLocalOcrClient {
    endpoint: AuthenticatedLocalEndpoint,
    http_policy: HttpPolicy,
}

impl EnvLocalOcrClient {
    /// `timeout_seconds` is D7's `[adapter.policy.offline_api].timeout_seconds`
    /// (07 §7); `None` keeps the shared default. A CPU-inference document
    /// pipeline is the case D7 was written for — a multi-page PDF can occupy
    /// the server for minutes with nothing on the socket.
    #[must_use]
    pub fn new(endpoint: AuthenticatedLocalEndpoint, timeout_seconds: Option<u64>) -> Self {
        Self {
            endpoint,
            http_policy: timeout_seconds
                .map_or_else(HttpPolicy::default, HttpPolicy::with_timeout_seconds),
        }
    }
}

impl LocalOcrClient for EnvLocalOcrClient {
    fn layout_parse(&self, file_base64: &str, file_type: LayoutFileType) -> Result<Value> {
        let url = format!("{}/layout-parsing", self.endpoint.base_url());
        let response = authenticated_local_agent(&self.endpoint, self.http_policy)?
            .post(&url)
            .send_json(json!({
                "file": file_base64,
                "fileType": file_type.wire_value(),
                // Asked for explicitly rather than left to the server default:
                // with layout detection off there are no block bboxes and no
                // reading order, which is the entire reason this endpoint was
                // chosen over the VLM-only one.
                "useLayoutDetection": true,
            }))
            .map_err(local_ocr_http_error)
            .and_then(|response| require_success(response, local_ocr_http_status_error))?;
        read_json_bounded(
            response,
            LAYOUT_PARSING_RESPONSE_MAX_BYTES,
            "local OCR layout-parsing response",
        )
    }
}

/// A local pipeline has no credential and no invoice, so `Auth` and
/// `QuotaExceeded` cannot arise. A full queue can, and that is a retry.
fn local_ocr_http_error(error: ureq::Error) -> AdapterError {
    if is_local_peer_tls_error(&error) {
        return AdapterError::LocalPeerAuth(format!(
            "local OCR service TLS authentication failed: {error}"
        ));
    }
    AdapterError::Network(format!("local OCR service unreachable: {error}"))
}

fn local_ocr_http_status_error(response: &HttpResponse) -> AdapterError {
    match response.status().as_u16() {
        429 => {
            let retry_after_ms = response
                .headers()
                .get("Retry-After")
                .and_then(|value| value.to_str().ok())
                .and_then(crate::http_policy::parse_retry_after_ms);
            AdapterError::RateLimit {
                message: format!("local OCR service queue is full ({})", response.status()),
                retry_after_ms,
            }
        }
        code => AdapterError::Network(format!("local OCR service returned HTTP {code}")),
    }
}

fn violation(message: impl Into<String>) -> AdapterError {
    AdapterError::ContractViolation(message.into())
}

/// One page of `/layout-parsing` output, before it becomes Kio units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutParsedPage {
    pub index: usize,
    pub markdown: String,
    pub images: Vec<OcrImage>,
    /// Every block of `prunedResult.parsing_res_list`, as the provider spelled it.
    pub blocks: Vec<LayoutBlock>,
}

/// One entry of `prunedResult.parsing_res_list`, kept verbatim.
///
/// **This is an observation, never a judgment.** "The provider labelled this span
/// `footer`" can be measured; "this span is furniture" cannot — see
/// `tasks/furniture-text-recovery-design.md` §2, where the label, the position and
/// the size were each measured against the nine captures and each failed to
/// separate the twelve dropped blocks into furniture and body. One `footer` holds
/// `Type a message...` and another holds `All critical tokens detected…`.
///
/// So the whole list is recorded rather than the subset something decided was
/// noise, and the naming stays the provider's. Whoever eventually has a corpus to
/// judge by will find the measurement here instead of a conclusion drawn without
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutBlock {
    pub label: String,
    pub bbox: Option<Vec<i64>>,
    pub content: String,
}

/// Labels whose `block_content` the service recognises and then leaves out of
/// `markdown.text`.
///
/// Measured over the nine captures in `tests/fixtures/layout-parsing/`: twelve
/// blocks hold content that never reaches the Markdown, and all twelve wear one of
/// these three labels. Four of the twelve are plainly body text — a routing slip's
/// title and its deadline, and an infographic's closing summary — which is why
/// they are recovered at all.
///
/// The list is the measurement and not a theory, so it stays at what was measured.
/// Widening it to "anything recognised but absent from the Markdown" reads better
/// and is wrong: a table's raw HTML is absent too, because
/// [`convert_html_tables_to_gfm`] already put its content there in GFM, and
/// recovering it would insert every table twice.
const RECOVERED_BLOCK_LABELS: [&str; 3] = ["header", "footer", "number"];

/// Turn the service's body into pages.
///
/// This is where V2's findings are actually enforced, so it is strict on
/// purpose: a mis-paired bbox is not a cosmetic defect. Image objects are
/// content-addressed and 07 §9 makes the first instance win, so a bbox attached
/// to the wrong figure is frozen for the life of the archive and can only be
/// undone by re-running markdownize over everything.
pub fn parse_layout_parsing(body: &Value) -> Result<Vec<LayoutParsedPage>> {
    parse_layout_parsing_with_budget(
        body,
        ImageAdmissionBudget {
            bytes_remaining: LOCAL_OCR_MAX_PERSISTED_IMAGE_BYTES,
            items_remaining: LOCAL_OCR_MAX_IMAGE_ITEMS,
        },
    )
}

fn parse_layout_parsing_with_budget(
    body: &Value,
    mut budget: ImageAdmissionBudget,
) -> Result<Vec<LayoutParsedPage>> {
    // The service answers inside an envelope — `{logId, errorCode, errorMsg,
    // result}` — and the pages live under `result`, not at the top level.
    // Measured 2026-08-02 against paddleocr-vl:latest-nvidia-gpu-offline; the
    // earlier top-level reading came from the docs and matched nothing the
    // server actually sends, so every real response was rejected.
    //
    // `errorCode` is checked before the payload because it rides on an HTTP
    // 200: the transport is happy while the pipeline is not, and reading past
    // it would turn a service-side failure into "a document with no pages".
    if let Some(code) = body.get("errorCode").and_then(Value::as_i64)
        && code != 0
    {
        let message = body
            .get("errorMsg")
            .and_then(Value::as_str)
            .unwrap_or("(no errorMsg)");
        return Err(violation(format!(
            "layout-parsing returned errorCode {code}: {message}"
        )));
    }
    let results = body
        .get("result")
        .and_then(|result| result.get("layoutParsingResults"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            violation("layout-parsing response has no result.layoutParsingResults array")
        })?;
    if results.is_empty() {
        return Err(violation("layout-parsing returned no pages"));
    }
    if results.len() > LOCAL_OCR_MAX_PAGES {
        return Err(violation(format!(
            "layout-parsing returned {} pages, over the {LOCAL_OCR_MAX_PAGES} limit",
            results.len()
        )));
    }
    results
        .iter()
        .enumerate()
        .map(|(index, result)| parse_one_page(index, result, &mut budget))
        .collect()
}

fn parse_one_page(
    index: usize,
    result: &Value,
    budget: &mut ImageAdmissionBudget,
) -> Result<LayoutParsedPage> {
    let markdown_obj = result
        .get("markdown")
        .and_then(Value::as_object)
        .ok_or_else(|| violation(format!("page {index} has no markdown object")))?;
    // Normalized before anything reads it, so `markdown_image_paths` below, the
    // URI rewrite in `replace_image_placeholders`, and `extract_related_images`
    // over in `kio-search` all see one image spelling instead of three chances
    // to disagree about what an image reference is.
    let markdown = normalize_html_image_refs(
        index,
        markdown_obj
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| violation(format!("page {index} has no markdown.text")))?,
    )?;
    let markdown = unwrap_presentational_html(&markdown);
    // After the two above, so a cell's figure is already `![](…)` and no `<div>`
    // survives inside one — the only markup left in a cell is markup nobody has
    // measured, which is exactly what the converter refuses to guess at.
    let markdown = convert_html_tables_to_gfm(&markdown);

    // `markdown.images` is a relative-path → bytes map. It is keyed by the path
    // the Markdown refers to, which is what lets a bbox be matched to a figure
    // by name instead of by position.
    // Validate and admit every source entry, including unreferenced images,
    // before allocating any decoded bytes. Keep only borrowed encoded strings.
    let mut image_sources = BTreeMap::new();
    if let Some(images) = markdown_obj.get("images").and_then(Value::as_object) {
        budget.charge(0, images.len())?;
        for (relative_path, encoded) in images {
            let encoded = encoded.as_str().ok_or_else(|| {
                violation(format!(
                    "page {index} image {relative_path} is not a base64 string"
                ))
            })?;
            let decoded_len = base64_decoded_len(encoded).ok_or_else(|| {
                violation(format!(
                    "page {index} image {relative_path} is not valid base64"
                ))
            })?;
            budget.charge(decoded_len, 0)?;
            image_sources.insert(relative_path.as_str(), (encoded, decoded_len, false));
        }
    }

    require_layout_parsing_response(index, result)?;
    let blocks = page_blocks(result);
    let markdown = append_recovered_blocks(&markdown, &blocks);
    let referenced = markdown_image_paths_with_budget(&markdown, budget)?;
    let images = images_with_their_own_boxes(index, &referenced, image_sources, budget)?;
    Ok(LayoutParsedPage {
        index,
        markdown,
        images,
        blocks,
    })
}

/// Read `prunedResult.parsing_res_list` into [`LayoutBlock`]s.
///
/// Lossy only where the response is: a block with no `block_label` is skipped
/// because there is nothing to record it as, and a malformed `block_bbox` becomes
/// `None` rather than a guess. [`require_layout_parsing_response`] has already
/// established that the list is present, so its absence here means an empty page
/// and not a wrong endpoint.
fn page_blocks(result: &Value) -> Vec<LayoutBlock> {
    let Some(list) = result
        .get("prunedResult")
        .and_then(|pruned| pruned.get("parsing_res_list"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|block| {
            let label = block.get("block_label").and_then(Value::as_str)?;
            let bbox = block
                .get("block_bbox")
                .and_then(Value::as_array)
                .map(|corners| corners.iter().filter_map(Value::as_i64).collect::<Vec<_>>())
                .filter(|corners| corners.len() == 4);
            Some(LayoutBlock {
                label: label.to_owned(),
                bbox,
                content: block
                    .get("block_content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
        })
        .collect()
}

/// Put the text the service recognised and then dropped back into the page.
///
/// Appended rather than interleaved, and that is a limitation worth naming. The
/// Markdown arrives already assembled by the service, and nothing maps a position
/// inside that string back to the block that produced it — a table's cells reach
/// it through [`convert_html_tables_to_gfm`], so even exact-matching a block's
/// content against the Markdown finds nothing for them. Appending is the option
/// that stays deterministic, and byte determinism is the property 07 §5.2.1
/// actually freezes. Within the appended run the order is the page's own, by box,
/// top to bottom and then left to right, so a reader gets reading order even
/// though the run as a whole sits at the end.
///
/// The content is escaped on the way in. 07 §5.2.1 requires it of provider raw
/// text embedded in Markdown body **whatever its origin** — the same
/// [`canonical_source_escape`] `bbox_annotation` applies — so a status bar reading
/// `<div>` cannot smuggle raw HTML past the acceptance check.
fn append_recovered_blocks(markdown: &str, blocks: &[LayoutBlock]) -> String {
    let mut recovered: Vec<&LayoutBlock> = blocks
        .iter()
        .filter(|block| RECOVERED_BLOCK_LABELS.contains(&block.label.as_str()))
        .filter(|block| !block.content.trim().is_empty())
        .collect();
    if recovered.is_empty() {
        return markdown.to_owned();
    }
    // Top to bottom, then left to right. Blocks without a box keep the response's
    // own order behind those that have one, rather than being dropped or sorted
    // against a coordinate that does not exist.
    recovered.sort_by_key(|block| {
        block
            .bbox
            .as_ref()
            .map(|corners| (0, corners[1], corners[0]))
            .unwrap_or((1, 0, 0))
    });

    let mut out = markdown.to_owned();
    for block in recovered {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&canonical_source_escape(block.content.trim()));
        out.push('\n');
    }
    out
}

/// Refuse a response that has Markdown but no `prunedResult.parsing_res_list`.
///
/// That shape is what the VLM-only `paddleocr genai_server` endpoint produces:
/// it recognizes text for a region nobody located, so it returns no layout, no
/// reading order and no boxes. Saying so beats letting the caller conclude the
/// page simply had no figures — the wrong endpoint is a configuration mistake
/// someone can fix, and it is the first thing to suspect when a real document
/// yields no images at all.
///
/// Nothing else is read from the list. Boxes come from each crop's own file name
/// (see [`images_with_their_own_boxes`]), which is why `block_order` and the
/// reading order it was supposed to give no longer matter here: the Markdown's
/// own reference order is the order, and each reference names its box.
fn require_layout_parsing_response(page_index: usize, result: &Value) -> Result<()> {
    if result
        .get("prunedResult")
        .and_then(|pruned| pruned.get("parsing_res_list"))
        .and_then(Value::as_array)
        .is_some()
    {
        return Ok(());
    }
    Err(violation(format!(
        "page {page_index} has no prunedResult.parsing_res_list — is this the \
         VLM-only genai_server endpoint rather than /layout-parsing?"
    )))
}

/// Relative image paths in the order the Markdown refers to them.
///
/// Order is what pairs a figure with a box, so this walks the text rather than
/// reading `markdown.images`' keys — a map is sorted by key, and key order has
/// nothing to do with where a figure sits on the page.
#[cfg(test)]
fn markdown_image_paths(markdown: &str) -> Vec<String> {
    markdown_image_paths_with_budget(
        markdown,
        &mut ImageAdmissionBudget {
            bytes_remaining: LOCAL_OCR_MAX_PERSISTED_IMAGE_BYTES,
            items_remaining: LOCAL_OCR_MAX_IMAGE_ITEMS,
        },
    )
    .unwrap()
}

fn markdown_image_paths_with_budget(
    markdown: &str,
    budget: &mut ImageAdmissionBudget,
) -> Result<Vec<String>> {
    // CommonMark only, because `normalize_html_image_refs` has already run and
    // every figure on this page is in that spelling by now.
    let bytes = markdown.as_bytes();
    let mut paths = Vec::new();
    let mut cursor = 0;
    while let Some(found) = markdown[cursor..].find("![") {
        let open = cursor + found;
        let Some(alt_end) = markdown[open..].find("](") else {
            break;
        };
        let target_start = open + alt_end + 2;
        let Some(target_len) = markdown[target_start..].find(')') else {
            break;
        };
        let target = markdown[target_start..target_start + target_len].trim();
        if !target.is_empty() {
            budget.charge(0, 1)?;
            paths.push(target.to_owned());
        }
        cursor = target_start + target_len + 1;
        if cursor >= bytes.len() {
            break;
        }
    }
    Ok(paths)
}

/// Rewrite PaddleOCR-VL's HTML figure markup into the CommonMark image form,
/// so that exactly one image spelling ever enters the archive.
///
/// Measured 2026-08-02: a figure comes back as
/// `<div style="text-align: center;"><img src="imgs/img_in_chart_box_….jpg"
/// alt="Image" width="82%" /></div>` — there is no `![](…)` anywhere in the
/// Markdown.
///
/// Normalizing here rather than teaching every reader about `<img>` is what
/// keeps the quirk contained. The URIs written into this Markdown are read back
/// by `kio-search`'s `extract_related_images`, which drives `related_images[]`,
/// which images get embedded, the scope projection, and purge's orphan test —
/// and `kio-search` cannot share code with this crate (neither depends on the
/// other), so a second spelling in the corpus means a second scanner kept in
/// step by hand. It was not, and every one of those four read zero images from
/// a page that had one.
///
/// The label is dropped rather than carried over from `alt`. Nothing in Kio
/// reads it — `extract_related_images` reads the *target* — and an empty label
/// is exactly what the online route emits, so both adapters produce the same
/// shape. It also leaves no place for provider text to reach a structural
/// position: an `alt` of `x](y) [` would otherwise rewrite the reference.
///
/// Deliberately not a general HTML parser: it reads `src` out of `<img …>` and
/// nothing else. The wrapping `<div>` is left alone — stripping provider HTML
/// in general is a separate question, and this is only about image references.
fn normalize_html_image_refs(page_index: usize, markdown: &str) -> Result<String> {
    // ASCII lowercasing preserves byte offsets, so indices into `lower` index
    // `markdown` too. Built once per page rather than once per tag.
    let lower = markdown.to_ascii_lowercase();
    let mut output = String::with_capacity(markdown.len());
    let mut cursor = 0;
    while let Some(tag) = next_html_image_tag(&lower, cursor) {
        let src = &markdown[tag.src.clone()];
        // A target that cannot round-trip through `![](…)` would be read back
        // truncated, and the truncated path would then miss in `markdown.images`
        // — a confusing failure two steps from its cause. Refuse it here, where
        // the reason is still visible.
        if let Some(bad) = src
            .chars()
            .find(|ch| matches!(ch, ')' | '(' | '<' | '>') || ch.is_whitespace())
        {
            return Err(violation(format!(
                "page {page_index} <img src> contains {bad:?}, which cannot be written as a \
                 Markdown image target: {src}"
            )));
        }
        output.push_str(&markdown[cursor..tag.tag.start]);
        output.push_str("![](");
        output.push_str(src);
        output.push(')');
        cursor = tag.tag.end;
    }
    output.push_str(&markdown[cursor..]);
    Ok(output)
}

/// Rewrite the service's HTML tables into the GFM table notation 07 §5 already
/// fixes for a table, so that a page containing one can be indexed at all.
///
/// Until 2026-08-06 these were left alone and the v1 acceptance check refused
/// the page. That was decided (S3-F) on two grounds, and only one still holds:
/// raw HTML is a v1 violation, yes — but the other was that nothing had measured
/// what PaddleOCR-VL actually sends, and inventing a conversion for an unmeasured
/// shape is how a wrong transformation gets frozen by 07 §9. Three real tables
/// are now committed under `tests/fixtures/layout-parsing/`, and the cost of the
/// refusal turned out to be two of the three real documents indexing nothing.
///
/// The measured shape is `<table border=1 style='…'>` / `<tr>` / `<td style='…'>`
/// and nothing else: no `<th>`, no `<thead>`/`<tbody>`, no `rowspan`/`colspan`,
/// no nesting. **Anything outside that is left byte-identical** and the page is
/// refused exactly as before — a loud failure that names the rule, rather than a
/// guess frozen into the archive.
///
/// # The header row is left empty, deliberately
///
/// GFM has no way to write a table without a header, so the obvious move is to
/// promote the first `<tr>`. Two of the three captures would survive that and
/// the third would not: the slide's left-hand table opens on a data row (an icon
/// beside "High text density"), while its right-hand table and the invoice both
/// open on real headers. Nothing in the response distinguishes them — there is no
/// `<th>` anywhere — so promoting the first row would relabel real data as a
/// column name on a third of the evidence, permanently.
///
/// An empty header asserts only what was observed: that these tables do not say
/// which row is a header. Every cell still appears verbatim, in its own row.
/// This is the same trap `block_label` set in S3-J, where a rule that held on one
/// page did not hold on the next.
fn convert_html_tables_to_gfm(markdown: &str) -> String {
    // ASCII lowercasing preserves byte offsets, so indices into `lower` index
    // `markdown` too.
    let lower = markdown.to_ascii_lowercase();
    let mut output = String::with_capacity(markdown.len());
    let mut cursor = 0;
    while let Some(span) = next_html_table(&lower, cursor) {
        output.push_str(&markdown[cursor..span.start]);
        // A GFM table has to begin a block. Every observed table sits alone
        // between blank lines; one spliced into a paragraph would render as
        // pipes rather than as a table, so it is left for the acceptance check.
        let starts_block = span.start == 0 || markdown[..span.start].ends_with("\n\n");
        let ends_block = span.end == markdown.len() || markdown[span.end..].starts_with('\n');
        let converted = (starts_block && ends_block)
            .then(|| gfm_table(&markdown[span.clone()], &lower[span.clone()]))
            .flatten();
        match converted {
            Some(table) => output.push_str(&table),
            None => output.push_str(&markdown[span.clone()]),
        }
        cursor = span.end;
    }
    output.push_str(&markdown[cursor..]);
    output
}

/// Byte range of the next `<table …>…</table>` at or after `cursor`, tags
/// included. An unterminated table is left alone rather than swallowing the rest
/// of the page, exactly as an unterminated `<div>` is.
fn next_html_table(lower: &str, cursor: usize) -> Option<std::ops::Range<usize>> {
    let mut scan = cursor;
    while scan < lower.len() {
        let open = lower[scan..].find("<table")? + scan;
        // `<table` must end the name there, so `<tablet>` is not a table.
        if matches!(
            lower.as_bytes().get(open + 6),
            Some(b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/')
        ) {
            let close = lower[open..].find("</table>")? + open;
            return Some(open..close + "</table>".len());
        }
        scan = open + 1;
    }
    None
}

/// The GFM rendering of one `<table>…</table>`, or `None` when the table is not
/// the shape that was measured — in which case the caller leaves it untouched.
///
/// `lower` must be `raw.to_ascii_lowercase()`, whose byte offsets are the same.
fn gfm_table(raw: &str, lower: &str) -> Option<String> {
    // Every construct the observed tables do not contain. `<th>` and a nested
    // table would both change what the rows mean; `rowspan`/`colspan` cannot be
    // written in GFM at all; the section elements were simply never seen.
    for unmeasured in [
        "<th", "<thead", "<tbody", "<tfoot", "<caption", "rowspan", "colspan",
    ] {
        if lower.contains(unmeasured) {
            return None;
        }
    }
    // The span above ended at the FIRST `</table>`, so a nested table means the
    // range describes neither table's real extent.
    if lower[1..].contains("<table") {
        return None;
    }

    let body_start = lower.find('>')? + 1;
    let body_end = lower.len() - "</table>".len();
    let (body, body_lower) = (&raw[body_start..body_end], &lower[body_start..body_end]);

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut scan = 0usize;
    while let Some(open) = next_element(body_lower, scan, "tr") {
        // Anything between rows other than whitespace is structure this does not
        // model, so the whole table goes back untouched.
        if !body[scan..open.start].trim().is_empty() {
            return None;
        }
        let close = body_lower[open.end..].find("</tr>")? + open.end;
        rows.push(table_row(
            &body[open.end..close],
            &body_lower[open.end..close],
        )?);
        scan = close + "</tr>".len();
    }
    if !body[scan..].trim().is_empty() {
        return None;
    }

    let width = rows.first()?.len();
    // A ragged table would have to be padded or truncated to become GFM, and
    // both invent content. Refusing keeps the page's failure honest.
    if width == 0 || rows.iter().any(|row| row.len() != width) {
        return None;
    }

    let mut out = String::new();
    out.push('|');
    out.push_str(&" |".repeat(width));
    out.push_str("\n|");
    out.push_str(&" --- |".repeat(width));
    for row in &rows {
        out.push_str("\n|");
        for cell in row {
            out.push(' ');
            out.push_str(cell);
            out.push_str(" |");
        }
    }
    Some(out)
}

/// One row's cells, or `None` if the row holds anything but `<td>` elements.
fn table_row(raw: &str, lower: &str) -> Option<Vec<String>> {
    let mut cells = Vec::new();
    let mut scan = 0usize;
    while let Some(open) = next_element(lower, scan, "td") {
        if !raw[scan..open.start].trim().is_empty() {
            return None;
        }
        let close = lower[open.end..].find("</td>")? + open.end;
        cells.push(table_cell(&raw[open.end..close])?);
        scan = close + "</td>".len();
    }
    raw[scan..].trim().is_empty().then_some(cells)
}

/// One cell's text, or `None` when it holds something this cannot carry across.
fn table_cell(raw: &str) -> Option<String> {
    // By this point `<img>` is already `![](…)` and `<div>` is gone, so a
    // remaining angle bracket is markup nobody has measured — or text that
    // cannot be told apart from it.
    if raw.contains(['<', '>']) {
        return None;
    }
    // A GFM cell is one line, and a backslash is what makes the pipe escape
    // below ambiguous. Neither was observed.
    if raw.contains(['\n', '\r', '\\']) {
        return None;
    }
    // `\|` is GFM's own escape for a pipe inside a cell. Not escaping it would
    // silently split the row — this is required by the target notation, not a
    // rule invented for an unobserved shape.
    Some(raw.trim().replace('|', "\\|"))
}

/// Byte range of the opening `<name …>` tag at or after `cursor`, requiring the
/// element name to end where it does so `<trailing>` is not a `<tr>`.
fn next_element(lower: &str, cursor: usize, name: &str) -> Option<std::ops::Range<usize>> {
    let mut scan = cursor;
    while scan < lower.len() {
        let open = lower[scan..].find(&format!("<{name}"))? + scan;
        if matches!(
            lower.as_bytes().get(open + 1 + name.len()),
            Some(b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/')
        ) {
            let end = lower[open..].find('>')? + open + 1;
            return Some(open..end);
        }
        scan = open + 1;
    }
    None
}

/// Drop the `<div>` wrappers the service centres figures and captions in,
/// keeping what is inside them.
///
/// Normalized Markdown v1 forbids raw HTML outright (07 §5), and these carry no
/// content — `<div style="text-align: center;">` is presentation, while the
/// caption text inside it is body. Unwrapping keeps the second and discards the
/// first. Measured 2026-08-02: a figure arrives as a centred div around the
/// `<img>`, and its caption as a second centred div around plain text.
///
/// **Only `<div>`, deliberately.** Anything else the service might wrap content
/// in has not been observed, and inventing a rule for it is how a wrong
/// transformation gets frozen by 07 §9. Whatever this does not handle stays in
/// the Markdown and is caught by the v1 acceptance check, which the offline
/// route treats as fatal — a loud failure that names the rule, and a prompt to
/// go measure the shape rather than guess at it.
fn unwrap_presentational_html(markdown: &str) -> String {
    // ASCII lowercasing preserves byte offsets, so indices into `lower` index
    // `markdown` too.
    let lower = markdown.to_ascii_lowercase();
    let mut output = String::with_capacity(markdown.len());
    let mut cursor = 0;
    while let Some(tag) = next_div_tag(&lower, cursor) {
        output.push_str(&markdown[cursor..tag.start]);
        cursor = tag.end;
    }
    output.push_str(&markdown[cursor..]);
    output
}

/// Byte range of the next `<div …>` or `</div>` at or after `cursor`.
fn next_div_tag(lower: &str, cursor: usize) -> Option<std::ops::Range<usize>> {
    let mut scan = cursor;
    while scan < lower.len() {
        let open = lower[scan..].find('<')? + scan;
        let rest = &lower[open..];
        // `<div` must end the name there, so `<divider>` is not a div.
        let is_div = rest.starts_with("</div>")
            || (rest.starts_with("<div")
                && matches!(
                    rest.as_bytes().get(4),
                    None | Some(b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/')
                ));
        if is_div {
            // An unterminated tag is left alone rather than swallowing the rest
            // of the page; the acceptance check is what judges it.
            let end = lower[open..].find('>')? + open + 1;
            return Some(open..end);
        }
        scan = open + 1;
    }
    None
}

/// The byte ranges of the next `<img …>` tag and of its `src` value.
struct HtmlImageTag {
    tag: std::ops::Range<usize>,
    src: std::ops::Range<usize>,
}

/// `lower` must be `markdown.to_ascii_lowercase()`, whose byte offsets are the
/// same — the caller keeps it so this is not rebuilt for every tag.
fn next_html_image_tag(lower: &str, cursor: usize) -> Option<HtmlImageTag> {
    let mut scan = cursor;
    while scan < lower.len() {
        let start = lower[scan..].find("<img")? + scan;
        // An unterminated tag ends at the end of the text rather than swallowing
        // the scan into a spin.
        let end = lower[start..]
            .find('>')
            .map_or(lower.len(), |offset| start + offset + 1);
        if let Some(src) = attribute_value(lower, start..end, "src=") {
            return Some(HtmlImageTag {
                tag: start..end,
                src,
            });
        }
        scan = end.max(start + 1);
    }
    None
}

/// Reads `<img>`'s own `src="…"` and not `data-src="…"`, by requiring the name
/// to start at an attribute boundary.
fn attribute_value(
    lower: &str,
    tag: std::ops::Range<usize>,
    name: &str,
) -> Option<std::ops::Range<usize>> {
    let mut scan = tag.start;
    while scan < tag.end {
        let at = lower[scan..tag.end].find(name)? + scan;
        let after = at + name.len();
        let boundary = lower[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
        if boundary
            && let Some(&quote) = lower.as_bytes().get(after)
            && (quote == b'"' || quote == b'\'')
        {
            let inner = after + 1;
            if let Some(len) = lower[inner..tag.end].find(quote as char) {
                return Some(inner..inner + len);
            }
        }
        scan = after;
    }
    None
}

/// Take each referenced image's box from its own file name, or refuse.
///
/// The service names every crop after the box it came from —
/// `imgs/img_in_chart_box_814_626_1634_904.jpg` — so the correspondence never
/// has to be inferred. That is the whole reason this reads names instead of
/// pairing by position.
///
/// # Why position does not work
///
/// This used to zip the referenced images against the figure blocks of
/// `parsing_res_list`, refusing when the counts disagreed. Measured 2026-08-03
/// against three real pages (`tests/fixtures/layout-parsing/`), **all three
/// disagreed**, each in its own way:
///
/// - `markdown.images` carries crops the Markdown never references — a
///   `footer_image` block's crop on the infographic page (19 referenced, 20
///   carried).
/// - A `table` block is figure-labelled but renders as an inline `<table>`, not
///   as an image, so the invoice page had 1 reference against 2 boxes.
/// - Most crops are **nested inside other blocks** and have no block of their
///   own at all: 10 of the slide page's 13 images are icons sitting inside table
///   cells (11 referenced, 3 top-level boxes).
///
/// `markdown.images` is a flat bag of every crop the renderer made, at any
/// depth. It was never a per-figure list, and no count check over it can be
/// made to hold.
///
/// The refusal discipline is unchanged and still the point: a name that does not
/// parse is refused rather than given a guessed box. A wrong bbox is frozen by
/// 07 §9's first-instance-wins over a content-addressed object, and it looks
/// exactly as plausible as the right one to everything downstream.
///
/// The box comes from the name, so `parsing_res_list` is no longer read for
/// figures at all — [`require_layout_parsing_response`] only checks that it is
/// there, because its absence means the wrong endpoint.
fn images_with_their_own_boxes(
    page_index: usize,
    referenced: &[String],
    mut image_sources: BTreeMap<&str, (&str, usize, bool)>,
    budget: &mut ImageAdmissionBudget,
) -> Result<Vec<OcrImage>> {
    referenced
        .iter()
        .map(|relative_path| {
            let (encoded, decoded_len, used) = image_sources.get_mut(relative_path.as_str()).ok_or_else(|| {
                violation(format!(
                    "page {page_index} Markdown references {relative_path}, which markdown.images \
                     does not carry"
                ))
            })?;
            let bbox = bbox_from_image_name(relative_path).ok_or_else(|| {
                violation(format!(
                    "page {page_index} image {relative_path} does not name the box it was cropped \
                     from — expected `…_box_<x0>_<y0>_<x1>_<y1>.<ext>`"
                ))
            })?;
            validate_annotation_bbox(bbox)?;
            // The source admission covers the first occurrence. Each repeated
            // reference owns another buffer and must be admitted separately.
            if *used {
                budget.charge(*decoded_len, 0)?;
            }
            *used = true;
            let bytes = decode_base64(encoded).ok_or_else(|| violation("invalid admitted base64"))?;
            let media_type = sniff_media_type(&bytes, relative_path);
            Ok(OcrImage {
                bytes,
                media_type,
                bbox: Some(bbox),
                confidence: None,
                // 07 §5.2's bbox_annotation is a Mistral-specific extra prompt
                // and a +25% charge. There is neither here: this pipeline
                // returns no figure description, and Kio does not prompt it.
                annotation: None,
            })
        })
        .collect()
}

/// The `[x0, y0, x1, y1]` encoded in a crop's file name.
///
/// Reads the **last** four underscore-separated integers before the extension,
/// which is what makes it independent of the label in the middle: the observed
/// names range from `img_in_chart_box_…` to `img_in_footer_image_box_…`, and the
/// label carries underscores of its own. Absolute pixels, the same convention
/// `block_bbox` uses (07 §5.2 / [`parse_block_bbox`]).
fn bbox_from_image_name(relative_path: &str) -> Option<[i64; 4]> {
    let stem = relative_path.rsplit('/').next()?;
    let stem = stem.rsplit_once('.').map_or(stem, |(before, _)| before);
    let mut tail = stem.rsplitn(5, '_');
    let mut bbox = [0_i64; 4];
    // rsplitn yields last-first, so fill the box backwards.
    for slot in bbox.iter_mut().rev() {
        *slot = tail.next()?.parse().ok()?;
    }
    // The remainder must actually end in `_box`, so an arbitrary name ending in
    // four numbers is not mistaken for a crop.
    tail.next()?.strip_suffix("_box").map(|_| ())?;
    Some(bbox)
}

/// Media type from the bytes, falling back to the extension.
///
/// Content first because the extension is the service's, not Kio's, and the
/// stored object is addressed by its bytes.
fn sniff_media_type(bytes: &[u8], relative_path: &str) -> String {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png".to_owned();
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg".to_owned();
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return "image/webp".to_owned();
    }
    match relative_path.rsplit('.').next() {
        Some(extension) if extension.eq_ignore_ascii_case("png") => "image/png".to_owned(),
        Some(extension)
            if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") =>
        {
            "image/jpeg".to_owned()
        }
        _ => "application/octet-stream".to_owned(),
    }
}

/// Exact decoded length, checked without allocating. Whitespace is accepted,
/// but padding, trailing bits and alphabet must form canonical padded base64.
/// Data URIs remain unsupported: their punctuation fails alphabet validation.
/// Counting symbols rather than input bytes prevents whitespace from inflating
/// the decoder's allocation beyond the admitted decoded-byte count.
fn base64_decoded_len(encoded: &str) -> Option<usize> {
    let mut symbols = 0_usize;
    let mut padding = 0_usize;
    let mut last = 0_u8;
    for byte in encoded.bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            padding += 1;
            if padding > 2 {
                return None;
            }
        } else {
            if padding != 0 {
                return None;
            }
            last = base64_value(byte)?;
            symbols = symbols.checked_add(1)?;
        }
    }
    if symbols.checked_add(padding)? % 4 != 0
        || (padding == 1 && last & 3 != 0)
        || (padding == 2 && last & 15 != 0)
    {
        return None;
    }
    symbols.checked_mul(6).map(|bits| bits / 8)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn decode_base64(encoded: &str) -> Option<Vec<u8>> {
    let decoded_len = base64_decoded_len(encoded)?;
    let mut out = Vec::with_capacity(decoded_len);
    let mut accumulator = 0_u32;
    let mut bits = 0_u32;
    for byte in encoded.bytes() {
        if byte.is_ascii_whitespace() || byte == b'=' {
            continue;
        }
        let value = base64_value(byte)?;
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((accumulator >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// The declared profile for a backend.
#[must_use]
pub fn profile_value_for(execution: LocalOcrExecution) -> Value {
    let mut profile = json!({
        "adapter_kind": "markdownize",
        "adapter_role": "multimodal",
        "model_or_tool_family": LOCAL_OCR_MODEL_FAMILY,
        "output_schema": "kio-markdown-v1",
        "runtime_kind": "local",
        "spec_version": 1
    });
    let fields = profile
        .as_object_mut()
        .expect("profile literal is an object");
    match execution {
        #[cfg(debug_assertions)]
        LocalOcrExecution::Mock => {
            fields.insert(
                "model_version_pin".to_owned(),
                json!("kio-local-ocr-mock-1.0.0"),
            );
        }
        LocalOcrExecution::Real => {
            fields.insert(
                "model_version_pin".to_owned(),
                json!(LOCAL_OCR_MODEL_VERSION_PIN),
            );
        }
    }
    profile
}

/// 03 §5.1: a weight-bearing local adapter pins the sha256 of the weights.
///
/// Measured 2026-08-03 over the single `model.safetensors` (1,917,255,968 B) of
/// the model the pipeline actually loads, at
/// `/home/paddleocr/.paddlex/official_models/PaddleOCR-VL-1.6` inside
/// `paddleocr-genai-vllm-server:latest-nvidia-gpu-offline`
/// (image digest `sha256:d0d32c04…`). One file, so 03 §5.1's shard aggregation
/// does not apply.
///
/// **The weights are `PaddleOCR-VL-1.6`, while [`LOCAL_OCR_DEFAULT_MODEL`] still
/// says `PaddleOCR-VL-0.9B`.** That is not a mismatch to fix: the model *name*
/// is what upstream keeps moving, and the digest is what identity rests on —
/// which is the reason 03 §5.1 pins by hash. The name is only used to check
/// what the server reports, by prefix.
///
/// Changing this value changes `tool_profile_hash`, and for markdownize that is
/// survivable where it would not be for embedding: 07 §9's first-instance-wins
/// plus 03 §2.1's gen+1 leave existing instances and their Evidence Pointers
/// alone, and 03 §7's cross-scope compatibility gate is embedding-only.
pub const LOCAL_OCR_MODEL_VERSION_PIN: &str =
    "sha256:85a479d506a11e724e7285d395c551be69f41dbc16b6342d3cacfb189aed71db";

#[must_use]
pub fn profile_for(execution: LocalOcrExecution) -> AdapterProfile {
    let profile = profile_value_for(execution);
    AdapterProfile {
        adapter_kind: AdapterKind::Markdownize,
        adapter_id: LOCAL_OCR_ADAPTER_ID.to_owned(),
        execution_mode: ExecutionMode::OfflineApi,
        tool_profile_hash: tool_profile_hash(&profile)
            .expect("built-in local OCR profile is valid"),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        capability_flags: vec!["markdown".to_owned(), "bbox".to_owned()],
        // 07 §3: an offline_api adapter reaches loopback only, which is what
        // the §3 consent-gate exemption keys off.
        allow_network: false,
        // Nothing to bill: the pipeline runs on hardware the user already has.
        billable_kinds: Vec::new(),
        reject_billing: None,
        provider_idempotency: crate::types::ProviderIdempotency::NotProvided,
    }
}

pub struct LocalOcrMarkdownizeAdapter<C = EnvLocalOcrClient> {
    client: C,
    execution: LocalOcrExecution,
    scope_id: String,
    image_store_dir: Option<PathBuf>,
    verified_raw_bytes: Option<Vec<u8>>,
}

impl<C> LocalOcrMarkdownizeAdapter<C> {
    pub fn new(client: C, execution: LocalOcrExecution, scope_id: impl Into<String>) -> Self {
        Self {
            client,
            execution,
            scope_id: scope_id.into(),
            image_store_dir: None,
            verified_raw_bytes: None,
        }
    }

    #[must_use]
    pub fn with_image_store(mut self, kio_dir: impl Into<PathBuf>) -> Self {
        self.image_store_dir = Some(kio_dir.into());
        self
    }

    #[must_use]
    pub fn with_verified_raw_bytes(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.verified_raw_bytes = Some(bytes.into());
        self
    }

    #[must_use]
    pub fn execution(&self) -> LocalOcrExecution {
        self.execution
    }
}

impl<C: LocalOcrClient + 'static> LocalOcrMarkdownizeAdapter<C> {
    /// Attach the store and the verified bytes and hand back a trait object.
    ///
    /// Exists so the catalog's two arms differ only in the client they build:
    /// both need the same two attachments, and spelling them twice is how one
    /// arm ends up shipping without an image store and silently producing
    /// empty `related_images[]`.
    #[must_use]
    pub fn into_boxed(
        self,
        kio_dir: &Path,
        verified_raw_bytes: &[u8],
    ) -> Box<dyn MarkdownizeAdapter> {
        Box::new(
            self.with_image_store(kio_dir.to_path_buf())
                .with_verified_raw_bytes(verified_raw_bytes.to_vec()),
        )
    }
}

impl<C: LocalOcrClient> MarkdownizeAdapter for LocalOcrMarkdownizeAdapter<C> {
    fn profile(&self) -> AdapterProfile {
        profile_for(self.execution)
    }

    fn markdownize(&self, request: MarkdownizeRequest) -> Result<MarkdownizeResponse> {
        // 07 §5.2's bbox_annotation is a Mistral-only prompt-and-charge feature.
        // Accepting the flag here and ignoring it would report annotations that
        // were never produced, so it is refused instead.
        if request.bbox_annotation_enabled {
            return Err(violation(
                "local OCR adapter does not implement bbox_annotation (07 §5.2 is Mistral-specific)",
            ));
        }
        let raw_bytes = self
            .verified_raw_bytes
            .as_deref()
            .ok_or_else(|| violation("local OCR adapter requires verified raw bytes"))?;
        let file_type = LayoutFileType::from_media_type(&request.media_type)?;
        let body = self
            .client
            .layout_parse(&encode_base64(raw_bytes), file_type)?;
        let pages = parse_layout_parsing(&body)?;

        let hints = match request
            .prepared_unit_hint
            .as_ref()
            .filter(|hints| !hints.is_empty())
        {
            Some(hints) => hints.clone(),
            None => discovered_page_hints(&request.media_type, &request.raw.raw_hash, &pages)?,
        };

        let images_persisted = self.image_store_dir.is_some();
        if let Some(kio_dir) = &self.image_store_dir {
            persist_pages_images(kio_dir, &pages)?;
        }

        let updated_units = hints
            .iter()
            .map(|hint| unit_from_hint(hint, &pages, &self.scope_id, images_persisted))
            .collect::<Result<Vec<_>>>()?;

        Ok(MarkdownizeResponse {
            mode_used: request.mode,
            updated_units,
            unchanged_unit_keys: Vec::new(),
            added_units: Vec::new(),
            removed_unit_keys: Vec::new(),
            failed_units: Vec::new(),
            fallback_to_full: false,
            reason: None,
            // No ledger charge exists for a local run, so reporting usage would
            // only invite a settlement against a price that is not there.
            usage: None,
        })
    }
}

fn unit_from_hint(
    hint: &PreparedUnitHint,
    pages: &[LayoutParsedPage],
    scope_id: &str,
    images_persisted: bool,
) -> Result<MarkdownUnit> {
    let page_index = usize::try_from(hint.order)
        .map_err(|_| violation("prepared page order exceeds platform range"))?;
    let page = pages
        .iter()
        .find(|page| page.index == page_index)
        .ok_or_else(|| violation(format!("layout-parsing response missing page {page_index}")))?;
    let markdown = crate::mistral_ocr::replace_image_placeholders(
        &page.markdown,
        scope_id,
        page.images.as_slice(),
    );
    // Last step, after the URI rewrite, so nothing downstream of it can put the
    // unit back out of Normalized Markdown v1.
    //
    // The service does not end a page the way v1 requires. Measured 2026-08-03
    // against paddleocr-vl:latest-nvidia-gpu-offline: a page ending in prose
    // came back with **no** trailing newline and one ending in a table with
    // **two** — never the single LF 07 §5.2.1 asks for. Since 86d4508 made the
    // acceptance check fatal on this route, that alone refused every page,
    // whatever was on it. It is the same normalizer the deterministic adapter
    // applies as *its* last step, for the same reason.
    let markdown = crate::deterministic::normalize_to_markdown_v1(&markdown);
    Ok(MarkdownUnit {
        unit_key: hint.unit_key.clone(),
        unit_type: hint.unit_kind,
        markdown,
        // The layout response's free-form markdown does not establish image
        // ownership. For persisted pages, hashes are re-derived from decoded
        // image bytes.
        owned_image_hashes: if images_persisted {
            page.images
                .iter()
                .map(|image| image_hash(&image.bytes))
                .collect()
        } else {
            Default::default()
        },
        metadata: page_metadata(&page.images, &page.blocks),
    })
}

fn page_metadata(images: &[OcrImage], blocks: &[LayoutBlock]) -> BTreeMap<String, Value> {
    let mut metadata = BTreeMap::new();
    metadata.insert(
        "model_version_pin".to_owned(),
        json!(LOCAL_OCR_MODEL_VERSION_PIN),
    );
    if !images.is_empty() {
        let values = images
            .iter()
            .map(|image| {
                json!({
                    "hash": image_hash(&image.bytes),
                    "media_type": image.media_type,
                    "bbox": image.bbox,
                    "confidence": image.confidence,
                })
            })
            .collect::<Vec<_>>();
        metadata.insert("images".to_owned(), json!(values));
    }
    if !blocks.is_empty() {
        // Every block, not the three that were recovered. Recording only those
        // would freeze "dropped or not" into the archive, and that split is
        // exactly the one the evidence cannot justify. `recovered` marks what this
        // adapter appended so a reader can tell the two apart without re-deriving
        // the label list, and `content` is the post-escape string, matching what
        // `bbox_annotation` persists (07 §5.2).
        let values = blocks
            .iter()
            .map(|block| {
                json!({
                    "label": block.label,
                    "bbox": block.bbox,
                    "content": canonical_source_escape(block.content.trim()),
                    "recovered": RECOVERED_BLOCK_LABELS.contains(&block.label.as_str())
                        && !block.content.trim().is_empty(),
                })
            })
            .collect::<Vec<_>>();
        metadata.insert("blocks".to_owned(), json!(values));
    }
    metadata
}

/// Mint one unit per returned page when the caller supplied no hints.
///
/// Local Prepare returns no units for a scanned PDF *or an image*, so the
/// service's page set is the first trusted unit boundary available — the same
/// reasoning the online adapter's `discovered_unit_hints` records.
/// `parse_layout_parsing` already enumerated the pages contiguously from zero,
/// so there is no gap to check for here.
///
/// The kind comes from the media type, not from the fact that the service
/// answers in pages whatever it was sent. An image unit-izes as `image:0` (04
/// §2), and the CLI checks the minted kind against the media type before it
/// will accept these — so calling a PNG's only unit `page:1` does not merely
/// misname it, it makes the file unindexable.
fn discovered_page_hints(
    media_type: &str,
    raw_hash: &str,
    pages: &[LayoutParsedPage],
) -> Result<Vec<PreparedUnitHint>> {
    let kind = crate::mistral_ocr::discovered_unit_kind(media_type)?;
    // One image is one unit. More pages than that means the response is not
    // about what was sent, and saying so here beats letting the CLI report it
    // as a canonicality problem several layers away.
    if kind == UnitKind::Image && pages.len() != 1 {
        return Err(violation(
            "standalone-image OCR must return exactly one page",
        ));
    }
    Ok(pages
        .iter()
        .map(|page| PreparedUnitHint {
            unit_key: crate::mistral_ocr::discovered_unit_key(kind, page.index),
            prepared_hash: raw_hash.to_owned(),
            unit_kind: kind,
            order: page.index as u64,
        })
        .collect())
}

fn persist_pages_images(kio_dir: &Path, pages: &[LayoutParsedPage]) -> Result<()> {
    let images = pages
        .iter()
        .flat_map(|page| page.images.iter())
        .collect::<Vec<_>>();
    // The returned hashes are the caller's own inputs re-derived, so they are
    // dropped here; what this call is wanted for is the bounded, verified write.
    crate::mistral_ocr::persist_image_refs_bounded(
        kio_dir,
        &images,
        LOCAL_OCR_MAX_PERSISTED_IMAGE_BYTES,
    )
    .map(|_hashes| ())
}

/// Standard-alphabet base64 with padding, matching [`decode_base64`].
fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// The mock's figure. A PNG signature and nothing after it: `sniff_media_type`
/// reads the signature, and no consumer of an image object decodes the pixels.
#[cfg(debug_assertions)]
const MOCK_FIGURE_PNG: &[u8] = b"\x89PNG\r\n\x1a\nkio local ocr mock figure";

/// The mock's decoration, for the `decorated` body below. Distinct bytes from
/// [`MOCK_FIGURE_PNG`] on purpose: identical bytes are one content-addressed
/// object, and a test about telling two images apart cannot use one image.
#[cfg(debug_assertions)]
const MOCK_ICON_PNG: &[u8] = b"\x89PNG\r\n\x1a\nkio local ocr mock icon";

/// Test-only body selector for [`MockLocalOcrClient`]. `decorated` answers with
/// a page carrying one real figure and one sticker, so that a consumer which
/// distinguishes them by size has something to distinguish.
///
/// Set to `table` for a page whose only structure is a table of the shape the
/// service really sends — the case `convert_html_tables_to_gfm` exists for, and
/// the one that used to make a page index nothing at all.
///
/// Set to `nonconforming` to make [`MockLocalOcrClient`] answer with a page this
/// adapter cannot bring into Normalized Markdown v1 — a table with a merged
/// cell, which GFM has no notation for and which the converter therefore leaves
/// as raw HTML on purpose.
///
/// It exists so the *refusal* can be exercised. Wiring a check and never seeing
/// it fire is how the acceptance check came to be advisory in the first place.
/// (Until 2026-08-06 this body was a plain `<table><tr><td>` — which the
/// converter now rewrites, so it would have stopped testing a refusal while
/// still passing.)
#[cfg(debug_assertions)]
pub const TEST_LOCAL_OCR_BODY_ENV: &str = "KIO_TEST_LOCAL_OCR_BODY";

/// The CI backend. Returns a fixed, well-formed `/layout-parsing` body so the
/// offline Markdownize *semantics* — no consent gate, no ledger charge, no
/// batch lane — can be exercised on a runner that has no GPU and never will.
#[derive(Debug, Clone, Default)]
#[cfg(debug_assertions)]
pub struct MockLocalOcrClient {
    body_mode: Option<kio_core::test_control::LocalOcrBodyMode>,
}

#[cfg(debug_assertions)]
impl MockLocalOcrClient {
    /// Capture the selected response body with the adapter operation, so the
    /// request cannot observe an environment mutation after construction.
    pub fn from_test_control() -> Self {
        Self {
            body_mode: crate::debug_test_control()
                .adapters
                .local_ocr_body
                .known()
                .copied(),
        }
    }
}

#[cfg(debug_assertions)]
impl LocalOcrClient for MockLocalOcrClient {
    fn layout_parse(&self, _file_base64: &str, _file_type: LayoutFileType) -> Result<Value> {
        if self.body_mode == Some(kio_core::test_control::LocalOcrBodyMode::Nonconforming) {
            // Parses fine — the refusal under test is the acceptance check's,
            // not this module's, so the page has to get all the way through
            // here to reach it.
            return Ok(json!({
                "errorCode": 0,
                "result": {
                    "layoutParsingResults": [{
                        "prunedResult": {"parsing_res_list": []},
                        "markdown": {
                            "text": "Kio local OCR mock page.\n\n\
                                     <table><tr><td rowspan=2>1</td><td>2</td></tr>\
                                     <tr><td>3</td></tr></table>\n",
                            "images": {}
                        }
                    }]
                }
            }));
        }
        if self.body_mode == Some(kio_core::test_control::LocalOcrBodyMode::Table) {
            // The shape three real captures share: `border=1`, single-quoted
            // `style` on every cell, no `<th>` anywhere, and a figure sitting
            // inside a cell. `Handwritten board` appears nowhere but that cell,
            // so finding it proves the table's text reached the index.
            return Ok(json!({
                "errorCode": 0,
                "result": {
                    "layoutParsingResults": [{
                        "prunedResult": {"parsing_res_list": []},
                        "markdown": {
                            "text": "Kio local OCR mock page.\n\n\
                                     <table border=1 style='margin: auto;'>\
                                     <tr><td style='text-align: center;'>Content Type</td>\
                                     <td style='text-align: center;'>Overall Risk</td></tr>\
                                     <tr><td style='text-align: center;'>\
                                     <img src=\"imgs/img_in_image_box_76_433_134_489.jpg\" \
                                     alt=\"Image\"\" /> \
                                     Handwritten board</td>\
                                     <td style='text-align: center;'>Very High</td></tr>\
                                     </table>\n",
                            "images": {
                                "imgs/img_in_image_box_76_433_134_489.jpg":
                                    encode_base64(MOCK_FIGURE_PNG),
                            }
                        }
                    }]
                }
            }));
        }
        if self.body_mode == Some(kio_core::test_control::LocalOcrBodyMode::Decorated) {
            // A figure and a sticker, in the proportions the real service
            // returns: on the measured infographic the decoration ran to about
            // a tenth of the largest figure's area, and here it is 1/68th.
            // Both are cited by the body, so anything downstream that thins
            // `related_images[]` has to do it by size and not by counting.
            return Ok(json!({
                "logId": "00000000-0000-0000-0000-000000000000",
                "errorCode": 0,
                "errorMsg": "Success",
                "result": {
                    "dataInfo": {"width": 1240, "height": 1754, "type": "image"},
                    "layoutParsingResults": [{
                        "prunedResult": {
                            "parsing_res_list": [
                                {"block_label": "chart", "block_bbox": [107, 567, 1130, 1073],
                                 "block_id": 0, "block_order": null},
                                {"block_label": "image", "block_bbox": [40, 120, 127, 207],
                                 "block_id": 1, "block_order": null}
                            ]
                        },
                        "markdown": {
                            "text": "Kio local OCR mock page.\n\n\
                                     <div style=\"text-align: center;\">\
                                     <img src=\"imgs/img_in_image_box_40_120_127_207.jpg\" \
                                     alt=\"Image\" /></div>\n\n\
                                     <div style=\"text-align: center;\">\
                                     <img src=\"imgs/img_in_chart_box_107_567_1130_1073.jpg\" \
                                     alt=\"Image\" width=\"82%\" /></div>\n",
                            "images": {
                                "imgs/img_in_chart_box_107_567_1130_1073.jpg":
                                    encode_base64(MOCK_FIGURE_PNG),
                                "imgs/img_in_image_box_40_120_127_207.jpg":
                                    encode_base64(MOCK_ICON_PNG)
                            }
                        }
                    }]
                }
            }));
        }
        // Shaped like the real envelope, measured 2026-08-02. A mock that
        // answers in a shape the service does not send is worse than no mock:
        // it turns CI green over a wire that cannot work, which is how the
        // top-level `layoutParsingResults` reading survived until a GPU box
        // finally sent a real response.
        //
        // It carries a figure for the same reason. The service wraps figures in
        // a centred div and writes them as HTML `<img>`, never `![](…)`, and a
        // figureless mock cannot tell whether that spelling survives all the way
        // to `related_images[]` — which for one release it did not.
        Ok(json!({
            "logId": "00000000-0000-0000-0000-000000000000",
            "errorCode": 0,
            "errorMsg": "Success",
            "result": {
                "dataInfo": {"width": 1240, "height": 1754, "type": "image"},
                "layoutParsingResults": [{
                    "prunedResult": {
                        "parsing_res_list": [
                            {"block_label": "text", "block_content": "Kio local OCR mock page.",
                             "block_bbox": [10, 10, 500, 40], "block_id": 0, "block_order": null},
                            {"block_label": "chart", "block_bbox": [107, 567, 1130, 1073],
                             "block_id": 1, "block_order": null}
                        ]
                    },
                    "markdown": {
                        "text": "Kio local OCR mock page.\n\n\
                                 <div style=\"text-align: center;\">\
                                 <img src=\"imgs/img_in_chart_box_107_567_1130_1073.jpg\" \
                                 alt=\"Image\" width=\"82%\" /></div>\n",
                        "images": {
                            "imgs/img_in_chart_box_107_567_1130_1073.jpg":
                                encode_base64(MOCK_FIGURE_PNG)
                        }
                    }
                }]
            }
        }))
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    /// The envelope the service actually sends (measured 2026-08-02): pages sit
    /// under `result`, beside a `dataInfo`, with `logId`/`errorCode`/`errorMsg`
    /// outside it. Tests build the real shape so that a schema regression fails
    /// here rather than only on a GPU box.
    fn page_body(markdown: &str, images: Value, blocks: Value) -> Value {
        json!({
            "logId": "00000000-0000-0000-0000-000000000000",
            "errorCode": 0,
            "errorMsg": "Success",
            "result": {
                "dataInfo": {"width": 1240, "height": 1754, "type": "image"},
                "layoutParsingResults": [{
                    "prunedResult": {"parsing_res_list": blocks},
                    "markdown": {"text": markdown, "images": images}
                }]
            }
        })
    }

    fn png_base64() -> String {
        // 8-byte PNG signature is enough for the sniffing path.
        encode_base64(b"\x89PNG\r\n\x1a\nrest")
    }

    fn parse_with_image_limits(
        body: &Value,
        bytes: usize,
        items: usize,
    ) -> Result<Vec<LayoutParsedPage>> {
        parse_layout_parsing_with_budget(
            body,
            ImageAdmissionBudget {
                bytes_remaining: bytes,
                items_remaining: items,
            },
        )
    }

    #[test]
    fn decoded_image_admission_accepts_exact_limit_and_rejects_one_byte_over() {
        let body = page_body(
            "![](imgs/a_box_0_0_1_1.png)",
            json!({"imgs/a_box_0_0_1_1.png": " YW Jj \n"}),
            json!([]),
        );
        let pages = parse_with_image_limits(&body, 3, 2).unwrap();
        assert_eq!(pages[0].images[0].bytes, b"abc");
        let error = parse_with_image_limits(&body, 2, 2)
            .unwrap_err()
            .to_string();
        assert!(error.contains("decoded image byte limit"), "{error}");
    }

    #[test]
    fn decoded_image_admission_is_aggregate_across_pages() {
        let mut body = page_body(
            "![](imgs/a_box_0_0_1_1.png)",
            json!({"imgs/a_box_0_0_1_1.png": "YWI="}),
            json!([]),
        );
        let pages = body["result"]["layoutParsingResults"]
            .as_array_mut()
            .unwrap();
        pages.push(pages[0].clone());
        assert!(parse_with_image_limits(&body, 4, 4).is_ok());
        assert!(
            parse_with_image_limits(&body, 3, 4)
                .unwrap_err()
                .to_string()
                .contains("decoded image byte limit")
        );
        assert!(
            parse_with_image_limits(&body, 4, 3)
                .unwrap_err()
                .to_string()
                .contains("image item limit")
        );
    }

    #[test]
    fn unreferenced_images_are_validated_and_charged() {
        let body = page_body("text", json!({"unused": "YWJj"}), json!([]));
        assert!(
            parse_with_image_limits(&body, 3, 1).unwrap()[0]
                .images
                .is_empty()
        );
        assert!(
            parse_with_image_limits(&body, 2, 1)
                .unwrap_err()
                .to_string()
                .contains("decoded image byte limit")
        );
        let invalid = page_body("text", json!({"unused": "!!!!"}), json!([]));
        assert!(
            parse_with_image_limits(&invalid, 3, 1)
                .unwrap_err()
                .to_string()
                .contains("not valid base64")
        );
    }

    #[test]
    fn repeated_image_references_keep_order_and_charge_each_owned_buffer() {
        let body = page_body(
            "![](z_box_1_2_3_4.png) ![](a_box_0_0_1_1.png) ![](z_box_1_2_3_4.png)",
            json!({"a_box_0_0_1_1.png": "Yg==", "z_box_1_2_3_4.png": "YWE="}),
            json!([]),
        );
        let pages = parse_with_image_limits(&body, 5, 5).unwrap();
        let images = &pages[0].images;
        assert_eq!(images.len(), 3);
        assert_eq!(images[0].bytes, b"aa");
        assert_eq!(images[1].bytes, b"b");
        assert_eq!(images[0], images[2]);
        assert_eq!(images[0].bbox, Some([1, 2, 3, 4]));
        assert!(
            parse_with_image_limits(&body, 4, 5)
                .unwrap_err()
                .to_string()
                .contains("decoded image byte limit")
        );
        assert!(
            parse_with_image_limits(&body, 5, 4)
                .unwrap_err()
                .to_string()
                .contains("image item limit")
        );
    }

    #[test]
    fn empty_images_still_consume_admission_items() {
        let body = page_body("text", json!({"a": "", "b": ""}), json!([]));
        assert!(parse_with_image_limits(&body, 0, 2).is_ok());
        assert!(
            parse_with_image_limits(&body, 0, 1)
                .unwrap_err()
                .to_string()
                .contains("image item limit")
        );
    }

    #[test]
    fn base64_admission_checks_padding_trailing_bits_and_data_uris() {
        for (encoded, len) in [
            ("", 0),
            ("YQ==", 1),
            ("YWI=", 2),
            ("YWJj", 3),
            (" Y Q = = \n", 1),
        ] {
            assert_eq!(base64_decoded_len(encoded), Some(len), "{encoded:?}");
            let bytes = decode_base64(encoded).unwrap();
            assert_eq!(bytes.len(), len);
            assert_eq!(bytes.capacity(), len);
        }
        for encoded in [
            "=",
            "==",
            "====",
            "Y",
            "YQ",
            "YQ=",
            "YWJj=",
            "YQ===",
            "YR==",
            "YWJ=",
            "YQ==YQ==",
            "!!!!",
            "data:image/png;base64,YQ==",
        ] {
            assert_eq!(base64_decoded_len(encoded), None, "{encoded:?}");
            assert!(decode_base64(encoded).is_none(), "{encoded:?}");
        }
    }

    #[test]
    fn image_admission_budget_underflow_is_rejected_without_mutation() {
        let mut budget = ImageAdmissionBudget {
            bytes_remaining: 1,
            items_remaining: 1,
        };
        assert!(budget.charge(usize::MAX, 0).is_err());
        assert!(budget.charge(1, usize::MAX).is_err());
        assert_eq!(budget.bytes_remaining, 1);
        assert_eq!(budget.items_remaining, 1);
    }

    #[test]
    fn base64_round_trips() {
        for case in [
            b"".as_slice(),
            b"a",
            b"ab",
            b"abc",
            b"abcd",
            b"\x00\xFF\x10binary\x00",
        ] {
            let encoded = encode_base64(case);
            assert_eq!(decode_base64(&encoded).as_deref(), Some(case), "{encoded}");
        }
    }

    #[test]
    fn base64_rejects_data_after_padding() {
        assert!(decode_base64("QQ==QQ").is_none());
    }

    #[test]
    fn markdown_image_paths_follow_document_order_not_key_order() {
        // Deliberately named so that alphabetical order is the REVERSE of the
        // order they appear in: this is exactly the mistake the pairing code
        // must not make.
        let markdown = "intro\n\n![](z-first.png)\n\ntext\n\n![](a-second.png)\n";
        assert_eq!(
            markdown_image_paths(markdown),
            vec!["z-first.png".to_owned(), "a-second.png".to_owned()]
        );
    }

    /// Text the service recognised and left out of its own Markdown reaches the
    /// unit, and every block reaches the metadata.
    ///
    /// The shape is the one measured on 2026-08-09 across the nine captures in
    /// `tests/fixtures/layout-parsing/`: twelve blocks hold `block_content` that
    /// never appears in `markdown.text`, all of them labelled `header`, `footer`
    /// or `number`. Eight are furniture and four are plainly body — a routing
    /// slip's title and deadline, an infographic's closing summary — and no
    /// measurable property separates the two groups, so dropping the class to be
    /// rid of the status bars takes the body with it. See
    /// `tasks/furniture-text-recovery-design.md`.
    #[test]
    fn text_the_service_recognised_and_omitted_is_recovered_into_the_unit() {
        let body = page_body(
            "Body the service did include.\n",
            json!({}),
            json!([
                {"block_label": "text", "block_bbox": [10, 200, 900, 300],
                 "block_content": "Body the service did include."},
                {"block_label": "footer", "block_bbox": [10, 1700, 900, 1740],
                 "block_content": "Deadline 7/10"},
                {"block_label": "header", "block_bbox": [10, 20, 900, 60],
                 "block_content": "Routing"},
            ]),
        );
        let pages = parse_layout_parsing(&body).unwrap();
        let markdown = &pages[0].markdown;

        // Reading order, not response order: the header sits last in the list and
        // highest on the page, so it comes back above the footer.
        let header_at = markdown.find("Routing").expect("header recovered");
        let footer_at = markdown.find("Deadline").expect("footer recovered");
        assert!(header_at < footer_at, "{markdown}");

        // 07 §5.2.1 asks for the CommonMark source escape on provider raw text
        // embedded in the body whatever its origin, so the slash arrives
        // backslashed rather than as a stray Markdown character.
        assert!(markdown.contains(r"Deadline 7\/10"), "{markdown}");

        // Every block is recorded, not the two that were recovered. Keeping only
        // the recovered ones would freeze "dropped or not" into the archive, and
        // that is the split the evidence cannot justify.
        let labels: Vec<&str> = pages[0]
            .blocks
            .iter()
            .map(|block| block.label.as_str())
            .collect();
        assert_eq!(labels, ["text", "footer", "header"]);

        let metadata = page_metadata(&pages[0].images, &pages[0].blocks);
        assert_eq!(metadata["blocks"][0]["recovered"], json!(false));
        assert_eq!(metadata["blocks"][1]["label"], json!("footer"));
        assert_eq!(metadata["blocks"][1]["recovered"], json!(true));
        assert_eq!(metadata["blocks"][2]["bbox"], json!([10, 20, 900, 60]));
    }

    #[test]
    fn each_image_carries_the_box_named_in_its_own_file() {
        // `parsing_res_list` deliberately disagrees with both images: one box,
        // in neither position, and a text block besides. None of it is read.
        // The box comes from the name, so nothing here has to line up.
        let body = page_body(
            "![](imgs/img_in_image_box_0_100_50_150.jpg)\n\n\
             ![](imgs/img_in_chart_box_0_200_50_250.jpg)\n",
            json!({
                "imgs/img_in_image_box_0_100_50_150.jpg": png_base64(),
                "imgs/img_in_chart_box_0_200_50_250.jpg": png_base64(),
            }),
            json!([{"block_label": "text", "block_bbox": [0, 0, 10, 10], "block_order": 1}]),
        );
        let pages = parse_layout_parsing(&body).unwrap();
        let images = &pages[0].images;
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].bbox, Some([0, 100, 50, 150]));
        assert_eq!(images[1].bbox, Some([0, 200, 50, 250]));
        assert_eq!(images[0].media_type, "image/png");
    }

    #[test]
    fn a_label_with_underscores_still_yields_its_box() {
        // `footer_image` and `vision_footnote` put underscores in the middle of
        // the name, which is why the parse reads the last four numbers rather
        // than counting fields from the left.
        assert_eq!(
            bbox_from_image_name("imgs/img_in_footer_image_box_805_1410_911_1499.jpg"),
            Some([805, 1410, 911, 1499])
        );
        assert_eq!(
            bbox_from_image_name("imgs/img_in_chart_box_814_626_1634_904.jpg"),
            Some([814, 626, 1634, 904])
        );
        // Not a crop name: no `_box` before the numbers.
        assert_eq!(bbox_from_image_name("imgs/photo_1_2_3_4.jpg"), None);
        // Too few numbers, and a non-numeric field.
        assert_eq!(bbox_from_image_name("imgs/img_in_x_box_1_2_3.jpg"), None);
        assert_eq!(bbox_from_image_name("imgs/img_in_x_box_a_2_3_4.jpg"), None);
    }

    #[test]
    fn the_html_figure_markup_the_service_really_emits_is_paired() {
        // Verbatim from a 2026-08-02 capture against
        // paddleocr-vl:latest-nvidia-gpu-offline. PaddleOCR-VL wraps figures in
        // a centred div and never writes `![](…)`, and it sends `block_order`
        // as null, so both of those are reproduced exactly rather than tidied.
        let body = page_body(
            "# Quarterly Reliability Review\n\nprose\n\n<div style=\"text-align: center;\">\
             <img src=\"imgs/img_in_chart_box_107_567_1130_1073.jpg\" alt=\"Image\" \
             width=\"82%\" /></div>\n\n\n<div style=\"text-align: center;\">\
             Figure 1: Incident count by month.</div>\n",
            json!({"imgs/img_in_chart_box_107_567_1130_1073.jpg": png_base64()}),
            json!([
                {"block_label": "doc_title", "block_bbox": [106, 91, 840, 146], "block_order": null},
                {"block_label": "text", "block_bbox": [102, 240, 1044, 356], "block_order": null},
                {"block_label": "chart", "block_bbox": [107, 567, 1130, 1073], "block_order": null},
                {"block_label": "figure_title", "block_bbox": [105, 1103, 667, 1136], "block_order": null},
            ]),
        );
        let pages = parse_layout_parsing(&body).unwrap();
        let images = &pages[0].images;
        assert_eq!(images.len(), 1, "the chart block is the only figure");
        // Absolute pixels, straight through: the source page is 1240x1754 and
        // this box is the chart's real position on it.
        assert_eq!(images[0].bbox, Some([107, 567, 1130, 1073]));
        // `figure_title` is a caption, not a figure — it must not become an image.
        assert_eq!(pages[0].images.len(), 1);
        // The `<img>` is gone by the time the page leaves the parser. What
        // replaces it is the form `kio-search`'s `extract_related_images` reads;
        // leaving the HTML spelling in place is what made `related_images[]`,
        // image embedding, the scope projection, and purge's orphan test all
        // see zero images on a page that has one.
        assert!(
            pages[0]
                .markdown
                .contains("![](imgs/img_in_chart_box_107_567_1130_1073.jpg)"),
            "{}",
            pages[0].markdown
        );
        assert!(!pages[0].markdown.contains("<img"), "{}", pages[0].markdown);
        // Normalized Markdown v1 forbids raw HTML outright (07 §5), so the
        // centring divs go too — but their contents stay, because the caption
        // inside the second one is body text, not presentation.
        assert!(!pages[0].markdown.contains("<div"), "{}", pages[0].markdown);
        assert!(
            pages[0]
                .markdown
                .contains("Figure 1: Incident count by month."),
            "the caption is content, not markup: {}",
            pages[0].markdown
        );
    }

    #[test]
    fn unwrapping_a_div_keeps_its_contents_and_drops_only_the_tags() {
        assert_eq!(
            unwrap_presentational_html(
                "<div style=\"text-align: center;\">Figure 1: caption.</div>\n"
            ),
            "Figure 1: caption.\n"
        );
        // Case and self-closing forms the service might use.
        assert_eq!(unwrap_presentational_html("<DIV>a</DIV>b"), "ab");
        assert_eq!(unwrap_presentational_html("<div/>text"), "text");
        // `<divider>` is not a div — the name has to end there.
        assert_eq!(
            unwrap_presentational_html("<divider>x</divider>"),
            "<divider>x</divider>"
        );
        // Not a general HTML stripper. Everything else is left for the v1
        // acceptance check to refuse, so an unmeasured shape fails loudly
        // instead of being silently rewritten.
        assert_eq!(
            unwrap_presentational_html("<table><tr><td>1</td></tr></table>"),
            "<table><tr><td>1</td></tr></table>"
        );
        // An unterminated tag must not swallow the rest of the page.
        assert_eq!(unwrap_presentational_html("<div rest"), "<div rest");
    }

    /// The shape all three real captures share, converted.
    #[test]
    fn a_table_of_the_measured_shape_becomes_a_gfm_table() {
        let converted = convert_html_tables_to_gfm(
            "before\n\n<table border=1 style='margin: auto;'>\
             <tr><td style='text-align: center;'>項目</td>\
             <td style='text-align: center;'>数量</td></tr>\
             <tr><td style='text-align: center;'></td>\
             <td style='text-align: center;'>3</td></tr></table>\n",
        );
        assert_eq!(
            converted,
            "before\n\n| | |\n| --- | --- |\n| 項目 | 数量 |\n|  | 3 |\n"
        );
    }

    /// The header row stays empty even when the first row obviously is one.
    ///
    /// Two of the three captures open on a header and the third opens on data,
    /// and no field tells them apart. Promoting the first row would be right
    /// twice and permanently wrong once — 07 §9 does not give the third one back.
    #[test]
    fn the_first_row_is_never_promoted_to_the_header() {
        let converted = convert_html_tables_to_gfm(
            "<table><tr><td>Content Type</td><td>Overall Risk</td></tr>\
             <tr><td>Raster chart</td><td>High</td></tr></table>",
        );
        assert!(
            converted.starts_with("| | |\n| --- | --- |\n| Content Type |"),
            "{converted}"
        );
    }

    /// Everything outside the measured shape comes back byte-identical, so the
    /// v1 acceptance check refuses the page exactly as it did before.
    #[test]
    fn a_table_outside_the_measured_shape_is_left_for_the_acceptance_check() {
        for untouched in [
            // GFM cannot express a merged cell at all.
            "<table><tr><td rowspan=2>a</td><td>b</td></tr><tr><td>c</td></tr></table>",
            "<table><tr><td colspan=2>a</td></tr></table>",
            // Never observed: a real header cell, the section elements, nesting.
            "<table><tr><th>a</th></tr></table>",
            "<table><thead><tr><td>a</td></tr></thead></table>",
            "<table><tr><td><table><tr><td>a</td></tr></table></td></tr></table>",
            // Ragged rows would have to be padded or truncated to become GFM.
            "<table><tr><td>a</td><td>b</td></tr><tr><td>c</td></tr></table>",
            // Markup in a cell that nothing has measured -- or text that cannot
            // be told apart from it.
            "<table><tr><td><span>a</span></td></tr></table>",
            "<table><tr><td>a < b</td></tr></table>",
            // A cell is one line in GFM, and a backslash makes the pipe escape
            // ambiguous.
            "<table><tr><td>a\nb</td></tr></table>",
            "<table><tr><td>a\\b</td></tr></table>",
            // Content between the rows, and an unterminated table.
            "<table>stray<tr><td>a</td></tr></table>",
            "<table><tr><td>a</td></tr>",
            // `<tablet>` is not a table -- the name has to end there.
            "<tablet><tr><td>a</td></tr></tablet>",
        ] {
            assert_eq!(
                convert_html_tables_to_gfm(untouched),
                untouched,
                "must be left untouched"
            );
        }
    }

    /// A table spliced into a paragraph would render as pipes, not as a table.
    #[test]
    fn a_table_that_does_not_begin_a_block_is_left_alone() {
        let inline = "text <table><tr><td>a</td></tr></table>\n";
        assert_eq!(convert_html_tables_to_gfm(inline), inline);
        // A single newline is a lazy continuation of the paragraph above it, so
        // a blank line is what the converter requires.
        let lazy = "text\n<table><tr><td>a</td></tr></table>\n";
        assert_eq!(convert_html_tables_to_gfm(lazy), lazy);
    }

    /// A pipe inside a cell has to be escaped or it silently splits the row.
    /// This is the target notation's own rule, not a guess about an input shape.
    #[test]
    fn a_pipe_in_a_cell_is_escaped_rather_than_splitting_the_row() {
        let converted =
            convert_html_tables_to_gfm("<table><tr><td>a|b</td><td>c</td></tr></table>");
        assert_eq!(converted, "| | |\n| --- | --- |\n| a\\|b | c |");
    }

    /// A figure inside a cell survives as a reference the rest of Kio can read.
    ///
    /// `<img>` is already `![](…)` by the time the table is converted, and the
    /// URI rewrite runs after both — so a cell's figure ends up in
    /// `related_images[]` like any other. The slide capture has eight of these.
    #[test]
    fn a_figure_inside_a_cell_stays_a_reference() {
        let crop = "imgs/img_in_image_box_76_433_134_489.jpg";
        let body = page_body(
            &format!(
                "<table><tr><td><img src=\"{crop}\" alt=\"Image\" /> label</td></tr></table>\n"
            ),
            json!({crop: png_base64()}),
            json!([]),
        );
        let pages = parse_layout_parsing(&body).unwrap();
        assert!(
            pages[0]
                .markdown
                .contains(&format!("| ![]({crop}) label |")),
            "{}",
            pages[0].markdown
        );
        assert_eq!(pages[0].images.len(), 1);
    }

    #[test]
    fn every_img_spelling_the_service_might_use_is_normalized_in_place() {
        // Upper case, single quotes, self-closing, and an already-CommonMark
        // reference that must be left exactly as it is. Document order is the
        // whole contract downstream — `pair_images_with_boxes` zips this list
        // against the boxes — so it is asserted rather than assumed.
        let markdown = "<img src=\"first.png\">\n\n![](second.png)\n\n<IMG SRC='third.png'/>\n";
        let normalized = normalize_html_image_refs(0, markdown).unwrap();
        assert_eq!(
            normalized,
            "![](first.png)\n\n![](second.png)\n\n![](third.png)\n"
        );
        assert_eq!(
            markdown_image_paths(&normalized),
            vec![
                "first.png".to_owned(),
                "second.png".to_owned(),
                "third.png".to_owned()
            ]
        );
    }

    #[test]
    fn a_malformed_img_tag_does_not_hang_or_invent_a_path() {
        for markdown in [
            "<img",
            "<img>",
            "<img src=>",
            "<img src=\"unterminated",
            "<img src=''>",
            "<img alt=\"no src\">",
            // `src=` here belongs to `data-src`, not to the tag.
            "<img data-src=\"x.png\">",
        ] {
            let normalized = normalize_html_image_refs(0, markdown).unwrap();
            assert!(
                markdown_image_paths(&normalized).is_empty(),
                "unexpected path from {markdown:?} -> {normalized:?}"
            );
        }
    }

    #[test]
    fn an_src_that_cannot_be_a_markdown_target_is_refused() {
        // `)` would be read back truncated, and the truncated path would then
        // miss in `markdown.images` — a failure two steps from its cause. The
        // refusal names the character while the reason is still visible.
        let error = normalize_html_image_refs(3, "<img src=\"a(b).png\">")
            .unwrap_err()
            .to_string();
        assert!(error.contains("page 3"), "{error}");
        assert!(error.contains("a(b).png"), "{error}");
    }

    #[test]
    fn an_alt_attribute_cannot_rewrite_the_reference_it_labels() {
        // Provider text in a structural position. Carrying `alt` over verbatim
        // would emit `![x](evil.png) [](real.png)` and hand the archive an image
        // reference the service never made.
        let normalized =
            normalize_html_image_refs(0, "<img alt=\"x](evil.png) [\" src=\"real.png\">").unwrap();
        assert_eq!(normalized, "![](real.png)");
        assert_eq!(
            markdown_image_paths(&normalized),
            vec!["real.png".to_owned()]
        );
    }

    #[test]
    fn an_image_that_does_not_name_its_box_is_refused_rather_than_guessed_at() {
        // The refusal that replaced the count checks. If upstream ever renames
        // its crops, this fires instead of a plausible-looking wrong bbox that
        // 07 §9 would freeze for the life of the archive.
        let body = page_body(
            "![](one.png)\n",
            json!({"one.png": png_base64()}),
            json!([{"block_label": "image", "block_bbox": [0, 0, 10, 10], "block_order": 0}]),
        );
        let error = parse_layout_parsing(&body).unwrap_err().to_string();
        assert!(error.contains("does not name the box"), "{error}");
    }

    #[test]
    fn images_the_markdown_never_references_are_ignored() {
        // Measured 2026-08-03: `markdown.images` is a flat bag of every crop the
        // renderer made, including ones with no reference in the text (a
        // `footer_image` on the infographic page) and ones nested inside other
        // blocks (icons in table cells on the slide page). The old count check
        // treated that as a contract violation and refused every real page.
        let body = page_body(
            "![](imgs/img_in_chart_box_0_100_50_150.jpg)\n",
            json!({
                "imgs/img_in_chart_box_0_100_50_150.jpg": png_base64(),
                "imgs/img_in_footer_image_box_9_9_19_19.jpg": png_base64(),
            }),
            json!([]),
        );
        let pages = parse_layout_parsing(&body).unwrap();
        assert_eq!(pages[0].images.len(), 1);
        assert_eq!(pages[0].images[0].bbox, Some([0, 100, 50, 150]));
    }

    #[test]
    fn a_reference_without_bytes_fails() {
        let body = page_body(
            "![](present.png)\n",
            json!({"absent.png": png_base64()}),
            json!([{"block_label": "image", "block_bbox": [0, 0, 10, 10], "block_order": 0}]),
        );
        assert!(parse_layout_parsing(&body).is_err());
    }

    #[test]
    fn text_block_boxes_are_not_treated_as_figures() {
        // A page of prose with no figures must yield no images — not one image
        // per paragraph.
        let body = page_body(
            "just prose\n",
            json!({}),
            json!([
                {"block_label": "text", "block_bbox": [0, 0, 100, 20], "block_order": 0},
                {"block_label": "title", "block_bbox": [0, 30, 100, 50], "block_order": 1},
            ]),
        );
        let pages = parse_layout_parsing(&body).unwrap();
        assert!(pages[0].images.is_empty());
    }

    #[test]
    fn a_vlm_only_response_is_named_as_such() {
        // This is the shape genai_server returns after the pipeline's own
        // post-processing is skipped: Markdown, no parsing_res_list. The error
        // has to point at the endpoint, because "no bboxes" is otherwise
        // indistinguishable from "a page with no figures".
        let body = json!({
            "errorCode": 0,
            "result": {
                "layoutParsingResults": [{"markdown": {"text": "recognized text\n"}}]
            }
        });
        let error = parse_layout_parsing(&body).unwrap_err().to_string();
        assert!(error.contains("genai_server"), "{error}");
    }

    #[test]
    fn an_empty_result_array_is_rejected() {
        let body = json!({"errorCode": 0, "result": {"layoutParsingResults": []}});
        assert!(parse_layout_parsing(&body).is_err());
    }

    #[test]
    fn pages_at_the_top_level_are_not_accepted() {
        // The pre-2026-08-02 reading. Keeping it rejected means a future change
        // back to the documented-but-wrong shape cannot pass silently.
        let body = json!({"layoutParsingResults": [{"markdown": {"text": "x\n"}}]});
        let error = parse_layout_parsing(&body).unwrap_err().to_string();
        assert!(error.contains("result.layoutParsingResults"), "{error}");
    }

    #[test]
    fn a_nonzero_error_code_is_not_read_past() {
        // The service returns HTTP 200 and reports failure in the body, so the
        // transport cannot catch this one.
        let body = json!({
            "errorCode": 500,
            "errorMsg": "Internal error",
            "result": {"layoutParsingResults": []}
        });
        let error = parse_layout_parsing(&body).unwrap_err().to_string();
        assert!(error.contains("errorCode 500"), "{error}");
        assert!(error.contains("Internal error"), "{error}");
    }

    /// A rejected input, in the exact bytes the service sent for one.
    ///
    /// Measured 2026-08-05 by posting a GIF, which PaddleOCR-VL refuses at input
    /// validation in about 4ms (S3-K). The shape is worth pinning because it is
    /// a fourth envelope: `result` is not empty here, it is **absent**, and the
    /// only other place a body has no `result` is the mistake this parser was
    /// written wrong against once already. Reaching the `result` read with a
    /// 422 in hand would report "no layoutParsingResults" -- true, useless, and
    /// silent about the service having said exactly what was wrong.
    ///
    /// Nothing routes a GIF here, so this is not about GIFs. It is about every
    /// supported input the service can still reject: a truncated PNG, a PDF it
    /// cannot open.
    #[test]
    fn a_refused_input_is_reported_with_what_the_service_said() {
        let body: Value = serde_json::from_str(
            r#"{"logId":"5fa5a39b-a40a-4a92-953a-b1961daa58d8","errorCode":422,"errorMsg":"Invalid input file"}"#,
        )
        .unwrap();
        let error = parse_layout_parsing(&body).unwrap_err().to_string();
        assert!(error.contains("errorCode 422"), "{error}");
        assert!(error.contains("Invalid input file"), "{error}");
    }

    #[test]
    fn a_degenerate_bbox_is_rejected() {
        // Zero area, now carried by the name rather than by block_bbox. The
        // online route already refuses these (07 §5.2); both OCR routes must
        // agree, or the same document parses differently depending on which
        // adapter ran.
        let body = page_body(
            "![](imgs/img_in_image_box_10_10_10_10.jpg)\n",
            json!({"imgs/img_in_image_box_10_10_10_10.jpg": png_base64()}),
            json!([]),
        );
        assert!(parse_layout_parsing(&body).is_err());
    }

    #[test]
    fn pdf_and_image_media_types_map_to_the_documented_file_type() {
        assert_eq!(
            LayoutFileType::from_media_type("application/pdf")
                .unwrap()
                .wire_value(),
            0
        );
        assert_eq!(
            LayoutFileType::from_media_type("image/png")
                .unwrap()
                .wire_value(),
            1
        );
        assert!(LayoutFileType::from_media_type("text/plain").is_err());
    }

    /// Routing a type here commits to minting its units, so this adapter must
    /// not accept anything discovery cannot name.
    ///
    /// S3-I was the two tables disagreeing about *which kind* a unit takes.
    /// They can also disagree about *whether the type exists at all*, and that
    /// failure looks identical from the outside: a `contract_violation` with no
    /// prepared units. `image/tiff` sat in exactly that state, saved only by
    /// being unreachable. Comparing the tables directly is what makes the
    /// disagreement visible without a file of that type to run through.
    #[test]
    fn every_routable_media_type_can_be_named_by_discovery() {
        for media_type in [
            "application/pdf",
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/tiff",
            "image/gif",
            "text/plain",
        ] {
            if LayoutFileType::from_media_type(media_type).is_ok() {
                assert!(
                    crate::mistral_ocr::discovered_unit_kind(media_type).is_ok(),
                    "{media_type} is routed to the local adapter but discovery \
                     cannot name its units, so it can only fail at hint time"
                );
            }
        }
    }

    #[test]
    fn mock_and_real_hash_to_different_profiles() {
        // Same reason as the embedding adapter: a mock page and a real one are
        // not interchangeable, and 03 §7 has only the profile hash to tell them
        // apart with.
        assert_ne!(
            profile_for(LocalOcrExecution::Mock).tool_profile_hash,
            profile_for(LocalOcrExecution::Real).tool_profile_hash
        );
    }

    #[test]
    fn the_profile_is_offline_and_unbillable() {
        let profile = profile_for(LocalOcrExecution::Mock);
        assert_eq!(profile.execution_mode, ExecutionMode::OfflineApi);
        assert!(!profile.allow_network);
        assert!(profile.billable_kinds.is_empty());
    }

    #[test]
    fn the_profile_records_no_prompt_template() {
        // Kio supplies no prompt to this pipeline, so hashing one would make
        // the identity describe an input the adapter never sends.
        let profile = profile_value_for(LocalOcrExecution::Real);
        assert!(profile.get("prompt_template_hash").is_none());
        assert!(profile.get("prompt_template_id").is_none());
    }

    #[test]
    fn the_real_weight_pin_is_the_measured_digest() {
        // Frozen on purpose. 03 §5.1 requires a sha256 of the weights, and this
        // is the one taken from the model.safetensors the pipeline loads
        // (2026-08-03). If it moves, the weights moved, and that is a profile
        // change to make deliberately rather than by editing a constant.
        assert_eq!(
            LOCAL_OCR_MODEL_VERSION_PIN,
            "sha256:85a479d506a11e724e7285d395c551be69f41dbc16b6342d3cacfb189aed71db"
        );
        // The placeholder shape must never come back: an `unmeasured:` value
        // reaching a Real profile would let two model versions claim one
        // identity, which is what the pin exists to prevent.
        assert!(!LOCAL_OCR_MODEL_VERSION_PIN.starts_with("unmeasured:"));
    }

    #[test]
    fn the_mock_client_body_parses() {
        let body = MockLocalOcrClient::default()
            .layout_parse("", LayoutFileType::Image)
            .unwrap();
        let pages = parse_layout_parsing(&body).unwrap();
        assert_eq!(pages.len(), 1);
        assert!(pages[0].markdown.contains("mock page"));
    }

    /// A client that records what it was handed, so the request side can be
    /// asserted on rather than only the response side.
    #[derive(Clone)]
    struct RecordingClient {
        body: Value,
        seen: std::sync::Arc<std::sync::Mutex<Option<(String, LayoutFileType)>>>,
    }

    impl LocalOcrClient for RecordingClient {
        fn layout_parse(&self, file_base64: &str, file_type: LayoutFileType) -> Result<Value> {
            *self.seen.lock().unwrap() = Some((file_base64.to_owned(), file_type));
            Ok(self.body.clone())
        }
    }

    fn request(media_type: &str) -> MarkdownizeRequest {
        MarkdownizeRequest {
            raw: crate::types::RawInput {
                raw_hash: "sha256:raw".to_owned(),
                path: None,
            },
            media_type: media_type.to_owned(),
            prepared_unit_hint: None,
            mode: crate::types::MarkdownizeMode::Full,
            previous: None,
            hints: None,
            restrict_to_hint_pages: false,
            bbox_annotation_enabled: false,
            tool_profile_hash: "sha256:profile".to_owned(),
            spec_version: 1,
            idempotency_token: None,
        }
    }

    #[test]
    fn the_page_ending_the_service_sends_is_normalized_to_one_lf() {
        // Both endings are measured, 2026-08-03: a page ending in prose came
        // back with no trailing newline, one ending in a table with two.
        // Normalized Markdown v1 wants exactly one, and since the acceptance
        // check became fatal on this route either ending refused the page
        // outright -- whatever else was on it.
        for (label, text) in [
            ("no trailing newline", "prose with no final newline"),
            ("two trailing newlines", "prose then a blank line\n\n"),
        ] {
            let client = RecordingClient {
                body: page_body(text, json!({}), json!([])),
                seen: std::sync::Arc::new(std::sync::Mutex::new(None)),
            };
            let adapter =
                LocalOcrMarkdownizeAdapter::new(client, LocalOcrExecution::Real, "scope-1")
                    .with_verified_raw_bytes(b"%PDF-1.7 bytes".to_vec());
            let response = adapter.markdownize(request("application/pdf")).unwrap();
            let markdown = &response.updated_units[0].markdown;
            assert!(markdown.ends_with('\n'), "{label}: {markdown:?}");
            assert!(!markdown.ends_with("\n\n"), "{label}: {markdown:?}");
        }
    }

    /// A standalone image discovers an `image:` unit, not a `page:` one.
    ///
    /// Nothing downstream can repair this. Local Prepare returns no units for
    /// an image (`prepare_units`' first branch), so discovery is the only way
    /// one ever becomes a unit, and the CLI checks the minted kind against the
    /// media type before accepting it. Answering `page:1` for a PNG fails that
    /// check and the file is refused -- with a message about canonical units
    /// that says nothing about the actual cause.
    #[test]
    fn a_standalone_image_discovers_an_image_unit_not_a_page() {
        for media_type in ["image/png", "image/jpeg", "image/webp"] {
            let client = RecordingClient {
                body: page_body("a page of prose\n", json!({}), json!([])),
                seen: std::sync::Arc::new(std::sync::Mutex::new(None)),
            };
            let adapter =
                LocalOcrMarkdownizeAdapter::new(client, LocalOcrExecution::Real, "scope-1")
                    .with_verified_raw_bytes(b"\x89PNG\r\n\x1a\n".to_vec());

            let response = adapter.markdownize(request(media_type)).unwrap();
            assert_eq!(response.updated_units.len(), 1, "{media_type}");
            assert_eq!(
                response.updated_units[0].unit_key, "image:0",
                "{media_type}"
            );
            assert_eq!(
                response.updated_units[0].unit_type,
                UnitKind::Image,
                "{media_type}"
            );
        }
    }

    /// A PDF keeps the 1-based `page:` spelling the other tests rely on.
    #[test]
    fn a_pdf_still_discovers_one_based_page_units() {
        let client = RecordingClient {
            body: json!({"errorCode": 0, "result": {"layoutParsingResults": [
                {"prunedResult": {"parsing_res_list": []}, "markdown": {"text": "one\n"}},
                {"prunedResult": {"parsing_res_list": []}, "markdown": {"text": "two\n"}},
            ]}}),
            seen: std::sync::Arc::new(std::sync::Mutex::new(None)),
        };
        let adapter = LocalOcrMarkdownizeAdapter::new(client, LocalOcrExecution::Real, "scope-1")
            .with_verified_raw_bytes(b"%PDF-1.7 bytes".to_vec());

        let response = adapter.markdownize(request("application/pdf")).unwrap();
        let keys = response
            .updated_units
            .iter()
            .map(|unit| unit.unit_key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(keys, ["page:1", "page:2"]);
        assert!(
            response
                .updated_units
                .iter()
                .all(|unit| unit.unit_type == UnitKind::Page)
        );
    }

    /// The service can only be answering about one image, so more than one
    /// parsed page means the request and the response disagree about what was
    /// sent. Refusing here names that; letting it through reaches the CLI's
    /// own count check, which reports it as a canonicality problem instead.
    #[test]
    fn a_standalone_image_answered_with_two_pages_is_refused() {
        let client = RecordingClient {
            body: json!({"errorCode": 0, "result": {"layoutParsingResults": [
                {"prunedResult": {"parsing_res_list": []}, "markdown": {"text": "one\n"}},
                {"prunedResult": {"parsing_res_list": []}, "markdown": {"text": "two\n"}},
            ]}}),
            seen: std::sync::Arc::new(std::sync::Mutex::new(None)),
        };
        let adapter = LocalOcrMarkdownizeAdapter::new(client, LocalOcrExecution::Real, "scope-1")
            .with_verified_raw_bytes(b"\x89PNG\r\n\x1a\n".to_vec());
        assert!(adapter.markdownize(request("image/png")).is_err());
    }

    #[test]
    fn the_adapter_mints_one_unit_per_page_and_rewrites_image_references() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let client = RecordingClient {
            body: page_body(
                "before\n\n![](imgs/img_in_image_box_4_8_40_80.jpg)\n\nafter\n",
                json!({"imgs/img_in_image_box_4_8_40_80.jpg": png_base64()}),
                json!([{"block_label": "image", "block_bbox": [4, 8, 40, 80], "block_order": 0}]),
            ),
            seen: std::sync::Arc::clone(&seen),
        };
        let adapter = LocalOcrMarkdownizeAdapter::new(client, LocalOcrExecution::Real, "scope-1")
            .with_verified_raw_bytes(b"%PDF-1.7 bytes".to_vec());

        let response = adapter.markdownize(request("application/pdf")).unwrap();

        // The service was told these are PDF bytes, base64 of exactly what was
        // verified — not a re-read of a path.
        let (sent, file_type) = seen.lock().unwrap().clone().unwrap();
        assert_eq!(file_type, LayoutFileType::Pdf);
        assert_eq!(decode_base64(&sent).unwrap(), b"%PDF-1.7 bytes");

        assert_eq!(response.updated_units.len(), 1);
        let unit = &response.updated_units[0];
        assert_eq!(unit.unit_key, "page:1");
        // The relative path the service invented must not survive into the
        // normalized Markdown: Stage 1.5's related_images[] reads kio:// URIs,
        // and a leftover fig-1.png would make it silently empty.
        assert!(
            !unit.markdown.contains("img_in_image_box"),
            "{}",
            unit.markdown
        );
        assert!(unit.markdown.contains("kio://"), "{}", unit.markdown);
        assert_eq!(unit.metadata["images"][0]["bbox"], json!([4, 8, 40, 80]));
        // A local run has no invoice, so there is nothing for the ledger to
        // settle against.
        assert!(response.usage.is_none());
    }

    #[test]
    fn the_adapter_refuses_bbox_annotation_rather_than_ignoring_it() {
        let mut request = request("image/png");
        request.bbox_annotation_enabled = true;
        let adapter = LocalOcrMarkdownizeAdapter::new(
            MockLocalOcrClient::default(),
            LocalOcrExecution::Mock,
            "scope-1",
        )
        .with_verified_raw_bytes(b"png".to_vec());
        let error = adapter.markdownize(request).unwrap_err().to_string();
        assert!(error.contains("bbox_annotation"), "{error}");
    }

    #[test]
    fn the_adapter_will_not_run_without_verified_bytes() {
        // Re-reading a path here would re-open bytes the caller already
        // verified, which is the hole the verified-bytes API exists to close.
        let adapter = LocalOcrMarkdownizeAdapter::new(
            MockLocalOcrClient::default(),
            LocalOcrExecution::Mock,
            "scope-1",
        );
        assert!(adapter.markdownize(request("image/png")).is_err());
    }

    #[test]
    fn supplied_hints_select_pages_instead_of_minting_new_ones() {
        let client = RecordingClient {
            body: json!({"errorCode": 0, "result": {"layoutParsingResults": [
                {"prunedResult": {"parsing_res_list": []}, "markdown": {"text": "one\n"}},
                {"prunedResult": {"parsing_res_list": []}, "markdown": {"text": "two\n"}},
            ]}}),
            seen: std::sync::Arc::new(std::sync::Mutex::new(None)),
        };
        let mut request = request("application/pdf");
        request.prepared_unit_hint = Some(vec![PreparedUnitHint {
            unit_key: "page:2".to_owned(),
            prepared_hash: "sha256:p2".to_owned(),
            unit_kind: UnitKind::Page,
            order: 1,
        }]);
        let adapter = LocalOcrMarkdownizeAdapter::new(client, LocalOcrExecution::Real, "scope-1")
            .with_verified_raw_bytes(b"pdf".to_vec());

        let response = adapter.markdownize(request).unwrap();
        assert_eq!(response.updated_units.len(), 1);
        assert_eq!(response.updated_units[0].unit_key, "page:2");
        assert_eq!(response.updated_units[0].markdown, "two\n");
    }

    #[test]
    fn a_hint_naming_a_page_the_service_did_not_return_fails() {
        let client = RecordingClient {
            body: MockLocalOcrClient::default()
                .layout_parse("", LayoutFileType::Image)
                .unwrap(),
            seen: std::sync::Arc::new(std::sync::Mutex::new(None)),
        };
        let mut request = request("application/pdf");
        request.prepared_unit_hint = Some(vec![PreparedUnitHint {
            unit_key: "page:9".to_owned(),
            prepared_hash: "sha256:p9".to_owned(),
            unit_kind: UnitKind::Page,
            order: 8,
        }]);
        let adapter = LocalOcrMarkdownizeAdapter::new(client, LocalOcrExecution::Mock, "scope-1")
            .with_verified_raw_bytes(b"pdf".to_vec());
        assert!(adapter.markdownize(request).is_err());
    }
}

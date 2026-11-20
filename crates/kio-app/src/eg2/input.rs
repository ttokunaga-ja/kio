use super::{MAX_REQUEST_BYTES, MAX_TEXT_BYTES, WireInput, WorkerError, request, window};
use crate::embedding::input::verified_image_mime;
use kio_core::eg2::{DOCUMENT_PROMPT, MAX_BODY_CHARS, QUERY_PROMPT};
use kio_index::embedding_store::contextualized_embedding_input;

#[derive(Clone, Copy)]
pub enum EmbeddingInput<'a> {
    /// The prepared chunk body and its humanized filename context.
    Document {
        body: &'a str,
        filename_context: Option<&'a str>,
    },
    Query(&'a str),
    /// Prepared PNG or JPEG bytes from the shared embedding image path.
    Image(&'a [u8]),
}

pub(super) enum PreparedInput<'a> {
    Text {
        body: &'a str,
        scalars: usize,
        prefix: String,
    },
    Image(&'a [u8]),
}

impl<'a> EmbeddingInput<'a> {
    pub(super) fn prepare(self) -> Result<PreparedInput<'a>, WorkerError> {
        let (body, prefix) = match self {
            Self::Document {
                body,
                filename_context,
            } => {
                if filename_context.is_some_and(|context| context.len() > MAX_TEXT_BYTES) {
                    return Err(WorkerError::InputLimit);
                }
                let context = contextualized_embedding_input(filename_context, "");
                (body, format!("{DOCUMENT_PROMPT}{context}"))
            }
            Self::Query(body) => (body, QUERY_PROMPT.to_owned()),
            Self::Image(bytes) => {
                if bytes.is_empty() || bytes.len() >= MAX_REQUEST_BYTES {
                    return Err(WorkerError::InputLimit);
                }
                if !matches!(verified_image_mime(bytes), Ok("image/png" | "image/jpeg")) {
                    return Err(WorkerError::InvalidInput);
                }
                return Ok(PreparedInput::Image(bytes));
            }
        };
        if body.is_empty() || body.len().saturating_add(prefix.len()) > MAX_TEXT_BYTES {
            return Err(WorkerError::InputLimit);
        }
        let scalars = body.chars().take(MAX_BODY_CHARS + 1).count();
        if scalars > MAX_BODY_CHARS {
            return Err(WorkerError::InputLimit);
        }
        Ok(PreparedInput::Text {
            body,
            scalars,
            prefix,
        })
    }
}

impl PreparedInput<'_> {
    pub(super) fn embed(
        self,
        mut send: impl FnMut(&[u8]) -> Result<Vec<f32>, WorkerError>,
    ) -> Result<Vec<f32>, WorkerError> {
        match self {
            Self::Text {
                body,
                scalars,
                prefix,
            } => window::embed_text(body, scalars, |window| {
                let text = format!("{prefix}{window}");
                send(&request(WireInput::Text(&text))?)
            }),
            Self::Image(bytes) => send(&request(WireInput::Image(bytes))?),
        }
    }
}

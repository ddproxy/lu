//! Bounded, streaming reads of stored artifacts. A request never loads a whole artifact into
//! memory: the file length is checked against the artifact cap, then only the requested range is
//! streamed. `Content-Length` is set from that range so clients still get a plain body.

use crate::error::ApiError;
use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;
use futures_util::StreamExt;
use std::path::Path;
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, SeekFrom};
use tokio_util::io::ReaderStream;

pub struct FileBody {
    pub body: Body,
    pub len: u64,
}

/// Opens a stored artifact without following a symlink at the final path. A local actor with
/// write access to `artifacts_dir` must not be able to redirect a read to an arbitrary file.
async fn open_artifact(path: &Path) -> Result<File, ApiError> {
    #[cfg(unix)]
    {
        let mut opts = tokio::fs::OpenOptions::new();
        opts.read(true).custom_flags(libc::O_NOFOLLOW);
        opts.open(path)
            .await
            .map_err(|_| ApiError::not_found("not found"))
    }
    #[cfg(not(unix))]
    {
        let meta = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|_| ApiError::not_found("not found"))?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(ApiError::not_found("not found"));
        }
        File::open(path)
            .await
            .map_err(|_| ApiError::not_found("not found"))
    }
}

/// The on-disk length, after checking the artifact cap. Callers that need a tail use this to seek
/// to `len - tail` without reading the prefix.
pub async fn file_len(path: &Path, cap: u64) -> Result<u64, ApiError> {
    let meta = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| ApiError::not_found("not found"))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(ApiError::not_found("not found"));
    }
    if meta.len() > cap {
        return Err(ApiError::too_large("artifact too large"));
    }
    Ok(meta.len())
}

/// Streams at most `len` bytes starting at `offset`, clamping to the file's actual length.
pub async fn stream_range(
    path: &Path,
    cap: u64,
    offset: u64,
    len: u64,
) -> Result<FileBody, ApiError> {
    let mut file = open_artifact(path).await?;
    let meta = file
        .metadata()
        .await
        .map_err(|_| ApiError::not_found("not found"))?;
    if !meta.is_file() {
        return Err(ApiError::not_found("not found"));
    }
    if meta.len() > cap {
        return Err(ApiError::too_large("artifact too large"));
    }
    let len = len.min(meta.len().saturating_sub(offset));
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(|_| ApiError::internal())?;
    }
    let take = usize::try_from(len).map_err(|_| ApiError::too_large("artifact too large"))?;
    Ok(FileBody {
        body: Body::from_stream(ReaderStream::new(file).take(take)),
        len,
    })
}

pub fn into_response(file: FileBody, content_type: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, file.len.to_string())
        .body(file.body)
        .expect("valid artifact response headers")
}

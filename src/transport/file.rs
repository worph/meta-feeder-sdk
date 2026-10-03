//! Serving a byte range straight off a file on the shared `/data` volume.
//!
//! The hull's local-first read and the usenet plugin's materialised/partial
//! file reads answer with the same headers, so they share this. Moved from
//! meta-share's `nzb::serve::serve_file_range`, minus the focus metering: the
//! caller meters the body (the hull wraps every response it serves, a plugin
//! doesn't meter its `/raw` at all — the hull does on relay).

use std::path::Path;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use super::range::{parse_range_header, RangeOutcome};

/// Stream a complete on-disk file of `total` bytes, honouring a single
/// `Range:` (206) else 200; 416 for an unsatisfiable range.
///
/// `Err` carries the IO error that stopped the open/seek, so a caller can tell
/// "the file moved under us" apart from a served response.
pub async fn serve_file_range(
    path: &Path,
    total: u64,
    range: Option<&HeaderValue>,
) -> std::io::Result<Response> {
    let ctype = content_type(path);
    match parse_range_header(range, total) {
        RangeOutcome::Unsatisfiable => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{total}")).unwrap(),
            );
            Ok((StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response())
        }
        RangeOutcome::Ok { start, end } => {
            let len = end - start + 1;
            let mut file = tokio::fs::File::open(path).await?;
            file.seek(std::io::SeekFrom::Start(start)).await?;
            let body = Body::from_stream(ReaderStream::new(file.take(len)));
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
            headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")).unwrap(),
            );
            Ok((StatusCode::PARTIAL_CONTENT, headers, body).into_response())
        }
        // Absent / Unsupported → full 200.
        _ => {
            let file = tokio::fs::File::open(path).await?;
            let body = Body::from_stream(ReaderStream::new(file));
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
            headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(total));
            Ok((StatusCode::OK, headers, body).into_response())
        }
    }
}

/// Coarse content-type from extension — enough for the player to pick a demuxer.
pub fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("mkv") => "video/x-matroska",
        Some("mp4") | Some("m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("avi") => "video/x-msvideo",
        Some("mov") => "video/quicktime",
        Some("ts") | Some("m2ts") => "video/mp2t",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body(r: Response) -> Vec<u8> {
        axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec()
    }

    #[tokio::test]
    async fn range_full_and_unsatisfiable() {
        let p = std::env::temp_dir().join(format!("file-range-{}.mkv", std::process::id()));
        std::fs::write(&p, b"0123456789").unwrap();
        let r = serve_file_range(&p, 10, Some(&HeaderValue::from_static("bytes=2-4"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(r.headers()[header::CONTENT_RANGE], "bytes 2-4/10");
        assert_eq!(r.headers()[header::CONTENT_TYPE], "video/x-matroska");
        assert_eq!(body(r).await, b"234");
        let r = serve_file_range(&p, 10, None).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(body(r).await, b"0123456789");
        let r = serve_file_range(&p, 10, Some(&HeaderValue::from_static("bytes=20-"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(r.headers()[header::CONTENT_RANGE], "bytes */10");
        let _ = std::fs::remove_file(&p);
        assert_eq!(
            serve_file_range(&p, 10, None).await.unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }
}

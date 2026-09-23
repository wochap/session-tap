//! Minimal HTTP/1.1 request reader for the hub and the example receiver.
//! It reads one request with an explicit `content-length`; chunked transfer
//! encoding is not supported.

use std::io;
use tokio::io::{AsyncRead, AsyncReadExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpLimits {
    /// Maximum bytes before the blank line that ends the headers.
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// First value of header `name`, compared case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HttpReadError {
    /// The request line or headers cannot be parsed, or the connection ended
    /// before the headers or declared body were complete.
    #[error("malformed HTTP request: {0}")]
    Malformed(&'static str),
    /// A body-carrying request has no numeric `content-length`.
    #[error("content-length required")]
    LengthRequired,
    #[error("request headers exceed limit")]
    HeadersTooLarge,
    #[error("request body exceeds limit")]
    BodyTooLarge,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Reads one HTTP request. `GET` and `HEAD` without `content-length` have an
/// empty body; any other method must declare a numeric `content-length`.
pub async fn read_http_request<R>(
    stream: &mut R,
    limits: HttpLimits,
) -> Result<HttpRequest, HttpReadError>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut bytes = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            if end > limits.max_header_bytes {
                return Err(HttpReadError::HeadersTooLarge);
            }
            break end + 4;
        }
        if bytes.len() > limits.max_header_bytes {
            return Err(HttpReadError::HeadersTooLarge);
        }
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            return Err(HttpReadError::Malformed("connection closed before headers"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    };
    let head = std::str::from_utf8(&bytes[..header_end - 4])
        .map_err(|_| HttpReadError::Malformed("headers are not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(path), Some(version), None) = (
        request_line.next(),
        request_line.next(),
        request_line.next(),
        request_line.next(),
    ) else {
        return Err(HttpReadError::Malformed("invalid request line"));
    };
    if method.is_empty() || path.is_empty() || !version.starts_with("HTTP/") {
        return Err(HttpReadError::Malformed("invalid request line"));
    }
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or(HttpReadError::Malformed("invalid header line"))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(HttpReadError::Malformed("empty header name"));
        }
        headers.push((name.to_owned(), value.trim().to_owned()));
    }
    let mut request = HttpRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        headers,
        body: Vec::new(),
    };
    let content_length = match request.header("content-length") {
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| HttpReadError::LengthRequired)?,
        None if matches!(request.method.as_str(), "GET" | "HEAD") => 0,
        None => return Err(HttpReadError::LengthRequired),
    };
    if content_length > limits.max_body_bytes {
        return Err(HttpReadError::BodyTooLarge);
    }
    while bytes.len() - header_end < content_length {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            return Err(HttpReadError::Malformed("connection closed before body"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    request.body = bytes[header_end..header_end + content_length].to_vec();
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: HttpLimits = HttpLimits {
        max_header_bytes: 1024,
        max_body_bytes: 16,
    };

    async fn read(raw: &[u8]) -> Result<HttpRequest, HttpReadError> {
        let mut input = raw;
        read_http_request(&mut input, LIMITS).await
    }

    #[tokio::test]
    async fn reads_request_with_body() {
        let request = read(
            b"POST /ingest HTTP/1.1\r\nContent-Length: 5\r\nAuthorization: Bearer t\r\n\r\nhello",
        )
        .await
        .unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/ingest");
        assert_eq!(request.header("authorization"), Some("Bearer t"));
        assert_eq!(request.body, b"hello");
    }

    #[tokio::test]
    async fn get_without_length_has_empty_body() {
        let request = read(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        assert!(request.body.is_empty());
    }

    #[tokio::test]
    async fn malformed_requests() {
        for raw in [
            &b"garbage"[..],
            b"POST /x HTTP/1.1\r\nContent-Length: 1\r\n",
            b"NOT-HTTP\r\n\r\n",
            b"POST /x FTP/1\r\n\r\n",
            b"POST /x HTTP/1.1\r\nno-colon\r\n\r\n",
            b"POST /x HTTP/1.1\r\nContent-Length: 4\r\n\r\nab",
        ] {
            assert!(
                matches!(read(raw).await, Err(HttpReadError::Malformed(_))),
                "{}",
                String::from_utf8_lossy(raw)
            );
        }
    }

    #[tokio::test]
    async fn length_required() {
        for raw in [
            &b"POST /x HTTP/1.1\r\n\r\n"[..],
            b"POST /x HTTP/1.1\r\nContent-Length: ten\r\n\r\n",
        ] {
            assert!(matches!(
                read(raw).await,
                Err(HttpReadError::LengthRequired)
            ));
        }
    }

    #[tokio::test]
    async fn body_too_large() {
        assert!(matches!(
            read(b"POST /x HTTP/1.1\r\nContent-Length: 17\r\n\r\n").await,
            Err(HttpReadError::BodyTooLarge)
        ));
    }

    #[tokio::test]
    async fn headers_too_large() {
        let mut raw = b"POST /x HTTP/1.1\r\nX: ".to_vec();
        raw.extend(std::iter::repeat_n(b'a', 2048));
        raw.extend_from_slice(b"\r\n\r\n");
        assert!(matches!(
            read(&raw).await,
            Err(HttpReadError::HeadersTooLarge)
        ));
    }

    #[tokio::test]
    async fn io_error_is_reported() {
        struct Broken;
        impl AsyncRead for Broken {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<io::Result<()>> {
                std::task::Poll::Ready(Err(io::Error::other("boom")))
            }
        }
        assert!(matches!(
            read_http_request(&mut Broken, LIMITS).await,
            Err(HttpReadError::Io(_))
        ));
    }
}

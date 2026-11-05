//! Read-only smart-HTTP clones through `git http-backend`.
//!
//! Only `info/refs?service=git-upload-pack` and `POST git-upload-pack` are
//! served; anything that would push is refused before git runs. The CGI
//! environment is built from the index (never from the raw URL), and both
//! bodies are streamed.

use std::process::Stdio;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::{StreamExt, TryStreamExt};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::io::{ReaderStream, StreamReader};

use crate::handlers::error;
use crate::index::Repo;
use crate::state::AppState;

/// CGI response headers larger than this are an error.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Whether `rest` (the path after the repository) is a clone endpoint.
pub fn is_clone_path(rest: &[Vec<u8>]) -> bool {
    matches!(
        rest,
        [a, b] if a == b"info" && b == b"refs"
    ) || matches!(rest, [a] if a == b"git-upload-pack" || a == b"git-receive-pack")
}

pub async fn serve(
    state: Arc<AppState>,
    repo: Arc<Repo>,
    rest: Vec<Vec<u8>>,
    req: Request,
) -> Response {
    let config = state.config.load_full();
    if !config.repos.https_clone {
        return error::page(&state, StatusCode::NOT_FOUND, "PAGE NOT FOUND");
    }
    let query = req.uri().query().unwrap_or_default().to_string();
    let service = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("service="))
        .unwrap_or_default();
    let endpoint = match (req.method(), rest.last().map(Vec::as_slice)) {
        (&Method::GET | &Method::HEAD, Some(b"refs")) => match service {
            "git-upload-pack" => "info/refs",
            "git-receive-pack" => return forbidden(),
            _ => {
                return (
                    StatusCode::FORBIDDEN,
                    "Only the smart HTTP protocol is supported.\n",
                )
                    .into_response();
            }
        },
        (&Method::POST, Some(b"git-upload-pack")) => "git-upload-pack",
        (_, Some(b"git-receive-pack")) => return forbidden(),
        _ => return StatusCode::METHOD_NOT_ALLOWED.into_response(),
    };

    let Ok(permit) = state.heavy_permits.clone().acquire_owned().await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let (Some(root), Some(dir)) = (repo.git_dir.parent(), repo.git_dir.file_name()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path_info = format!("/{}/{endpoint}", dir.to_string_lossy());

    let headers = req.headers();
    let header = |name: HeaderName| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let mut cmd = Command::new("git");
    cmd.arg("http-backend")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_PROJECT_ROOT", root)
        .env("PATH_INFO", path_info)
        .env("REQUEST_METHOD", req.method().as_str())
        .env("QUERY_STRING", &query)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        // Only the repository's own config, plus ours.
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "http.receivepack")
        .env("GIT_CONFIG_VALUE_0", "false")
        // Visibility is decided by the index; ownership is not our concern.
        .env("GIT_CONFIG_KEY_1", "safe.directory")
        .env("GIT_CONFIG_VALUE_1", "*")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (var, name) in [
        ("CONTENT_TYPE", header::CONTENT_TYPE),
        ("CONTENT_LENGTH", header::CONTENT_LENGTH),
        ("HTTP_CONTENT_ENCODING", header::CONTENT_ENCODING),
    ] {
        if let Some(value) = header(name) {
            cmd.env(var, value);
        }
    }
    if let Some(protocol) = header(HeaderName::from_static("git-protocol")) {
        cmd.env("HTTP_GIT_PROTOCOL", protocol);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            tracing::error!("cannot run git http-backend: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    // Request body → stdin, then EOF.
    let body = req
        .into_body()
        .into_data_stream()
        .map_err(std::io::Error::other);
    tokio::spawn(async move {
        let mut reader = StreamReader::new(body);
        if let Err(e) = tokio::io::copy(&mut reader, &mut stdin).await {
            tracing::debug!("clone request body: {e}");
        }
    });
    let name = repo.name.clone();
    tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            tracing::warn!("{name}: git http-backend: {line}");
        }
    });

    // CGI headers, then the rest of stdout as the body.
    let mut buf = Vec::with_capacity(4096);
    let (status, cgi_headers, body_start) = loop {
        if let Some(parsed) = parse_cgi_headers(&buf) {
            break parsed;
        }
        if buf.len() > MAX_HEADER_BYTES {
            tracing::error!("git http-backend sent oversized headers");
            return StatusCode::BAD_GATEWAY.into_response();
        }
        match stdout.read_buf(&mut buf).await {
            Ok(0) | Err(_) => {
                tracing::error!("git http-backend ended before its headers");
                return StatusCode::BAD_GATEWAY.into_response();
            }
            Ok(_) => {}
        }
    };
    let head = buf.split_off(body_start);
    let stream = futures_util::stream::once(async move { Ok(bytes::Bytes::from(head)) })
        .chain(ReaderStream::new(stdout));
    // Reap git and release the permit when it exits.
    tokio::spawn(async move {
        let _permit = permit;
        let _ = child.wait().await;
    });

    let mut res = Body::from_stream(stream).into_response();
    *res.status_mut() = status;
    for (name, value) in cgi_headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            res.headers_mut().insert(name, value);
        }
    }
    res
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "This server is read-only.\n").into_response()
}

/// A parsed CGI response head: status, headers, and where the body starts.
pub type CgiHead = (StatusCode, Vec<(String, String)>, usize);

/// Parse a CGI response head, or `None` while the blank line hasn't
/// arrived yet.
pub fn parse_cgi_headers(buf: &[u8]) -> Option<CgiHead> {
    let (end, sep) = [&b"\r\n\r\n"[..], b"\n\n"]
        .iter()
        .filter_map(|sep| {
            buf.windows(sep.len())
                .position(|w| w == *sep)
                .map(|i| (i, sep.len()))
        })
        .min()?;
    let head = String::from_utf8_lossy(&buf[..end]);
    let mut status = StatusCode::OK;
    let mut headers = Vec::new();
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_string();
        if name.eq_ignore_ascii_case("status") {
            if let Some(code) = value
                .split_whitespace()
                .next()
                .and_then(|c| c.parse::<u16>().ok())
                .and_then(|c| StatusCode::from_u16(c).ok())
            {
                status = code;
            }
        } else {
            headers.push((name.trim().to_string(), value));
        }
    }
    Some((status, headers, end + sep))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgi_headers() {
        let raw = b"Expires: Fri, 01 Jan 1980 00:00:00 GMT\r\nContent-Type: application/x-git-upload-pack-advertisement\r\n\r\n001e# service";
        let (status, headers, start) = parse_cgi_headers(raw).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[1].1, "application/x-git-upload-pack-advertisement");
        assert_eq!(&raw[start..], b"001e# service");

        let raw = b"Status: 403 Forbidden\nContent-Type: text/plain\n\nno";
        let (status, headers, start) = parse_cgi_headers(raw).unwrap();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(headers, vec![("Content-Type".into(), "text/plain".into())]);
        assert_eq!(&raw[start..], b"no");

        assert!(parse_cgi_headers(b"Content-Type: x\r\n").is_none());
    }

    #[test]
    fn clone_paths() {
        let p = |s: &[&str]| s.iter().map(|x| x.as_bytes().to_vec()).collect::<Vec<_>>();
        assert!(is_clone_path(&p(&["info", "refs"])));
        assert!(is_clone_path(&p(&["git-upload-pack"])));
        assert!(is_clone_path(&p(&["git-receive-pack"])));
        assert!(!is_clone_path(&p(&["info"])));
        assert!(!is_clone_path(&p(&["tree", "main"])));
    }
}

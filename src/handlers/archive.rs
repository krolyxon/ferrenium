//! `/{repo}/archive/{rev}.tar.gz|.zip` — a snapshot of a revision,
//! streamed while it is being written.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio_util::io::{ReaderStream, SyncIoBridge};

use super::repo::{Ctx, set_cache_headers};
use crate::config::SnapshotFormat;
use crate::git::archive::{self, Format};
use crate::git::refs;
use crate::render::encode_path;

const PIPE_BYTES: usize = 64 * 1024;

pub fn serve(ctx: &Ctx, segments: &[Vec<u8>]) -> Response {
    let Ok(name) = String::from_utf8(segments.join(&b'/')) else {
        return ctx.not_found("UNKNOWN REVISION");
    };
    let (rev_name, format) = if let Some(stem) = name.strip_suffix(".tar.gz") {
        (stem, Format::TarGz)
    } else if let Some(stem) = name.strip_suffix(".zip") {
        (stem, Format::Zip)
    } else {
        return ctx.not_found("UNKNOWN ARCHIVE FORMAT");
    };
    let wanted = match format {
        Format::TarGz => SnapshotFormat::TarGz,
        Format::Zip => SnapshotFormat::Zip,
    };
    if !ctx.config.repos.snapshots.contains(&wanted) {
        return ctx.not_found("ARCHIVE FORMAT DISABLED");
    }
    let Some(rev) = refs::resolve(&ctx.git, rev_name) else {
        return ctx.not_found("UNKNOWN REVISION");
    };
    let etag = format!("W/\"{}-{}\"", rev.commit, format.extension());
    let immutable = rev.is_immutable();
    if let Some(res) = ctx.not_modified(&etag, immutable) {
        return res;
    }

    let base = archive_name(&ctx.repo.name, rev_name);
    let prefix = format!("{base}/");
    let (reader, writer) = tokio::io::duplex(PIPE_BYTES);
    let gix = ctx.repo.gix.clone();
    let permit = ctx.take_heavy_permit();
    let repo_name = ctx.repo.name.clone();
    let commit = rev.commit;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let repo = crate::git::handle(&gix);
        let out = std::io::BufWriter::with_capacity(PIPE_BYTES, SyncIoBridge::new(writer));
        if let Err(e) = archive::write(&repo, commit, format, &prefix, out) {
            // Usually the client went away.
            tracing::debug!("{repo_name}: archive of {commit} stopped: {e:#}");
        }
    });

    let mut res = Body::from_stream(ReaderStream::new(reader)).into_response();
    *res.status_mut() = StatusCode::OK;
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(format.content_type()),
    );
    let file = format!("{base}.{}", format.extension());
    let disposition = format!(
        "attachment; filename=\"{file}\"; filename*=UTF-8''{}",
        encode_path(&file)
    );
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    set_cache_headers(&mut res, &etag, immutable);
    res
}

/// `{repo}-{rev}`, using the last component of the repository name and
/// a revision name made filename-safe (`feature/x` → `feature-x`).
pub fn archive_name(repo: &str, rev: &str) -> String {
    let repo = repo.rsplit('/').next().unwrap_or(repo);
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    };
    format!("{}-{}", clean(repo), clean(rev))
}

#[cfg(test)]
mod tests {
    use super::archive_name;

    #[test]
    fn names() {
        assert_eq!(archive_name("forks/foo", "v1.0"), "foo-v1.0");
        assert_eq!(archive_name("foo", "feature/x"), "foo-feature-x");
        assert_eq!(archive_name("a b", "\"x\""), "a-b--x-");
    }
}

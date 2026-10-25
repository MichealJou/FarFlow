use axum::{http::header, middleware, response::Response, Router};
use farflow_core::{regular_directory, Error, Result};
use std::{fs, path::Path};
use tower_http::services::{ServeDir, ServeFile};
const INVALID_BUILD: &str = "网页构建目录无效，需要 index.html 和 assets 文件夹";

pub(super) fn router(api: Router, directory: &Path, data: &Path) -> Result<Router> {
    regular_directory(directory)?;
    let directory = fs::canonicalize(directory).map_err(invalid)?;
    let data = fs::canonicalize(data).map_err(invalid)?;
    if directory.starts_with(&data) || data.starts_with(&directory) {
        return Err(Error::Validation(
            "网页目录和服务数据目录必须分开保存".into(),
        ));
    }
    let index = directory.join("index.html");
    if !fs::symlink_metadata(&index)
        .map_err(invalid)?
        .file_type()
        .is_file()
    {
        return Err(Error::Validation(INVALID_BUILD.into()));
    }
    let assets = directory.join("assets");
    regular_directory(&assets)?;
    let mut pending = vec![assets.clone()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path).map_err(invalid)? {
            let entry = entry.map_err(invalid)?;
            let kind = entry.file_type().map_err(invalid)?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if !kind.is_file() {
                return Err(Error::Validation(
                    "网页资源不能包含符号链接或特殊文件".into(),
                ));
            }
        }
    }
    Ok(api
        .route_service("/", ServeFile::new(index))
        .nest_service("/assets", ServeDir::new(assets))
        .layer(middleware::map_response(headers)))
}

fn invalid(_: std::io::Error) -> Error {
    Error::Validation(INVALID_BUILD.into())
}

async fn headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    headers.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    headers.insert(header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' blob:; connect-src 'self' http: https:; frame-src blob:; object-src 'none'; base-uri 'self'; form-action 'self'".parse().unwrap());
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
        routing::get,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn assets_and_index_use_real_bytes_and_leave_api_routes_intact() {
        let temp = tempfile::tempdir().unwrap();
        let public = temp.path().join("web");
        let data = temp.path().join("data");
        fs::create_dir_all(public.join("assets")).unwrap();
        fs::create_dir(&data).unwrap();
        fs::write(
            public.join("index.html"),
            "<!doctype html><title>FarFlow 研流</title>",
        )
        .unwrap();
        fs::write(public.join("assets/app.js"), "console.log('FarFlow')").unwrap();
        fs::write(public.join("private.json"), "不可通过静态路由读取").unwrap();
        fs::write(data.join("secret.txt"), "不可通过静态路由读取").unwrap();
        let api = Router::new().route("/api/probe", get(|| async { "真实接口" }));
        let app = router(api, &public, &data).unwrap();
        for (url, expected, mime) in [
            (
                "/",
                "<!doctype html><title>FarFlow 研流</title>",
                "text/html",
            ),
            (
                "/assets/app.js",
                "console.log('FarFlow')",
                "text/javascript",
            ),
            ("/api/probe", "真实接口", "text/plain"),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with(mime));
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
            assert_eq!(response.headers()[header::X_FRAME_OPTIONS], "DENY");
            assert!(response.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("object-src 'none'"));
            assert_eq!(
                to_bytes(response.into_body(), 1024).await.unwrap(),
                expected
            );
        }
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty());
        for url in [
            "/private.json",
            "/data/secret.txt",
            "/assets/../private.json",
            "/assets/%2e%2e/private.json",
            "/assets/missing.js",
            "/api/missing",
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND,
                "{url}"
            );
        }
        assert_eq!(
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[test]
    fn invalid_builds_and_overlapping_data_directories_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let public = temp.path().join("web");
        let data = temp.path().join("data");
        fs::create_dir(&public).unwrap();
        fs::create_dir(&data).unwrap();
        assert!(router(Router::new(), &temp.path().join("missing"), &data).is_err());
        assert!(router(Router::new(), &public, &temp.path().join("missing")).is_err());
        assert!(router(Router::new(), &public, &data).is_err());
        fs::create_dir(public.join("index.html")).unwrap();
        assert!(router(Router::new(), &public, &data).is_err());
        fs::remove_dir(public.join("index.html")).unwrap();
        fs::write(public.join("index.html"), "FarFlow").unwrap();
        assert!(router(Router::new(), &public, &data).is_err());
        fs::create_dir(public.join("assets")).unwrap();
        fs::create_dir(public.join("assets/nested")).unwrap();
        fs::write(public.join("assets/nested/mark.txt"), "图标").unwrap();
        assert!(router(Router::new(), &public, &public).is_err());
        assert!(router(Router::new(), temp.path(), &data).is_err());
        assert!(router(Router::new(), &public, temp.path()).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                data.join("private.html"),
                public.join("assets/private.html"),
            )
            .unwrap();
            assert!(router(Router::new(), &public, &data).is_err());
            fs::remove_file(public.join("assets/private.html")).unwrap();
            fs::remove_file(public.join("index.html")).unwrap();
            fs::write(data.join("private.html"), "私有数据").unwrap();
            std::os::unix::fs::symlink(data.join("private.html"), public.join("index.html"))
                .unwrap();
            assert!(router(Router::new(), &public, &data).is_err());
        }
    }
}

//! HTTP routing for the snapshot store, factored as a pure function so the
//! method/path/auth/status logic is unit-testable without sockets. The tokio
//! server (`bin/snapstore-server.rs`) is thin glue: parse a request, call
//! [`route`], write the [`Response`].
//!
//! API (writes need the admin token; reads are open and immutable → cacheable):
//!   GET    /pages/<hex>        fetch a page            200 / 404 / 400
//!   HEAD   /pages/<hex>        existence (skip upload) 200 / 404 / 400
//!   PUT    /pages/<hex>        store a page (verified) 201 new / 200 dedup / 400 / 401
//!   GET    /manifests          list snapshot ids       200 (JSON array)
//!   GET    /manifests/<id>     fetch a manifest        200 / 404 / 400
//!   PUT    /manifests/<id>     store a manifest        200 / 400 / 401
//!
//! Auth: the admin token is supplied via `Authorization: Bearer <token>`. Writes
//! require it to match the configured token (constant-time); if no token is
//! configured the store is read-only (every write → 401), failing closed.

use crate::{from_hex, PutError, Store};

/// A minimal HTTP response: status, content type, and body.
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    fn text(status: u16, msg: &str) -> Self {
        Response {
            status,
            content_type: "text/plain; charset=utf-8",
            body: msg.as_bytes().to_vec(),
        }
    }
    fn bytes(body: Vec<u8>) -> Self {
        Response {
            status: 200,
            content_type: "application/octet-stream",
            body,
        }
    }
}

/// The reason phrase for the few statuses we emit.
pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

/// Constant-time string equality (no early-out on the first differing byte), so
/// comparing the admin token doesn't leak its length-prefix via timing.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Extract a bearer token from an `Authorization` header value, if present.
pub fn bearer(auth_header: Option<&str>) -> Option<&str> {
    auth_header?.strip_prefix("Bearer ").map(str::trim)
}

/// Route one request. `admin_token` is the configured secret (None = writes
/// disabled). `auth` is the bearer token the client presented. `body` is the
/// request body (empty for GET/HEAD).
pub fn route(
    store: &Store,
    admin_token: Option<&str>,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: &[u8],
) -> Response {
    let authorized =
        admin_token.is_some() && auth.is_some() && ct_eq(auth.unwrap(), admin_token.unwrap());

    if let Some(hex) = path.strip_prefix("/pages/") {
        let Some(hash) = from_hex(hex) else {
            return Response::text(400, "bad page hash");
        };
        return match method {
            "HEAD" => {
                if store.has_page(&hash) {
                    Response {
                        status: 200,
                        content_type: "application/octet-stream",
                        body: Vec::new(),
                    }
                } else {
                    Response::text(404, "no such page")
                }
            }
            "GET" => match store.get_page(&hash) {
                Ok(Some(b)) => Response::bytes(b),
                Ok(None) => Response::text(404, "no such page"),
                Err(_) => Response::text(500, "read error"),
            },
            "PUT" => {
                if !authorized {
                    return Response::text(401, "admin token required");
                }
                match store.put_page(&hash, body) {
                    Ok(true) => Response::text(201, "stored"),
                    Ok(false) => Response::text(200, "already present"),
                    Err(PutError::HashMismatch) => Response::text(400, "body != hash"),
                    Err(_) => Response::text(500, "write error"),
                }
            }
            _ => Response::text(405, "method not allowed"),
        };
    }

    if path == "/manifests" {
        if method != "GET" {
            return Response::text(405, "method not allowed");
        }
        return match store.list_manifests() {
            Ok(ids) => {
                // ids are validated [A-Za-z0-9._-] → no JSON escaping needed.
                let items: Vec<String> = ids.iter().map(|id| format!("\"{id}\"")).collect();
                Response {
                    status: 200,
                    content_type: "application/json",
                    body: format!("[{}]", items.join(",")).into_bytes(),
                }
            }
            Err(_) => Response::text(500, "list error"),
        };
    }

    if let Some(id) = path.strip_prefix("/manifests/") {
        return match method {
            "GET" => match store.get_manifest(id) {
                Ok(Some(b)) => Response::bytes(b),
                Ok(None) => Response::text(404, "no such manifest"),
                Err(PutError::BadName) => Response::text(400, "bad manifest id"),
                Err(_) => Response::text(500, "read error"),
            },
            "PUT" => {
                if !authorized {
                    return Response::text(401, "admin token required");
                }
                match store.put_manifest(id, body) {
                    Ok(()) => Response::text(200, "stored"),
                    Err(PutError::BadName) => Response::text(400, "bad manifest id"),
                    Err(_) => Response::text(500, "write error"),
                }
            }
            _ => Response::text(405, "method not allowed"),
        };
    }

    Response::text(404, "not found")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_store() -> (Store, PathBuf) {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("wwwvm-snaphttp-{}-{n}", std::process::id()));
        (Store::open(&root).unwrap(), root)
    }

    fn hash(body: &[u8]) -> String {
        crate::to_hex(blake3::hash(body).as_bytes())
    }

    const TOK: Option<&str> = Some("s3cret");

    #[test]
    fn put_page_requires_token_and_verifies() {
        let (store, root) = tmp_store();
        let body = b"page bytes";
        let path = format!("/pages/{}", hash(body));

        // No token → 401, nothing stored.
        assert_eq!(route(&store, TOK, "PUT", &path, None, body).status, 401);
        // Wrong token → 401.
        assert_eq!(
            route(&store, TOK, "PUT", &path, Some("nope"), body).status,
            401
        );
        // Correct token → 201 (created).
        assert_eq!(route(&store, TOK, "PUT", &path, TOK, body).status, 201);
        // Again → 200 (dedup).
        assert_eq!(route(&store, TOK, "PUT", &path, TOK, body).status, 200);
        // Body not matching the claimed hash → 400.
        assert_eq!(
            route(&store, TOK, "PUT", &path, TOK, b"different").status,
            400
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn get_and_head_pages() {
        let (store, root) = tmp_store();
        let body = b"hello page";
        let path = format!("/pages/{}", hash(body));
        route(&store, TOK, "PUT", &path, TOK, body);

        let r = route(&store, TOK, "GET", &path, None, b"");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, body);
        assert_eq!(route(&store, TOK, "HEAD", &path, None, b"").status, 200);

        let absent = format!("/pages/{}", hash(b"absent"));
        assert_eq!(route(&store, TOK, "GET", &absent, None, b"").status, 404);
        assert_eq!(route(&store, TOK, "HEAD", &absent, None, b"").status, 404);
        // Malformed hash → 400.
        assert_eq!(
            route(&store, TOK, "GET", "/pages/xyz", None, b"").status,
            400
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn manifests_put_get_list() {
        let (store, root) = tmp_store();
        // PUT needs token.
        assert_eq!(
            route(&store, TOK, "PUT", "/manifests/task1", None, b"m").status,
            401
        );
        assert_eq!(
            route(&store, TOK, "PUT", "/manifests/task1", TOK, b"M1").status,
            200
        );
        // GET open.
        let r = route(&store, TOK, "GET", "/manifests/task1", None, b"");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"M1");
        // List as JSON.
        route(&store, TOK, "PUT", "/manifests/task2", TOK, b"M2");
        let r = route(&store, TOK, "GET", "/manifests", None, b"");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, br#"["task1","task2"]"#);
        // Bad id → 400.
        assert_eq!(
            route(&store, TOK, "PUT", "/manifests/../x", TOK, b"m").status,
            400
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn writes_disabled_when_no_token_configured() {
        let (store, root) = tmp_store();
        let body = b"x";
        let path = format!("/pages/{}", hash(body));
        // admin_token = None → fail closed even if a client sends one.
        assert_eq!(
            route(&store, None, "PUT", &path, Some("any"), body).status,
            401
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bearer_parsing_and_ct_eq() {
        assert_eq!(bearer(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer(Some("Basic abc")), None);
        assert_eq!(bearer(None), None);
        assert!(ct_eq("hunter2", "hunter2"));
        assert!(!ct_eq("hunter2", "hunter3"));
        assert!(!ct_eq("short", "longer"));
    }
}

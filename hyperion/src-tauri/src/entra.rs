// Hyperion — Microsoft Entra SSO (Phase 1 step 2, M6).
//
// Gates the encrypted vault behind a real Microsoft Entra sign-in. Uses the
// OAuth 2.0 authorization-code flow with PKCE — the correct flow for a native
// desktop public client (no client secret):
//
//   1. Generate a PKCE verifier + S256 challenge, plus CSRF `state` and `nonce`.
//   2. Bind a loopback listener on 127.0.0.1:<ephemeral>; that becomes the
//      redirect URI `http://localhost:<port>` (Azure allows any loopback port
//      for the "Mobile and desktop applications" platform).
//   3. Open the system browser to the authorize endpoint; the user signs in.
//   4. Entra redirects to the loopback with `?code=...&state=...`.
//   5. Exchange the code at the token endpoint (TLS) for an id_token, sending
//      the PKCE verifier. The TLS channel + PKCE prove the token's origin.
//   6. Decode the id_token claims; verify aud / iss / exp / nonce; surface the
//      signed-in identity.
//
// The tokens are NOT persisted — only the in-memory "signed in" fact and the
// display identity. App stays read-only toward bOS; everything here is local.
//
// Tracked hardening (Phase 1 follow-up): full id_token *signature* validation
// against the tenant JWKS. Tokens here arrive directly from the token endpoint
// over authenticated TLS (not the front channel), so per OIDC the signature is
// not strictly required for this flow, but JWKS validation is defense-in-depth.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::Engine;
use rand::RngCore;
use serde_json::Value;
use sha2::{Digest, Sha256};

// Public client identifiers (NOT secrets — no client secret in the PKCE flow).
// Overridable via env for other tenants/deployments.
const DEFAULT_CLIENT_ID: &str = "b540d203-be61-41b4-bb3d-60f2c48a4812";
const DEFAULT_TENANT_ID: &str = "efacdbb3-8b4e-4d16-8110-4bfb66410cd7";
// No `offline_access`: this is a sign-in gate, not a session — we need only the
// id_token, never a refresh token. Requesting one would create credential
// surface we immediately discard.
const SCOPE: &str = "openid profile";
/// How long to wait for the browser redirect before giving up.
const REDIRECT_TIMEOUT: Duration = Duration::from_secs(180);

fn client_id() -> String {
    std::env::var("HYPERION_ENTRA_CLIENT_ID").unwrap_or_else(|_| DEFAULT_CLIENT_ID.to_string())
}
fn tenant_id() -> String {
    std::env::var("HYPERION_ENTRA_TENANT_ID").unwrap_or_else(|_| DEFAULT_TENANT_ID.to_string())
}
fn authority() -> String {
    format!("https://login.microsoftonline.com/{}", tenant_id())
}

/// The signed-in user (display only; no tokens retained).
#[derive(Clone)]
pub struct Identity {
    pub name: String,
    pub username: String,
}

impl Identity {
    pub fn to_json(&self) -> Value {
        serde_json::json!({ "name": self.name, "username": self.username })
    }
}

/// Auth state: whether an Entra sign-in is currently held.
#[derive(Default)]
pub struct Auth {
    identity: Option<Identity>,
}

impl Auth {
    pub fn is_authenticated(&self) -> bool {
        self.identity.is_some()
    }

    pub fn sign_out(&mut self) {
        self.identity = None;
    }

    pub fn status(&self) -> Value {
        match &self.identity {
            Some(id) => serde_json::json!({ "authenticated": true, "identity": id.to_json() }),
            None => serde_json::json!({ "authenticated": false, "identity": Value::Null }),
        }
    }

    /// Run the full interactive sign-in. Blocking: opens a browser and waits for
    /// the loopback redirect, then exchanges the code. On success the identity
    /// is stored and returned.
    pub fn sign_in(&mut self) -> Result<Identity, String> {
        let id = interactive_sign_in()?;
        self.identity = Some(id.clone());
        Ok(id)
    }
}

fn random_b64url(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(&buf)
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn now_secs() -> u64 {
    // Fail CLOSED on a clock error: return the max so any expiry check treats
    // the token as expired rather than silently accepting it.
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(u64::MAX)
}

fn open_url(url: &str) -> Result<(), String> {
    let mut cmd;
    #[cfg(target_os = "windows")]
    {
        // rundll32 handles the full URL (with `&` query separators) without the
        // cmd `start` shell-parsing pitfalls.
        cmd = std::process::Command::new("rundll32");
        cmd.args(["url.dll,FileProtocolHandler", url]);
    }
    #[cfg(target_os = "macos")]
    {
        cmd = std::process::Command::new("open");
        cmd.arg(url);
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        cmd = std::process::Command::new("xdg-open");
        cmd.arg(url);
    }
    cmd.spawn().map_err(|e| format!("open browser: {e}"))?;
    Ok(())
}

/// Parse `code` / `state` / `error` out of the loopback request's query string.
fn parse_query(target: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let val = urlencoding::decode(v)
                .map(|c| c.into_owned())
                .unwrap_or_default();
            out.insert(k.to_string(), val);
        }
    }
    out
}

/// Read one HTTP request line+headers from a stream, accumulating until the
/// header terminator (or an 8 KiB cap). The stream is forced to blocking with a
/// short read timeout first, so this is deterministic on Windows and Unix alike
/// (an accepted socket's blocking flag is otherwise platform-dependent).
fn read_request(stream: &mut std::net::TcpStream) -> Result<String, String> {
    stream
        .set_nonblocking(false)
        .map_err(|e| format!("stream config: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|e| format!("stream timeout: {e}"))?;
    let mut data: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break, // peer closed
            Ok(n) => {
                data.extend_from_slice(&chunk[..n]);
                if data.windows(4).any(|w| w == b"\r\n\r\n") || data.len() >= 8192 {
                    break;
                }
            }
            // A retryable timeout/would-block: stop reading with what we have.
            Err(ref e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(e) => return Err(format!("read redirect: {e}")),
        }
    }
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// Wait (up to REDIRECT_TIMEOUT) for the browser redirect and return its query
/// parameters. Ignores stray connections (browser preconnects, favicon probes):
/// only a request whose query carries `code` or `error` is treated as the
/// redirect; everything else is answered and the loop keeps waiting.
fn await_redirect(
    listener: &TcpListener,
) -> Result<std::collections::HashMap<String, String>, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("listener config: {e}"))?;
    let deadline = SystemTime::now() + REDIRECT_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let req = read_request(&mut stream)?;
                let target = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("");
                let params = parse_query(target);
                let is_redirect = params.contains_key("code") || params.contains_key("error");
                let body = if is_redirect {
                    "<!doctype html><html><body style=\"font:16px system-ui;padding:40px;color:#1f2a36\">\
                     <h2>Signed in to Hyperion</h2><p>You can close this tab and return to the app.</p></body></html>"
                } else {
                    "<!doctype html><html><body></body></html>"
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                if is_redirect {
                    return Ok(params);
                }
                // stray connection — keep waiting (subject to the deadline below)
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(120));
            }
            Err(e) => return Err(format!("accept redirect: {e}")),
        }
        if SystemTime::now() >= deadline {
            return Err("timed out waiting for the browser sign-in".into());
        }
    }
}

fn interactive_sign_in() -> Result<Identity, String> {
    let verifier = random_b64url(32); // 43-char high-entropy verifier
    let challenge = pkce_challenge(&verifier);
    let state = random_b64url(24);
    let nonce = random_b64url(24);

    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind loopback: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("loopback addr: {e}"))?
        .port();
    // Use the literal loopback IP that we actually bound (RFC 8252 §7.3): avoids
    // `localhost` resolving to ::1 where nothing is listening. Azure treats a
    // registered `http://localhost` redirect as equivalent to 127.0.0.1 loopback.
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let cid = client_id();
    let authorize_url = format!(
        "{}/oauth2/v2.0/authorize?client_id={}&response_type=code&redirect_uri={}&response_mode=query&scope={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256&prompt=select_account",
        authority(),
        urlencoding::encode(&cid),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(SCOPE),
        urlencoding::encode(&state),
        urlencoding::encode(&nonce),
        urlencoding::encode(&challenge),
    );

    open_url(&authorize_url)?;
    let params = await_redirect(&listener)?;

    // CSRF first (RFC 9700 §4.2.4): validate state before trusting any other
    // field, so an injected redirect can't surface a crafted error in its place.
    match params.get("state") {
        Some(s) if s == &state => {}
        _ => return Err("Entra sign-in failed: state mismatch (possible CSRF)".into()),
    }
    if let Some(err) = params.get("error") {
        let desc = params.get("error_description").cloned().unwrap_or_default();
        return Err(format!("Entra sign-in failed: {err} {desc}"));
    }
    let code = params
        .get("code")
        .ok_or_else(|| "Entra sign-in failed: no authorization code returned".to_string())?;

    let id_token = exchange_code(&cid, code, &redirect_uri, &verifier)?;
    let claims = decode_id_token(&id_token)?;
    verify_claims(&claims, &cid, &nonce)?;

    let name = claims
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)")
        .to_string();
    let username = claims
        .get("preferred_username")
        .or_else(|| claims.get("upn"))
        .or_else(|| claims.get("email"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(Identity { name, username })
}

/// POST the authorization code to the token endpoint and return the id_token.
fn exchange_code(
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<String, String> {
    let token_url = format!("{}/oauth2/v2.0/token", authority());
    let resp = ureq::post(&token_url).send_form(&[
        ("client_id", client_id),
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
        ("scope", SCOPE),
    ]);
    let body = match resp {
        Ok(r) => r
            .into_string()
            .map_err(|e| format!("read token body: {e}"))?,
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            // Surface the Entra error code, not any token material.
            let short: String = detail.chars().take(300).collect();
            return Err(format!("token endpoint returned HTTP {code}: {short}"));
        }
        Err(e) => return Err(format!("token request failed: {e}")),
    };
    let json: Value = serde_json::from_str(&body).map_err(|e| format!("parse token json: {e}"))?;
    json.get("id_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "token response had no id_token".to_string())
}

/// Decode (without signature verification) the JWT payload into claims.
fn decode_id_token(id_token: &str) -> Result<Value, String> {
    let payload = id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| "malformed id_token".to_string())?;
    // JWTs use base64url; some encoders pad, most don't.
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| {
            base64::engine::general_purpose::URL_SAFE
                .decode(payload)
                .or_else(|_| STANDARD_NO_PAD.decode(payload))
        })
        .map_err(|e| format!("decode id_token: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("parse id_token claims: {e}"))
}

/// True if the `aud` claim matches `client_id`. OIDC permits `aud` to be either
/// a string or an array of strings.
fn aud_matches(aud: &Value, client_id: &str) -> bool {
    match aud {
        Value::String(s) => s == client_id,
        Value::Array(items) => items.iter().any(|v| v.as_str() == Some(client_id)),
        _ => false,
    }
}

/// Verify audience, issuer (tenant), expiry, and nonce.
fn verify_claims(claims: &Value, client_id: &str, nonce: &str) -> Result<(), String> {
    let aud = claims.get("aud").unwrap_or(&Value::Null);
    if !aud_matches(aud, client_id) {
        return Err("id_token audience mismatch".into());
    }
    // Exact issuer match — substring matching would accept e.g.
    // https://evil.example/<tenant>/v2.0.
    let iss = claims.get("iss").and_then(|v| v.as_str()).unwrap_or("");
    let expected_iss = format!("https://login.microsoftonline.com/{}/v2.0", tenant_id());
    if iss != expected_iss {
        return Err("id_token issuer mismatch (wrong tenant)".into());
    }
    let exp = claims.get("exp").and_then(|v| v.as_u64()).unwrap_or(0);
    // 120s skew allowance; saturating_add avoids overflow on a crafted exp.
    if exp.saturating_add(120) < now_secs() {
        return Err("id_token is expired".into());
    }
    let tok_nonce = claims.get("nonce").and_then(|v| v.as_str()).unwrap_or("");
    if tok_nonce != nonce {
        return Err("id_token nonce mismatch (possible replay)".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_known_vector() {
        // RFC 7636 Appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = pkce_challenge(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn random_b64url_is_url_safe_and_fresh() {
        let a = random_b64url(32);
        let b = random_b64url(32);
        assert_ne!(a, b);
        assert!(!a.contains('+') && !a.contains('/') && !a.contains('='));
    }

    #[test]
    fn parse_query_decodes_pairs() {
        let q = parse_query("/?code=abc%20123&state=xy_z&error=");
        assert_eq!(q.get("code").unwrap(), "abc 123");
        assert_eq!(q.get("state").unwrap(), "xy_z");
    }

    #[test]
    fn verify_claims_checks_aud_iss_nonce() {
        let good = serde_json::json!({
            "aud": "cid", "iss": format!("https://login.microsoftonline.com/{}/v2.0", DEFAULT_TENANT_ID),
            "exp": now_secs() + 3600, "nonce": "n1"
        });
        assert!(verify_claims(&good, "cid", "n1").is_ok());
        // wrong audience
        let bad_aud = serde_json::json!({ "aud": "other", "iss": good["iss"], "exp": now_secs()+3600, "nonce": "n1" });
        assert!(verify_claims(&bad_aud, "cid", "n1").is_err());
        // nonce replay
        let bad_nonce = serde_json::json!({ "aud": "cid", "iss": good["iss"], "exp": now_secs()+3600, "nonce": "evil" });
        assert!(verify_claims(&bad_nonce, "cid", "n1").is_err());
        // expired
        let expired = serde_json::json!({ "aud": "cid", "iss": good["iss"], "exp": now_secs()-3600, "nonce": "n1" });
        assert!(verify_claims(&expired, "cid", "n1").is_err());
        // aud as a single-element array is accepted
        let aud_arr = serde_json::json!({ "aud": ["cid"], "iss": good["iss"], "exp": now_secs()+3600, "nonce": "n1" });
        assert!(verify_claims(&aud_arr, "cid", "n1").is_ok());
        // issuer substring attack is rejected (exact match required)
        let bad_iss = serde_json::json!({ "aud": "cid",
            "iss": format!("https://evil.example/{}/v2.0", DEFAULT_TENANT_ID),
            "exp": now_secs()+3600, "nonce": "n1" });
        assert!(verify_claims(&bad_iss, "cid", "n1").is_err());
    }
}

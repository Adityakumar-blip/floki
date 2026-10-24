//! Signing for JWT Bearer, OAuth 1.0, Hawk and NTLM, and the OAuth 2.0 authorization code flow.

use crate::model::{resolve, Hawk, Jwt, Ntlm, OAuth1, OAuth2};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::{Map, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

type Vars<'a> = &'a [(String, String)];

macro_rules! hmac {
    ($hash:ty, $key:expr, $data:expr) => {{
        let mut mac = <Hmac<$hash>>::new_from_slice($key).expect("HMAC takes any key length");
        mac.update($data);
        mac.finalize().into_bytes().to_vec()
    }};
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).expect("OS random number generator");
    bytes
}

fn random_token(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    random_bytes::<64>().iter().take(len).map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}

/// Reads a PEM key pasted into the field, or from the file the field names.
fn pem_der(source: &str) -> Result<Vec<u8>, String> {
    let text = if source.contains("-----BEGIN") {
        source.to_string()
    } else {
        std::fs::read_to_string(source.trim()).map_err(|e| format!("Could not read private key \"{}\": {e}", source.trim()))?
    };
    let body: String = text.lines().filter(|l| !l.starts_with("-----")).map(str::trim).collect();
    STANDARD.decode(body).map_err(|_| "The private key is not valid PEM".to_string())
}

/// Builds and signs the JWT described by `j`.
pub fn jwt(j: &Jwt, vars: Vars) -> Result<String, String> {
    let parse = |label: &str, text: &str| -> Result<Map<String, Value>, String> {
        let text = resolve(text, vars);
        if text.trim().is_empty() {
            return Ok(Map::new());
        }
        match serde_json::from_str(&text) {
            Ok(Value::Object(map)) => Ok(map),
            _ => Err(format!("JWT {label} must be a JSON object")),
        }
    };
    let mut header = parse("headers", &j.headers)?;
    header.insert("alg".into(), Value::String(j.algorithm.clone()));
    header.entry("typ").or_insert_with(|| Value::String("JWT".into()));
    let payload = parse("payload", &j.payload)?;
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(Value::Object(header).to_string()),
        URL_SAFE_NO_PAD.encode(Value::Object(payload).to_string())
    );
    let secret = resolve(&j.secret, vars);
    let message = signing_input.as_bytes();
    let signature = match j.algorithm.as_str() {
        alg @ ("HS256" | "HS384" | "HS512") => {
            let key = if j.secret_encoding == "base64" {
                STANDARD.decode(secret.trim()).map_err(|_| "The JWT secret is not valid Base64".to_string())?
            } else {
                secret.into_bytes()
            };
            match alg {
                "HS256" => hmac!(sha2::Sha256, &key, message),
                "HS384" => hmac!(sha2::Sha384, &key, message),
                _ => hmac!(sha2::Sha512, &key, message),
            }
        }
        alg @ ("RS256" | "RS384" | "RS512" | "PS256" | "PS384" | "PS512") => {
            use ring::signature as sig;
            let der = pem_der(&secret)?;
            let key = sig::RsaKeyPair::from_pkcs8(&der)
                .or_else(|_| sig::RsaKeyPair::from_der(&der))
                .map_err(|e| format!("Not an RSA private key: {e}"))?;
            let padding: &dyn sig::RsaEncoding = match alg {
                "RS256" => &sig::RSA_PKCS1_SHA256,
                "RS384" => &sig::RSA_PKCS1_SHA384,
                "RS512" => &sig::RSA_PKCS1_SHA512,
                "PS256" => &sig::RSA_PSS_SHA256,
                "PS384" => &sig::RSA_PSS_SHA384,
                _ => &sig::RSA_PSS_SHA512,
            };
            let mut out = vec![0; key.public().modulus_len()];
            key.sign(padding, &ring::rand::SystemRandom::new(), message, &mut out).map_err(|_| "RSA signing failed".to_string())?;
            out
        }
        alg @ ("ES256" | "ES384") => {
            use ring::signature as sig;
            let curve = if alg == "ES256" { &sig::ECDSA_P256_SHA256_FIXED_SIGNING } else { &sig::ECDSA_P384_SHA384_FIXED_SIGNING };
            let rng = ring::rand::SystemRandom::new();
            let key = sig::EcdsaKeyPair::from_pkcs8(curve, &pem_der(&secret)?, &rng).map_err(|_| {
                "ES keys must be PKCS#8 (\"BEGIN PRIVATE KEY\"); convert with: openssl pkcs8 -topk8 -nocrypt -in key.pem".to_string()
            })?;
            key.sign(&rng, message).map_err(|_| "ECDSA signing failed".to_string())?.as_ref().to_vec()
        }
        other => return Err(format!("Unsupported JWT algorithm {other}")),
    };
    Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

/// RFC 3986 percent-encoding, as OAuth 1.0 requires.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// OAuth 1.0 `oauth_*` parameters including the signature, for `method url` with these form-body parameters.
pub fn oauth1_params(
    o: &OAuth1,
    vars: Vars,
    method: &str,
    url: &url::Url,
    form: &[(String, String)],
    nonce: &str,
    timestamp: i64,
) -> Vec<(String, String)> {
    let r = |s: &str| resolve(s.trim(), vars);
    let mut oauth = vec![
        ("oauth_consumer_key".to_string(), r(&o.consumer_key)),
        ("oauth_nonce".into(), nonce.into()),
        ("oauth_signature_method".into(), o.signature_method.clone()),
        ("oauth_timestamp".into(), timestamp.to_string()),
        ("oauth_version".into(), "1.0".into()),
    ];
    let token = r(&o.token);
    if !token.is_empty() {
        oauth.push(("oauth_token".into(), token));
    }
    let mut all: Vec<(String, String)> = oauth
        .iter()
        .cloned()
        .chain(url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())))
        .chain(form.iter().cloned())
        .map(|(k, v)| (encode(&k), encode(&v)))
        .collect();
    all.sort();
    let normalized = all.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let mut base_url = url.clone();
    base_url.set_query(None);
    base_url.set_fragment(None);
    let base = format!("{}&{}&{}", method.to_uppercase(), encode(base_url.as_str()), encode(&normalized));
    let key = format!("{}&{}", encode(&r(&o.consumer_secret)), encode(&r(&o.token_secret)));
    let signature = match o.signature_method.as_str() {
        "HMAC-SHA256" => STANDARD.encode(hmac!(sha2::Sha256, key.as_bytes(), base.as_bytes())),
        "HMAC-SHA512" => STANDARD.encode(hmac!(sha2::Sha512, key.as_bytes(), base.as_bytes())),
        "PLAINTEXT" => key,
        _ => STANDARD.encode(hmac!(sha1::Sha1, key.as_bytes(), base.as_bytes())),
    };
    oauth.push(("oauth_signature".into(), signature));
    oauth
}

pub fn oauth1_header(o: &OAuth1, vars: Vars, params: &[(String, String)]) -> String {
    let realm = resolve(o.realm.trim(), vars);
    let realm = (!realm.is_empty()).then(|| format!("realm=\"{}\"", encode(&realm)));
    let parts: Vec<String> = realm.into_iter().chain(params.iter().map(|(k, v)| format!("{k}=\"{}\"", encode(v)))).collect();
    format!("OAuth {}", parts.join(", "))
}

pub fn oauth1_nonce() -> String {
    random_token(32)
}

/// The Hawk Authorization header for a request (no payload hash).
pub fn hawk_header(h: &Hawk, vars: Vars, method: &str, url: &url::Url, nonce: &str, timestamp: i64) -> String {
    let (id, key, ext) = (resolve(h.id.trim(), vars), resolve(h.key.trim(), vars), resolve(&h.ext, vars));
    let resource = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_string(),
    };
    let normalized = format!(
        "hawk.1.header\n{timestamp}\n{nonce}\n{}\n{resource}\n{}\n{}\n\n{ext}\n",
        method.to_uppercase(),
        url.host_str().unwrap_or_default().to_ascii_lowercase(),
        url.port_or_known_default().unwrap_or(80)
    );
    let mac = if h.algorithm == "sha1" {
        hmac!(sha1::Sha1, key.as_bytes(), normalized.as_bytes())
    } else {
        hmac!(sha2::Sha256, key.as_bytes(), normalized.as_bytes())
    };
    let mut header = format!("Hawk id=\"{id}\", ts=\"{timestamp}\", nonce=\"{nonce}\", mac=\"{}\"", STANDARD.encode(mac));
    if !ext.is_empty() {
        header.push_str(&format!(", ext=\"{}\"", ext.replace('\\', "\\\\").replace('"', "\\\"")));
    }
    header
}

pub fn hawk_nonce() -> String {
    random_token(6)
}

const NTLM_FLAGS: u32 = 0x0000_0001 // unicode
    | 0x0000_0004 // request target
    | 0x0000_0200 // NTLM
    | 0x0000_8000 // always sign
    | 0x0008_0000 // extended session security
    | 0x2000_0000 // 128-bit
    | 0x8000_0000; // 56-bit

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// NTLM negotiate (type 1) message, Base64-encoded for the Authorization header.
pub fn ntlm_negotiate() -> String {
    let mut m = b"NTLMSSP\0".to_vec();
    m.extend(1_u32.to_le_bytes());
    m.extend(NTLM_FLAGS.to_le_bytes());
    m.extend([0_u8; 16]); // empty domain and workstation
    STANDARD.encode(m)
}

/// NTOWFv2: HMAC-MD5 keyed with the MD4 of the password, over the uppercased user and the domain.
fn ntowf_v2(user: &str, password: &str, domain: &str) -> Vec<u8> {
    use md4::Digest;
    let nt_hash = md4::Md4::digest(utf16(password));
    hmac!(md5::Md5, &nt_hash, &utf16(&format!("{}{domain}", user.to_uppercase())))
}

/// (NTLMv2 response, LMv2 response) for a server challenge and target info.
fn ntlm_v2_responses(key: &[u8], server: &[u8], client: &[u8; 8], time: u64, target_info: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut blob = vec![1, 1, 0, 0, 0, 0, 0, 0];
    blob.extend(time.to_le_bytes());
    blob.extend(client);
    blob.extend([0; 4]);
    blob.extend(target_info);
    blob.extend([0; 4]);
    let proof = hmac!(md5::Md5, key, &[server, &blob].concat());
    let lm = [hmac!(md5::Md5, key, &[server, client.as_slice()].concat()), client.to_vec()].concat();
    ([proof, blob].concat(), lm)
}

pub fn ntlm_authenticate(n: &Ntlm, vars: Vars, challenge_b64: &str) -> Result<String, String> {
    let challenge = STANDARD.decode(challenge_b64.trim()).map_err(|_| "Invalid NTLM challenge".to_string())?;
    if challenge.len() < 32 || !challenge.starts_with(b"NTLMSSP\0") || challenge[8] != 2 {
        return Err("Invalid NTLM challenge".into());
    }
    let u16_at = |i: usize| u16::from_le_bytes([challenge[i], challenge[i + 1]]) as usize;
    let server = &challenge[24..32];
    let flags = u32::from_le_bytes(challenge[20..24].try_into().unwrap_or_default());
    let target_info = if challenge.len() >= 48 {
        let (len, offset) = (u16_at(40), u32::from_le_bytes(challenge[44..48].try_into().unwrap_or_default()) as usize);
        challenge.get(offset..offset + len).unwrap_or_default().to_vec()
    } else {
        Vec::new()
    };
    let r = |s: &str| resolve(s.trim(), vars);
    let (user, mut domain, workstation) = (r(&n.username), r(&n.domain), r(&n.workstation));
    // "DOMAIN\user" in the username field is common.
    let user = match user.split_once('\\') {
        Some((d, u)) if domain.is_empty() => {
            domain = d.to_string();
            u.to_string()
        }
        _ => user,
    };
    let key = ntowf_v2(&user, &r(&n.password), &domain);
    let time = (chrono::Utc::now().timestamp() as u64 + 11_644_473_600) * 10_000_000;
    let (nt, lm) = ntlm_v2_responses(&key, server, &random_bytes::<8>(), time, &target_info);
    let fields = [lm, nt, utf16(&domain), utf16(&user), utf16(&workstation), Vec::new()];
    let mut header = b"NTLMSSP\0".to_vec();
    header.extend(3_u32.to_le_bytes());
    let mut payload = Vec::new();
    let base = 64;
    for field in &fields {
        let len = field.len() as u16;
        header.extend(len.to_le_bytes());
        header.extend(len.to_le_bytes());
        header.extend(((base + payload.len()) as u32).to_le_bytes());
        payload.extend(field);
    }
    header.extend((flags & NTLM_FLAGS | 0x1).to_le_bytes());
    Ok(STANDARD.encode([header, payload].concat()))
}

/// An authorization code flow waiting for the browser to come back to Floki.
pub struct CodeFlow {
    pub url: String,
    listener: TcpListener,
    verifier: Option<String>,
    state: String,
    redirect: String,
}

/// Starts the authorization code flow: the caller opens `url` in the browser, then calls `finish_code_flow`.
pub fn start_code_flow(o: &OAuth2, vars: Vars) -> Result<CodeFlow, String> {
    let r = |s: &str| resolve(s.trim(), vars);
    let auth_url = r(&o.auth_url);
    if auth_url.is_empty() {
        return Err("Enter the Auth URL".into());
    }
    let callback = r(&o.callback_url);
    let (listener, redirect) = if callback.is_empty() {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        (listener, format!("http://127.0.0.1:{port}/callback"))
    } else {
        let url = url::Url::parse(&callback).map_err(|e| format!("Invalid Callback URL: {e}"))?;
        let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if url.scheme() != "http" || !local {
            return Err("The Callback URL must be http://localhost:PORT/... so Floki can receive the code; leave it empty to pick a free port".into());
        }
        let port = url.port().unwrap_or(80);
        let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("Could not listen on port {port}: {e}"))?;
        (listener, callback)
    };
    let state = random_token(24);
    let verifier = (o.grant_type == "authorization_code_pkce").then(|| random_token(64));
    let mut url = url::Url::parse(&auth_url).map_err(|e| format!("Invalid Auth URL: {e}"))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &r(&o.client_id))
            .append_pair("redirect_uri", &redirect)
            .append_pair("state", &state);
        let scope = r(&o.scope);
        if !scope.is_empty() {
            q.append_pair("scope", &scope);
        }
        if let Some(v) = &verifier {
            use sha2::Digest;
            q.append_pair("code_challenge", &URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(v.as_bytes())))
                .append_pair("code_challenge_method", "S256");
        }
    }
    Ok(CodeFlow { url: url.into(), listener, verifier, state, redirect })
}

/// Waits up to five minutes for the browser redirect, then trades the code for an access token.
pub fn finish_code_flow(flow: CodeFlow, o: &OAuth2, vars: Vars, settings: &crate::model::Settings) -> Result<String, String> {
    flow.listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let (mut stream, _) = loop {
        match flow.listener.accept() {
            Ok(conn) => break conn,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => return Err("Timed out waiting for the browser to sign in".into()),
        }
    };
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
    let mut buf = [0_u8; 8192];
    let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let target = head.split_whitespace().nth(1).unwrap_or("/");
    let query = url::Url::parse(&format!("http://localhost{target}")).map_err(|e| e.to_string())?;
    let param = |k: &str| query.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned());
    let page = |msg: &str| {
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><title>Floki</title><body style=\"font-family:sans-serif;padding:40px\"><h2>{msg}</h2><p>You can close this tab and return to Floki.</p></body>")
    };
    if param("state").as_deref() != Some(flow.state.as_str()) {
        let _ = stream.write_all(page("Authorization failed").as_bytes());
        return Err("Authorization failed: the state did not match".into());
    }
    let Some(code) = param("code") else {
        let _ = stream.write_all(page("Authorization was cancelled").as_bytes());
        let reason = [param("error"), param("error_description")].into_iter().flatten().collect::<Vec<_>>().join(": ");
        return Err(format!("Authorization cancelled: {reason}"));
    };
    let mut form = vec![("grant_type", "authorization_code".to_string()), ("code", code), ("redirect_uri", flow.redirect)];
    if let Some(v) = flow.verifier {
        form.push(("code_verifier", v));
    }
    let result = crate::http::request_token(o, vars, settings, form);
    let _ = stream.write_all(page(if result.is_ok() { "Access token received" } else { "Getting the token failed" }).as_bytes());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth1_matches_the_twitter_example() {
        let o = OAuth1 {
            consumer_key: "xvz1evFS4wEEPTGEFPHBog".into(),
            consumer_secret: "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw".into(),
            token: "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb".into(),
            token_secret: "LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE".into(),
            ..OAuth1::default()
        };
        let url = url::Url::parse("https://api.twitter.com/1.1/statuses/update.json?include_entities=true").unwrap();
        let form = [("status".to_string(), "Hello Ladies + Gentlemen, a signed OAuth request!".to_string())];
        let params = oauth1_params(&o, &[], "post", &url, &form, "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg", 1318622958);
        assert_eq!(params.last().unwrap().1, "hCtSmYh+iHYCEqBWrE7C7hYmtUk=");
        let header = oauth1_header(&OAuth1 { realm: "x".into(), ..o }, &[], &params);
        assert!(header.starts_with("OAuth realm=\"x\", oauth_consumer_key=\"xvz1evFS4wEEPTGEFPHBog\""));
        assert!(header.contains("oauth_signature=\"hCtSmYh%2BiHYCEqBWrE7C7hYmtUk%3D\""));
    }

    #[test]
    fn hawk_matches_the_spec_example() {
        let h = Hawk {
            id: "dh37fgj492je".into(),
            key: "werxhqb98rpaxn39848xrunpaw3489ruxnpa98w4rxn".into(),
            algorithm: "sha256".into(),
            ext: "some-app-ext-data".into(),
        };
        let url = url::Url::parse("http://example.com:8000/resource/1?b=1&a=2").unwrap();
        let header = hawk_header(&h, &[], "GET", &url, "j4h3g2", 1353832234);
        assert_eq!(
            header,
            r#"Hawk id="dh37fgj492je", ts="1353832234", nonce="j4h3g2", mac="6R4rV5iE+NPoym+WwjeHzjAGXUtLNIxmo1vpMofpLAE=", ext="some-app-ext-data""#
        );
    }

    #[test]
    fn ntlm_v2_matches_ms_nlmp_test_vectors() {
        let key = ntowf_v2("User", "Password", "Domain");
        assert_eq!(hex::encode(&key), "0c868a403bfd7a93a3001ef22ef02e3f");
        let target_info = hex::decode("02000c0044006f006d00610069006e0001000c0053006500720076006500720000000000").unwrap();
        let (nt, lm) = ntlm_v2_responses(&key, &hex::decode("0123456789abcdef").unwrap(), &[0xaa; 8], 0, &target_info);
        assert_eq!(hex::encode(&nt[..16]), "68cd0ab851e51c96aabc927bebef6a1c");
        assert_eq!(hex::encode(&lm), "86c35097ac9cec102554764a57cccc19aaaaaaaaaaaaaaaa");
        let message = STANDARD.decode(ntlm_negotiate()).unwrap();
        assert_eq!(&message[..9], b"NTLMSSP\0\x01");
    }

    #[test]
    fn jwt_hs256_and_es256() {
        let j = Jwt { secret: "{{s}}".into(), payload: r#"{"sub": "{{user}}"}"#.into(), ..Jwt::default() };
        let vars = [("s".to_string(), "your-256-bit-secret".to_string()), ("user".to_string(), "1234567890".to_string())];
        let token = jwt(&j, &vars).unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!((header["alg"].as_str(), header["typ"].as_str()), (Some("HS256"), Some("JWT")));
        let expected = hmac!(sha2::Sha256, b"your-256-bit-secret", format!("{}.{}", parts[0], parts[1]).as_bytes());
        assert_eq!(URL_SAFE_NO_PAD.decode(parts[2]).unwrap(), expected);
        assert!(jwt(&Jwt { payload: "[1]".into(), ..Jwt::default() }, &[]).is_err());

        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pem = format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----", STANDARD.encode(pkcs8.as_ref()));
        let token = jwt(&Jwt { algorithm: "ES256".into(), secret: pem, ..Jwt::default() }, &[]).unwrap();
        let parts: Vec<&str> = token.rsplitn(2, '.').collect();
        let key = ring::signature::EcdsaKeyPair::from_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        use ring::signature::KeyPair;
        let public = ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, key.public_key().as_ref());
        assert!(public.verify(parts[1].as_bytes(), &URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).is_ok());
    }
}

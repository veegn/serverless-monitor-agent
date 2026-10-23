use crate::dns::resolve_host;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn get_header(&self, key: &str) -> Option<&str> {
        let key_lower = key.to_ascii_lowercase();
        self.headers.get(&key_lower).map(|s| s.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

pub fn get(
    url_str: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
    use_public_dns: bool,
) -> Result<HttpResponse, String> {
    request_with_redirects("GET", url_str, headers, None, timeout, use_public_dns, 5)
}

pub fn post(
    url_str: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
    use_public_dns: bool,
) -> Result<HttpResponse, String> {
    request_with_redirects("POST", url_str, headers, Some(body), timeout, use_public_dns, 0)
}

pub fn download_to_file<P: AsRef<Path>>(
    url_str: &str,
    headers: &[(&str, &str)],
    dest: P,
    max_bytes: u64,
    timeout: Duration,
    use_public_dns: bool,
) -> Result<(), String> {
    let resp = request_with_redirects("GET", url_str, headers, None, timeout, use_public_dns, 5)?;
    if resp.status < 200 || resp.status >= 300 {
        return Err(format!("download {} returned http {}", url_str, resp.status));
    }
    if resp.body.len() as u64 > max_bytes || resp.body.is_empty() {
        return Err(format!(
            "download {} has invalid size {} (limit {})",
            url_str,
            resp.body.len(),
            max_bytes
        ));
    }
    let dest_path = dest.as_ref();
    if let Some(parent) = dest_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = dest_path.with_extension("download");
    let mut file = File::create(&tmp).map_err(|e| e.to_string())?;
    file.write_all(&resp.body).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
    }
    std::fs::rename(&tmp, dest_path).map_err(|e| e.to_string())
}

fn request_with_redirects(
    method: &str,
    current_url: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout: Duration,
    use_public_dns: bool,
    max_redirects: usize,
) -> Result<HttpResponse, String> {
    let mut redirect_count = 0;
    let mut owned_url = current_url.to_string();

    loop {
        let resp = do_single_http_request(method, &owned_url, headers, body, timeout, use_public_dns)?;
        if (resp.status == 301 || resp.status == 302 || resp.status == 303 || resp.status == 307 || resp.status == 308)
            && redirect_count < max_redirects
        {
            if let Some(loc) = resp.get_header("location") {
                let next_url = if loc.starts_with("http://") || loc.starts_with("https://") {
                    loc.to_string()
                } else {
                    // relative URL
                    let parsed = url::Url::parse(&owned_url).map_err(|e| e.to_string())?;
                    parsed.join(loc).map_err(|e| e.to_string())?.to_string()
                };
                owned_url = next_url;
                redirect_count += 1;
                continue;
            }
        }
        return Ok(resp);
    }
}

static TLS_CONFIG: std::sync::LazyLock<Arc<ClientConfig>> = std::sync::LazyLock::new(|| {
    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    Arc::new(config)
});

pub fn create_tls_config() -> Arc<ClientConfig> {
    TLS_CONFIG.clone()
}

enum HttpStream {
    Plain(TcpStream),
    Tls(StreamOwned<ClientConnection, TcpStream>),
}

impl Read for HttpStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            HttpStream::Plain(s) => s.read(buf),
            HttpStream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for HttpStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            HttpStream::Plain(s) => s.write(buf),
            HttpStream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            HttpStream::Plain(s) => s.flush(),
            HttpStream::Tls(s) => s.flush(),
        }
    }
}

fn do_single_http_request(
    method: &str,
    url_str: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout: Duration,
    use_public_dns: bool,
) -> Result<HttpResponse, String> {
    let parsed_url = url::Url::parse(url_str).map_err(|e| e.to_string())?;
    let scheme = parsed_url.scheme();
    let is_tls = match scheme {
        "https" => true,
        "http" => false,
        _ => return Err(format!("unsupported URL scheme: {}", scheme)),
    };

    let host = parsed_url.host_str().ok_or("missing host")?;
    let port = parsed_url.port().unwrap_or(if is_tls { 443 } else { 80 });

    let ips = resolve_host(host, use_public_dns)?;
    let mut tcp_stream = None;
    let mut last_err = String::new();

    for ip in ips {
        let addr = SocketAddr::new(ip, port);
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => {
                let _ = s.set_read_timeout(Some(timeout));
                let _ = s.set_write_timeout(Some(timeout));
                let _ = s.set_nodelay(true);
                tcp_stream = Some(s);
                break;
            }
            Err(e) => {
                last_err = format!("failed to connect to {}: {}", addr, e);
            }
        }
    }

    let tcp = tcp_stream.ok_or_else(|| {
        if last_err.is_empty() {
            format!("failed to connect to host: {}", host)
        } else {
            last_err
        }
    })?;

    let mut stream = if is_tls {
        let tls_config = create_tls_config();
        let server_name = ServerName::try_from(host.to_string())
            .map_err(|e| format!("invalid TLS server name {}: {}", host, e))?;
        let conn = ClientConnection::new(tls_config, server_name)
            .map_err(|e| format!("TLS client error: {}", e))?;
        HttpStream::Tls(StreamOwned::new(conn, tcp))
    } else {
        HttpStream::Plain(tcp)
    };

    // Format HTTP request
    let path_and_query = match parsed_url.query() {
        Some(q) => format!("{}?{}", parsed_url.path(), q),
        None => parsed_url.path().to_string(),
    };
    let path_str = if path_and_query.is_empty() {
        "/"
    } else {
        &path_and_query
    };

    let host_header = if (is_tls && port == 443) || (!is_tls && port == 80) {
        host.to_string()
    } else {
        format!("{}:{}", host, port)
    };

    let mut req_str = format!("{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n", method, path_str, host_header);
    let mut has_content_length = false;

    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") {
            has_content_length = true;
        }
        req_str.push_str(&format!("{}: {}\r\n", k, v));
    }

    if let Some(b) = body {
        if !has_content_length {
            req_str.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
    } else if method == "POST" && !has_content_length {
        req_str.push_str("Content-Length: 0\r\n");
    }

    req_str.push_str("\r\n");

    stream.write_all(req_str.as_bytes()).map_err(|e| e.to_string())?;
    if let Some(b) = body {
        stream.write_all(b).map_err(|e| e.to_string())?;
    }
    stream.flush().map_err(|e| e.to_string())?;

    // Read response
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).map_err(|e| e.to_string())?;

    let parts: Vec<&str> = status_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
        return Err(format!("invalid HTTP status line: {:?}", status_line));
    }
    let status: u16 = parts[1].parse().map_err(|e| format!("invalid status code: {}", e))?;

    let mut resp_headers = HashMap::new();
    let mut is_chunked = false;
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if bytes_read == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key_lower = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            if key_lower == "transfer-encoding" && val.to_ascii_lowercase().contains("chunked") {
                is_chunked = true;
            }
            if key_lower == "content-length" {
                if let Ok(cl) = val.parse::<usize>() {
                    content_length = Some(cl);
                }
            }
            resp_headers.insert(key_lower, val);
        }
    }

    let mut body_bytes = Vec::new();
    if is_chunked {
        loop {
            let mut size_line = String::new();
            reader.read_line(&mut size_line).map_err(|e| e.to_string())?;
            let size_str = size_line.trim().split(';').next().unwrap_or("").trim();
            let chunk_size = usize::from_str_radix(size_str, 16).map_err(|_| "invalid chunk size")?;
            if chunk_size == 0 {
                // skip trailer headers until blank line
                loop {
                    let mut trailer = String::new();
                    reader.read_line(&mut trailer).map_err(|e| e.to_string())?;
                    if trailer == "\r\n" || trailer == "\n" || trailer.is_empty() {
                        break;
                    }
                }
                break;
            }
            let mut chunk_buf = vec![0u8; chunk_size];
            reader.read_exact(&mut chunk_buf).map_err(|e| e.to_string())?;
            body_bytes.extend_from_slice(&chunk_buf);
            let mut crlf = [0u8; 2];
            let _ = reader.read_exact(&mut crlf);
        }
    } else if let Some(cl) = content_length {
        body_bytes.resize(cl, 0);
        reader.read_exact(&mut body_bytes).map_err(|e| e.to_string())?;
    } else {
        reader.read_to_end(&mut body_bytes).map_err(|e| e.to_string())?;
    }

    Ok(HttpResponse {
        status,
        headers: resp_headers,
        body: body_bytes,
    })
}

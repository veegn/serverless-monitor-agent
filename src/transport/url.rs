//! WebSocket URL transformations.

pub fn report_websocket_url(raw: &str) -> Result<String, String> {
    let mut u = url::Url::parse(raw.trim()).map_err(|e| e.to_string())?;
    let host = u.host_str().ok_or_else(|| format!("missing host in {:?}", raw))?;
    if host.is_empty() {
        return Err(format!("missing host in {:?}", raw));
    }
    match u.scheme().to_ascii_lowercase().as_str() {
        "https" => {
            u.set_scheme("wss").map_err(|_| "failed to set wss scheme")?;
        }
        "http" => {
            u.set_scheme("ws").map_err(|_| "failed to set ws scheme")?;
        }
        "wss" | "ws" => {}
        other => return Err(format!("unsupported scheme {:?}", other)),
    }
    Ok(u.to_string())
}

pub fn report_websocket_url_with_config(raw: &str, schema: &str, md5: &str) -> Result<String, String> {
    let ws_url = report_websocket_url(raw)?;
    let mut u = url::Url::parse(&ws_url).map_err(|e| e.to_string())?;
    {
        let mut pairs = u.query_pairs_mut();
        if !schema.trim().is_empty() {
            pairs.append_pair("config_schema", schema.trim());
        }
        if !md5.trim().is_empty() {
            pairs.append_pair("config_md5", md5.trim());
        }
    }
    Ok(u.to_string())
}

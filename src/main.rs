use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use mongodb::{Client, bson::doc};
use regex::Regex;
use serde_json::{Map, Value, json};
use warp::Filter;
use warp::reply::Reply;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Config {
    ollama_url: String,
    upstream_api_key: String,
    testing: bool,
    responses_path_re: Regex,
    web_search_mode: String,
    fill_tool_strict: bool,
    compat_log: bool,
}

#[derive(Debug, Clone)]
struct Alias {
    namespace: String,
    name: String,
}

#[derive(Debug, Default)]
struct CompatLog {
    flattened: Vec<String>,
    stubbed: Vec<String>,
    restored: usize,
    dropped: Vec<String>,
}

#[derive(Debug)]
struct CompatResult {
    body: Value,
    alias: HashMap<String, Alias>,
    log: CompatLog,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_obj(v: &Value) -> bool {
    v.is_object()
}

fn join_tool_name(ns: &str, name: &str) -> String {
    if ns.is_empty() {
        return name.to_string();
    }
    let trimmed_ns = ns.trim_end_matches('_');
    let trimmed_name = name.trim_start_matches('_');
    format!("{}__{}", trimmed_ns, trimmed_name)
}

fn remove_trailing_slash(s: &str) -> String {
    if s.ends_with('/') {
        s[..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Compat: request rewrite
// ---------------------------------------------------------------------------

fn web_search_stub() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("name".into(), json!("web_search"));
    m.insert("description".into(), json!("Search the web"));
    m.insert("strict".into(), json!(true));
    m.insert(
        "parameters".into(),
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" }
            },
            "required": ["query"]
        }),
    );
    m
}

fn flatten_tools(
    tools: &[Value],
    alias: &mut HashMap<String, Alias>,
    log: &mut CompatLog,
    config: &Config,
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let strip: HashSet<&str> = ["type", "tools", "namespace", "defer_loading", "function"]
        .iter()
        .cloned()
        .collect();

    fn emit(
        wire: &str,
        fn_obj: &Map<String, Value>,
        _why: &str,
        out: &mut Vec<Value>,
        seen: &mut HashSet<String>,
        log: &mut CompatLog,
    ) {
        if seen.contains(wire) {
            log.dropped
                .push(format!("{} \"{}\" (name collision)", _why, wire));
            return;
        }
        seen.insert(wire.to_string());
        let mut entry = fn_obj.clone();
        entry.insert("type".into(), json!("function"));
        out.push(Value::Object(entry));
    }

    fn walk(
        tool: &Value,
        ns: Option<&str>,
        depth: usize,
        out: &mut Vec<Value>,
        seen: &mut HashSet<String>,
        alias: &mut HashMap<String, Alias>,
        log: &mut CompatLog,
        strip: &HashSet<&str>,
        config: &Config,
    ) {
        if !is_obj(tool) || depth > 8 {
            return;
        }

        let obj = tool.as_object().unwrap();
        let tool_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // 1. namespace envelope -> recurse children
        if tool_type == "namespace" {
            let ns_name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(ns.unwrap_or(""))
                .to_string();
            let children: Vec<&Value> = obj
                .get("tools")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().collect())
                .unwrap_or_default();
            for child in &children {
                walk(
                    child,
                    Some(&ns_name),
                    depth + 1,
                    out,
                    seen,
                    alias,
                    log,
                    strip,
                    config,
                );
            }
            log.flattened.push(format!(
                "namespace \"{}\" -> {} child tool(s)",
                ns_name,
                children.len()
            ));
            return;
        }

        // 2. web_search -> synthesize stub
        if tool_type == "web_search" {
            if config.web_search_mode == "drop" {
                log.dropped.push("web_search (drop)".into());
                return;
            }
            if !seen.contains("web_search") {
                let stub = web_search_stub();
                emit("web_search", &stub, "web_search", out, seen, log);
                log.stubbed.push("web_search".into());
            }
            return;
        }

        // 3. plain function tool
        let has_name = obj.get("name").and_then(|v| v.as_str()).is_some();
        let has_params = obj.contains_key("parameters");
        let has_fn = obj.get("function").map(is_obj).unwrap_or(false);

        if tool_type == "function" || (has_name && (has_params || has_fn)) || has_fn {
            let inner = obj.get("function").filter(|v| is_obj(v));
            let inner = inner.and_then(|v| v.as_object());

            // Drill down double-nesting
            let inner = if let Some(i) = inner {
                if i.get("function").map(|v| is_obj(v)).unwrap_or(false) {
                    i.get("function").and_then(|v| v.as_object())
                } else {
                    Some(i)
                }
            } else {
                None
            };

            let mut fn_obj: Map<String, Value> = inner.cloned().unwrap_or_default();

            // Copy non-stripped keys from the outer tool
            for (k, v) in obj {
                if !strip.contains(k.as_str()) {
                    fn_obj.insert(k.clone(), v.clone());
                }
            }

            // Hard guarantee against double-nesting
            fn_obj.remove("function");
            fn_obj.remove("type");

            let raw = fn_obj
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if raw.is_empty() {
                log.dropped.push("function tool with no name".into());
                return;
            }

            let wire = match ns {
                Some(prefix) if !prefix.is_empty() => join_tool_name(prefix, &raw),
                _ => raw.clone(),
            };

            fn_obj.insert("name".into(), json!(wire));

            if config.fill_tool_strict && !fn_obj.contains_key("strict") {
                fn_obj.insert("strict".into(), json!(true));
            }

            emit(&wire, &fn_obj, "function", out, seen, log);

            if let Some(prefix) = ns {
                if !prefix.is_empty() {
                    alias.insert(
                        wire.clone(),
                        Alias {
                            namespace: prefix.to_string(),
                            name: raw,
                        },
                    );
                }
            }
            return;
        }

        // 4. everything else
        if !tool_type.is_empty() {
            log.dropped.push(format!("type \"{}\"", tool_type));
        }
    }

    for tool in tools {
        walk(
            tool, None, 0, &mut out, &mut seen, alias, log, &strip, config,
        );
    }
    out
}

fn reverse_index(
    alias: &HashMap<String, Alias>,
) -> (HashMap<String, String>, HashMap<String, Option<String>>) {
    let mut by_ns_name: HashMap<String, String> = HashMap::new();
    let mut by_name: HashMap<String, Option<String>> = HashMap::new();

    for (wire, entry) in alias {
        let key = format!("{}\x00{}", entry.namespace, entry.name);
        by_ns_name.insert(key, wire.clone());

        let existing = by_name.get(&entry.name);
        match existing {
            None => {
                by_name.insert(entry.name.clone(), Some(wire.clone()));
            }
            Some(Some(_)) => {
                by_name.insert(entry.name.clone(), None); // collision -> ambiguous
            }
            Some(None) => {}
        }
    }
    (by_ns_name, by_name)
}

fn rewrite_input(node: &mut Value, alias: &HashMap<String, Alias>, depth: usize) {
    if depth > 12 || alias.is_empty() {
        return;
    }

    if let Some(arr) = node.as_array_mut() {
        for item in arr.iter_mut() {
            rewrite_input(item, alias, depth + 1);
        }
        return;
    }

    if !is_obj(node) {
        return;
    }

    let obj = node.as_object_mut().unwrap();

    let type_str = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let name_str = obj.get("name").and_then(|v| v.as_str());

    if type_str == "function_call" && name_str.is_some() {
        let name = name_str.unwrap().to_string();
        let (by_ns_name, by_name) = reverse_index(alias);

        let ns_raw = obj.get("namespace").and_then(|v| v.as_str());
        let ns = ns_raw.filter(|s| !s.is_empty() && *s != "functions");

        let joined = if let Some(prefix) = ns {
            join_tool_name(prefix, &name)
        } else {
            name.clone()
        };

        let wire: Option<String> = if let Some(prefix) = ns {
            let key = format!("{}\x00{}", prefix, name);
            by_ns_name.get(&key).cloned().or_else(|| {
                if alias.contains_key(&joined) {
                    Some(joined.clone())
                } else {
                    None
                }
            })
        } else {
            by_name.get(&name).cloned().flatten().or_else(|| {
                if alias.contains_key(&name) {
                    Some(name.clone())
                } else {
                    None
                }
            })
        };

        if let Some(w) = wire {
            obj.insert("name".into(), json!(w));
            obj.remove("namespace");
        }
        return;
    }

    // Recurse into all values
    let keys: Vec<String> = obj.keys().cloned().collect();
    for k in keys {
        if let Some(v) = obj.get_mut(&k) {
            rewrite_input(v, alias, depth + 1);
        }
    }
}

fn remap_tool_choice(
    choice: &Value,
    alias: &HashMap<String, Alias>,
    _log: &mut CompatLog,
) -> Value {
    if alias.is_empty() || choice.is_null() {
        return choice.clone();
    }

    // String choice
    if let Some(s) = choice.as_str() {
        if s == "auto" || s == "none" || s == "required" {
            return choice.clone();
        }
        let (_, by_name) = reverse_index(alias);
        let wire = by_name.get(s).cloned().flatten().or_else(|| {
            if alias.contains_key(s) {
                Some(s.to_string())
            } else {
                None
            }
        });

        if let Some(w) = wire {
            return json!(w);
        }
        return choice.clone();
    }

    if !is_obj(choice) {
        return choice.clone();
    }

    let obj = choice.as_object().unwrap();
    let tool_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let inner = obj
        .get("function")
        .filter(|v| is_obj(v))
        .and_then(|v| v.as_object());
    let raw = obj
        .get("name")
        .and_then(|v| v.as_str())
        .or_else(|| inner.and_then(|i| i.get("name")).and_then(|v| v.as_str()))
        .unwrap_or("");

    if tool_type == "function" && !raw.is_empty() {
        let (by_ns_name, by_name) = reverse_index(alias);
        let ns = obj.get("namespace").and_then(|v| v.as_str());

        let wire: Option<String> = ns
            .and_then(|prefix| {
                let key = format!("{}\x00{}", prefix, raw);
                by_ns_name.get(&key).cloned()
            })
            .or_else(|| {
                if alias.contains_key(raw) {
                    Some(raw.to_string())
                } else {
                    None
                }
            })
            .or_else(|| by_name.get(raw).cloned().flatten());

        if let Some(w) = wire {
            let mut out = obj.clone();
            out.insert("type".into(), json!("function"));
            out.insert("name".into(), json!(&w));
            out.remove("namespace");
            if let Some(inner_obj) = inner {
                let mut fi = inner_obj.clone();
                fi.insert("name".into(), json!(&w));
                out.insert("function".into(), Value::Object(fi));
            }
            return Value::Object(out);
        }
        return choice.clone();
    }

    choice.clone()
}

fn hoist_system_messages(body: &mut Value) {
    let target_key = if body.get("input").and_then(|v| v.as_array()).is_some() {
        "input"
    } else if body.get("messages").and_then(|v| v.as_array()).is_some() {
        "messages"
    } else {
        return;
    };

    let target = body
        .as_object_mut()
        .and_then(|o| o.get_mut(target_key))
        .and_then(|v| v.as_array_mut());

    let target = match target {
        Some(t) => t,
        None => return,
    };

    let mut system_msgs = Vec::new();
    let mut other_msgs = Vec::new();

    for msg in target.iter() {
        let role = msg.get("role").and_then(|v| v.as_str());
        if role == Some("system") || role == Some("developer") {
            system_msgs.push(msg.clone());
        } else {
            other_msgs.push(msg.clone());
        }
    }

    if !system_msgs.is_empty() {
        target.clear();
        target.extend(system_msgs);
        target.extend(other_msgs);
    }
}

fn compat_request(body: &Value, config: &Config) -> CompatResult {
    let mut alias = HashMap::new();
    let mut log = CompatLog::default();
    let mut out = body.as_object().cloned().unwrap_or_default();

    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        if !tools.is_empty() {
            let flat = flatten_tools(tools, &mut alias, &mut log, config);
            if !flat.is_empty() {
                out.insert("tools".into(), Value::Array(flat));
            } else {
                out.remove("tools");
            }
        }
    }

    if !alias.is_empty() {
        if let Some(input) = out.get_mut("input") {
            rewrite_input(input, &alias, 0);
        }
    }

    if out.contains_key("tool_choice") {
        let tc = out["tool_choice"].clone();
        let remapped = remap_tool_choice(&tc, &alias, &mut log);
        out.insert("tool_choice".into(), remapped);
    }

    // Hoist system messages
    let mut out_val = Value::Object(out);
    hoist_system_messages(&mut out_val);

    CompatResult {
        body: out_val,
        alias,
        log,
    }
}

// ---------------------------------------------------------------------------
// Compat: response restore
// ---------------------------------------------------------------------------

fn restore_function_call_names(
    node: &mut Value,
    alias: &HashMap<String, Alias>,
    depth: usize,
) -> usize {
    if alias.is_empty() || depth > 12 {
        return 0;
    }

    if let Some(arr) = node.as_array_mut() {
        let mut n = 0;
        for item in arr.iter_mut() {
            n += restore_function_call_names(item, alias, depth + 1);
        }
        return n;
    }

    if !is_obj(node) {
        return 0;
    }

    let obj = node.as_object_mut().unwrap();
    let mut n = 0;

    let tool_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let call_like = tool_type == "function_call"
        || tool_type.starts_with("response.function_call")
        || obj.contains_key("arguments")
        || obj.contains_key("call_id")
        || obj.contains_key("item_id");

    let mut restored_name: Option<String> = None;

    if call_like {
        if let Some(name) = obj
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        {
            if let Some(hit) = alias.get(&name) {
                obj.insert("name".into(), json!(hit.name.clone()));
                obj.insert("namespace".into(), json!(hit.namespace.clone()));
                restored_name = Some(hit.name.clone());
                n += 1;
            }
        }
    }

    if let Some(fn_obj) = obj.get("function").filter(|v| is_obj(v)).cloned() {
        if let Some(fn_name) = fn_obj.get("name").and_then(|v| v.as_str()) {
            if let Some(hit) = alias.get(fn_name) {
                if restored_name.as_deref() != Some(hit.name.as_str()) {
                    let mut new_fn = fn_obj.as_object().unwrap().clone();
                    new_fn.insert("name".into(), json!(hit.name));
                    obj.insert("function".into(), Value::Object(new_fn));
                    obj.insert("namespace".into(), json!(hit.namespace));
                    n += 1;
                }
            }
        }
    }

    // Recurse into all values
    let keys: Vec<String> = obj.keys().cloned().collect();
    for k in keys {
        if let Some(v) = obj.get_mut(&k) {
            n += restore_function_call_names(v, alias, depth + 1);
        }
    }
    n
}

fn patch_usage_deep(node: &mut Value, depth: usize) {
    if depth > 12 {
        return;
    }

    if let Some(arr) = node.as_array_mut() {
        for item in arr.iter_mut() {
            patch_usage_deep(item, depth + 1);
        }
        return;
    }

    if !is_obj(node) {
        return;
    }

    let obj = node.as_object_mut().unwrap();

    let looks_like_usage = (obj.contains_key("input_tokens") || obj.contains_key("prompt_tokens"))
        && (obj.contains_key("output_tokens") || obj.contains_key("completion_tokens"));

    if looks_like_usage
        && obj
            .get("output_tokens_details")
            .map(|v| v.is_null())
            .unwrap_or(true)
    {
        let legacy = obj.get("output_token_details").filter(|v| is_obj(v));
        let r = legacy
            .and_then(|v| v.get("reasoning_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        obj.insert(
            "output_tokens_details".into(),
            json!({ "reasoning_tokens": r }),
        );
    }

    let keys: Vec<String> = obj.keys().cloned().collect();
    for k in keys {
        if let Some(v) = obj.get_mut(&k) {
            patch_usage_deep(v, depth + 1);
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming SSE transform
// ---------------------------------------------------------------------------

fn transform_sse_line(
    line: &str,
    alias: &HashMap<String, Alias>,
    log: &mut CompatLog,
) -> Option<String> {
    let is_data = line.starts_with("data:") && !line.starts_with("data: [DONE]");
    if !is_data {
        return Some(format!("{}\n", line));
    }

    let payload = if line.starts_with("data: ") {
        &line[6..]
    } else {
        &line[5..]
    };

    match serde_json::from_str::<Value>(payload) {
        Ok(mut data) => {
            patch_usage_deep(&mut data, 0);
            log.restored += restore_function_call_names(&mut data, alias, 0);
            Some(format!("data: {}\n", serde_json::to_string(&data).unwrap()))
        }
        Err(_) => Some(format!("{}\n", line)),
    }
}

// ---------------------------------------------------------------------------
// Proxy handler
// ---------------------------------------------------------------------------

async fn proxy_handler(
    method: warp::hyper::Method,
    path: String,
    query: String,
    headers: warp::http::HeaderMap,
    body: Vec<u8>,
    config: Arc<Config>,
    client: reqwest::Client,
) -> Result<warp::reply::Response, warp::Rejection> {
    let is_get_like = method == warp::hyper::Method::GET || method == warp::hyper::Method::HEAD;

    // 1. Body compat rewrite
    let upstream_body: Option<Vec<u8>>;
    let mut alias = HashMap::new();
    let mut log: Option<CompatLog> = None;

    if is_get_like {
        upstream_body = None;
    } else if config.responses_path_re.is_match(&path) && !body.is_empty() {
        match serde_json::from_slice::<Value>(&body) {
            Ok(parsed) if parsed.get("tools").and_then(|v| v.as_array()).is_some() => {
                let compat = compat_request(&parsed, &config);
                alias = compat.alias;
                log = Some(compat.log);

                if config.compat_log {
                    let l = log.as_ref().unwrap();
                    if !l.flattened.is_empty() || !l.stubbed.is_empty() || !l.dropped.is_empty() {
                        tracing::info!(
                            "[tool-compat] flattened=[{}] stubbed=[{}] dropped=[{}]",
                            l.flattened.join("; "),
                            l.stubbed.join(","),
                            l.dropped.join(", ")
                        );
                    }
                }

                upstream_body = Some(serde_json::to_vec(&compat.body).unwrap());
            }
            _ => {
                upstream_body = Some(body.clone());
            }
        }
    } else {
        upstream_body = Some(body.clone());
    }

    // 2. Rebuild request headers
    let hop_by_hop: HashSet<&str> = [
        "host",
        "content-length",
        "content-encoding",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "upgrade",
        "te",
        "trailer",
        "proxy-authorization",
        "accept-encoding",
    ]
    .iter()
    .cloned()
    .collect();

    let mut req_headers = warp::http::HeaderMap::new();
    for (k, v) in &headers {
        let key_lower = k.as_str().to_lowercase();
        if !hop_by_hop.contains(key_lower.as_str()) {
            req_headers.insert(k.clone(), v.clone());
        }
    }

    if !config.upstream_api_key.is_empty() {
        let value = warp::http::HeaderValue::from_str(&format!("Bearer {}", config.upstream_api_key))
            .unwrap_or_else(|_| warp::http::HeaderValue::from_static("Bearer invalid-header-value"));
        req_headers.insert(warp::http::header::AUTHORIZATION, value);
    }
    req_headers.insert(
        warp::http::header::ACCEPT_ENCODING,
        warp::http::HeaderValue::from_static("identity"),
    );

    // Forward request
    let url = format!(
        "{}{}{}",
        config.ollama_url,
        path,
        if query.is_empty() { "" } else { &query }
    );

    tracing::debug!("upstream url: {}", url);

    let mut req = client
        .request(
            warp::hyper::Method::from_bytes(method.as_str().as_bytes()).unwrap(),
            &url,
        )
        .headers(req_headers);

    if let Some(b) = upstream_body {
        req = req.body(b);
    }

    let upstream_resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Error forwarding request: {}", e);
            let resp = warp::reply::with_status(
                warp::reply::json(&json!({ "message": "Internal Server Error" })),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            );
            return Ok(resp.into_response());
        }
    };

    // Strip hop-by-hop from response headers
    let resp_headers = upstream_resp.headers().clone();
    let status = upstream_resp.status();

    // 3A. Streaming SSE
    let content_type = resp_headers
        .get(warp::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if content_type.contains("text/event-stream") {
        let alias_owned = alias;
        let log_owned = log.unwrap_or_default();

        // Use a fold to process SSE lines with buffer state
        let byte_stream = upstream_resp.bytes_stream();

        let sse_stream = async_stream::stream! {
            let mut buffer: Vec<u8> = Vec::new();
            let alias = alias_owned;
            let mut log = log_owned;

            futures_util::pin_mut!(byte_stream);
            while let Some(chunk_result) = byte_stream.next().await {
                match chunk_result {
                    Ok(bytes) => {
                        // Keep raw bytes so multibyte UTF-8 chars split across
                        // TCP chunks are not corrupted.
                        buffer.extend_from_slice(&bytes);
                        while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = buffer.drain(..=pos).collect();
                            let line = match std::str::from_utf8(&line) {
                                Ok(s) => s.trim_end_matches(['\r', '\n']).to_string(),
                                Err(_) => String::from_utf8_lossy(&line)
                                    .trim_end()
                                    .to_string(),
                            };

                            if let Some(transformed) = transform_sse_line(&line, &alias, &mut log) {
                                yield Ok::<_, std::io::Error>(Bytes::from(transformed));
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(std::io::Error::new(std::io::ErrorKind::Other, e));
                        break;
                    }
                }
            }
            // Flush remaining buffer
            if !buffer.is_empty() {
                yield Ok(Bytes::from(buffer));
            }
        };

        let mut resp_headers_clean = resp_headers.clone();
        resp_headers_clean.remove(warp::http::header::CONTENT_LENGTH);
        resp_headers_clean.remove(warp::http::header::CONTENT_ENCODING);
        resp_headers_clean.remove(warp::http::header::TRANSFER_ENCODING);

        // Build response manually to set status + headers
        let mut response = warp::reply::stream(sse_stream).into_response();
        *response.status_mut() = status;
        response.headers_mut().extend(resp_headers_clean);
        response.headers_mut().insert(
            "x-proxy",
            warp::http::HeaderValue::from_static("ollama-token-api"),
        );

        return Ok(response);
    }

    // 3B. Buffered JSON
    if content_type.contains("application/json") {
        let bytes = match upstream_resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("Error reading upstream response body: {}", e);
                let resp = warp::reply::with_status(
                    warp::reply::json(&json!({ "message": "Bad Gateway" })),
                    warp::http::StatusCode::BAD_GATEWAY,
                );
                return Ok(resp.into_response());
            }
        };
        let mut data: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));

        patch_usage_deep(&mut data, 0);
        if let Some(mut l) = log.take() {
            l.restored += restore_function_call_names(&mut data, &alias, 0);
            if l.restored > 0 && config.compat_log {
                tracing::info!(
                    "[tool-compat] restored {} function_call name(s)",
                    l.restored
                );
            }
        } else {
            restore_function_call_names(&mut data, &alias, 0);
        }

        let mut resp_headers_clean = resp_headers.clone();
        resp_headers_clean.remove(warp::http::header::CONTENT_LENGTH);
        resp_headers_clean.remove(warp::http::header::CONTENT_ENCODING);
        resp_headers_clean.remove(warp::http::header::TRANSFER_ENCODING);
        resp_headers_clean.insert(
            warp::http::header::CONTENT_TYPE,
            warp::http::HeaderValue::from_static("application/json; charset=utf-8"),
        );

        let body_bytes = serde_json::to_vec(&data).unwrap();
        let mut response = warp::reply::Response::new(body_bytes.into());
        *response.status_mut() = status;
        *response.headers_mut() = resp_headers_clean;
        return Ok(response);
    }

    // 3C. Everything else (streamed pass-through)
    let byte_stream = upstream_resp.bytes_stream();
    let passthrough = byte_stream.map(|item| {
        item.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    });

    let mut resp_headers_clean = resp_headers;
    resp_headers_clean.remove(warp::http::header::CONTENT_LENGTH);
    resp_headers_clean.remove(warp::http::header::CONTENT_ENCODING);
    resp_headers_clean.remove(warp::http::header::TRANSFER_ENCODING);

    let mut response = warp::reply::stream(passthrough).into_response();
    *response.status_mut() = status;
    *response.headers_mut() = resp_headers_clean;
    Ok(response)
}

// ---------------------------------------------------------------------------
// Auth filter
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Unauthorized;

impl warp::reject::Reject for Unauthorized {}

impl warp::reply::Reply for Unauthorized {
    fn into_response(self) -> warp::reply::Response {
        warp::reply::with_status(
            warp::reply::json(&json!({ "error": { "message": "Unauthorized" } })),
            warp::http::StatusCode::UNAUTHORIZED,
        )
        .into_response()
    }
}

#[derive(Debug)]
struct DatabaseError;

impl warp::reject::Reject for DatabaseError {}

impl warp::reply::Reply for DatabaseError {
    fn into_response(self) -> warp::reply::Response {
        warp::reply::with_status(
            warp::reply::json(&json!({ "error": { "message": "Internal Server Error" } })),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )
        .into_response()
    }
}

async fn verify_token(
    token: &str,
    db: &mongodb::Database,
    config: &Config,
) -> Result<bool, mongodb::error::Error> {
    if config.testing && token == "testing" {
        return Ok(true);
    }
    let collection = db.collection::<mongodb::bson::Document>("tokens");
    match collection.find_one(doc! { "token": token }).await {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(e) => {
            tracing::error!("MongoDB error verifying token: {}", e);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // Config
    let mongo_uri = env::var("MONGODB_URI").expect("MONGODB_URI not set");
    let ollama_url = remove_trailing_slash(&env::var("OLLAMA_URL").expect("OLLAMA_URL not set"));
    let upstream_api_key = env::var("UPSTREAM_API_KEY").unwrap_or_default();
    let testing = env::var("TESTING").is_ok();
    let responses_path_re = Regex::new(
        &env::var("RESPONSES_PATH_RE").unwrap_or_else(|_| "/v1/responses/?$".to_string()),
    )
    .unwrap();
    let web_search_mode = env::var("CODEX_WEB_SEARCH")
        .unwrap_or_else(|_| "stub".to_string())
        .to_lowercase();
    let fill_tool_strict =
        env::var("TOOL_STRICT").unwrap_or_else(|_| "fill".to_string()) != "leave";
    let compat_log = env::var("TOOL_COMPAT_LOG").unwrap_or_else(|_| "1".to_string()) != "0";

    let config = Arc::new(Config {
        ollama_url,
        upstream_api_key,
        testing,
        responses_path_re,
        web_search_mode,
        fill_tool_strict,
        compat_log,
    });

    // MongoDB
    let client = Client::with_uri_str(&mongo_uri)
        .await
        .expect("Failed to connect to MongoDB");
    let db = client.database("ollama");

    // HTTP client: connect timeout only, no total timeout (SSE may be long-lived)
    let http_client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("Failed to build HTTP client");

    // Auth filter: extract bearer token, verify against MongoDB
    let db_for_filter = db.clone();
    let config_for_filter = config.clone();

    let auth_filter = warp::any()
        .and(warp::header::optional::<String>("authorization"))
        .and_then(move |auth_header: Option<String>| {
            let db = db_for_filter.clone();
            let config = config_for_filter.clone();
            async move {
                let Some(auth_header) = auth_header else {
                    return Err(warp::reject::custom(Unauthorized));
                };
                let token = auth_header.strip_prefix("Bearer ").unwrap_or(&auth_header);
                match verify_token(token, &db, &config).await {
                    Ok(true) => Ok::<(), warp::Rejection>(()),
                    Ok(false) => Err(warp::reject::custom(Unauthorized)),
                    Err(_) => Err(warp::reject::custom(DatabaseError)),
                }
            }
        });

    // Catch-all proxy route
    let config_for_route = config.clone();
    let http_client_for_route = http_client.clone();

    let proxy_route = warp::path::full()
        .and(warp::method())
        .and(warp::header::headers_cloned())
        .and(warp::body::bytes())
        .and(warp::filters::body::content_length_limit(32 * 1024 * 1024))
        .and(with_config(config_for_route))
        .and(with_http_client(http_client_for_route))
        .and_then(
            move |full_path: warp::path::FullPath,
                  method: warp::hyper::Method,
                  headers: warp::http::HeaderMap,
                  body: Bytes,
                  config: Arc<Config>,
                  http_client: reqwest::Client| async move {
                let full = full_path.as_str();
                let (path_only, query) = if let Some(idx) = full.find('?') {
                    (&full[..idx], Some(full[idx + 1..].to_string()))
                } else {
                    (full, None)
                };
                let path = path_only.to_string();
                let query = match query {
                    Some(q) if !q.is_empty() => format!("?{}", q),
                    _ => String::new(),
                };

                proxy_handler(
                    method,
                    path,
                    query,
                    headers,
                    body.to_vec(),
                    config,
                    http_client,
                )
                .await
            },
        );

    // Combine: auth + proxy
    let app = warp::any()
        .and(auth_filter)
        .and(proxy_route)
        .map(|_auth: (), resp: warp::reply::Response| resp);

    // Logging middleware
    let app = app.with(warp::log::custom(|info| {
        tracing::info!(
            "{} {} {:?} status={} duration={:?}",
            info.method(),
            info.path(),
            info.version(),
            info.status().as_u16(),
            info.elapsed()
        );
    }));

    tracing::info!("Listening on 0.0.0.0:8000");
    warp::serve(app).run(([0, 0, 0, 0], 8000)).await;
}

fn with_config(
    config: Arc<Config>,
) -> impl Filter<Extract = (Arc<Config>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || config.clone())
}

fn with_http_client(
    client: reqwest::Client,
) -> impl Filter<Extract = (reqwest::Client,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || client.clone())
}

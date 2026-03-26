use serde_json::Value;
use url::Url;

pub(crate) fn is_rewritable_host(host: &str) -> bool {
    matches!(
        host,
        "127.0.0.1" | "localhost" | "::1" | "0.0.0.0" | "host.docker.internal"
    )
}

pub(crate) fn render_url_without_root_slash(url: &Url) -> String {
    let rendered = url.to_string();
    if url.path() == "/" && url.query().is_none() && url.fragment().is_none() {
        rendered.trim_end_matches('/').to_string()
    } else {
        rendered
    }
}

pub(crate) fn rewrite_url_base_in_place(
    text: &mut String,
    original_port: u16,
    replacement_base_url: &str,
) {
    let Ok(mut url) = Url::parse(text) else {
        return;
    };
    let Some(host) = url.host_str() else {
        return;
    };
    if !is_rewritable_host(host) {
        return;
    }
    if url.port() != Some(original_port) {
        return;
    }
    let Ok(replacement) = Url::parse(replacement_base_url) else {
        return;
    };
    let _ = url.set_scheme(replacement.scheme());
    let _ = url.set_host(replacement.host_str());
    let _ = url.set_port(replacement.port());
    *text = render_url_without_root_slash(&url);
}

pub(crate) fn rewrite_value_url_bases(
    value: &mut Value,
    original_port: u16,
    replacement_base_url: &str,
) {
    match value {
        Value::String(text) => rewrite_url_base_in_place(text, original_port, replacement_base_url),
        Value::Array(items) => {
            for item in items {
                rewrite_value_url_bases(item, original_port, replacement_base_url);
            }
        }
        Value::Object(object) => {
            for item in object.values_mut() {
                rewrite_value_url_bases(item, original_port, replacement_base_url);
            }
        }
        _ => {}
    }
}

//! Web tools: web_search (DuckDuckGo HTML backend, per plan Q1) and
//! web_fetch (reqwest + trafilatura).
//!
//! Search is the same `{title, url, body}` shape ddgs's HTML backend
//! produced; the parse is verified against html.duckduckgo.com (10 results
//! per page, `result__a` + `result__snippet`, 403 without a real User-Agent).

use scraper::{Html, Selector};
use serde_json::json;

use super::registry::{cap, register, ToolContext, ToolResult};

const FETCH_TIMEOUT_SECS: u64 = 30;
const USER_AGENT: &str = "Mozilla/5.0 (Fedora; Linux x86_64) harness/0.1";
const SEARCH_SNIPPET_CHARS: usize = 300;
const SEARCH_RESULT_CHARS: usize = 6000;

fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    let verify = match std::env::var("HARNESS_TLS_VERIFY") {
        Ok(v) if v == "0" || v.eq_ignore_ascii_case("false") => false,
        _ => true,
    };
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .danger_accept_invalid_certs(!verify) // like Python HARNESS_TLS_VERIFY=0
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()
}

/// Decode a DDG redirect link (`uddg=`) to the real URL.
fn ddg_real_url(href: &str) -> String {
    let decoded = urlencoding::decode(href)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| href.to_string());
    let query = decoded
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("");
    for pair in query.split('&') {
        if let Some(val) = pair.strip_prefix("uddg=") {
            return urlencoding::decode(val)
                .map(|s| s.into_owned())
                .unwrap_or_else(|_| val.to_string());
        }
    }
    if decoded.starts_with("//") {
        format!("https:{decoded}")
    } else {
        decoded
    }
}

async fn do_search(query: &str, max_results: u32) -> ToolResult {
    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        urlencoding::encode(query)
    );
    let resp = match http_client() {
        Ok(c) => match c.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("error: search failed: {e}")),
        },
        Err(e) => return ToolResult::err(format!("error: search failed: {e}")),
    };
    if resp.status().as_u16() >= 400 {
        return ToolResult::err(format!(
            "error: search failed: HTTP {}",
            resp.status()
        ));
    }
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => return ToolResult::err(format!("error: search failed: {e}")),
    };
    let doc = Html::parse_document(&body);
    let sel_a = Selector::parse("a.result__a").expect("static selector");
    let sel_s = Selector::parse("a.result__snippet").expect("static selector");
    let mut out: Vec<String> = Vec::new();
    let mut i = 0u32;
    for el in doc.select(&sel_a) {
        i += 1;
        if i > max_results {
            break;
        }
        let title = el
            .text()
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let href = el
            .value()
            .attr("href")
            .map(ddg_real_url)
            .unwrap_or_default();
        // snippet lives in the same result block; find the matching element
        // by shared redirect URL.
        let mut snippet = String::new();
        for s in doc.select(&sel_s) {
            if s.value().attr("href").map(ddg_real_url).as_deref() == Some(href.as_str()) {
                // flatten `<b>`/whitespace runs left by the markup
                snippet = s
                    .text()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                break;
            }
        }
        let snippet = snippet.trim().to_string();
        let snippet = if snippet.chars().count() > SEARCH_SNIPPET_CHARS {
            let cut: String = snippet.chars().take(SEARCH_SNIPPET_CHARS).collect();
            format!("{cut}...")
        } else {
            snippet
        };
        out.push(format!("{i}. {title}\n   {href}\n   {snippet}"));
    }
    if out.is_empty() {
        return ToolResult::res(format!("no results for {query:?}"));
    }
    ToolResult::res(cap(&out.join("\n"), SEARCH_RESULT_CHARS))
}

async fn do_fetch(ctx: &ToolContext, url: &str) -> ToolResult {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return ToolResult::err("error: url must start with http:// or https://");
    }
    let resp = match http_client() {
        Ok(c) => match c.get(url).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("error: fetch failed: {e}")),
        },
        Err(e) => return ToolResult::err(format!("error: fetch failed: {e}")),
    };
    if resp.status().as_u16() >= 400 {
        return ToolResult::err(format!("error: fetch failed: HTTP {} for {url}", resp.status()));
    }
    let html = match resp.text().await {
        Ok(b) => b,
        Err(e) => return ToolResult::err(format!("error: fetch failed: {e}")),
    };
    let text = match trafilatura::extract(&html, &trafilatura::Options::default()) {
        Ok(r) if !r.content_text.is_empty() => r.content_text,
        _ => html.clone(),
    };
    let text = text
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    ToolResult::res(cap(&text, ctx.limits().web_max_chars))
}

async fn _web_search(args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
    let Some(query) = args.get("query").and_then(serde_json::Value::as_str) else {
        return ToolResult::err("error: query is required");
    };
    if query.is_empty() {
        return ToolResult::err("error: query is required");
    }
    let max_results = args
        .get("max_results")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(ctx.cfg.search.max_results as u64)
        .min(10) as u32;
    do_search(query, max_results).await
}

async fn _web_fetch(args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
    let Some(url) = args.get("url").and_then(serde_json::Value::as_str) else {
        return ToolResult::err("error: url is required");
    };
    if url.is_empty() {
        return ToolResult::err("error: url is required");
    }
    do_fetch(ctx, url).await
}

pub fn register_all() {
    register(
        "web_search",
        "Search the web. Returns numbered results with title, URL, and snippet.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 10,
                                 "description": "Number of results (default: configured max_results)"}
            },
            "required": ["query"]
        }),
        |args, ctx| Box::pin(async move { _web_search(args, &ctx).await }),
    );
    register(
        "web_fetch",
        "Fetch a URL and return the main content as plain text (docs, READMEs, issue pages). Use after web_search to read a result in full.",
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Absolute http(s) URL"}
            },
            "required": ["url"]
        }),
        |args, ctx| Box::pin(async move { _web_fetch(args, &ctx).await }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddg_redirect_decode() {
        assert_eq!(
            ddg_real_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Ftokio.rs%2F&utm=1"),
            "https://tokio.rs/"
        );
        assert_eq!(
            ddg_real_url("https://duckduckgo.com/l/?rut=abc&uddg=https%3A%2F%2Fdocs.rs%2Ftokio"),
            "https://docs.rs/tokio"
        );
        assert_eq!(ddg_real_url("https://plain.example/x"), "https://plain.example/x");
    }
}

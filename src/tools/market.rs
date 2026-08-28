//! Real-time A-share market quotes from public HTTP endpoints.
//!
//! `market_quote` fetches index or stock snapshots from Eastmoney's free
//! `ulist.np` quote JSON API. No API key, no JavaScript, no browser session is
//! required, so it works where scraping JS-rendered financial pages fails.
//! Reuses the shared `web::fetch` helper so SSRF validation, redirect handling,
//! timeouts, and output bounds all still apply.

use chrono::{DateTime, Local};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use super::{ToolError, web};

/// Eastmoney quote hosts in priority order. Both serve the same `ulist.np`
/// JSON schema; `push2delay` tolerates plain user agents and burst requests,
/// while the primary `push2` host occasionally throttles scripts.
const QUOTE_HOSTS: [&str; 2] = ["push2delay.eastmoney.com", "push2.eastmoney.com"];
const QUOTE_FIELDS: &str = "f12,f14,f2,f3,f4,f5,f6,f124";
const MAX_SYMBOLS: usize = 20;

/// Default watchlist: SSE Composite, SZSE Component, ChiNext.
const DEFAULT_SYMBOLS: [&str; 3] = ["sh000001", "sz399001", "sz399006"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuoteArgs {
    #[serde(default)]
    symbols: Option<Vec<String>>,
}

pub async fn quote(
    value: &Value,
    max_bytes: usize,
    allow_private: bool,
) -> Result<String, ToolError> {
    let args: QuoteArgs = serde_json::from_value(value.clone())?;
    let mut symbols: Vec<String> = args.symbols.unwrap_or_default();
    if symbols.is_empty() {
        symbols = DEFAULT_SYMBOLS.iter().map(|s| (*s).to_owned()).collect();
    }
    if symbols.len() > MAX_SYMBOLS {
        return Err(ToolError::Execution(format!(
            "market_quote accepts at most {MAX_SYMBOLS} symbols"
        )));
    }
    let mut secids = Vec::with_capacity(symbols.len());
    for symbol in &symbols {
        secids.push(to_secid(symbol)?);
    }

    let mut last_error: Option<ToolError> = None;
    for host in QUOTE_HOSTS {
        let mut url =
            Url::parse(&format!("https://{host}/api/qt/ulist.np/get")).map_err(execution_error)?;
        url.query_pairs_mut()
            .append_pair("secids", &secids.join(","))
            .append_pair("fields", QUOTE_FIELDS);
        let fetched = web::fetch(
            &json!({"url": url.as_str(), "method": "GET", "max_bytes": max_bytes}),
            max_bytes,
            allow_private,
        )
        .await;
        let fetched = match fetched {
            Ok(text) => text,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        match format_quote(&fetched, &symbols, max_bytes) {
            Ok(output) => return Ok(output),
            Err(error) => {
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        ToolError::Execution("all quote providers failed to return usable data".into())
    }))
}

/// Maps a friendly symbol (`sh600519`, `sz000001`) to an Eastmoney `secid`
/// (`1.600519`, `0.000001`). Only Shanghai/Shenzhen A-shares are supported.
fn to_secid(symbol: &str) -> Result<String, ToolError> {
    let symbol = symbol.trim().to_ascii_lowercase();
    if symbol.len() != 8 || !symbol[2..].bytes().all(|b| b.is_ascii_digit()) {
        return Err(ToolError::Execution(format!(
            "invalid market symbol {symbol:?}: expected sh/sz plus 6 digits (e.g. sh600519, sz000001)"
        )));
    }
    let market = match &symbol[..2] {
        "sh" => "1",
        "sz" => "0",
        _ => {
            return Err(ToolError::Execution(format!(
                "invalid market symbol {symbol:?}: prefix must be sh or sz"
            )));
        }
    };
    Ok(format!("{market}.{}", &symbol[2..]))
}

/// Parses the `web::fetch` envelope and renders the quote JSON as a table.
fn format_quote(
    fetched: &str,
    requested: &[String],
    max_bytes: usize,
) -> Result<String, ToolError> {
    // The web_fetch envelope is "URL: ...\nStatus: ...\nContent-Type: ...\n
    // Truncated: ...\n\n{body}". Eastmoney returns minified single-line JSON,
    // so the last blank-line separator precedes the body.
    let body = fetched
        .rsplit_once("\n\n")
        .map(|(_, body)| body)
        .ok_or_else(|| {
            ToolError::Execution("unexpected response format from quote provider".into())
        })?;
    let parsed: Value = serde_json::from_str(body).map_err(|error| {
        ToolError::Execution(format!("quote provider returned non-JSON data: {error}"))
    })?;
    let diff = parsed
        .get("data")
        .and_then(|data| data.get("diff"))
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::Execution("quote provider returned an empty response".into()))?;

    let mut rows: Vec<String> = Vec::new();
    let mut updated: Option<i64> = None;
    for item in diff {
        let Some(code) = item.get("f12").and_then(Value::as_str) else {
            continue;
        };
        let name = item.get("f14").and_then(Value::as_str).unwrap_or(code);
        let latest = item.get("f2").and_then(Value::as_f64);
        let change = item.get("f4").and_then(Value::as_f64);
        let pct = item.get("f3").and_then(Value::as_f64);
        let amount = item.get("f6").and_then(Value::as_f64);
        if let Some(ts) = item.get("f124").and_then(Value::as_i64) {
            updated = Some(ts);
        }
        rows.push(format!(
            "| {code} | {name} | {latest} | {change} | {pct} | {amount} |",
            latest = latest
                .map(|v| format!("{:.2}", v / 100.0))
                .unwrap_or_else(|| "—".into()),
            change = change
                .map(|v| format!("{}{:.2}", if v < 0.0 { "" } else { "+" }, v / 100.0))
                .unwrap_or_else(|| "—".into()),
            pct = pct
                .map(|v| format!("{}{:.2}%", if v < 0.0 { "" } else { "+" }, v / 100.0))
                .unwrap_or_else(|| "—".into()),
            amount = amount.map(format_amount).unwrap_or_else(|| "—".into()),
        ));
    }
    if rows.is_empty() {
        return Err(ToolError::Execution(
            "no quote data returned for the requested symbols; check that codes use the sh/sz prefix (e.g. sh600519, sz000001)"
                .into(),
        ));
    }

    let mut output = format!(
        "实时 A 股行情（来源：东方财富行情接口，更新时间 {}）\n\n\
         | 代码 | 名称 | 最新价 | 涨跌 | 涨跌幅 | 成交额 |\n\
         |---|---|---|---|---|---|\n",
        updated.map(format_ts).unwrap_or_else(|| "—".into()),
    );
    for row in &rows {
        if output.len() + row.len() + 2 > max_bytes {
            output.push_str("[output truncated]\n");
            break;
        }
        output.push_str(row);
        output.push('\n');
    }
    if rows.len() < requested.len() {
        output
            .push_str("[note] some requested symbols returned no data (possibly invalid codes)\n");
    }
    Ok(output)
}

fn format_amount(yuan: f64) -> String {
    if yuan >= 1e8 {
        format!("{:.2}亿", yuan / 1e8)
    } else if yuan >= 1e4 {
        format!("{:.2}万", yuan / 1e4)
    } else {
        format!("{yuan:.0}元")
    }
}

fn format_ts(timestamp: i64) -> String {
    DateTime::from_timestamp(timestamp, 0)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| timestamp.to_string())
}

fn execution_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::Execution(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(body: &str) -> String {
        format!(
            "URL: https://push2delay.eastmoney.com/api/qt/ulist.np/get\n\
             Status: 200\n\
             Content-Type: application/json;charset=UTF-8\n\
             Truncated: false\n\n\
             {body}"
        )
    }

    #[test]
    fn defaults_to_three_major_indices() {
        let mut url = Url::parse("https://push2delay.eastmoney.com/api/qt/ulist.np/get").unwrap();
        let secids = ["sh000001", "sz399001", "sz399006"]
            .iter()
            .map(|symbol| to_secid(symbol).unwrap())
            .collect::<Vec<_>>();
        url.query_pairs_mut()
            .append_pair("secids", &secids.join(","));
        let value = url
            .query_pairs()
            .find(|(key, _)| key == "secids")
            .map(|(_, value)| value.into_owned())
            .unwrap();
        assert_eq!(value, "1.000001,0.399001,0.399006");
    }

    #[test]
    fn symbol_mapping_and_validation() {
        assert_eq!(to_secid("sh600519").unwrap(), "1.600519");
        assert_eq!(to_secid("SZ000001").unwrap(), "0.000001");
        for bad in ["600519", "sh12345", "abc123", "hk00700", "sh6005190", ""] {
            assert!(to_secid(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn quote_formats_bounded_table() {
        let body = r#"{"rc":0,"rt":11,"data":{"total":2,"diff":[
            {"f2":395218,"f3":-11,"f4":-439,"f5":510581645,"f6":970365152112.9,"f12":"000001","f14":"上证指数","f124":1787904693},
            {"f2":1395307,"f3":-68,"f4":-9581,"f5":630346080,"f6":1131349872245.798,"f12":"399001","f14":"深证成指","f124":1787904693}
        ]}}"#;
        let output = format_quote(
            &envelope(body),
            &["sh000001".into(), "sz399001".into()],
            64 * 1024,
        )
        .unwrap();
        assert!(output.contains("上证指数"), "{output}");
        assert!(output.contains("3952.18"), "{output}");
        assert!(output.contains("-0.11%"), "{output}");
        assert!(output.contains("9703.65亿"), "{output}");
        assert!(output.contains("深证成指"), "{output}");
        assert!(output.len() <= 64 * 1024);
    }

    #[test]
    fn empty_diff_is_an_error() {
        let body = r#"{"rc":0,"data":{"total":0,"diff":[]}}"#;
        assert!(
            format_quote(&envelope(body), &["sh000001".into()], 1024).is_err(),
            "empty diff should be an error"
        );
    }

    #[test]
    fn envelope_without_body_separator_is_an_error() {
        assert!(
            format_quote("URL: x\nStatus: 200", &["sh000001".into()], 1024).is_err(),
            "missing envelope separator should be an error"
        );
    }

    #[test]
    fn partial_result_notes_missing_symbols() {
        let body = r#"{"rc":0,"data":{"total":1,"diff":[
            {"f2":395218,"f3":-11,"f4":-439,"f5":510581645,"f6":970365152112.9,"f12":"000001","f14":"上证指数"}
        ]}}"#;
        let output = format_quote(
            &envelope(body),
            &["sh000001".into(), "sh999999".into()],
            64 * 1024,
        )
        .unwrap();
        assert!(output.contains("no data"), "{output}");
    }

    #[tokio::test]
    #[ignore = "requires public network access"]
    async fn market_quote_smoke_test() {
        let output = quote(&json!({}), 64 * 1024, false)
            .await
            .expect("public quote should succeed");
        assert!(output.contains("上证指数"), "{output}");
        assert!(output.len() <= 64 * 1024);
    }
}

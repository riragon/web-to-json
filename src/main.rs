use actix_web::{
    web, App, HttpResponse, HttpServer, Responder,
};
use serde::{Serialize, Deserialize};
use regex::Regex;
use tokio::task::{spawn_blocking};
use std::time::Duration;
use open;
use url::Url;
use sanitize_filename::sanitize;
use scraper::{Html, Selector, ElementRef};
use scraper::node::Node;

/// 複数URLを改行区切りで受け取るフォーム
#[derive(Deserialize)]
struct UrlForm {
    urls: String,
    include_subpages: Option<String>,
}

/// JSON 出力用: 通常ノード or テーブル
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum DomContent {
    Node(DomNode),
    Table(TableData),
}

/// 通常ノード
#[derive(Debug, Serialize)]
struct DomNode {
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    href: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,

    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<DomContent>,

    #[serde(skip_serializing_if = "Option::is_none")]
    link_subpage: Option<Box<DomContent>>,
}

/// テーブル構造
#[derive(Debug, Serialize)]
struct TableData {
    table_headers: Vec<String>,
    rows: Vec<serde_json::Value>,
}

// =================== メイン ===================

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::io::Result<()> {
    let server = HttpServer::new(|| {
        App::new()
            .route("/", web::get().to(show_form))
            .route("/", web::post().to(process_form))
    })
    .bind(("127.0.0.1", 8080))?
    .run();

    // 起動後にブラウザを自動で開く
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _ = open::that("http://127.0.0.1:8080/");
    });

    server.await
}

/// (GET) フォーム画面
async fn show_form() -> impl Responder {
    let html = r#"
<!DOCTYPE html>
<html><head><meta charset="UTF-8"/><title>Web to JSON (Multiple URLs)</title></head>
<body>
  <h1>複数URLを改行区切りで入力</h1>
  <form action="/" method="post">
    <textarea name="urls" rows="5" cols="80" placeholder="https://example.com&#10;https://example.org"></textarea>
    <br/>
    <label>
      <input type="checkbox" name="include_subpages" value="true"/>
      1階層リンク先を含める
    </label>
    <button type="submit">JSON変換</button>
  </form>
</body></html>
    "#;
    HttpResponse::Ok().content_type("text/html; charset=utf-8").body(html)
}

/// (POST) 複数URL対応
async fn process_form(form: web::Form<UrlForm>) -> impl Responder {
    // 複数行 -> split
    let lines = form.urls.replace('\r', "");
    let url_list: Vec<_> = lines
        .split('\n')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    let include_subpages = form.include_subpages.as_deref() == Some("true");

    // 解析結果を格納
    let mut results = Vec::new();

    for url_str in &url_list {
        let Ok(parsed_url) = Url::parse(url_str) else {
            // URL parse エラー
            let error_node = DomContent::Node(DomNode {
                tag: Some("ErrorURL".to_string()),
                href: None,
                text: Some(format!("URL parse error: {url_str}")),
                children: vec![],
                link_subpage: None,
            });
            results.push(error_node);
            continue;
        };

        // HTTP GET
        let resp_body = match reqwest::get(parsed_url.clone()).await {
            Ok(resp) => match resp.text().await {
                Ok(b) => b,
                Err(e) => {
                    let error_node = DomContent::Node(DomNode {
                        tag: Some("ErrorFetch".to_string()),
                        href: None,
                        text: Some(format!("Error reading response: {e}")),
                        children: vec![],
                        link_subpage: None,
                    });
                    results.push(error_node);
                    continue;
                }
            },
            Err(e) => {
                let error_node = DomContent::Node(DomNode {
                    tag: Some("ErrorFetch".to_string()),
                    href: None,
                    text: Some(format!("Request error: {e}")),
                    children: vec![],
                    link_subpage: None,
                });
                results.push(error_node);
                continue;
            }
        };

        // 同期パース
        let mut root_content = match spawn_blocking({
            let resp_body_clone = resp_body.clone(); // move でエラー回避
            move || parse_html_sync(&resp_body_clone)
        }).await {
            Ok(dom) => dom,
            Err(e_spawn) => {
                let error_node = DomContent::Node(DomNode {
                    tag: Some("ErrorSpawnBlock".to_string()),
                    href: None,
                    text: Some(format!("spawn_blocking error: {e_spawn:?}")),
                    children: vec![],
                    link_subpage: None,
                });
                results.push(error_node);
                continue;
            }
        };

        // サブページ
        if include_subpages {
            let _ = fetch_subpages_for_depth_one(&mut root_content, &parsed_url).await;
        }

        // 追加
        results.push(root_content);
    }

    // 配列に
    let json_arr = serde_json::Value::Array(
        results.into_iter()
            .map(|c| serde_json::to_value(c).unwrap_or(serde_json::Value::Null))
            .collect()
    );

    let json_str = match serde_json::to_string(&json_arr) {
        Ok(j) => j,
        Err(e) => return HttpResponse::InternalServerError()
                        .body(format!("JSON serialize error: {e}")),
    };

    // 総文字数
    let total_chars = json_str.chars().count();

    let escaped_json = json_str
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");

    // ダウンロード用ファイル名
    let file_name = format!("multi_urls_{}.json", sanitize("result"));

    let msg_subpage = if include_subpages {
        "（1階層リンク先含む）"
    } else {
        ""
    };

    let html = format!(r#"
<!DOCTYPE html>
<html><head><meta charset="UTF-8"/><title>Web to JSON</title></head>
<body>
  <h1>結果</h1>
  <p>複数URLを解析し、配列形式の JSON を生成しました。{msg_subpage}</p>
  <p>総文字数: {total_chars}</p>

  <button onclick="downloadJson()">JSONをダウンロード</button>
  <hr/>
  <textarea id="jsonText" rows="12" cols="90" style="white-space: pre;">{escaped_json}</textarea><br/>
  <button onclick="copyToClipboard()">コピー</button>

  <script>
    function downloadJson() {{
      const text = `{json_str}`;
      const blob = new Blob([text], {{ type: 'application/json' }});
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = "{file_name}";
      a.click();
      URL.revokeObjectURL(url);
    }}
    function copyToClipboard() {{
      const textArea = document.getElementById('jsonText');
      navigator.clipboard.writeText(textArea.value)
        .then(() => alert('クリップボードにコピーしました。'))
        .catch(err => alert('コピー失敗: ' + err));
    }}
  </script>

  <hr/>
  <h2>再度URLを入力</h2>
  <form action="/" method="post">
    <textarea name="urls" rows="5" cols="80" placeholder="https://example.com&#10;https://example.org"></textarea>
    <br/>
    <label>
      <input type="checkbox" name="include_subpages" value="true"/>
      1階層リンク先を含める
    </label>
    <button type="submit">JSON変換</button>
  </form>
</body>
</html>
"#,
        msg_subpage = msg_subpage,
        total_chars = total_chars,
        escaped_json = escaped_json,
        json_str = json_str,
        file_name = file_name
    );

    HttpResponse::Ok().content_type("text/html; charset=utf-8").body(html)
}

/// HTMLを解析 (同期)
fn parse_html_sync(body: &str) -> DomContent {
    let doc = Html::parse_document(body);
    let sel_html = Selector::parse("html").unwrap();
    if let Some(html_el) = doc.select(&sel_html).next() {
        DomContent::Node(DomNode {
            tag: Some("html".to_string()),
            href: None,
            text: None,
            children: parse_children(html_el),
            link_subpage: None,
        })
    } else {
        DomContent::Node(DomNode {
            tag: Some("html".to_string()),
            href: None,
            text: Some("(No <html> found)".to_string()),
            children: vec![],
            link_subpage: None,
        })
    }
}

/// 再帰的に子を解析
fn parse_children(el: ElementRef) -> Vec<DomContent> {
    let mut result = Vec::new();

    for child in el.children() {
        match child.value() {
            Node::Element(e) => {
                let tag_name = e.name().to_lowercase();
                if skip_tag(&tag_name) {
                    continue;
                }
                if tag_name == "table" {
                    if let Some(tbl) = ElementRef::wrap(child) {
                        let table_data = parse_table(tbl);
                        result.push(DomContent::Table(table_data));
                    }
                }
                else if is_target_tag(&tag_name) {
                    // a, p, h*, etc
                    let mut link = None;
                    if tag_name == "a" {
                        for (attr_name, attr_value) in e.attrs() {
                            if attr_name.eq_ignore_ascii_case("href") {
                                link = Some(attr_value.to_string());
                            }
                        }
                    }
                    if let Some(sub_el) = ElementRef::wrap(child) {
                        let children = parse_children(sub_el);
                        result.push(DomContent::Node(DomNode {
                            tag: Some(tag_name),
                            href: link,
                            text: None,
                            children,
                            link_subpage: None,
                        }));
                    }
                }
                else {
                    // 中身だけ取り出す
                    if let Some(sub_el) = ElementRef::wrap(child) {
                        let sub = parse_children(sub_el);
                        result.extend(sub);
                    }
                }
            }
            Node::Text(txt_node) => {
                let c = clean_text(&txt_node.text);
                if !c.is_empty() {
                    result.push(DomContent::Node(DomNode {
                        tag: None,
                        href: None,
                        text: Some(c),
                        children: vec![],
                        link_subpage: None,
                    }));
                }
            }
            _ => {}
        }
    }

    result
}

/// テーブル解析
fn parse_table(table_el: ElementRef) -> TableData {
    let mut headers = Vec::new();
    let mut rows = Vec::new();

    let tr_sel = Selector::parse("tr").unwrap();
    let td_sel = Selector::parse("th,td").unwrap();
    let mut first_row = true;

    for tr_el in table_el.select(&tr_sel) {
        let mut cells = Vec::new();
        for cell_el in tr_el.select(&td_sel) {
            let raw_text = cell_el.text().collect::<String>();
            let clean = clean_text(&raw_text);
            cells.push(clean);
        }
        if cells.is_empty() {
            continue;
        }
        if first_row {
            // ヘッダ行
            headers = cells;
            first_row = false;
        } else {
            // データ行
            let mut obj_map = serde_json::Map::new();
            for (i, val) in cells.iter().enumerate() {
                let col_name = if i < headers.len() {
                    headers[i].clone()
                } else {
                    format!("col{i}")
                };
                obj_map.insert(col_name, serde_json::Value::String(val.clone()));
            }
            rows.push(serde_json::Value::Object(obj_map));
        }
    }

    TableData {
        table_headers: headers,
        rows,
    }
}

/// aタグ => link_subpage
async fn fetch_subpages_for_depth_one(content: &mut DomContent, base_url: &Url) -> Result<(), String> {
    let mut stack = vec![content as *mut DomContent];
    while let Some(ptr) = stack.pop() {
        let node_content = unsafe { &mut *ptr };
        match node_content {
            DomContent::Table(_) => { /* skip table sub links */ }
            DomContent::Node(node) => {
                // BFS
                for c in node.children.iter_mut() {
                    stack.push(c as *mut DomContent);
                }
                if let Some(t) = &node.tag {
                    if t == "a" {
                        if let Some(href) = &node.href {
                            if let Ok(sub_url) = base_url.join(href) {
                                if ["http","https"].contains(&sub_url.scheme()) {
                                    let body = match reqwest::get(sub_url.clone()).await {
                                        Ok(r) => match r.text().await {
                                            Ok(tx) => tx,
                                            Err(_e) => { continue; } // _e -> discard
                                        },
                                        Err(_e) => { continue; } // _e -> discard
                                    };
                                    let subdom = spawn_blocking({
                                        let body_clone = body.clone();
                                        move || parse_html_sync(&body_clone)
                                    }).await.map_err(|e_spawn| format!("spawn_blocking: {e_spawn:?}"))?;
                                    node.link_subpage = Some(Box::new(subdom));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// テキスト整形
fn clean_text(raw: &str) -> String {
    let replaced = raw.replace('\n', " ");
    let re = Regex::new(r"\s+").unwrap();
    re.replace_all(&replaced, " ").trim().to_string()
}

/// スキップ対象タグ
fn skip_tag(tag_name: &str) -> bool {
    matches!(
        tag_name,
        "script" | "style" | "meta" | "link" | "noscript" |
        "svg" | "iframe" |
        "nav" | "footer" | "header"
    )
}

/// パース対象タグ
fn is_target_tag(tag_name: &str) -> bool {
    matches!(
        tag_name,
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" |
        "p" |
        "ul" | "ol" | "li" |
        "a"
    )
}

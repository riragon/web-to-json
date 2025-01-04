use actix_web::{
    web, App, HttpResponse, HttpServer, Responder,
};
use serde::{Serialize, Deserialize};
use regex::Regex;
use tokio::task::{spawn, spawn_blocking};
use std::time::Duration;
use open;
use url::Url;

// ここを正しく修正
use sanitize_filename::sanitize;

// scraper で必要な型まとめ
use scraper::{Html, Selector, ElementRef, Node};

/// フォーム入力用
#[derive(Deserialize)]
struct UrlForm {
    url: String,
}

/// JSON 出力用データ (DOMツリー + 1階層リンク先)
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
    children: Vec<DomNode>,

    #[serde(skip_serializing_if = "Option::is_none")]
    link_subpage: Option<Box<DomNode>>,
}

/// (GET) フォーム表示
async fn show_form() -> impl Responder {
    let html = r#"
<!DOCTYPE html>
<html><head><meta charset="UTF-8"/><title>Web to JSON</title></head>
<body>
 <h1>URLを入力</h1>
 <form action="/" method="post">
   <input type="text" name="url" size="50"/>
   <button type="submit">JSON変換</button>
 </form>
</body></html>
    "#;

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(html)
}

/// (POST) URLを受け取り → HTML を同期パース → 1階層リンク先も解析 → JSONを1行で表示
/// ブラウザでダウンロード（Blob方式）＆テキストコピー
async fn process_form(form: web::Form<UrlForm>) -> impl Responder {
    let url_str = &form.url;

    // 1. URLをパース
    let parsed_url = match Url::parse(url_str) {
        Ok(u) => u,
        Err(e) => {
            return HttpResponse::BadRequest().body(format!("URL parse error: {e}"));
        }
    };

    // 2. HTTP GET (非同期)
    let main_html = match reqwest::get(parsed_url.clone()).await {
        Ok(resp) => match resp.text().await {
            Ok(body) => body,
            Err(e) => {
                return HttpResponse::InternalServerError()
                    .body(format!("Error reading response: {e}"));
            }
        },
        Err(e) => {
            return HttpResponse::InternalServerError()
                .body(format!("Request error: {e}"));
        }
    };

    // 3. 同期パース (spawn_blocking)
    let mut dom_tree = match spawn_blocking(move || parse_html_sync(&main_html)).await {
        Ok(tree) => tree,
        Err(e) => {
            return HttpResponse::InternalServerError()
                .body(format!("spawn_blocking error: {e:?}"));
        }
    };

    // 4. 1階層リンク先を取得 (BFS + spawn_blocking)
    if let Err(e) = fetch_subpages_for_depth_one(&mut dom_tree, &parsed_url).await {
        return HttpResponse::InternalServerError()
            .body(format!("Subpage fetch error: {e}"));
    }

    // 5. JSON を1行でシリアライズ
    let json_str = match serde_json::to_string(&dom_tree) {
        Ok(j) => j,  // ← 1行の JSON
        Err(e) => {
            return HttpResponse::InternalServerError()
                .body(format!("JSON serialize error: {e}"));
        }
    };

    // 6. ファイル名 (ダウンロード用)
    let domain = parsed_url.host_str().unwrap_or("nodomain").to_string();
    let file_name = format!("{}_{}.json",
        sanitize(domain),
        get_last_path_segment(url_str).unwrap_or_else(|| "nopath".to_string())
    );

    // 7. テキスト表示用にエスケープ
    let escaped_json = json_str
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");

    // HTMLレスポンス
    let response_html = format!(
        r#"
<!DOCTYPE html>
<html><head><meta charset="UTF-8"/><title>Web to JSON (1line JSON)</title></head>
<body>
  <h1>結果</h1>
  <p>"{url_str}" を解析し、1階層リンク先を含む JSON (1行) を生成しました。</p>

  <!-- ダウンロード (Blob方式) -->
  <button onclick="downloadJson()">JSONをダウンロード</button>

  <!-- テキスト表示 & コピー -->
  <hr/>
  <h2>JSONテキスト (コピー可, 1行表示)</h2>
  <textarea id="jsonText" rows="10" cols="90" style="white-space: pre;">{escaped_json}</textarea><br/>
  <button onclick="copyToClipboard()">コピー</button>

  <script>
    // ダウンロード: Blob -> createObjectURL -> aタグ.click()
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

    // テキストコピー
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
    <input type="text" name="url" size="50"/>
    <button type="submit">JSON変換</button>
  </form>
</body></html>
        "#,
        url_str = url_str,
        json_str = json_str,
        escaped_json = escaped_json,
        file_name = file_name
    );

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(response_html)
}

/// メイン関数 (Tokio マルチスレッドランタイム)
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
    spawn(async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _ = open::that("http://127.0.0.1:8080/");
    });

    server.await
}

// ========== 以下：同期パース + BFS ==========

fn parse_html_sync(html_str: &str) -> DomNode {
    let doc = Html::parse_document(html_str);
    let sel_html = Selector::parse("html").unwrap();
    if let Some(html_el) = doc.select(&sel_html).next() {
        DomNode {
            tag: Some("html".to_string()),
            href: None,
            text: None,
            children: parse_children(html_el),
            link_subpage: None,
        }
    } else {
        DomNode {
            tag: Some("html".to_string()),
            href: None,
            text: Some("(No <html> found)".to_string()),
            children: vec![],
            link_subpage: None,
        }
    }
}

fn parse_children(el: ElementRef) -> Vec<DomNode> {
    let mut result = Vec::new();
    for child in el.children() {
        match child.value() {
            Node::Element(ele_val) => {
                let tag_name = ele_val.name().to_lowercase();
                if is_target_tag(&tag_name) {
                    let mut link = None;
                    if tag_name == "a" {
                        for (attr_name, attr_value) in ele_val.attrs() {
                            if attr_name.eq_ignore_ascii_case("href") {
                                link = Some(attr_value.to_string());
                            }
                        }
                    }
                    let sub_nodes = parse_children(ElementRef::wrap(child).unwrap());
                    result.push(DomNode {
                        tag: Some(tag_name),
                        href: link,
                        text: None,
                        children: sub_nodes,
                        link_subpage: None,
                    });
                } else {
                    // 対象外タグ -> 子要素だけたどる
                    let sub = parse_children(ElementRef::wrap(child).unwrap());
                    result.extend(sub);
                }
            }
            Node::Text(txt) => {
                let c = clean_text(txt);
                if !c.is_empty() {
                    result.push(DomNode {
                        tag: None,
                        href: None,
                        text: Some(c),
                        children: vec![],
                        link_subpage: None,
                    });
                }
            }
            _ => {}
        }
    }
    result
}

/// イテレーティブ BFS: node.children / <a>タグを探してリンク先を parse_html_sync
async fn fetch_subpages_for_depth_one(root: &mut DomNode, base_url: &Url) -> Result<(), String> {
    let mut stack = vec![root as *mut DomNode];
    while let Some(ptr) = stack.pop() {
        let node = unsafe { &mut *ptr };

        // 子ノードを追加
        for child in node.children.iter_mut() {
            stack.push(child as *mut DomNode);
        }

        // <a>タグならリンク先を fetch → spawn_blocking で parse
        if let Some(ref t) = node.tag {
            if t == "a" {
                if let Some(ref href) = node.href {
                    let sub_url = base_url.join(href)
                        .map_err(|e| e.to_string())?;

                    let body = reqwest::get(sub_url.clone()).await
                        .map_err(|e| format!("fetch subpage error: {e}"))?
                        .text().await
                        .map_err(|e| format!("subpage .text() error: {e}"))?;

                    let subdom = spawn_blocking(move || parse_html_sync(&body))
                        .await
                        .map_err(|e| format!("spawn_blocking error: {e:?}"))?;

                    node.link_subpage = Some(Box::new(subdom));
                }
            }
        }
    }
    Ok(())
}

/// テキストの改行や連続空白を整形
fn clean_text(raw: &str) -> String {
    let replaced = raw.replace('\n', " ");
    let re = Regex::new(r"\s+").unwrap();
    re.replace_all(&replaced, " ").trim().to_string()
}

/// パース対象タグかどうか
fn is_target_tag(tag_name: &str) -> bool {
    matches!(
        tag_name,
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            | "p"
            | "ul" | "ol" | "li"
            | "a"
    )
}

/// URLの末尾パス要素 (例: index.html)
fn get_last_path_segment(url_str: &str) -> Option<String> {
    let parsed = Url::parse(url_str).ok()?;
    let mut segments = parsed.path_segments()?;
    segments.next_back().map(|s| s.to_string())
}

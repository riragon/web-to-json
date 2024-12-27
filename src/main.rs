use actix_web::{
    http::header::ContentType,
    web, App, HttpResponse, HttpServer, Responder,
};
use scraper::{ElementRef, Html, Node, Selector};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::task::spawn;
use open;
use regex::Regex;
use url::Url;
use sanitize_filename::sanitize;

/// フォームで受け取るデータ
#[derive(Deserialize)]
struct UrlForm {
    url: String,
}

/// JSON出力用のデータ構造
#[derive(Serialize, Debug)]
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
}

/// 対象とするタグかどうかを判定 (h1-h6, p, ul, ol, li, aなど)
fn is_target_tag(tag_name: &str) -> bool {
    matches!(
        tag_name,
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            | "p"
            | "ul" | "ol" | "li"
            | "a"
    )
}

/// テキストから改行をスペースにし、連続空白をまとめる
fn clean_text(raw: &str) -> String {
    let replaced = raw.replace('\n', " ");
    let re = Regex::new(r"\s+").unwrap();
    let single_spaced = re.replace_all(&replaced, " ");
    single_spaced.trim().to_string()
}

/// 再帰的に子ノードを解析 → Vec<DomNode>
fn parse_element_rec(el: ElementRef) -> Vec<DomNode> {
    let evalue = el.value();
    let tag_name = evalue.name().to_lowercase();

    let mut children_nodes = vec![];
    for child in el.children() {
        match child.value() {
            Node::Element(_) => {
                if let Some(child_el) = ElementRef::wrap(child) {
                    let mut sub = parse_element_rec(child_el);
                    children_nodes.append(&mut sub);
                }
            }
            Node::Text(txt) => {
                let cleaned = clean_text(txt);
                if !cleaned.is_empty() {
                    children_nodes.push(DomNode {
                        tag: None,
                        href: None,
                        text: Some(cleaned),
                        children: vec![],
                    });
                }
            }
            _ => {}
        }
    }

    // 対象タグならDomNode生成、そうでなければ子どもだけを返す
    if is_target_tag(&tag_name) {
        let mut link = None;
        if tag_name == "a" {
            for (attr_name, attr_value) in evalue.attrs() {
                if attr_name.eq_ignore_ascii_case("href") {
                    link = Some(attr_value.to_string());
                }
            }
        }
        vec![DomNode {
            tag: Some(tag_name),
            href: link,
            text: None,
            children: children_nodes,
        }]
    } else {
        children_nodes
    }
}

/// HTML文字列をパースして DomNode を構築
fn parse_html_dom(html_str: &str) -> DomNode {
    let doc = Html::parse_document(html_str);
    let sel_html = Selector::parse("html").unwrap();

    if let Some(html_el) = doc.select(&sel_html).next() {
        let kids = parse_element_rec(html_el);
        DomNode {
            tag: Some("html".to_string()),
            href: None,
            text: None,
            children: kids,
        }
    } else {
        DomNode {
            tag: Some("html".to_string()),
            href: None,
            text: Some("(No <html> found)".to_string()),
            children: vec![],
        }
    }
}

/// URL の末尾パス要素を取得 (例: "apinode.htm" など)
fn get_last_path_segment(url_str: &str) -> Option<String> {
    let parsed = Url::parse(url_str).ok()?;
    let mut segments = parsed.path_segments()?;
    segments.next_back().map(|s| s.to_string())
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
        .content_type(ContentType::html())
        .body(html)
}

/// (POST) 同ページでフォーム→JSON化→結果表示
async fn process_form(form: web::Form<UrlForm>) -> impl Responder {
    let url_str = &form.url;

    // 1. 指定URLのHTMLを取得
    let body = match reqwest::get(url_str).await {
        Ok(resp) => match resp.text().await {
            Ok(text) => text,
            Err(e) => {
                return HttpResponse::InternalServerError().body(format!("Error reading response: {e}"));
            }
        },
        Err(e) => {
            return HttpResponse::InternalServerError().body(format!("Request error: {e}"));
        }
    };

    // 2. 保存用ファイル名
    let domain = Url::parse(url_str)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "nodomain".to_string());

    let last_seg = get_last_path_segment(url_str).unwrap_or_else(|| "nopath".to_string());
    let filename_part = sanitize(format!("{}_{}", domain, last_seg)); // 例: "rchelp.capturingreality.com_apinode.htm"
    let file_name = format!("{filename_part}.json"); // 例: "rchelp.capturingreality.com_apinode.htm.json"

    // 3. HTMLをDomNodeにパース → JSON化(1行)
    let dom_tree = parse_html_dom(&body);
    let json = match serde_json::to_string(&dom_tree) {
        Ok(j) => j,
        Err(e) => {
            return HttpResponse::InternalServerError().body(format!("JSON error: {e}"));
        }
    };

    // 4. exeファイルと同じフォルダに出力する
    let exe_path = env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = exe_path.parent().unwrap_or_else(|| Path::new("."));
    let file_path = exe_dir.join(&file_name);

    match File::create(&file_path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(json.as_bytes()) {
                return HttpResponse::InternalServerError().body(format!("Write error: {e}"));
            }
        }
        Err(e) => {
            return HttpResponse::InternalServerError().body(format!("Create file error: {e}"));
        }
    }

    // 5. 成功メッセージを同じページに表示
    let response_html = format!(
        r#"
<!DOCTYPE html>
<html><head><meta charset="UTF-8"/><title>Web to JSON</title></head>
<body>
  <h1>URLを入力</h1>
  <form action="/" method="post">
    <input type="text" name="url" size="50"/>
    <button type="submit">JSON変換</button>
  </form>
  <hr/>
  <p>"{url_str}" → "{file_name}" に保存しました (1行JSON)</p>
</body></html>
        "#
    );

    HttpResponse::Ok()
        .content_type(ContentType::html())
        .body(response_html)
}

/// メイン関数
#[tokio::main]
async fn main() -> std::io::Result<()> {
    let server = HttpServer::new(|| {
        App::new()
            // GET / : フォーム表示
            .route("/", web::get().to(show_form))
            // POST / : 同じURLで処理・結果表示
            .route("/", web::post().to(process_form))
    })
    .bind(("127.0.0.1", 8080))?
    .run();

    // サーバ起動後にブラウザを自動で開く
    spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let _ = open::that("http://127.0.0.1:8080/");
    });

    server.await
}

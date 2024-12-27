use actix_files::NamedFile;
use actix_web::{
    http::header::{ContentDisposition, DispositionParam, DispositionType},
    web, App, HttpResponse, HttpServer, Responder, Result,
};
use scraper::{ElementRef, Html, Node, Selector};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use tokio::task::spawn;
use open;
use regex::Regex;
use url::Url;
use sanitize_filename::sanitize;

/// フォームで受け取る
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

/// 必要なタグだけ残す (h1-h6, p, ul, ol, li, aなど)
fn is_target_tag(tag_name: &str) -> bool {
    matches!(
        tag_name,
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            | "p"
            | "ul" | "ol" | "li"
            | "a"
    )
}

/// テキストから `\n` をスペースへ置換し、連続空白をまとめる
fn clean_text(raw: &str) -> String {
    // 改行をスペースに
    let replaced = raw.replace('\n', " ");
    // 正規表現で連続空白を1つに
    let re = Regex::new(r"\s+").unwrap();
    let single_spaced = re.replace_all(&replaced, " ");
    // 前後をtrim
    single_spaced.trim().to_string()
}

/// 再帰的に子ノードを解析
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

    // ホワイトリストタグなら DomNode を生成
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
        // スキップ対象タグは自分を作らず、子だけ返す
        children_nodes
    }
}

/// HTML全体をパース → DomNode
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

/// URL末尾パス要素 (例: rccmd.htm)
fn get_last_path_segment(url_str: &str) -> Option<String> {
    let parsed = Url::parse(url_str).ok()?;
    let mut segments = parsed.path_segments()?;
    segments.next_back().map(|s| s.to_string())
}

/// トップ: URL入力フォーム & ダウンロードボタン
async fn index() -> impl Responder {
    let html = r#"
<!DOCTYPE html>
<html><head><meta charset="UTF-8"/><title>Web to JSON</title></head>
<body>
 <h1>URLを入力</h1>
 <form action="/convert" method="post">
   <input type="text" name="url" size="50"/>
   <button type="submit">JSON変換</button>
 </form>
 <hr/>
 <button onclick="location.href='/download'">JSONファイルを保存</button>
</body></html>
    "#;
    HttpResponse::Ok().content_type("text/html; charset=utf-8").body(html)
}

/// HTML→DomNode→1行JSON → ファイル保存
async fn convert(form: web::Form<UrlForm>) -> impl Responder {
    let url_str = &form.url;

    // ページ取得
    let body = match reqwest::get(url_str).await {
        Ok(resp) => match resp.text().await {
            Ok(text) => text,
            Err(e) => return HttpResponse::InternalServerError().body(format!("Error reading: {e}")),
        },
        Err(e) => return HttpResponse::InternalServerError().body(format!("Request error: {e}")),
    };

    // ドメイン
    let domain = Url::parse(url_str)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "nodomain".into());

    // 末尾パス要素
    let last_seg = get_last_path_segment(url_str).unwrap_or_else(|| "nopath".into());

    // ファイル名 "domain_lastseg.json"
    let filename_part = sanitize(format!("{}_{}", domain, last_seg));
    let file_name = format!("{filename_part}.json");

    // HTMLパース
    let dom_tree = parse_html_dom(&body);

    // **ここがポイント** : 1行JSONにする
    let json = match serde_json::to_string(&dom_tree) {
        Ok(j) => j,
        Err(e) => return HttpResponse::InternalServerError().body(format!("JSON error: {e}")),
    };

    // ファイル保存
    match File::create(&file_name) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(json.as_bytes()) {
                return HttpResponse::InternalServerError().body(format!("Write error: {e}"));
            }
        }
        Err(e) => return HttpResponse::InternalServerError().body(format!("Create file error: {e}")),
    }

    // 結果表示
    let msg = format!(
        r#"
        <html><body>
          <h2>"{url_str}" → "{file_name}" に保存しました (1行JSON)</h2>
          <p><a href="/">別のURLを変換する</a></p>
        </body></html>
        "#
    );
    HttpResponse::Ok().content_type("text/html; charset=utf-8").body(msg)
}

/// ダウンロード: 常に "webpage_data.json" を返す例
async fn download_file() -> Result<NamedFile> {
    let path: PathBuf = PathBuf::from("webpage_data.json");
    let file = NamedFile::open(path)?;
    Ok(file.set_content_disposition(ContentDisposition {
        disposition: DispositionType::Attachment,
        parameters: vec![DispositionParam::Filename(
            "webpage_data.json".to_owned(),
        )],
    }))
}

/// メイン関数
#[tokio::main]
async fn main() -> std::io::Result<()> {
    let server = HttpServer::new(|| {
        App::new()
            .route("/", web::get().to(index))
            .route("/convert", web::post().to(convert))
            .route("/download", web::get().to(download_file))
    })
    .bind(("127.0.0.1", 8080))?
    .run();

    // サーバ起動後にブラウザを自動起動
    spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let _ = open::that("http://127.0.0.1:8080/");
    });

    server.await
}

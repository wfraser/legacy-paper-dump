use anyhow::{Context, Result};
use dropbox_sdk::default_client::UserAuthDefaultClient;
use dropbox_sdk::paper::{self, ExportFormat, ListPaperDocsArgs, ListPaperDocsContinueArgs, PaperDocExport};
use std::env;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

fn get_oauth2_token() -> String {
    env::var("DBX_OAUTH_TOKEN")
        .expect("need environment variable DBX_OAUTH_TOKEN to be set")
}

fn main() -> Result<()> {
    let mut export = false;
    if env::args().skip(1).next().as_deref() == Some("--export") {
        export = true;
    }

    let client = UserAuthDefaultClient::new(get_oauth2_token());

    if export {
        let _ = fs::create_dir("docs");
    }

    let mut result = paper::docs_list(&client, &ListPaperDocsArgs::default())
        .context("paper/docs/list HTTP or transport err")?
        .context("paper/docs/list API err")?;
    let mut ids = result.doc_ids;
    while result.has_more {
        result = paper::docs_list_continue(&client, &ListPaperDocsContinueArgs::new(
                result.cursor.value))
            .context("paper/docs/list/continue HTTP or transport err")?
            .context("paper/docs/list/continue API err")?;
        ids.extend_from_slice(&result.doc_ids);
    }

    'doc: for id in &ids {
        println!();
        println!("https://paper.dropbox.com/doc/{}", id);

        let mut failures = 0;
        let mut export_result = loop {
            if failures >= 3 {
                println!("too many errors; skipping doc");
                continue 'doc;
            }

            match paper::docs_download(
                &client,
                &PaperDocExport::new(id.to_owned(), ExportFormat::Html),
                if export { None } else { Some(0) },
                if export { None } else { Some(0) },
            ) {
                Ok(Ok(result)) => break result,
                Ok(Err(api_err)) => {
                    println!("API error: {}", api_err);
                    // Not retriable. Skip this doc.
                    continue 'doc;
                }
                Err(dropbox_sdk::Error::ServerError(_)) => {
                    // Don't print the error; it's got a big HTML page text in it.
                    println!("HTTP 503");
                }
                Err(e) => {
                    println!("HTTP transport error: {}", e);
                }
            }
            failures += 1;
            thread::sleep(Duration::from_secs(3));
        };

        println!("title: {}", export_result.result.title);
        println!("owner: {}", export_result.result.owner);

        if !export {
            continue;
        }

        let path = PathBuf::from("docs")
            .join(format!("{} ({}).html", export_result.result.title.replace('/', "_"), id));
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(mut f) => {
                if let Err(e) = io::copy(export_result.body.as_mut().expect("missing body"), &mut f) {
                    println!("I/O error writing doc {:?}: {}", path, e);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                println!("file already downloaded; skipping");
            }
            Err(e) => {
                println!("failed to create file {:?}: {}", path, e);
            }
        }
    }

    Ok(())
}

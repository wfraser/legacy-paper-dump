use anyhow::{Context, Result};
use dropbox_sdk::default_client::UserAuthDefaultClient;
use dropbox_sdk::paper::{self, ExportFormat, ListPaperDocsArgs, ListPaperDocsContinueArgs, PaperDocExport};
use std::env;

fn get_oauth2_token() -> String {
    env::var("DBX_OAUTH_TOKEN")
        .expect("need environment variable DBX_OAUTH_TOKEN to be set")
}

fn main() -> Result<()> {
    let client = UserAuthDefaultClient::new(get_oauth2_token());

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
        let export_result = loop {
            if failures >= 3 {
                continue 'doc;
            }

            match paper::docs_download(
                &client,
                &PaperDocExport::new(id.to_owned(), ExportFormat::Html),
                Some(0), // don't need the content
                Some(0), // so just request zero bytes
            ) {
                Ok(Ok(result)) => break result.result,
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
            std::thread::sleep(std::time::Duration::from_secs(3));
        };
        println!("title: {}", export_result.title);
        println!("owner: {}", export_result.owner);
    }

    Ok(())
}

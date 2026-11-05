use anyhow::{Context as _, Result, ensure};
use futures::{AsyncBufRead, AsyncBufReadExt as _};
use serde_json::Value;

pub(crate) async fn read(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Value> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        ensure!(
            !available.is_empty(),
            if bytes.is_empty() {
                "Codex closed stdout"
            } else {
                "Truncated Codex frame"
            }
        );
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        // Inline images make otherwise normal history replies arbitrarily large.
        // Grow on demand rather than impose a protocol limit or discard a reply.
        bytes
            .try_reserve(consumed)
            .context("Not enough memory to receive Codex frame")?;
        bytes.extend_from_slice(&available[..consumed]);
        reader.consume_unpin(consumed);
        if newline.is_some() {
            return serde_json::from_slice(&bytes).context("Invalid Codex JSONL frame");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_large_response_and_preserves_the_next_frame() {
        futures::executor::block_on(async {
            let payload = "x".repeat(17 * 1024 * 1024);
            let mut reader = futures::io::BufReader::with_capacity(
                8191,
                futures::io::Cursor::new(format!(
                    "{{\"result\":\"{payload}\",\"id\":7}}\n{{\"id\":8,\"result\":true}}\n"
                )),
            );
            let response = read(&mut reader).await.unwrap();
            assert_eq!(response["id"], 7);
            assert_eq!(response["result"].as_str().unwrap(), payload);
            drop(response);
            assert_eq!(read(&mut reader).await.unwrap()["id"], 8);
        });
    }

    #[test]
    fn large_notifications_and_server_requests_are_not_discarded() {
        futures::executor::block_on(async {
            let payload = "x".repeat(17 * 1024 * 1024);
            for header in [
                r#"{"method":"notice","params":""#,
                r#"{"id":"approval-id","method":"approval","params":""#,
            ] {
                let mut reader = futures::io::Cursor::new(format!("{header}{payload}\"}}\n"));
                let frame = read(&mut reader).await.unwrap();
                assert_eq!(frame["params"].as_str().unwrap(), payload);
                assert!(frame["method"].is_string());
            }
        });
    }

    #[test]
    fn rejects_invalid_json_and_truncated_frames() {
        futures::executor::block_on(async {
            for payload in ["", "not-json\n", "{\"id\":1}", "{\"id\":"] {
                assert!(read(&mut futures::io::Cursor::new(payload)).await.is_err());
            }
        });
    }

    #[test]
    fn accepts_frames_beyond_the_previous_discard_budget() {
        use std::io::Read as _;
        futures::executor::block_on(async {
            let length = 129 * 1024 * 1024;
            let input = std::io::Cursor::new(b"{\"id\":9,\"result\":\"")
                .chain(std::io::repeat(b'x').take(length))
                .chain(std::io::Cursor::new(b"\"}\n"));
            let mut reader = futures::io::BufReader::new(futures::io::AllowStdIo::new(input));
            let frame = read(&mut reader).await.unwrap();
            assert_eq!(frame["result"].as_str().unwrap().len() as u64, length);
        });
    }
}

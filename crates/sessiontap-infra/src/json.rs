use serde::Serialize;
use std::io;
use tokio::io::{AsyncWrite, AsyncWriteExt};

/// Writes `value` as one JSON document followed by a newline.
pub async fn write_json_line<W, T>(write: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
    T: Serialize + ?Sized,
{
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    write.write_all(&line).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_one_line_per_value() {
        let mut out = Vec::new();
        write_json_line(&mut out, &serde_json::json!({"a": 1}))
            .await
            .unwrap();
        write_json_line(&mut out, "b").await.unwrap();
        assert_eq!(out, b"{\"a\":1}\n\"b\"\n");
    }
}

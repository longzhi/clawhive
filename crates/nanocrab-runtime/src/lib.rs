use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait TaskExecutor: Send + Sync {
    async fn execute(&self, input: &str) -> Result<String>;
}

pub struct NativeExecutor;

#[async_trait]
impl TaskExecutor for NativeExecutor {
    async fn execute(&self, input: &str) -> Result<String> {
        Ok(input.to_string())
    }
}

pub struct WasmExecutor;

#[async_trait]
impl TaskExecutor for WasmExecutor {
    async fn execute(&self, _input: &str) -> Result<String> {
        anyhow::bail!("WASM executor not implemented yet")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_executor_passthrough() {
        let exec = NativeExecutor;
        let result = exec.execute("hello world").await.unwrap();
        assert_eq!(result, "hello world");
    }

    #[tokio::test]
    async fn wasm_executor_not_implemented() {
        let exec = WasmExecutor;
        let result = exec.execute("test").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not implemented"));
    }
}

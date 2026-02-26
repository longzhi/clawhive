use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait TaskExecutor: Send + Sync {
    /// Pre-process user input before sending to LLM.
    /// NativeExecutor: passthrough. WasmExecutor: sandboxed transform.
    async fn preprocess_input(&self, input: &str) -> Result<String>;

    /// Post-process LLM output before returning to user.
    /// NativeExecutor: passthrough. WasmExecutor: sandboxed transform.
    async fn postprocess_output(&self, output: &str) -> Result<String>;
}

pub struct NativeExecutor;

#[async_trait]
impl TaskExecutor for NativeExecutor {
    async fn preprocess_input(&self, input: &str) -> Result<String> {
        Ok(input.to_string())
    }

    async fn postprocess_output(&self, output: &str) -> Result<String> {
        Ok(output.to_string())
    }
}

pub struct WasmExecutor;

#[async_trait]
impl TaskExecutor for WasmExecutor {
    async fn preprocess_input(&self, _input: &str) -> Result<String> {
        anyhow::bail!("WASM executor not implemented yet")
    }

    async fn postprocess_output(&self, _output: &str) -> Result<String> {
        anyhow::bail!("WASM executor not implemented yet")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_preprocess_passthrough() {
        let exec = NativeExecutor;
        let result = exec.preprocess_input("hello world").await.unwrap();
        assert_eq!(result, "hello world");
    }

    #[tokio::test]
    async fn native_postprocess_passthrough() {
        let exec = NativeExecutor;
        let result = exec.postprocess_output("response text").await.unwrap();
        assert_eq!(result, "response text");
    }

    #[tokio::test]
    async fn wasm_preprocess_not_implemented() {
        let exec = WasmExecutor;
        let result = exec.preprocess_input("test").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not implemented"));
    }

    #[tokio::test]
    async fn wasm_postprocess_not_implemented() {
        let exec = WasmExecutor;
        let result = exec.postprocess_output("test").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not implemented"));
    }
}

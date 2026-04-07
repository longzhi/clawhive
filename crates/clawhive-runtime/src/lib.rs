use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait TaskExecutor: Send + Sync {
    /// Pre-process user input before sending to LLM.
    async fn preprocess_input(&self, input: &str) -> Result<String>;

    /// Post-process LLM output before returning to user.
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
}

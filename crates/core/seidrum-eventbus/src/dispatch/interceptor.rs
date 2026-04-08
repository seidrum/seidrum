use async_trait::async_trait;

/// The result of an interceptor processing an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterceptResult {
    /// Event was modified in place. Continue the chain.
    Modified,
    /// Event passes through unchanged. Continue the chain.
    Pass,
    /// Event is dropped. Abort the chain, do not deliver to
    /// async subscribers.
    Drop,
}

/// A sync interceptor that can inspect, modify, or drop events
/// as they pass through the dispatch pipeline.
///
/// Interceptors are called sequentially in priority order (lower number = first).
/// Each interceptor gets mutable access to the payload and can:
/// - Return `Pass` to continue unchanged
/// - Mutate `payload` in place and return `Modified` to propagate changes
/// - Return `Drop` to abort the entire dispatch chain
#[async_trait]
pub trait Interceptor: Send + Sync + 'static {
    /// Process the event. The payload can be mutated in place.
    async fn intercept(&self, subject: &str, payload: &mut Vec<u8>) -> InterceptResult;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    struct PassInterceptor;

    #[async_trait]
    impl Interceptor for PassInterceptor {
        async fn intercept(&self, _subject: &str, _payload: &mut Vec<u8>) -> InterceptResult {
            InterceptResult::Pass
        }
    }

    struct UppercaseInterceptor;

    #[async_trait]
    impl Interceptor for UppercaseInterceptor {
        async fn intercept(&self, _subject: &str, payload: &mut Vec<u8>) -> InterceptResult {
            payload.iter_mut().for_each(|b| *b = b.to_ascii_uppercase());
            InterceptResult::Modified
        }
    }

    struct DropInterceptor;

    #[async_trait]
    impl Interceptor for DropInterceptor {
        async fn intercept(&self, _subject: &str, _payload: &mut Vec<u8>) -> InterceptResult {
            InterceptResult::Drop
        }
    }

    struct CountingInterceptor {
        count: Arc<AtomicU32>,
    }

    #[async_trait]
    impl Interceptor for CountingInterceptor {
        async fn intercept(&self, _subject: &str, _payload: &mut Vec<u8>) -> InterceptResult {
            self.count.fetch_add(1, Ordering::SeqCst);
            InterceptResult::Pass
        }
    }

    #[tokio::test]
    async fn test_pass_interceptor() {
        let interceptor = PassInterceptor;
        let mut payload = b"hello".to_vec();
        let result = interceptor.intercept("test", &mut payload).await;
        assert_eq!(result, InterceptResult::Pass);
        assert_eq!(payload, b"hello");
    }

    #[tokio::test]
    async fn test_modify_interceptor() {
        let interceptor = UppercaseInterceptor;
        let mut payload = b"hello".to_vec();
        let result = interceptor.intercept("test", &mut payload).await;
        assert_eq!(result, InterceptResult::Modified);
        assert_eq!(payload, b"HELLO");
    }

    #[tokio::test]
    async fn test_drop_interceptor() {
        let interceptor = DropInterceptor;
        let mut payload = b"hello".to_vec();
        let result = interceptor.intercept("test", &mut payload).await;
        assert_eq!(result, InterceptResult::Drop);
    }

    #[tokio::test]
    async fn test_counting_interceptor() {
        let count = Arc::new(AtomicU32::new(0));
        let interceptor = CountingInterceptor {
            count: Arc::clone(&count),
        };
        let mut payload = b"hello".to_vec();
        interceptor.intercept("test", &mut payload).await;
        interceptor.intercept("test", &mut payload).await;
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}

//! Integration tests for API gateway security features.

#[cfg(test)]
mod tests {
    use seidrum_api_gateway::audit::{AuditEntryBuilder, AuditLog};
    use seidrum_api_gateway::auth::AuthHandler;
    use seidrum_api_gateway::jwt::JwtService;
    use seidrum_api_gateway::rate_limiter::{RateLimitConfig, RateLimiter};

    #[test]
    fn test_auth_handler_api_key() {
        let handler = AuthHandler::new("test-secret".to_string(), None, 3600);
        let result = handler.authenticate("", Some("test-secret".to_string()));
        assert!(result.is_some());
        let auth = result.unwrap();
        assert_eq!(auth.role, "admin");
    }

    #[test]
    fn test_auth_handler_jwt() {
        let jwt_secret = "jwt-secret";
        let handler = AuthHandler::new("api-key".to_string(), Some(jwt_secret.to_string()), 3600);

        let jwt_svc = handler.jwt_service.as_ref().unwrap();
        let token = jwt_svc
            .generate_token("testuser", "user", vec!["read".to_string()])
            .unwrap();

        let result = handler.authenticate(&format!("Bearer {}", token), None);
        assert!(result.is_some());
        let auth = result.unwrap();
        assert_eq!(auth.subject, "testuser");
        assert_eq!(auth.role, "user");
    }

    #[test]
    fn test_jwt_generation_and_validation() {
        let jwt = JwtService::new("secret123", 3600);
        let token = jwt
            .generate_token("user1", "admin", vec!["scope_root".to_string()])
            .unwrap();

        let claims = jwt.validate_token(&token).unwrap();
        assert_eq!(claims.sub, "user1");
        assert_eq!(claims.role, "admin");
        assert_eq!(claims.scopes, vec!["scope_root"]);
    }

    #[test]
    fn test_jwt_expiration() {
        let jwt = JwtService::new("secret123", 0);
        let token = jwt.generate_token("user1", "admin", vec![]).unwrap();

        std::thread::sleep(std::time::Duration::from_secs(1));
        let result = jwt.validate_token(&token);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_rate_limiter_basic() {
        let config = RateLimitConfig {
            regular_rpm: 3,
            admin_rpm: 10,
            cleanup_interval_secs: 300,
        };
        let limiter = RateLimiter::new(config);

        // First 3 requests should pass for regular user
        for i in 0..3 {
            let (allowed, _, _) = limiter.check_rate_limit("user1", false).await;
            assert!(allowed, "Request {} should be allowed", i + 1);
        }

        // 4th request should fail
        let (allowed, _, retry) = limiter.check_rate_limit("user1", false).await;
        assert!(!allowed);
        assert!(retry.is_some());
    }

    #[tokio::test]
    async fn test_rate_limiter_admin_higher() {
        let config = RateLimitConfig {
            regular_rpm: 1,
            admin_rpm: 5,
            cleanup_interval_secs: 300,
        };
        let limiter = RateLimiter::new(config);

        // Admin should get more requests
        let (allowed1, _, _) = limiter.check_rate_limit("admin", true).await;
        let (allowed2, _, _) = limiter.check_rate_limit("admin", true).await;
        assert!(allowed1);
        assert!(allowed2);

        // Regular user gets fewer
        let (allowed1, _, _) = limiter.check_rate_limit("user", false).await;
        let (allowed2, _, _) = limiter.check_rate_limit("user", false).await;
        assert!(allowed1);
        assert!(!allowed2);
    }

    #[tokio::test]
    async fn test_audit_log_logging() {
        let log = AuditLog::new(100);

        let entry = AuditEntryBuilder::new("test.action", "user1", "resource1")
            .method("POST")
            .path("/api/v1/test")
            .status(200)
            .build();

        log.log(entry.clone()).await;
        assert_eq!(log.count().await, 1);

        let results = log.query(10, None).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].action, "test.action");
        assert_eq!(results[0].subject, "user1");
    }

    #[tokio::test]
    async fn test_audit_log_max_entries() {
        let log = AuditLog::new(5);

        // Log 10 entries but max is 5
        for i in 0..10 {
            let entry = AuditEntryBuilder::new("action", &format!("user{}", i), "resource").build();
            log.log(entry).await;
        }

        assert_eq!(log.count().await, 5);
    }

    #[tokio::test]
    async fn test_audit_log_ordering() {
        let log = AuditLog::new(100);

        // Log 3 entries
        for i in 0..3 {
            let entry = AuditEntryBuilder::new("action", &format!("user{}", i), "resource").build();
            log.log(entry).await;
        }

        let results = log.query(10, None).await;
        // Most recent first
        assert_eq!(results[0].subject, "user2");
        assert_eq!(results[1].subject, "user1");
        assert_eq!(results[2].subject, "user0");
    }

    #[test]
    fn test_auth_handler_invalid_credentials() {
        let handler = AuthHandler::new("correct-key".to_string(), None, 3600);
        let result = handler.authenticate("", Some("wrong-key".to_string()));
        assert!(result.is_none());
    }
}

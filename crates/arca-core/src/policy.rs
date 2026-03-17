//! IAM-compatible policy types and evaluation engine.
//!
//! This module is pure (zero I/O) and independently testable.
//! It implements AWS IAM policy document parsing, validation,
//! and evaluation with deny-overrides semantics.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// S3 actions
// ---------------------------------------------------------------------------

/// Known S3 action strings.
pub mod actions {
    // Bucket actions
    pub const S3_CREATE_BUCKET: &str = "s3:CreateBucket";
    pub const S3_DELETE_BUCKET: &str = "s3:DeleteBucket";
    pub const S3_LIST_BUCKET: &str = "s3:ListBucket";
    pub const S3_LIST_ALL_MY_BUCKETS: &str = "s3:ListAllMyBuckets";
    pub const S3_GET_BUCKET_LOCATION: &str = "s3:GetBucketLocation";
    pub const S3_GET_BUCKET_ENCRYPTION: &str = "s3:GetBucketEncryption";
    pub const S3_PUT_BUCKET_ENCRYPTION: &str = "s3:PutBucketEncryption";
    pub const S3_DELETE_BUCKET_ENCRYPTION: &str = "s3:DeleteBucketEncryption";

    // Object actions
    pub const S3_GET_OBJECT: &str = "s3:GetObject";
    pub const S3_PUT_OBJECT: &str = "s3:PutObject";
    pub const S3_DELETE_OBJECT: &str = "s3:DeleteObject";

    // Admin actions
    pub const ARCA_VIEW_SERVER_INFO: &str = "arca:ViewServerInfo";
    pub const ARCA_MANAGE_USERS: &str = "arca:ManageUsers";
    pub const ARCA_MANAGE_TEAMS: &str = "arca:ManageTeams";
    pub const ARCA_MANAGE_GRANTS: &str = "arca:ManageGrants";
    pub const ARCA_MANAGE_CREDENTIALS: &str = "arca:ManageCredentials";
    pub const ARCA_CREATE_PRESIGNED_URL: &str = "arca:CreatePresignedUrl";
    pub const ARCA_CREATE_ARCHIVE: &str = "arca:CreateArchive";

    /// All known action strings for validation.
    pub const ALL_KNOWN: &[&str] = &[
        S3_CREATE_BUCKET,
        S3_DELETE_BUCKET,
        S3_LIST_BUCKET,
        S3_LIST_ALL_MY_BUCKETS,
        S3_GET_BUCKET_LOCATION,
        S3_GET_BUCKET_ENCRYPTION,
        S3_PUT_BUCKET_ENCRYPTION,
        S3_DELETE_BUCKET_ENCRYPTION,
        S3_GET_OBJECT,
        S3_PUT_OBJECT,
        S3_DELETE_OBJECT,
        ARCA_VIEW_SERVER_INFO,
        ARCA_MANAGE_USERS,
        ARCA_MANAGE_TEAMS,
        ARCA_MANAGE_GRANTS,
        ARCA_MANAGE_CREDENTIALS,
        ARCA_CREATE_PRESIGNED_URL,
        ARCA_CREATE_ARCHIVE,
    ];

    /// Returns true if the action is a known action or a valid wildcard pattern.
    pub fn is_valid_action(action: &str) -> bool {
        if action == "*" || action == "s3:*" || action == "arca:*" {
            return true;
        }
        ALL_KNOWN.contains(&action)
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Effect of a policy statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    Allow,
    Deny,
}

/// A single statement in a policy document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Statement {
    /// Optional statement identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,

    pub effect: Effect,

    /// Actions this statement applies to (e.g. `["s3:GetObject", "s3:PutObject"]`).
    /// Supports wildcards: `"s3:*"`, `"*"`.
    #[serde(with = "string_or_array")]
    pub action: Vec<String>,

    /// Resources this statement applies to (ARN patterns).
    /// Supports wildcards: `"*"`, `"arn:aws:s3:::bucket/*"`.
    #[serde(with = "string_or_array")]
    pub resource: Vec<String>,
}

/// An IAM-compatible policy document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PolicyDocument {
    /// Policy version (should be "2012-10-17").
    pub version: String,

    /// One or more policy statements.
    #[serde(with = "statement_or_array")]
    pub statement: Vec<Statement>,
}

/// Result of evaluating a single statement against an action/resource pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evaluation {
    /// The statement explicitly allows the action.
    Allow,
    /// The statement explicitly denies the action.
    Deny,
    /// The statement does not apply to this action/resource.
    NoMatch,
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

/// Matches an action pattern against a concrete action string.
///
/// Supports:
/// - Exact match: `"s3:GetObject"` matches `"s3:GetObject"`
/// - Full wildcard: `"*"` matches anything
/// - Service wildcard: `"s3:*"` matches `"s3:GetObject"`, `"s3:PutObject"`, etc.
/// - Prefix wildcard: `"s3:Get*"` matches `"s3:GetObject"`, `"s3:GetBucketLocation"`
///
/// Matching is case-sensitive.
pub fn matches_action(pattern: &str, action: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        action.starts_with(prefix)
    } else {
        pattern == action
    }
}

/// Matches a resource pattern against a concrete resource ARN.
///
/// Supports:
/// - Exact match: `"arn:aws:s3:::my-bucket"` matches `"arn:aws:s3:::my-bucket"`
/// - Full wildcard: `"*"` matches anything
/// - Suffix wildcard: `"arn:aws:s3:::my-bucket/*"` matches `"arn:aws:s3:::my-bucket/key"`
/// - Multi-segment wildcards via `*` (matches any sequence of characters)
/// - `?` matches exactly one character
///
/// Matching is case-sensitive.
pub fn matches_resource(pattern: &str, resource: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    glob_match(pattern, resource)
}

/// Simple glob matching supporting `*` (any chars) and `?` (one char).
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    let (plen, tlen) = (pat.len(), txt.len());
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0usize);

    while ti < tlen {
        if pi < plen && (pat[pi] == '?' || pat[pi] == txt[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < plen && pat[pi] == '*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < plen && pat[pi] == '*' {
        pi += 1;
    }
    pi == plen
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Evaluates a single statement against an action and resource.
pub fn evaluate_statement(stmt: &Statement, action: &str, resource: &str) -> Evaluation {
    let action_matches = stmt.action.iter().any(|p| matches_action(p, action));
    if !action_matches {
        return Evaluation::NoMatch;
    }

    let resource_matches = stmt.resource.iter().any(|p| matches_resource(p, resource));
    if !resource_matches {
        return Evaluation::NoMatch;
    }

    match stmt.effect {
        Effect::Allow => Evaluation::Allow,
        Effect::Deny => Evaluation::Deny,
    }
}

/// Evaluates a single policy document against an action and resource.
pub fn evaluate_policy(doc: &PolicyDocument, action: &str, resource: &str) -> Evaluation {
    let mut found_allow = false;

    for stmt in &doc.statement {
        match evaluate_statement(stmt, action, resource) {
            Evaluation::Deny => return Evaluation::Deny,
            Evaluation::Allow => found_allow = true,
            Evaluation::NoMatch => {}
        }
    }

    if found_allow {
        Evaluation::Allow
    } else {
        Evaluation::NoMatch
    }
}

/// Evaluates multiple policy documents (grants) against an action and resource.
///
/// Implements AWS-style deny-overrides evaluation:
/// 1. If any statement in any grant explicitly denies, the result is Deny.
/// 2. If at least one statement allows (and none deny), the result is Allow.
/// 3. If no statement matches, the result is NoMatch (implicit deny).
pub fn evaluate_grants(grants: &[PolicyDocument], action: &str, resource: &str) -> Evaluation {
    let mut found_allow = false;

    for doc in grants {
        match evaluate_policy(doc, action, resource) {
            Evaluation::Deny => return Evaluation::Deny,
            Evaluation::Allow => found_allow = true,
            Evaluation::NoMatch => {}
        }
    }

    if found_allow {
        Evaluation::Allow
    } else {
        Evaluation::NoMatch
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validates a policy document, returning a list of errors (empty = valid).
pub fn validate_policy_document(doc: &PolicyDocument) -> Vec<String> {
    let mut errors = Vec::new();

    if doc.version != "2012-10-17" {
        errors.push(format!(
            "Unsupported policy version \"{}\", expected \"2012-10-17\"",
            doc.version
        ));
    }

    if doc.statement.is_empty() {
        errors.push("Policy must contain at least one statement".to_string());
    }

    for (i, stmt) in doc.statement.iter().enumerate() {
        let label = stmt
            .sid
            .as_deref()
            .map(|s| format!("Statement \"{}\"", s))
            .unwrap_or_else(|| format!("Statement[{}]", i));

        if stmt.action.is_empty() {
            errors.push(format!("{}: Action must not be empty", label));
        }
        for action in &stmt.action {
            if !actions::is_valid_action(action) {
                errors.push(format!("{}: Unknown action \"{}\"", label, action));
            }
        }

        if stmt.resource.is_empty() {
            errors.push(format!("{}: Resource must not be empty", label));
        }
        for resource in &stmt.resource {
            if !is_valid_resource(resource) {
                errors.push(format!("{}: Invalid resource \"{}\"", label, resource));
            }
        }
    }

    errors
}

/// Returns true if the resource string is a valid ARN pattern or wildcard.
fn is_valid_resource(resource: &str) -> bool {
    if resource == "*" {
        return true;
    }
    // Must start with arn:aws:s3:::
    resource.starts_with("arn:aws:s3:::")
}

/// Parses a JSON string into a PolicyDocument.
pub fn parse_policy_document(json: &str) -> Result<PolicyDocument, String> {
    serde_json::from_str(json).map_err(|e| format!("Invalid policy JSON: {}", e))
}

// ---------------------------------------------------------------------------
// Serde helpers: Action/Resource can be a single string or an array
// ---------------------------------------------------------------------------

mod string_or_array {
    use serde::{self, Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(value: &[String], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.to_vec().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum StringOrArray {
            Single(String),
            Array(Vec<String>),
        }

        match StringOrArray::deserialize(deserializer)? {
            StringOrArray::Single(s) => Ok(vec![s]),
            StringOrArray::Array(a) => Ok(a),
        }
    }
}

/// Serde helper: Statement field can be a single statement or an array.
mod statement_or_array {
    use super::Statement;
    use serde::{self, Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(value: &[Statement], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.to_vec().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<Statement>, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum StatementOrArray {
            Single(Statement),
            Array(Vec<Statement>),
        }

        match StatementOrArray::deserialize(deserializer)? {
            StatementOrArray::Single(s) => Ok(vec![s]),
            StatementOrArray::Array(a) => Ok(a),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Action matching --

    #[test]
    fn action_exact_match() {
        assert!(matches_action("s3:GetObject", "s3:GetObject"));
        assert!(!matches_action("s3:GetObject", "s3:PutObject"));
    }

    #[test]
    fn action_full_wildcard() {
        assert!(matches_action("*", "s3:GetObject"));
        assert!(matches_action("*", "arca:ManageUsers"));
        assert!(matches_action("*", "anything"));
    }

    #[test]
    fn action_service_wildcard() {
        assert!(matches_action("s3:*", "s3:GetObject"));
        assert!(matches_action("s3:*", "s3:PutObject"));
        assert!(!matches_action("s3:*", "arca:ManageUsers"));
    }

    #[test]
    fn action_prefix_wildcard() {
        assert!(matches_action("s3:Get*", "s3:GetObject"));
        assert!(matches_action("s3:Get*", "s3:GetBucketLocation"));
        assert!(!matches_action("s3:Get*", "s3:PutObject"));
    }

    #[test]
    fn action_arca_wildcard() {
        assert!(matches_action("arca:*", "arca:ManageUsers"));
        assert!(matches_action("arca:*", "arca:ViewServerInfo"));
        assert!(!matches_action("arca:*", "s3:GetObject"));
    }

    #[test]
    fn action_case_sensitive() {
        assert!(!matches_action("s3:getobject", "s3:GetObject"));
        assert!(!matches_action("S3:*", "s3:GetObject"));
    }

    // -- Resource matching --

    #[test]
    fn resource_exact_match() {
        assert!(matches_resource(
            "arn:aws:s3:::my-bucket",
            "arn:aws:s3:::my-bucket"
        ));
        assert!(!matches_resource(
            "arn:aws:s3:::my-bucket",
            "arn:aws:s3:::other-bucket"
        ));
    }

    #[test]
    fn resource_full_wildcard() {
        assert!(matches_resource("*", "arn:aws:s3:::my-bucket"));
        assert!(matches_resource("*", "arn:aws:s3:::my-bucket/key"));
        assert!(matches_resource("*", "anything"));
    }

    #[test]
    fn resource_suffix_wildcard() {
        assert!(matches_resource(
            "arn:aws:s3:::my-bucket/*",
            "arn:aws:s3:::my-bucket/key"
        ));
        assert!(matches_resource(
            "arn:aws:s3:::my-bucket/*",
            "arn:aws:s3:::my-bucket/path/to/key"
        ));
        assert!(!matches_resource(
            "arn:aws:s3:::my-bucket/*",
            "arn:aws:s3:::other-bucket/key"
        ));
    }

    #[test]
    fn resource_prefix_wildcard_in_key() {
        assert!(matches_resource(
            "arn:aws:s3:::my-bucket/logs/*",
            "arn:aws:s3:::my-bucket/logs/2024/file.txt"
        ));
        assert!(!matches_resource(
            "arn:aws:s3:::my-bucket/logs/*",
            "arn:aws:s3:::my-bucket/data/file.txt"
        ));
    }

    #[test]
    fn resource_question_mark_wildcard() {
        assert!(matches_resource(
            "arn:aws:s3:::bucket-?",
            "arn:aws:s3:::bucket-1"
        ));
        assert!(!matches_resource(
            "arn:aws:s3:::bucket-?",
            "arn:aws:s3:::bucket-12"
        ));
    }

    #[test]
    fn resource_bucket_only_vs_object() {
        // Bucket ARN should NOT match object ARN
        assert!(!matches_resource(
            "arn:aws:s3:::my-bucket",
            "arn:aws:s3:::my-bucket/key"
        ));
    }

    // -- Statement evaluation --

    #[test]
    fn statement_allow_match() {
        let stmt = Statement {
            sid: None,
            effect: Effect::Allow,
            action: vec!["s3:GetObject".to_string()],
            resource: vec!["arn:aws:s3:::bucket/*".to_string()],
        };
        assert_eq!(
            evaluate_statement(&stmt, "s3:GetObject", "arn:aws:s3:::bucket/key"),
            Evaluation::Allow
        );
    }

    #[test]
    fn statement_deny_match() {
        let stmt = Statement {
            sid: None,
            effect: Effect::Deny,
            action: vec!["s3:DeleteObject".to_string()],
            resource: vec!["*".to_string()],
        };
        assert_eq!(
            evaluate_statement(&stmt, "s3:DeleteObject", "arn:aws:s3:::bucket/key"),
            Evaluation::Deny
        );
    }

    #[test]
    fn statement_no_action_match() {
        let stmt = Statement {
            sid: None,
            effect: Effect::Allow,
            action: vec!["s3:GetObject".to_string()],
            resource: vec!["*".to_string()],
        };
        assert_eq!(
            evaluate_statement(&stmt, "s3:PutObject", "arn:aws:s3:::bucket/key"),
            Evaluation::NoMatch
        );
    }

    #[test]
    fn statement_no_resource_match() {
        let stmt = Statement {
            sid: None,
            effect: Effect::Allow,
            action: vec!["s3:GetObject".to_string()],
            resource: vec!["arn:aws:s3:::bucket-a/*".to_string()],
        };
        assert_eq!(
            evaluate_statement(&stmt, "s3:GetObject", "arn:aws:s3:::bucket-b/key"),
            Evaluation::NoMatch
        );
    }

    #[test]
    fn statement_multiple_actions() {
        let stmt = Statement {
            sid: None,
            effect: Effect::Allow,
            action: vec!["s3:GetObject".to_string(), "s3:PutObject".to_string()],
            resource: vec!["*".to_string()],
        };
        assert_eq!(
            evaluate_statement(&stmt, "s3:GetObject", "arn:aws:s3:::b/k"),
            Evaluation::Allow
        );
        assert_eq!(
            evaluate_statement(&stmt, "s3:PutObject", "arn:aws:s3:::b/k"),
            Evaluation::Allow
        );
        assert_eq!(
            evaluate_statement(&stmt, "s3:DeleteObject", "arn:aws:s3:::b/k"),
            Evaluation::NoMatch
        );
    }

    #[test]
    fn statement_multiple_resources() {
        let stmt = Statement {
            sid: None,
            effect: Effect::Allow,
            action: vec!["s3:GetObject".to_string()],
            resource: vec![
                "arn:aws:s3:::bucket-a/*".to_string(),
                "arn:aws:s3:::bucket-b/*".to_string(),
            ],
        };
        assert_eq!(
            evaluate_statement(&stmt, "s3:GetObject", "arn:aws:s3:::bucket-a/key"),
            Evaluation::Allow
        );
        assert_eq!(
            evaluate_statement(&stmt, "s3:GetObject", "arn:aws:s3:::bucket-b/key"),
            Evaluation::Allow
        );
        assert_eq!(
            evaluate_statement(&stmt, "s3:GetObject", "arn:aws:s3:::bucket-c/key"),
            Evaluation::NoMatch
        );
    }

    // -- Policy evaluation --

    #[test]
    fn policy_allow_only() {
        let doc = PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![Statement {
                sid: None,
                effect: Effect::Allow,
                action: vec!["s3:*".to_string()],
                resource: vec!["*".to_string()],
            }],
        };
        assert_eq!(
            evaluate_policy(&doc, "s3:GetObject", "arn:aws:s3:::b/k"),
            Evaluation::Allow
        );
    }

    #[test]
    fn policy_deny_overrides_allow() {
        let doc = PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![
                Statement {
                    sid: Some("AllowAll".to_string()),
                    effect: Effect::Allow,
                    action: vec!["s3:*".to_string()],
                    resource: vec!["*".to_string()],
                },
                Statement {
                    sid: Some("DenyDelete".to_string()),
                    effect: Effect::Deny,
                    action: vec!["s3:DeleteObject".to_string()],
                    resource: vec!["*".to_string()],
                },
            ],
        };
        assert_eq!(
            evaluate_policy(&doc, "s3:GetObject", "arn:aws:s3:::b/k"),
            Evaluation::Allow
        );
        assert_eq!(
            evaluate_policy(&doc, "s3:DeleteObject", "arn:aws:s3:::b/k"),
            Evaluation::Deny
        );
    }

    #[test]
    fn policy_no_matching_statement() {
        let doc = PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![Statement {
                sid: None,
                effect: Effect::Allow,
                action: vec!["s3:GetObject".to_string()],
                resource: vec!["arn:aws:s3:::bucket-a/*".to_string()],
            }],
        };
        assert_eq!(
            evaluate_policy(&doc, "s3:PutObject", "arn:aws:s3:::bucket-a/key"),
            Evaluation::NoMatch
        );
    }

    // -- Multi-grant evaluation --

    #[test]
    fn grants_combined_allow() {
        let grants = vec![
            PolicyDocument {
                version: "2012-10-17".to_string(),
                statement: vec![Statement {
                    sid: None,
                    effect: Effect::Allow,
                    action: vec!["s3:GetObject".to_string()],
                    resource: vec!["*".to_string()],
                }],
            },
            PolicyDocument {
                version: "2012-10-17".to_string(),
                statement: vec![Statement {
                    sid: None,
                    effect: Effect::Allow,
                    action: vec!["s3:PutObject".to_string()],
                    resource: vec!["*".to_string()],
                }],
            },
        ];
        assert_eq!(
            evaluate_grants(&grants, "s3:GetObject", "arn:aws:s3:::b/k"),
            Evaluation::Allow
        );
        assert_eq!(
            evaluate_grants(&grants, "s3:PutObject", "arn:aws:s3:::b/k"),
            Evaluation::Allow
        );
        assert_eq!(
            evaluate_grants(&grants, "s3:DeleteObject", "arn:aws:s3:::b/k"),
            Evaluation::NoMatch
        );
    }

    #[test]
    fn grants_deny_in_one_overrides_allow_in_another() {
        let grants = vec![
            PolicyDocument {
                version: "2012-10-17".to_string(),
                statement: vec![Statement {
                    sid: None,
                    effect: Effect::Allow,
                    action: vec!["s3:*".to_string()],
                    resource: vec!["*".to_string()],
                }],
            },
            PolicyDocument {
                version: "2012-10-17".to_string(),
                statement: vec![Statement {
                    sid: None,
                    effect: Effect::Deny,
                    action: vec!["s3:DeleteObject".to_string()],
                    resource: vec!["arn:aws:s3:::protected/*".to_string()],
                }],
            },
        ];
        assert_eq!(
            evaluate_grants(&grants, "s3:GetObject", "arn:aws:s3:::protected/key"),
            Evaluation::Allow
        );
        assert_eq!(
            evaluate_grants(
                &grants,
                "s3:DeleteObject",
                "arn:aws:s3:::protected/key"
            ),
            Evaluation::Deny
        );
        // Delete on a different bucket is allowed (deny is bucket-scoped)
        assert_eq!(
            evaluate_grants(&grants, "s3:DeleteObject", "arn:aws:s3:::other/key"),
            Evaluation::Allow
        );
    }

    #[test]
    fn grants_empty_implicit_deny() {
        let grants: Vec<PolicyDocument> = vec![];
        assert_eq!(
            evaluate_grants(&grants, "s3:GetObject", "arn:aws:s3:::b/k"),
            Evaluation::NoMatch
        );
    }

    // -- JSON parsing --

    #[test]
    fn parse_full_policy() {
        let json = r#"{
            "Version": "2012-10-17",
            "Statement": [
                {
                    "Sid": "AllowRead",
                    "Effect": "Allow",
                    "Action": ["s3:GetObject", "s3:ListBucket"],
                    "Resource": ["arn:aws:s3:::my-bucket", "arn:aws:s3:::my-bucket/*"]
                }
            ]
        }"#;
        let doc = parse_policy_document(json).unwrap();
        assert_eq!(doc.version, "2012-10-17");
        assert_eq!(doc.statement.len(), 1);
        assert_eq!(doc.statement[0].sid, Some("AllowRead".to_string()));
        assert_eq!(doc.statement[0].effect, Effect::Allow);
        assert_eq!(doc.statement[0].action.len(), 2);
        assert_eq!(doc.statement[0].resource.len(), 2);
    }

    #[test]
    fn parse_single_string_action_and_resource() {
        let json = r#"{
            "Version": "2012-10-17",
            "Statement": {
                "Effect": "Allow",
                "Action": "s3:*",
                "Resource": "*"
            }
        }"#;
        let doc = parse_policy_document(json).unwrap();
        assert_eq!(doc.statement.len(), 1);
        assert_eq!(doc.statement[0].action, vec!["s3:*"]);
        assert_eq!(doc.statement[0].resource, vec!["*"]);
    }

    #[test]
    fn parse_single_statement_not_array() {
        let json = r#"{
            "Version": "2012-10-17",
            "Statement": {
                "Effect": "Deny",
                "Action": ["s3:DeleteObject"],
                "Resource": ["*"]
            }
        }"#;
        let doc = parse_policy_document(json).unwrap();
        assert_eq!(doc.statement.len(), 1);
        assert_eq!(doc.statement[0].effect, Effect::Deny);
    }

    #[test]
    fn parse_invalid_json() {
        let result = parse_policy_document("not json");
        assert!(result.is_err());
    }

    #[test]
    fn parse_missing_version() {
        let json = r#"{"Statement": [{"Effect": "Allow", "Action": "*", "Resource": "*"}]}"#;
        let result = parse_policy_document(json);
        assert!(result.is_err());
    }

    // -- Serialization round-trip --

    #[test]
    fn serialize_roundtrip() {
        let doc = PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![Statement {
                sid: Some("Test".to_string()),
                effect: Effect::Allow,
                action: vec!["s3:GetObject".to_string()],
                resource: vec!["*".to_string()],
            }],
        };
        let json = serde_json::to_string(&doc).unwrap();
        let parsed: PolicyDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version, doc.version);
        assert_eq!(parsed.statement.len(), 1);
        assert_eq!(parsed.statement[0].effect, Effect::Allow);
    }

    // -- Validation --

    #[test]
    fn validate_valid_policy() {
        let doc = PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![Statement {
                sid: None,
                effect: Effect::Allow,
                action: vec!["s3:GetObject".to_string()],
                resource: vec!["arn:aws:s3:::bucket/*".to_string()],
            }],
        };
        assert!(validate_policy_document(&doc).is_empty());
    }

    #[test]
    fn validate_wrong_version() {
        let doc = PolicyDocument {
            version: "2023-01-01".to_string(),
            statement: vec![Statement {
                sid: None,
                effect: Effect::Allow,
                action: vec!["s3:*".to_string()],
                resource: vec!["*".to_string()],
            }],
        };
        let errors = validate_policy_document(&doc);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("Unsupported policy version"));
    }

    #[test]
    fn validate_empty_statements() {
        let doc = PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![],
        };
        let errors = validate_policy_document(&doc);
        assert!(errors.iter().any(|e| e.contains("at least one statement")));
    }

    #[test]
    fn validate_unknown_action() {
        let doc = PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![Statement {
                sid: Some("Bad".to_string()),
                effect: Effect::Allow,
                action: vec!["s3:MakeBreakfast".to_string()],
                resource: vec!["*".to_string()],
            }],
        };
        let errors = validate_policy_document(&doc);
        assert!(errors.iter().any(|e| e.contains("Unknown action")));
    }

    #[test]
    fn validate_invalid_resource() {
        let doc = PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![Statement {
                sid: None,
                effect: Effect::Allow,
                action: vec!["s3:GetObject".to_string()],
                resource: vec!["not-an-arn".to_string()],
            }],
        };
        let errors = validate_policy_document(&doc);
        assert!(errors.iter().any(|e| e.contains("Invalid resource")));
    }

    #[test]
    fn validate_empty_action_list() {
        let doc = PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![Statement {
                sid: None,
                effect: Effect::Allow,
                action: vec![],
                resource: vec!["*".to_string()],
            }],
        };
        let errors = validate_policy_document(&doc);
        assert!(errors.iter().any(|e| e.contains("Action must not be empty")));
    }

    #[test]
    fn validate_empty_resource_list() {
        let doc = PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![Statement {
                sid: None,
                effect: Effect::Allow,
                action: vec!["s3:GetObject".to_string()],
                resource: vec![],
            }],
        };
        let errors = validate_policy_document(&doc);
        assert!(errors
            .iter()
            .any(|e| e.contains("Resource must not be empty")));
    }

    #[test]
    fn validate_wildcards_accepted() {
        let doc = PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![Statement {
                sid: None,
                effect: Effect::Allow,
                action: vec!["*".to_string()],
                resource: vec!["*".to_string()],
            }],
        };
        assert!(validate_policy_document(&doc).is_empty());
    }

    #[test]
    fn validate_service_wildcards_accepted() {
        let doc = PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![
                Statement {
                    sid: None,
                    effect: Effect::Allow,
                    action: vec!["s3:*".to_string()],
                    resource: vec!["*".to_string()],
                },
                Statement {
                    sid: None,
                    effect: Effect::Allow,
                    action: vec!["arca:*".to_string()],
                    resource: vec!["*".to_string()],
                },
            ],
        };
        assert!(validate_policy_document(&doc).is_empty());
    }

    #[test]
    fn validate_statement_sid_in_error() {
        let doc = PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![Statement {
                sid: Some("MyStatement".to_string()),
                effect: Effect::Allow,
                action: vec!["s3:FakeAction".to_string()],
                resource: vec!["*".to_string()],
            }],
        };
        let errors = validate_policy_document(&doc);
        assert!(errors[0].contains("MyStatement"));
    }

    // -- Glob matching edge cases --

    #[test]
    fn glob_empty_pattern_empty_text() {
        assert!(glob_match("", ""));
    }

    #[test]
    fn glob_star_matches_empty() {
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_multiple_stars() {
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(glob_match("a*b*c", "abc"));
        assert!(!glob_match("a*b*c", "aXXc"));
    }

    #[test]
    fn glob_question_mark() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("a?c", "abbc"));
    }

    // -- Built-in grant templates --

    #[test]
    fn builtin_administrator_access() {
        let json = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["*"],"Resource":["*"]}]}"#;
        let doc = parse_policy_document(json).unwrap();
        assert_eq!(
            evaluate_policy(&doc, "s3:GetObject", "arn:aws:s3:::b/k"),
            Evaluation::Allow
        );
        assert_eq!(
            evaluate_policy(&doc, "arca:ManageUsers", "*"),
            Evaluation::Allow
        );
    }

    #[test]
    fn builtin_s3_read_only() {
        let json = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:GetObject","s3:ListBucket","s3:ListAllMyBuckets","s3:GetBucketLocation","s3:GetBucketEncryption"],"Resource":["*"]}]}"#;
        let doc = parse_policy_document(json).unwrap();
        assert_eq!(
            evaluate_policy(&doc, "s3:GetObject", "arn:aws:s3:::b/k"),
            Evaluation::Allow
        );
        assert_eq!(
            evaluate_policy(&doc, "s3:PutObject", "arn:aws:s3:::b/k"),
            Evaluation::NoMatch
        );
        assert_eq!(
            evaluate_policy(&doc, "s3:DeleteObject", "arn:aws:s3:::b/k"),
            Evaluation::NoMatch
        );
        assert_eq!(
            evaluate_policy(&doc, "arca:ManageUsers", "*"),
            Evaluation::NoMatch
        );
    }

    // -- Bucket-scoped grant --

    #[test]
    fn bucket_scoped_read_write() {
        let json = r#"{
            "Version": "2012-10-17",
            "Statement": [
                {
                    "Effect": "Allow",
                    "Action": ["s3:ListBucket"],
                    "Resource": ["arn:aws:s3:::my-bucket"]
                },
                {
                    "Effect": "Allow",
                    "Action": ["s3:GetObject", "s3:PutObject"],
                    "Resource": ["arn:aws:s3:::my-bucket/*"]
                }
            ]
        }"#;
        let doc = parse_policy_document(json).unwrap();

        // Allowed on my-bucket
        assert_eq!(
            evaluate_policy(&doc, "s3:ListBucket", "arn:aws:s3:::my-bucket"),
            Evaluation::Allow
        );
        assert_eq!(
            evaluate_policy(&doc, "s3:GetObject", "arn:aws:s3:::my-bucket/file.txt"),
            Evaluation::Allow
        );
        assert_eq!(
            evaluate_policy(&doc, "s3:PutObject", "arn:aws:s3:::my-bucket/file.txt"),
            Evaluation::Allow
        );

        // Denied on other-bucket
        assert_eq!(
            evaluate_policy(&doc, "s3:ListBucket", "arn:aws:s3:::other-bucket"),
            Evaluation::NoMatch
        );
        assert_eq!(
            evaluate_policy(
                &doc,
                "s3:GetObject",
                "arn:aws:s3:::other-bucket/file.txt"
            ),
            Evaluation::NoMatch
        );

        // Delete not granted even on my-bucket
        assert_eq!(
            evaluate_policy(
                &doc,
                "s3:DeleteObject",
                "arn:aws:s3:::my-bucket/file.txt"
            ),
            Evaluation::NoMatch
        );
    }
}

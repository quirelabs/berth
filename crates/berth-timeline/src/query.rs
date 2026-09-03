//! The search box grammar: free text plus `host:`, `since:` and `exit:`.
//!
//! Parsing is pure and total. Anything that does not parse as a filter is
//! treated as search text, so the box never rejects what the user typed.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// Free text, already split into terms.
    pub terms: Vec<String>,
    pub hosts: Vec<String>,
    /// Seconds back from now.
    pub since_secs: Option<u64>,
    pub exit: Option<ExitFilter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitFilter {
    Is(i64),
    IsNot(i64),
}

impl ExitFilter {
    pub fn matches(self, code: Option<i64>) -> bool {
        match (self, code) {
            (ExitFilter::Is(want), Some(got)) => want == got,
            (ExitFilter::IsNot(skip), Some(got)) => skip != got,
            // A command with no recorded exit code cannot satisfy either.
            (_, None) => false,
        }
    }
}

impl Query {
    pub fn parse(input: &str) -> Self {
        let mut q = Query::default();
        for token in input.split_whitespace() {
            match token.split_once(':') {
                Some(("host", value)) if !value.is_empty() => q.hosts.push(value.to_string()),
                Some(("since", value)) => match parse_duration(value) {
                    Some(secs) => q.since_secs = Some(secs),
                    None => q.terms.push(token.to_string()),
                },
                Some(("exit", value)) => match parse_exit(value) {
                    Some(f) => q.exit = Some(f),
                    None => q.terms.push(token.to_string()),
                },
                _ => q.terms.push(token.to_string()),
            }
        }
        q
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
            && self.hosts.is_empty()
            && self.since_secs.is_none()
            && self.exit.is_none()
    }

    /// The MATCH expression for FTS5. Terms are quoted so that punctuation in
    /// user input cannot be read as query syntax.
    pub fn fts_match(&self) -> Option<String> {
        if self.terms.is_empty() {
            return None;
        }
        let quoted: Vec<String> = self
            .terms
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect();
        Some(quoted.join(" AND "))
    }

    /// Absolute cutoff in unix seconds, given the current time.
    pub fn cutoff(&self, now_secs: u64) -> Option<i64> {
        self.since_secs.map(|s| now_secs.saturating_sub(s) as i64)
    }
}

/// `30s`, `15m`, `6h`, `7d`, `2w`. A bare number is days.
fn parse_duration(value: &str) -> Option<u64> {
    if value.is_empty() {
        return None;
    }
    let (digits, unit) = match value.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => value.split_at(i),
        None => (value, "d"),
    };
    let n: u64 = digits.parse().ok()?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return None,
    };
    n.checked_mul(multiplier)
}

/// `0`, `1`, `!0`.
fn parse_exit(value: &str) -> Option<ExitFilter> {
    match value.strip_prefix('!') {
        Some(rest) => rest.parse().ok().map(ExitFilter::IsNot),
        None => value.parse().ok().map(ExitFilter::Is),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_text_is_all_terms() {
        let q = Query::parse("nginx restart");
        assert_eq!(q.terms, vec!["nginx", "restart"]);
        assert!(q.hosts.is_empty());
    }

    #[test]
    fn empty_query_is_empty() {
        assert!(Query::parse("").is_empty());
        assert!(Query::parse("   ").is_empty());
    }

    #[test]
    fn parses_the_documented_filters() {
        let q = Query::parse("timeout host:prod since:7d exit:!0");
        assert_eq!(q.terms, vec!["timeout"]);
        assert_eq!(q.hosts, vec!["prod"]);
        assert_eq!(q.since_secs, Some(7 * 86_400));
        assert_eq!(q.exit, Some(ExitFilter::IsNot(0)));
    }

    #[test]
    fn several_hosts_accumulate() {
        let q = Query::parse("host:web-1 host:web-2");
        assert_eq!(q.hosts, vec!["web-1", "web-2"]);
    }

    #[test]
    fn duration_units() {
        assert_eq!(parse_duration("30s"), Some(30));
        assert_eq!(parse_duration("15m"), Some(900));
        assert_eq!(parse_duration("6h"), Some(21_600));
        assert_eq!(parse_duration("7d"), Some(604_800));
        assert_eq!(parse_duration("2w"), Some(1_209_600));
        assert_eq!(parse_duration("3"), Some(259_200), "bare number means days");
    }

    #[test]
    fn unparseable_filters_fall_back_to_search_text() {
        let q = Query::parse("since:banana exit:maybe");
        assert_eq!(q.terms, vec!["since:banana", "exit:maybe"]);
        assert_eq!(q.since_secs, None);
        assert_eq!(q.exit, None);
    }

    #[test]
    fn colons_in_ordinary_text_are_kept() {
        let q = Query::parse("http://example.com");
        assert_eq!(q.terms, vec!["http://example.com"]);
    }

    #[test]
    fn exit_filter_semantics() {
        assert!(ExitFilter::Is(0).matches(Some(0)));
        assert!(!ExitFilter::Is(0).matches(Some(1)));
        assert!(ExitFilter::IsNot(0).matches(Some(2)));
        assert!(!ExitFilter::IsNot(0).matches(Some(0)));
        assert!(
            !ExitFilter::IsNot(0).matches(None),
            "unknown exit code matches neither filter"
        );
    }

    #[test]
    fn fts_match_quotes_terms() {
        let q = Query::parse("failed bind");
        assert_eq!(q.fts_match().as_deref(), Some(r#""failed" AND "bind""#));
    }

    #[test]
    fn fts_match_escapes_embedded_quotes() {
        let q = Query::parse(r#"say"hi"#);
        assert_eq!(q.fts_match().as_deref(), Some(r#""say""hi""#));
    }

    #[test]
    fn fts_syntax_characters_cannot_escape_the_quotes() {
        // `*` and `OR` would change the query if terms were unquoted.
        let q = Query::parse("drop* OR");
        assert_eq!(q.fts_match().as_deref(), Some(r#""drop*" AND "OR""#));
    }

    #[test]
    fn fts_match_is_none_without_terms() {
        assert_eq!(Query::parse("host:prod").fts_match(), None);
    }

    #[test]
    fn cutoff_saturates_at_the_epoch() {
        let q = Query::parse("since:2w");
        assert_eq!(q.cutoff(1_000), Some(0));
        assert_eq!(q.cutoff(2_000_000), Some(2_000_000 - 1_209_600));
    }
}

//! Glob-based path filtering for scoped review packs.

use glob::Pattern;

/// Filter that determines which file paths are in scope.
pub struct ScopeFilter {
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
}

impl ScopeFilter {
    /// Create a new filter from include/exclude glob pattern strings.
    /// Returns an error if any pattern is invalid.
    pub fn new(include: &[String], exclude: &[String]) -> Result<Self, glob::PatternError> {
        let include = include
            .iter()
            .map(|p| Pattern::new(p))
            .collect::<Result<Vec<_>, _>>()?;
        let exclude = exclude
            .iter()
            .map(|p| Pattern::new(p))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self { include, exclude })
    }

    /// Check if a file path matches the scope.
    /// Logic: if include patterns exist, path must match at least one.
    /// Then, path must NOT match any exclude pattern.
    pub fn matches(&self, path: &str) -> bool {
        let included = if self.include.is_empty() {
            true
        } else {
            self.include.iter().any(|p| p.matches(path))
        };

        if !included {
            return false;
        }

        !self.exclude.iter().any(|p| p.matches(path))
    }

    /// Filter a list of paths, returning only those in scope.
    pub fn filter_paths<'a>(&self, paths: &'a [String]) -> Vec<&'a str> {
        paths
            .iter()
            .filter(|p| self.matches(p))
            .map(|p| p.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_only() {
        let filter =
            ScopeFilter::new(&["src/payments/**".into(), "src/portal/**".into()], &[]).unwrap();

        assert!(filter.matches("src/payments/cta.rs"));
        assert!(filter.matches("src/portal/entry.rs"));
        assert!(!filter.matches("src/onboarding/flow.rs"));
        assert!(!filter.matches("README.md"));
    }

    #[test]
    fn exclude_only() {
        let filter = ScopeFilter::new(&[], &["src/onboarding/**".into(), "*.md".into()]).unwrap();

        assert!(filter.matches("src/payments/cta.rs"));
        assert!(!filter.matches("src/onboarding/flow.rs"));
        assert!(!filter.matches("README.md"));
    }

    #[test]
    fn include_and_exclude() {
        let filter = ScopeFilter::new(&["src/**".into()], &["src/tests/**".into()]).unwrap();

        assert!(filter.matches("src/payments/cta.rs"));
        assert!(!filter.matches("src/tests/payment_test.rs"));
        assert!(!filter.matches("docs/readme.md"));
    }

    #[test]
    fn exclude_wins_over_include() {
        let filter = ScopeFilter::new(
            &["src/payments/**".into()],
            &["src/payments/legacy/**".into()],
        )
        .unwrap();

        assert!(filter.matches("src/payments/cta.rs"));
        assert!(!filter.matches("src/payments/legacy/old.rs"));
    }

    #[test]
    fn empty_filter_matches_all() {
        let filter = ScopeFilter::new(&[], &[]).unwrap();
        assert!(filter.matches("anything/goes.rs"));
    }
}

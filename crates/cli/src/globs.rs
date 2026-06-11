//! rg `-g` semantics over object keys: gitignore-style globs, `!` excludes,
//! last match wins, bare-name globs match the final key segment.

pub(crate) struct GlobFilter {
    globs: Vec<CompiledGlob>,
    has_include: bool,
}

struct CompiledGlob {
    matcher: globset::GlobMatcher,
    negated: bool,
    /// Glob contained no `/`: match against the final key segment.
    basename: bool,
}

/// `None` when no globs were given. Globs keep CLI order (last match wins).
pub(crate) fn build_glob_filter(patterns: &[String]) -> anyhow::Result<Option<GlobFilter>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut globs = Vec::with_capacity(patterns.len());
    let mut has_include = false;
    for raw in patterns {
        let (negated, body) = match raw.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, raw.as_str()),
        };
        let body = body.strip_prefix('/').unwrap_or(body); // keys have no root slash
        let matcher = globset::GlobBuilder::new(body)
            .literal_separator(true)
            .build()
            .map_err(|err| anyhow::anyhow!("invalid glob {raw:?}: {err}"))?
            .compile_matcher();
        has_include |= !negated;
        globs.push(CompiledGlob {
            matcher,
            negated,
            basename: !body.contains('/'),
        });
    }
    Ok(Some(GlobFilter { globs, has_include }))
}

impl GlobFilter {
    pub(crate) fn admits(&self, key: &str) -> bool {
        let basename = key.rsplit('/').next().unwrap_or(key);
        let last = self
            .globs
            .iter()
            .rev()
            .find(|g| g.matcher.is_match(if g.basename { basename } else { key }));
        match last {
            Some(glob) => !glob.negated,
            None => !self.has_include,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(globs: &[&str]) -> GlobFilter {
        build_glob_filter(&globs.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
            .unwrap()
            .unwrap()
    }

    #[test]
    fn include_whitelists() {
        let f = filter(&["*.gz"]);
        assert!(f.admits("logs/a/b.gz"));
        assert!(!f.admits("logs/a/b.txt"));
    }

    #[test]
    fn exclude_only_admits_rest() {
        let f = filter(&["!*.tmp"]);
        assert!(f.admits("a/b.log"));
        assert!(!f.admits("a/b.tmp"));
    }

    #[test]
    fn last_match_wins_both_orders() {
        let f = filter(&["*.gz", "!prod/**"]);
        assert!(f.admits("dev/a.gz"));
        assert!(!f.admits("prod/a.gz"));
        let f = filter(&["!prod/**", "*.gz"]);
        assert!(f.admits("prod/a.gz")); // include listed later wins
    }

    #[test]
    fn basename_and_separator_rules() {
        let f = filter(&["*.toml"]);
        assert!(f.admits("a/b/c.toml")); // bare glob matches basename
        let f = filter(&["foo"]);
        assert!(!f.admits("foo/x")); // -g foo does not admit keys under foo/
        assert!(f.admits("foo"));
        let f = filter(&["a/*.log"]);
        assert!(f.admits("a/x.log"));
        assert!(!f.admits("a/b/x.log")); // * does not cross /
    }

    #[test]
    fn include_then_exclude_same_set_admits_nothing() {
        let f = filter(&["*.toml", "!*.toml"]);
        assert!(!f.admits("x.toml"));
        assert!(!f.admits("y.rs"));
    }
}

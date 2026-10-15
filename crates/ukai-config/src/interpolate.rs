//! `${env:NAME}` and `${keychain:NAME}` expansion for values that must not be
//! written to the file itself, such as tokens in agent headers.

/// Where a `${...}` reference reads its value from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `${env:NAME}`: the process environment.
    Env,
    /// `${keychain:NAME}`: the OS credential store, under service `ukai` and
    /// account `NAME`. Useful for an app launched from the dock, which does
    /// not inherit a shell's environment.
    Keychain,
}

/// A `${...}` reference that could not be expanded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InterpolateError {
    #[error("environment variable `{0}` is not set")]
    MissingEnv(String),
    #[error("keychain item `{0}` (service `ukai`) is not readable")]
    MissingKeychain(String),
    #[error(
        "unsupported reference `${{{0}}}`; only `${{env:NAME}}` and `${{keychain:NAME}}` are supported"
    )]
    Unsupported(String),
    #[error("unterminated `${{` in `{0}`")]
    Unterminated(String),
}

/// Expand every `${env:NAME}` and `${keychain:NAME}` in `template` through
/// `lookup`.
///
/// `$$` is a literal `$`. Lookup is injected so callers decide where values
/// come from (the process environment and the keychain in the app, a map in
/// tests).
pub fn interpolate(
    template: &str,
    lookup: impl Fn(Source, &str) -> Option<String>,
) -> Result<String, InterpolateError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            out.push('$');
            rest = tail;
        } else if let Some(body) = after.strip_prefix('{') {
            let end = body
                .find('}')
                .ok_or_else(|| InterpolateError::Unterminated(template.to_owned()))?;
            let reference = &body[..end];
            let value = if let Some(name) = reference.strip_prefix("env:") {
                lookup(Source::Env, name)
                    .ok_or_else(|| InterpolateError::MissingEnv(name.to_owned()))?
            } else if let Some(name) = reference.strip_prefix("keychain:") {
                lookup(Source::Keychain, name)
                    .ok_or_else(|| InterpolateError::MissingKeychain(name.to_owned()))?
            } else {
                return Err(InterpolateError::Unsupported(reference.to_owned()));
            };
            out.push_str(&value);
            rest = &body[end + 1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn lookup(source: Source, name: &str) -> Option<String> {
        match (source, name) {
            (Source::Env, "TOKEN") => Some("s3cret".to_owned()),
            (Source::Keychain, "gateway") => Some("k3y".to_owned()),
            _ => None,
        }
    }

    #[rstest]
    #[case::plain("https://example.com/acp", "https://example.com/acp")]
    #[case::env("Bearer ${env:TOKEN}", "Bearer s3cret")]
    #[case::keychain("Bearer ${keychain:gateway}", "Bearer k3y")]
    #[case::both("${env:TOKEN}:${keychain:gateway}", "s3cret:k3y")]
    #[case::twice("${env:TOKEN}:${env:TOKEN}", "s3cret:s3cret")]
    #[case::escaped("$${env:TOKEN}", "${env:TOKEN}")]
    #[case::lone_dollar("cost: $5", "cost: $5")]
    #[case::trailing_dollar("x$", "x$")]
    fn expands(#[case] template: &str, #[case] expected: &str) {
        assert_eq!(interpolate(template, lookup), Ok(expected.to_owned()));
    }

    #[rstest]
    #[case::missing("${env:NOPE}", InterpolateError::MissingEnv("NOPE".into()))]
    #[case::missing_keychain(
        "${keychain:nope}",
        InterpolateError::MissingKeychain("nope".into())
    )]
    // A source reads only its own namespace.
    #[case::env_name_in_keychain(
        "${keychain:TOKEN}",
        InterpolateError::MissingKeychain("TOKEN".into())
    )]
    #[case::unsupported("${project.root}", InterpolateError::Unsupported("project.root".into()))]
    #[case::unterminated("${env:TOKEN", InterpolateError::Unterminated("${env:TOKEN".into()))]
    fn rejects(#[case] template: &str, #[case] expected: InterpolateError) {
        assert_eq!(interpolate(template, lookup), Err(expected));
    }
}

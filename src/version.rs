/// Formats the compile-time `hct` version string from `CARGO_PKG_VERSION` and optional Git
/// metadata (`git describe --tags --always --dirty --match "v[0-9]*"` and short commit SHA).
///
/// Output rules:
/// - Exact clean release tag matching `v{pkg_version}` -> `"{pkg_version}"` (e.g. `"0.2.0"`)
/// - Dirty working tree on exact release tag -> `"{pkg_version} ({sha}-dirty)"`
/// - `N` commits ahead of matching `v{pkg_version}` tag -> SemVer build metadata
///   `"{pkg_version}+{N}.g{sha}"` (or `".dirty"` suffix when dirty)
/// - Untagged repo, mismatched ancestor tag (e.g. Release PR branch before tag creation), or
///   sandbox `HCT_GIT_SHA` override -> `"{pkg_version} ({sha})"` / `"{pkg_version} ({sha}-dirty)"`
/// - Source archive without Git metadata -> `"{pkg_version}"`
pub fn format_build_version(
    pkg_version: &str,
    git_describe: Option<&str>,
    git_sha: Option<&str>,
) -> String {
    let pkg_version = pkg_version.trim();
    let desc_opt = git_describe.map(str::trim).filter(|s| !s.is_empty());
    let sha_opt = git_sha.map(str::trim).filter(|s| !s.is_empty());

    if let Some(desc) = desc_opt {
        let expected_tag = format!("v{pkg_version}");
        if desc == expected_tag {
            if let Some(sha) = sha_opt {
                if let Some(base_sha) = sha.strip_suffix("-dirty") {
                    if !base_sha.is_empty() {
                        return format!("{pkg_version} ({base_sha}-dirty)");
                    }
                    return format!("{pkg_version} (dirty)");
                }
            }
            return pkg_version.to_string();
        }

        if let Some(rest) = desc.strip_prefix(&expected_tag) {
            if rest == "-dirty" {
                if let Some(base_sha) = sha_opt
                    .map(|s| s.strip_suffix("-dirty").unwrap_or(s))
                    .filter(|s| !s.is_empty() && *s != "dirty")
                {
                    return format!("{pkg_version} ({base_sha}-dirty)");
                }
                return format!("{pkg_version} (dirty)");
            }

            if let Some(after_dash) = rest.strip_prefix('-') {
                if let Some((distance, sha_and_dirty)) = parse_distance_and_sha(after_dash) {
                    let (clean_sha, is_dirty) = match sha_and_dirty.strip_suffix("-dirty") {
                        Some(clean) => (clean, true),
                        None => (
                            sha_and_dirty,
                            sha_opt.is_some_and(|s| s.ends_with("-dirty") || s == "dirty"),
                        ),
                    };
                    if !clean_sha.is_empty() && clean_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                        if pkg_version.contains('+') {
                            return if is_dirty {
                                format!("{pkg_version} ({distance}-g{clean_sha}-dirty)")
                            } else {
                                format!("{pkg_version} ({distance}-g{clean_sha})")
                            };
                        }
                        return if is_dirty {
                            format!("{pkg_version}+{distance}.g{clean_sha}.dirty")
                        } else {
                            format!("{pkg_version}+{distance}.g{clean_sha}")
                        };
                    }
                }
            }
        }

        let is_dirty = desc.ends_with("-dirty")
            || desc == "dirty"
            || sha_opt.is_some_and(|s| s.ends_with("-dirty") || s == "dirty");
        let base_desc = desc.strip_suffix("-dirty").unwrap_or(desc);
        let resolved_sha = sha_opt
            .map(|s| s.strip_suffix("-dirty").unwrap_or(s))
            .filter(|s| !s.is_empty() && *s != "dirty")
            .or_else(|| extract_describe_hex_sha(base_desc))
            .or_else(|| {
                (!base_desc.is_empty() && base_desc.bytes().all(|b| b.is_ascii_hexdigit()))
                    .then_some(base_desc)
            });

        if let Some(sha) = resolved_sha {
            return if is_dirty {
                format!("{pkg_version} ({sha}-dirty)")
            } else {
                format!("{pkg_version} ({sha})")
            };
        }

        return format!("{pkg_version} ({desc})");
    }

    if let Some(sha) = sha_opt {
        if sha == "dirty" || sha == "-dirty" {
            return format!("{pkg_version} (dirty)");
        }
        return format!("{pkg_version} ({sha})");
    }

    pkg_version.to_string()
}

fn parse_distance_and_sha(s: &str) -> Option<(&str, &str)> {
    let (distance, sha_part) = s.split_once("-g")?;
    if distance.is_empty() || !distance.bytes().all(|b| b.is_ascii_digit()) || sha_part.is_empty() {
        return None;
    }
    Some((distance, sha_part))
}

fn extract_describe_hex_sha(base_desc: &str) -> Option<&str> {
    let (prefix, sha) = base_desc.rsplit_once("-g")?;
    let (_, distance) = prefix.rsplit_once('-')?;
    if !distance.is_empty()
        && distance.bytes().all(|b| b.is_ascii_digit())
        && !sha.is_empty()
        && sha.bytes().all(|b| b.is_ascii_hexdigit())
    {
        Some(sha)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::format_build_version;

    #[test]
    fn formats_untagged_clean_and_dirty_commits() {
        assert_eq!(
            format_build_version("0.1.0", Some("dd9d24c"), Some("dd9d24c")),
            "0.1.0 (dd9d24c)"
        );
        assert_eq!(
            format_build_version("0.1.0", Some("dd9d24c-dirty"), Some("dd9d24c")),
            "0.1.0 (dd9d24c-dirty)"
        );
    }

    #[test]
    fn formats_exact_matching_release_tag() {
        assert_eq!(
            format_build_version("0.2.0", Some("v0.2.0"), Some("6abf832")),
            "0.2.0"
        );
        assert_eq!(
            format_build_version("0.2.0", Some("v0.2.0-dirty"), Some("6abf832")),
            "0.2.0 (6abf832-dirty)"
        );
        assert_eq!(
            format_build_version("0.2.0", Some("v0.2.0-dirty"), None),
            "0.2.0 (dirty)"
        );
    }

    #[test]
    fn formats_commits_ahead_of_matching_tag_as_semver_build_metadata() {
        assert_eq!(
            format_build_version("0.2.0", Some("v0.2.0-3-gdd9d24c"), Some("dd9d24c")),
            "0.2.0+3.gdd9d24c"
        );
        assert_eq!(
            format_build_version("0.2.0", Some("v0.2.0-3-gdd9d24c-dirty"), Some("dd9d24c")),
            "0.2.0+3.gdd9d24c.dirty"
        );
        assert_eq!(
            format_build_version(
                "0.2.0-rc.1",
                Some("v0.2.0-rc.1-2-gabcdef0"),
                Some("abcdef0")
            ),
            "0.2.0-rc.1+2.gabcdef0"
        );
    }

    #[test]
    fn falls_back_to_commit_sha_when_cargo_version_exceeds_latest_tag() {
        // E.g. building on a release-please PR branch (`0.2.0`) before `v0.2.0` is tagged.
        assert_eq!(
            format_build_version("0.2.0", Some("v0.1.0-5-g1234abc"), Some("1234abc")),
            "0.2.0 (1234abc)"
        );
        assert_eq!(
            format_build_version("0.2.0", Some("v0.1.0-5-g1234abc-dirty"), None),
            "0.2.0 (1234abc-dirty)"
        );
        assert_eq!(
            format_build_version("0.2.0", Some("v0.1.0-dirty"), Some("1234abc")),
            "0.2.0 (1234abc-dirty)"
        );
    }

    #[test]
    fn supports_nix_sandbox_sha_override_and_tarball_fallback() {
        assert_eq!(
            format_build_version("0.1.0", None, Some("dd9d24c")),
            "0.1.0 (dd9d24c)"
        );
        assert_eq!(
            format_build_version("0.1.0", None, Some("dd9d24c-dirty")),
            "0.1.0 (dd9d24c-dirty)"
        );
        assert_eq!(format_build_version("0.1.0", None, None), "0.1.0");
        assert_eq!(format_build_version("0.1.0", Some("  "), Some("")), "0.1.0");
    }
}

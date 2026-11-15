//! The framework's Cargo features: which a new project starts with, and adding more later
//! (`cadmus add google`). A project compiles only the database driver, storage backend and
//! login methods it asks for.

use std::path::Path;

/// Where generated login methods go in `src/settings.rs`.
pub(crate) const METHODS_MARKER: &str =
    "// cadmus:methods (login methods are added above this line)";

/// What a feature brings, for the settings and `.env` it needs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Database,
    Storage,
    /// A login method: the lines that add it to `methods()`, and the `.env` lines it reads.
    Login {
        code: &'static str,
        env: &'static str,
    },
    Mail,
}

pub(crate) struct Feature {
    pub(crate) name: &'static str,
    pub(crate) kind: Kind,
    /// What to do next, shown after adding it.
    pub(crate) next: &'static str,
}

pub(crate) const FEATURES: &[Feature] = &[
    Feature {
        name: "sqlite",
        kind: Kind::Database,
        next: "set DATABASE_URL=sqlite://db.sqlite3?mode=rwc in .env",
    },
    Feature {
        name: "postgres",
        kind: Kind::Database,
        next: "set DATABASE_URL=postgres://user:password@localhost:5432/name in .env",
    },
    Feature {
        name: "mysql",
        kind: Kind::Database,
        next: "set DATABASE_URL=mysql://user:password@localhost:3306/name in .env (MariaDB too)",
    },
    Feature {
        name: "s3",
        kind: Kind::Storage,
        next: "set uploads.storage_url to s3://bucket/prefix in src/settings.rs, and AWS_* in .env",
    },
    Feature {
        name: "azure",
        kind: Kind::Storage,
        next: "set uploads.storage_url to az://container/prefix in src/settings.rs, and AZURE_STORAGE_* in .env",
    },
    Feature {
        name: "gcs",
        kind: Kind::Storage,
        next: "set uploads.storage_url to gs://bucket/prefix in src/settings.rs, and GOOGLE_SERVICE_ACCOUNT in .env",
    },
    Feature {
        name: "smtp",
        kind: Kind::Mail,
        next: "set MAIL_URL in .env (e.g. smtps://user:password@smtp.example.com); until then messages go to the log",
    },
    Feature {
        name: "magic-link",
        kind: Kind::Login {
            code: "    methods.push(AuthMethod::MagicLink);\n",
            env: "",
        },
        next: "login links are on; they are e-mailed (add smtp, or copy them from the log)",
    },
    Feature {
        name: "google",
        kind: Kind::Login {
            code: "    if let Some(client) = OAuthClient::from_env_if_set(\"GOOGLE\")? {\n        methods.push(AuthMethod::Google(client));\n    }\n",
            env: "# Google: an OAuth client at https://console.cloud.google.com/apis/credentials\nGOOGLE_CLIENT_ID=\nGOOGLE_CLIENT_SECRET=\n# Other client IDs (mobile apps) whose ID tokens the API accepts, comma-separated:\n# GOOGLE_AUDIENCES=\n",
        },
        next: "register an OAuth client with Google and put its ID and secret in .env (GOOGLE_*)",
    },
    Feature {
        name: "microsoft",
        kind: Kind::Login {
            code: "    if let Some(client) = OAuthClient::from_env_if_set(\"MICROSOFT\")? {\n        let tenant = env_opt(\"MICROSOFT_TENANT\").unwrap_or_else(|| \"common\".into());\n        methods.push(AuthMethod::Microsoft { client, tenant });\n    }\n",
            env: "# Microsoft: an app registration in Microsoft Entra\nMICROSOFT_CLIENT_ID=\nMICROSOFT_CLIENT_SECRET=\nMICROSOFT_TENANT=common\n",
        },
        next: "register an app in Microsoft Entra and put its ID and secret in .env (MICROSOFT_*)",
    },
    Feature {
        name: "github",
        kind: Kind::Login {
            code: "    if let Some(client) = OAuthClient::from_env_if_set(\"GITHUB\")? {\n        methods.push(AuthMethod::GitHub(client));\n    }\n",
            env: "# GitHub: an OAuth app at https://github.com/settings/developers\nGITHUB_CLIENT_ID=\nGITHUB_CLIENT_SECRET=\n",
        },
        next: "register an OAuth app with GitHub and put its ID and secret in .env (GITHUB_*)",
    },
    Feature {
        name: "facebook",
        kind: Kind::Login {
            code: "    if let Some(client) = OAuthClient::from_env_if_set(\"FACEBOOK\")? {\n        methods.push(AuthMethod::Facebook(client));\n    }\n",
            env: "# Facebook: an app with Facebook Login at https://developers.facebook.com\nFACEBOOK_CLIENT_ID=\nFACEBOOK_CLIENT_SECRET=\n",
        },
        next: "create a Facebook app with Facebook Login and put its ID and secret in .env (FACEBOOK_*)",
    },
    Feature {
        name: "oidc",
        kind: Kind::Login {
            code: "    if let Some(client) = OAuthClient::from_env_if_set(\"OIDC\")? {\n        let issuer = env(\"OIDC_ISSUER\")?;\n        methods.push(AuthMethod::Oidc(OidcProvider::new(\"sso\", \"Single sign-on\", issuer, client)));\n    }\n",
            env: "# Any OpenID Connect provider (Okta, Auth0, Keycloak, GitLab, ...)\nOIDC_ISSUER=\nOIDC_CLIENT_ID=\nOIDC_CLIENT_SECRET=\n",
        },
        next: "register an app with your OpenID Connect provider and put its issuer, ID and secret in .env (OIDC_*); rename \"sso\" in src/settings.rs if you like",
    },
];

/// The features named, checked; unknown names are an error listing the known ones.
pub(crate) fn lookup<S: AsRef<str>>(names: &[S]) -> Result<Vec<&'static Feature>, String> {
    let mut found: Vec<&'static Feature> = Vec::new();
    for name in names
        .iter()
        .flat_map(|n| n.as_ref().split(','))
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        let feature = FEATURES.iter().find(|f| f.name == name).ok_or_else(|| {
            format!(
                "unknown feature `{name}`; known: {}",
                FEATURES
                    .iter()
                    .map(|f| f.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        if !found.iter().any(|f| f.name == feature.name) {
            found.push(feature);
        }
    }
    Ok(found)
}

/// The `cadmus = ...` value for Cargo.toml: `source` is `version = "0.2"` or `path = "..."`.
pub(crate) fn dependency(source: &str, features: &[&str]) -> String {
    let list = features
        .iter()
        .map(|f| format!("{f:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{ {source}, default-features = false, features = [{list}] }}")
}

/// The body of `methods()` in a new project's settings, with these login features on.
pub(crate) fn methods_function(features: &[&'static Feature]) -> String {
    let mut code = String::new();
    for feature in features {
        if let Kind::Login { code: lines, .. } = feature.kind {
            code.push_str(lines);
        }
    }
    format!(
        r"
/// How people log in, in the order the login page shows them; apps use the same methods
/// through the REST API (/api/auth/...). Each method beyond the password is a Cargo feature:
/// `cadmus add google` (or microsoft, github, facebook, oidc, magic-link) adds it here.
///
/// Another site needs an app registered with it, whose redirect URI is
/// <PUBLIC_URL>/admin/auth/<name>/callback (plus your own app's addresses), and its keys in
/// .env. Until they are there the site is left out, so the project runs without them.
fn methods() -> Result<Vec<AuthMethod>, AdminError> {{
    let mut methods = Vec::new();
    methods.push(AuthMethod::Password);
{code}    {METHODS_MARKER}
    Ok(methods)
}}
"
    )
}

/// The `.env` lines these features read.
pub(crate) fn env_lines(features: &[&'static Feature]) -> String {
    features
        .iter()
        .filter_map(|f| match f.kind {
            Kind::Login { env, .. } if !env.is_empty() => Some(env),
            _ => None,
        })
        .collect()
}

/// `cadmus add`: turns features on in an existing project. Edits Cargo.toml, adds login
/// methods to `methods()` in src/settings.rs and their keys to .env and .env.example.
pub(crate) fn add(project: &Path, names: &[String]) -> Result<String, String> {
    let wanted = lookup(names)?;
    if wanted.is_empty() {
        return Err("name the features to add, e.g. `cadmus add google github`".into());
    }
    let manifest_path = project.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path).map_err(|e| {
        format!(
            "{}: {e} (run this in the project folder)",
            manifest_path.display()
        )
    })?;
    let (manifest, added) = add_to_manifest(&manifest, &wanted)?;

    let mut report = Vec::new();
    let mut by_hand = Vec::new();
    let new: Vec<&'static Feature> = wanted
        .iter()
        .copied()
        .filter(|f| added.contains(&f.name))
        .collect();
    for feature in &wanted {
        if !added.contains(&feature.name) {
            report.push(format!("{} was already on", feature.name));
        }
    }
    if new.is_empty() {
        return Ok(report.join("\n"));
    }
    std::fs::write(&manifest_path, manifest)
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    report.push(format!(
        "Cargo.toml: added {}",
        new.iter().map(|f| f.name).collect::<Vec<_>>().join(", ")
    ));

    let logins: Vec<&'static Feature> = new
        .iter()
        .copied()
        .filter(|f| matches!(f.kind, Kind::Login { .. }))
        .collect();
    if !logins.is_empty() {
        let settings_path = project.join("src/settings.rs");
        let settings = std::fs::read_to_string(&settings_path).unwrap_or_default();
        let code: String = logins
            .iter()
            .filter_map(|f| match f.kind {
                Kind::Login { code, .. } => Some(code),
                _ => None,
            })
            .collect();
        match settings.find(METHODS_MARKER) {
            Some(at) => {
                let line_start = settings[..at].rfind('\n').map_or(0, |i| i + 1);
                let updated = format!("{}{code}{}", &settings[..line_start], &settings[line_start..]);
                std::fs::write(&settings_path, updated)
                    .map_err(|e| format!("{}: {e}", settings_path.display()))?;
                report.push("src/settings.rs: added to methods()".into());
            }
            None => by_hand.push(format!(
                "add to the login methods in src/settings.rs (no `{METHODS_MARKER}` line found):\n{code}"
            )),
        }
        let env = env_lines(&logins);
        if !env.is_empty() {
            for file in [".env", ".env.example"] {
                let path = project.join(file);
                let Ok(current) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let missing: String = env
                    .lines()
                    .filter(|line| {
                        let key = line.split('=').next().unwrap_or_default();
                        line.starts_with('#')
                            || !current.lines().any(|l| l.split('=').next() == Some(key))
                    })
                    .map(|line| format!("{line}\n"))
                    .collect();
                if missing.lines().any(|l| !l.starts_with('#')) {
                    let separator = if current.ends_with('\n') || current.is_empty() {
                        ""
                    } else {
                        "\n"
                    };
                    std::fs::write(&path, format!("{current}{separator}{missing}"))
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    report.push(format!("{file}: added the keys to fill in"));
                }
            }
        }
    }
    report.push(String::new());
    report.push("next:".into());
    for feature in &new {
        report.push(format!("  {}: {}", feature.name, feature.next));
    }
    for note in by_hand {
        report.push(format!("  {note}"));
    }
    Ok(report.join("\n"))
}

/// Adds the features to the `cadmus = ...` dependency; returns the new manifest and the names
/// that were not there yet.
fn add_to_manifest(
    manifest: &str,
    wanted: &[&'static Feature],
) -> Result<(String, Vec<&'static str>), String> {
    let mut lines: Vec<String> = manifest.lines().map(str::to_owned).collect();
    let index = lines
        .iter()
        .position(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("cadmus =") || trimmed.starts_with("cadmus=")
        })
        .ok_or("Cargo.toml has no `cadmus = ...` dependency line")?;
    let line = lines[index].clone();
    let value = line
        .split_once('=')
        .map(|(_, v)| v.trim())
        .unwrap_or_default();
    let mut current: Vec<String> = Vec::new();
    let rebuilt_head;
    if let Some(version) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        // `cadmus = "0.2"`: default features (sqlite) stay on.
        rebuilt_head = format!("{{ version = {version:?}");
    } else if value.starts_with('{') && value.ends_with('}') {
        let inner = value[1..value.len() - 1].trim();
        // The `features` key itself, not the end of `default-features`.
        let key = inner.match_indices("features").map(|(i, _)| i).find(|&i| {
            let before_ok = i == 0
                || inner[..i]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_whitespace() || c == ',');
            before_ok && inner[i + "features".len()..].trim_start().starts_with('=')
        });
        if let Some(start) = key {
            let open = inner[start..]
                .find('[')
                .map(|i| start + i)
                .ok_or("cannot read the features of the cadmus dependency")?;
            let close = inner[open..]
                .find(']')
                .map(|i| open + i)
                .ok_or("cannot read the features of the cadmus dependency")?;
            current = inner[open + 1..close]
                .split(',')
                .map(|f| f.trim().trim_matches('"').to_owned())
                .filter(|f| !f.is_empty())
                .collect();
            let before = inner[..start].trim().trim_end_matches(',').trim();
            let after = inner[close + 1..].trim().trim_start_matches(',').trim();
            let rest: Vec<&str> = [before, after]
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect();
            rebuilt_head = format!("{{ {}", rest.join(", "));
        } else {
            rebuilt_head = format!("{{ {inner}");
        }
    } else {
        return Err(format!(
            "cannot read `{}` in Cargo.toml; add the features to it by hand",
            line.trim()
        ));
    }
    let mut added = Vec::new();
    for feature in wanted {
        if !current.iter().any(|c| c == feature.name) {
            current.push(feature.name.to_owned());
            added.push(feature.name);
        }
    }
    let list = current
        .iter()
        .map(|f| format!("{f:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    lines[index] = format!("{indent}cadmus = {rebuilt_head}, features = [{list}] }}");
    let mut out = lines.join("\n");
    if manifest.ends_with('\n') {
        out.push('\n');
    }
    Ok((out, added))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<&'static Feature> {
        lookup(list).unwrap()
    }

    #[test]
    fn features_are_added_to_every_shape_of_dependency() {
        let cases = [
            (
                "cadmus = \"0.2\"",
                "cadmus = { version = \"0.2\", features = [\"google\"] }",
            ),
            (
                "cadmus = { version = \"0.2\", default-features = false, features = [\"sqlite\"] }",
                "cadmus = { version = \"0.2\", default-features = false, features = [\"sqlite\", \"google\"] }",
            ),
            (
                "cadmus = { path = \"../cadmus\" }",
                "cadmus = { path = \"../cadmus\", features = [\"google\"] }",
            ),
            (
                "cadmus = { features = [\"s3\"], version = \"0.2\" }",
                "cadmus = { version = \"0.2\", features = [\"s3\", \"google\"] }",
            ),
        ];
        for (before, after) in cases {
            let manifest = format!("[dependencies]\n{before}\n");
            let (out, added) = add_to_manifest(&manifest, &names(&["google"])).unwrap();
            assert_eq!(out, format!("[dependencies]\n{after}\n"), "{before}");
            assert_eq!(added, ["google"]);
        }
        let manifest = "cadmus = { version = \"0.2\", features = [\"google\"] }\n";
        let (_, added) = add_to_manifest(manifest, &names(&["google"])).unwrap();
        assert!(added.is_empty());
        assert!(add_to_manifest("[dependencies]\n", &names(&["google"])).is_err());
    }

    #[test]
    fn unknown_features_are_refused_with_the_list() {
        let error = lookup(&["google,linkedin"]).err().unwrap();
        assert!(
            error.contains("linkedin") && error.contains("github"),
            "{error}"
        );
        assert_eq!(names(&["google", "google,github"]).len(), 2);
    }
}

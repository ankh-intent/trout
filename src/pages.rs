//! The browser pages: home (sign-in), dashboard and the LLM provider comparison. Each is one
//! self-contained file with the shared top navigation bar put in when it is first served.

use std::sync::LazyLock;

use axum::body::Body;
use axum::http::HeaderValue;
use axum::http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::response::Response;
use pulldown_cmark::{Event, Options, Parser, Tag, html};

use crate::error::{SOURCE_HEADER, SOURCE_RELAY};

pub const HOME_PATH: &str = "/";
pub const DASHBOARD_PATH: &str = "/dashboard";
pub const PROVIDERS_PATH: &str = "/providers";

/// The pages load nothing from anywhere else and may only call back to this origin.
const PAGE_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; \
                           connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

const NAV_SLOT: &str = "<!--NAV-->";
const NAV_CSS_SLOT: &str = "/*NAV_CSS*/";
const DATA_SLOT: &str = "/*DATA*/";
const README_SLOT: &str = "<!--README-->";
const REPO_SLOT: &str = "<!--REPO-->";

/// Where the source lives; the home page links to it and README links to files resolve against it.
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const REPOSITORY_BRANCH: &str = "master";

/// Navigation items in display order: key, path, label.
const NAV_ITEMS: [(&str, &str, &str); 3] = [
    ("home", HOME_PATH, "Home"),
    ("dashboard", DASHBOARD_PATH, "Dashboard"),
    ("providers", PROVIDERS_PATH, "LLM providers"),
];

const NAV_CSS: &str = "
  .topnav {
    --nav-bg: #ffffff; --nav-line: #e2e5ea; --nav-text: #1b1f24; --nav-muted: #6b7480;
    --nav-accent: #1f7a5c; --nav-active: #eef0f3;
    position: sticky; top: 0; z-index: 20; display: flex; align-items: center; gap: 4px;
    height: 48px; padding: 0 16px; overflow-x: auto; white-space: nowrap;
    background: var(--nav-bg); border-bottom: 1px solid var(--nav-line);
    font: 14px/1 system-ui, -apple-system, 'Segoe UI', sans-serif;
  }
  @media (prefers-color-scheme: dark) {
    .topnav {
      --nav-bg: #1d2125; --nav-line: #2c3238; --nav-text: #e8eaed; --nav-muted: #98a1ab;
      --nav-accent: #4cc59a; --nav-active: #2c3238;
    }
  }
  .topnav a { color: var(--nav-muted); text-decoration: none; padding: 8px 10px; border-radius: 6px; }
  .topnav a:hover { color: var(--nav-text); }
  .topnav a[aria-current] { color: var(--nav-text); font-weight: 600; background: var(--nav-active); }
  .topnav a.brand { color: var(--nav-accent); font-weight: 700; letter-spacing: 0.02em; margin-right: 12px; padding-left: 0; }
  body { padding: 0; }
  main { padding: 24px 16px 48px; }
";

const HOME_SOURCE: &str = include_str!("pages/home.html");
/// Shown on the home page, so the project's documentation and the running relay cannot drift.
const README: &str = include_str!("../README.md");
const DASHBOARD_SOURCE: &str = include_str!("dashboard.html");
const PROVIDERS_SOURCE: &str = include_str!("pages/providers.html");
/// The figures behind the provider page. Updating the page is a change to this file alone.
const PROVIDERS_DATA: &str = include_str!("pages/providers.json");

static HOME: LazyLock<String> = LazyLock::new(|| {
    with_nav(HOME_SOURCE, "home")
        .replace(REPO_SLOT, REPOSITORY)
        .replace(README_SLOT, &readme_html(README))
});
static DASHBOARD: LazyLock<String> = LazyLock::new(|| with_nav(DASHBOARD_SOURCE, "dashboard"));
static PROVIDERS: LazyLock<String> =
    LazyLock::new(|| with_nav(PROVIDERS_SOURCE, "providers").replace(DATA_SLOT, &embeddable(PROVIDERS_DATA)));

fn nav(active: &str) -> String {
    let mut html =
        format!(r#"<nav class="topnav" aria-label="Main"><a class="brand" href="{HOME_PATH}">trout</a>"#);

    for (key, path, label) in NAV_ITEMS {
        let current = if key == active {
            r#" aria-current="page""#
        } else {
            ""
        };

        html.push_str(&format!(r#"<a href="{path}"{current}>{label}</a>"#));
    }

    html.push_str("</nav>");

    html
}

fn with_nav(source: &str, active: &str) -> String {
    source
        .replace(NAV_CSS_SLOT, NAV_CSS)
        .replace(NAV_SLOT, &nav(active))
}

/// A README link as it should work from the home page: links to files in the repository point at
/// the repository, everything else is left alone.
fn absolute_link(destination: &str) -> String {
    if destination.starts_with('#') || destination.contains("://") || destination.starts_with("mailto:") {
        return destination.to_string();
    }

    format!(
        "{REPOSITORY}/blob/{REPOSITORY_BRANCH}/{}",
        destination.trim_start_matches("./")
    )
}

fn readme_html(markdown: &str) -> String {
    let events =
        Parser::new_ext(markdown, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH).map(|event| {
            match event {
                Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                }) => Event::Start(Tag::Link {
                    link_type,
                    dest_url: absolute_link(&dest_url).into(),
                    title,
                    id,
                }),
                other => other,
            }
        });
    let mut out = String::new();

    html::push_html(&mut out, events);

    out
}

/// JSON made safe to sit inside a `<script>` element: compact, and with no `<` that could close
/// the element early. Data that does not parse becomes `null`, which the page shows as empty.
fn embeddable(json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(json)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| "null".to_string())
        .replace('<', "\\u003c")
}

fn respond(page: &'static str) -> Response {
    let mut response = Response::new(Body::from(page));
    let headers = response.headers_mut();

    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(PAGE_POLICY));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(SOURCE_HEADER, HeaderValue::from_static(SOURCE_RELAY));

    response
}

/// `GET /`: the sign-in form. It checks the key against `/stats` and opens the dashboard.
pub async fn home() -> Response {
    respond(HOME.as_str())
}

/// `GET /dashboard`: one static page. It carries no data; it reads `/stats` with the key the
/// home page stored for this browser tab.
pub async fn dashboard() -> Response {
    respond(DASHBOARD.as_str())
}

/// `GET /providers`: the LLM provider comparison, drawn from `pages/providers.json`.
pub async fn providers() -> Response {
    respond(PROVIDERS.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_data_is_valid_and_complete() {
        let data: serde_json::Value =
            serde_json::from_str(PROVIDERS_DATA).expect("providers.json must parse");

        for section in [
            "meta",
            "providers",
            "share",
            "affiliations",
            "gaps",
            "sources",
        ] {
            assert!(!data[section].is_null(), "providers.json needs `{section}`");
        }

        let providers = data["providers"].as_array().unwrap();

        assert!(!providers.is_empty());

        for provider in providers {
            let name = provider["name"].as_str().expect("provider name");

            assert!(
                provider["rank"].is_number() || provider["rank"].is_null(),
                "{name}: rank must be a number or null"
            );

            let models = provider["models"].as_array().expect("provider models");

            assert!(!models.is_empty(), "{name}: needs at least one model");

            for model in models {
                let model_name = model["name"].as_str().expect("model name");

                assert!(model["index"].is_number(), "{name} {model_name}: index");

                for optional in ["price_in", "price_out", "cost_per_task", "speed"] {
                    assert!(
                        model[optional].is_number() || model[optional].is_null(),
                        "{name} {model_name}: {optional} must be a number or null"
                    );
                }
            }

            for link in ["pricing_url", "signup_url"] {
                assert!(
                    provider[link]
                        .as_str()
                        .is_some_and(|url| url.starts_with("https://")),
                    "{name}: {link} must be an https link"
                );
            }
        }
    }

    #[test]
    fn every_page_has_its_slots_filled() {
        for page in [HOME.as_str(), DASHBOARD.as_str(), PROVIDERS.as_str()] {
            assert!(page.contains(r#"<nav class="topnav""#));

            for slot in [NAV_SLOT, NAV_CSS_SLOT, DATA_SLOT, README_SLOT, REPO_SLOT] {
                assert!(!page.contains(slot), "`{slot}` was left in a page");
            }
        }
    }

    #[test]
    fn readme_is_rendered_with_links_into_the_repository() {
        let html = readme_html(
            "# Title\n\nSee [`fly.toml`](fly.toml), [docs](./deploy/x.toml), [site](https://example.com) and [top](#title).\n\n| A | B |\n|---|---|\n| 1 | 2 |\n",
        );

        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<table>"));
        assert!(html.contains(&format!(
            r#"href="{REPOSITORY}/blob/{REPOSITORY_BRANCH}/fly.toml""#
        )));
        assert!(html.contains(&format!(
            r#"href="{REPOSITORY}/blob/{REPOSITORY_BRANCH}/deploy/x.toml""#
        )));
        assert!(html.contains(r#"href="https://example.com""#));
        assert!(html.contains(r##"href="#title""##));
    }

    #[test]
    fn home_page_shows_the_readme() {
        assert!(REPOSITORY.starts_with("https://"));
        assert!(HOME.contains("<h2>Using it</h2>"));
        assert!(HOME.contains(&format!(r#"href="{REPOSITORY}""#)));
        assert!(!HOME.contains(r#"href="fly.toml""#));
    }

    #[test]
    fn nav_marks_only_the_current_page() {
        let html = nav("dashboard");

        assert_eq!(html.matches("aria-current").count(), 1);
        assert!(html.contains(r#"<a href="/dashboard" aria-current="page">Dashboard</a>"#));
        assert!(html.contains(r#"<a href="/providers">LLM providers</a>"#));
    }

    #[test]
    fn embedded_json_cannot_close_its_script_element() {
        let embedded = embeddable(r#"{ "note": "</script><b>" }"#);

        assert!(!embedded.contains('<'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&embedded).unwrap()["note"],
            "</script><b>"
        );
        assert_eq!(embeddable("not json"), "null");
    }
}

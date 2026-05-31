//! Feature-flag identifiers and the CLI-side env-var gate.
//!
//! This is the slice of the flag system that doesn't touch the DB. The
//! [`Feature`] enum is the stable identifier shared by the CLI, the
//! server, and the web UI; [`feature_enabled_in_cli`] reads
//! `OAK_FEATURES` to decide whether a gated subcommand is unlocked in
//! the CLI.
//!
//! Server-side state (admin lists, DB-backed flag rows, `feature_enabled`
//! with a user) lives in `oak_server::features` — that side pulls in the
//! storage layer so it stays out of `oak-core`.

/// Individual flags. Add a variant here, gate the relevant handlers and
/// nav entries with `oak_server::features::feature_enabled`, surface the
/// flag in `/root`, and seed an initial row in `feature_flags` via a
/// migration. The [`Feature::slug`] value is the stable identifier used
/// as the DB key and in URLs — don't rename it without a migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Feature {
    Releases,
    /// New-account creation. **Inverted polarity vs. the other flags**:
    /// seeded as `on` so signups are open by default. Flip to `off` from
    /// /root as a quick "we're getting hammered, close the doors" kill
    /// switch — the email signup form, the JSON API, and the new-user
    /// branch of the GitHub OAuth callback all check this and refuse new
    /// accounts. Existing users keep logging in either way. Admin-only
    /// preview (`admin`) doesn't make sense for this flag — signups happen
    /// before the user is signed in, so there's no admin to preview them
    /// to. We treat `admin` the same as `off` at the callsites.
    Signups,
    /// Require Cloudflare Turnstile on every email signup and password
    /// login. Repeated failed logins can still escalate to Turnstile when
    /// this flag is off, provided Turnstile credentials are configured.
    TurnstileVerification,
    /// API key management — the Settings → API Keys tab and the
    /// `/settings/api-keys/*` POST routes. Seeded as `admin` so it's
    /// hidden from regular users until flipped `on` from /root. The
    /// underlying `/api/api-keys` bearer-token routes are not gated
    /// (they're infrastructure, not a product surface).
    ApiKeys,
    /// Site-wide non-admin access. **Inverted polarity, and not surfaced
    /// in the regular feature-flags table on /root** (it gets its own
    /// "Site access" section). Seeded as `on` so oak is open to everyone
    /// by default. Flip to `off` as a kill switch when you need to take
    /// the product surface offline for non-admins — deploy gone bad,
    /// security incident, data-fix mid-flight. While off, every product
    /// UI route under `build_app_web_routes` returns a "we're in
    /// admin-only mode" page to non-admin (and unauthenticated) callers.
    /// Admins keep full access; `/login`, `/logout`, `/signup`,
    /// `/forgot-password`, and `/reset-password` always work so admins
    /// can still sign in. Marketing routes (`/`, `/blog`, etc.) and the
    /// JSON API (`/api/*`) are unaffected. `admin` state reads the same
    /// as `off` — there's no meaningful "admin preview" of a site-wide
    /// access gate.
    SiteAccess,
    /// Per-user custom profile page served at `<username>.oak.space` —
    /// the GitHub-Pages-style personal site. Gates the Settings → Profile
    /// Page tab, the toggle POST at `/settings/profile-page`, the
    /// "<u>.oak.space" link on `/profile/<u>`, the "Set up
    /// <u>.oak.space" prompt on the home hero, and the subdomain serving
    /// path in `oak-web::sites`. Seeded as `admin` so it's hidden from
    /// regular users until flipped `on` from /root. Existing users who
    /// already enabled their page keep the row in the DB; when the flag
    /// is off (or admin-only) the public-facing subdomain stops
    /// resolving for non-admin viewers.
    ProfilePage,
}

impl Feature {
    pub fn slug(self) -> &'static str {
        match self {
            Feature::Releases => "releases",
            Feature::Signups => "signups",
            Feature::TurnstileVerification => "turnstile-verification",
            Feature::ApiKeys => "api-keys",
            Feature::SiteAccess => "site-access",
            Feature::ProfilePage => "profile-page",
        }
    }

    /// Features that surface in the regular feature-flags table on /root.
    /// `SiteAccess` is excluded — it's the site-wide kill switch and
    /// renders in its own dedicated section with a binary toggle, since
    /// the tri-state on/admin/off semantics (and especially the "off =
    /// hidden from admins too" pill copy) don't read intuitively for an
    /// inverted-polarity gate on the entire product UI.
    pub fn shown_on_root(self) -> bool {
        !matches!(self, Feature::SiteAccess)
    }

    pub fn label(self) -> &'static str {
        match self {
            Feature::Releases => "Releases",
            Feature::Signups => "Signups",
            Feature::TurnstileVerification => "Turnstile verification",
            Feature::ApiKeys => "API Keys",
            Feature::SiteAccess => "Site access",
            Feature::ProfilePage => "Profile page",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Feature::Releases => {
                "GitHub-style per-repo releases, the marketing /releases page, \
                 and the CLI binary download/changelog UI."
            }
            Feature::Signups => {
                "New-account creation (email signup, /api/auth/signup, first-time \
                 GitHub OAuth). On = open. Off = closed, with a 'check back later' \
                 page pointing visitors at the email list. Existing users are \
                 unaffected. Use as a kill switch under load."
            }
            Feature::TurnstileVerification => {
                "Require Cloudflare Turnstile on every email signup and password \
                 login. Repeated failed logins still escalate to Turnstile when \
                 credentials are configured, even if this flag is off."
            }
            Feature::ApiKeys => {
                "API key management — the Settings → API Keys tab and the \
                 /settings/api-keys/* form routes. Admin-only by default; flip to \
                 on when ready to open up. The /api/api-keys bearer-token endpoints \
                 are not gated."
            }
            Feature::SiteAccess => {
                "Site-wide non-admin access (kill switch). On = oak is open to \
                 everyone. Off = only admins can reach the product UI; non-admins \
                 see a 'we're in admin-only mode' page. Marketing pages and the \
                 JSON API are unaffected."
            }
            Feature::ProfilePage => {
                "Per-user custom profile pages served at <username>.oak.space. \
                 Gates the Settings → Profile Page tab, the toggle POST, and the \
                 subdomain serving path. Admin-only by default; flip to on when \
                 ready to open up. Existing enabled rows stop resolving for \
                 non-admin viewers while the flag is off."
            }
        }
    }

    pub fn all() -> &'static [Feature] {
        &[
            Feature::Releases,
            Feature::Signups,
            Feature::TurnstileVerification,
            Feature::ApiKeys,
            Feature::SiteAccess,
            Feature::ProfilePage,
        ]
    }

    /// Best-effort reverse lookup. Returns `None` for unknown slugs (the
    /// /root handler refuses those before they reach the DB).
    pub fn from_slug(slug: &str) -> Option<Feature> {
        Feature::all().iter().copied().find(|f| f.slug() == slug)
    }
}

/// Env var name the CLI checks to unlock gated subcommands.
pub const CLI_FEATURES_ENV: &str = "OAK_FEATURES";

/// CLI-side feature gate. The web UI keys off the signed-in user; the CLI
/// has no user concept, so we key off `OAK_FEATURES`. Accepts:
///
/// - `1`, `true`, `all` — enable every gated feature
/// - comma-separated slugs (see [`Feature::slug`]), e.g. `releases,ci`
/// - unset / empty / `0` / `false` — every gated feature is hidden
///
/// Matching is case-insensitive and accepts both hyphen and underscore
/// in slugs for ergonomics.
pub fn feature_enabled_in_cli(feature: Feature) -> bool {
    let raw = match std::env::var(CLI_FEATURES_ENV) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "0" | "false" | "no" | "off" => return false,
        "1" | "true" | "yes" | "on" | "all" => return true,
        _ => {}
    }
    let want = feature.slug().to_ascii_lowercase();
    let want_alt = want.replace('-', "_");
    lower.split(',').any(|part| {
        let p = part.trim();
        p == want || p == want_alt
    })
}

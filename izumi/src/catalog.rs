//! The source-kind catalog border — the ONE trait every izumi consumer's
//! kind enum satisfies, plus the [`catalog!`](macro@crate::catalog) macro that
//! authors a compliant enum from a declarative table.
//!
//! CATALOG REFLECTION: `ALL` + the six table methods are the reflection
//! surface tooling / config / tests iterate. The macro generates every method
//! as an exhaustive `match`, so adding a variant without declaring its slug /
//! label / emoji / urgency / auth / cadence is a compile error — the catalog
//! can never drift from the code.

use crate::item::Urgency;

/// A source-kind catalog: a small `Copy` enum where every variant names one
/// data source and carries its table row (slug, label, emoji, default
/// urgency, auth requirement, poll cadence).
///
/// The serde wire form is the kebab slug (a [`catalog!`](macro@crate::catalog)-
/// generated impl serializes via `serialize_str(slug)` and deserializes by
/// slug match) — byte-identical to a `#[serde(rename_all = "kebab-case")]`
/// unit-enum derive when the variant names kebab-case to the slug, which is
/// how the mado `SourceKind` wire form is preserved.
///
/// `Ord` follows DECLARATION order (the macro derives it on the enum as
/// written), so a catalog's authored order is its canonical sort order.
pub trait Catalog:
    Copy
    + Clone
    + Ord
    + PartialOrd
    + Eq
    + PartialEq
    + core::hash::Hash
    + core::fmt::Debug
    + Send
    + Sync
    + serde::Serialize
    + serde::de::DeserializeOwned
    + 'static
{
    /// Every variant, in catalog (declaration) order — the reflection surface.
    const ALL: &'static [Self];

    /// Kebab slug — the stable id-derivation key + serde wire form.
    fn slug(self) -> &'static str;

    /// Human label for config docs / tooling.
    fn label(self) -> &'static str;

    /// One-glyph emoji signal for a board row.
    fn emoji(self) -> &'static str;

    /// Default urgency a fresh item from this source carries (sources may
    /// raise it per-item).
    fn default_urgency(self) -> Urgency;

    /// Whether the source needs a token/credential to return anything (an
    /// unauthed source returns empty, never errors).
    fn needs_auth(self) -> bool;

    /// Default poll cadence in seconds — local/cheap sources poll often,
    /// slow or rate-limited remote ones poll lazily.
    fn default_interval_secs(self) -> u64;

    /// Resolve a slug back to its kind (config parse / round-trip).
    #[must_use]
    fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.slug() == s)
    }
}

/// Author a [`Catalog`] enum from a declarative table.
///
/// ```
/// izumi::catalog! {
///     pub enum SourceKind {
///         GitBranchPr { slug: "git-branch-pr", emoji: "X", label: "git branch and PR", urgency: Low, needs_auth: false, interval_secs: 30 },
///         Safra { slug: "safra", emoji: "Y", label: "safra curated signals", urgency: Normal, needs_auth: false, interval_secs: 60 },
///     }
/// }
/// # fn main() {}
/// ```
///
/// Generates:
///
/// * the enum with `Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord`
///   in DECLARATION order (`Ord` = declaration order matters);
/// * an exhaustive [`Catalog`] impl (`ALL` + the six table methods);
/// * manual serde impls whose wire form is the slug (`serialize_str(slug)`;
///   deserialize matches the slug and errors listing the valid slugs on an
///   unknown one) — byte-identical to a kebab-case unit-enum derive when the
///   variant names kebab-case to the slugs;
/// * a `Display` impl rendering the slug;
/// * a COMPILE-TIME slug-uniqueness check (`const _: () = …`) — a duplicate
///   slug is a build error, not a runtime surprise; and
/// * a `#[cfg(test)] mod __izumi_catalog_tests` verifying slug uniqueness,
///   `from_slug` round-trips, non-empty labels/emoji, positive intervals, and
///   `ALL.len()` matching the variant count.
///
/// The generated test module has a FIXED name — `catalog!` is once-per-module.
/// A second invocation in one module is a compile error (duplicate
/// `__izumi_catalog_tests`); give each catalog its own module.
#[macro_export]
macro_rules! catalog {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$vmeta:meta])*
                $variant:ident {
                    slug: $slug:literal,
                    emoji: $emoji:literal,
                    label: $label:literal,
                    urgency: $urg:ident,
                    needs_auth: $auth:literal,
                    interval_secs: $interval:literal $(,)?
                }
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
        $vis enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $crate::Catalog for $name {
            const ALL: &'static [Self] = &[ $( Self::$variant, )+ ];

            fn slug(self) -> &'static str {
                match self { $( Self::$variant => $slug, )+ }
            }

            #[allow(clippy::match_same_arms)]
            fn label(self) -> &'static str {
                match self { $( Self::$variant => $label, )+ }
            }

            #[allow(clippy::match_same_arms)]
            fn emoji(self) -> &'static str {
                match self { $( Self::$variant => $emoji, )+ }
            }

            #[allow(clippy::match_same_arms)]
            fn default_urgency(self) -> $crate::Urgency {
                match self { $( Self::$variant => $crate::Urgency::$urg, )+ }
            }

            #[allow(clippy::match_same_arms)]
            fn needs_auth(self) -> bool {
                match self { $( Self::$variant => $auth, )+ }
            }

            #[allow(clippy::match_same_arms)]
            fn default_interval_secs(self) -> u64 {
                match self { $( Self::$variant => $interval, )+ }
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> ::core::result::Result<S::Ok, S::Error>
            where
                S: ::serde::Serializer,
            {
                serializer.serialize_str($crate::Catalog::slug(*self))
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> ::core::result::Result<Self, D::Error>
            where
                D: ::serde::Deserializer<'de>,
            {
                struct __IzumiSlugVisitor;
                impl ::serde::de::Visitor<'_> for __IzumiSlugVisitor {
                    type Value = $name;
                    fn expecting(
                        &self,
                        f: &mut ::core::fmt::Formatter<'_>,
                    ) -> ::core::fmt::Result {
                        ::core::write!(f, "one of the catalog slugs:")?;
                        $( ::core::write!(f, " {:?}", $slug)?; )+
                        ::core::result::Result::Ok(())
                    }
                    fn visit_str<E>(
                        self,
                        v: &str,
                    ) -> ::core::result::Result<Self::Value, E>
                    where
                        E: ::serde::de::Error,
                    {
                        match v {
                            $( $slug => ::core::result::Result::Ok(<$name>::$variant), )+
                            other => ::core::result::Result::Err(E::invalid_value(
                                ::serde::de::Unexpected::Str(other),
                                &self,
                            )),
                        }
                    }
                }
                deserializer.deserialize_str(__IzumiSlugVisitor)
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                let slug = $crate::Catalog::slug(*self);
                ::core::write!(f, "{slug}")
            }
        }

        // COMPILE-TIME slug uniqueness: a duplicate slug fails the build here,
        // never at runtime.
        const _: () = {
            const fn __izumi_bytes_eq(a: &[u8], b: &[u8]) -> bool {
                if a.len() != b.len() {
                    return false;
                }
                let mut i = 0;
                while i < a.len() {
                    if a[i] != b[i] {
                        return false;
                    }
                    i += 1;
                }
                true
            }
            const SLUGS: &[&str] = &[ $( $slug, )+ ];
            let mut i = 0;
            while i < SLUGS.len() {
                let mut j = i + 1;
                while j < SLUGS.len() {
                    assert!(
                        !__izumi_bytes_eq(SLUGS[i].as_bytes(), SLUGS[j].as_bytes()),
                        "catalog! slugs must be unique"
                    );
                    j += 1;
                }
                i += 1;
            }
        };

        #[cfg(test)]
        mod __izumi_catalog_tests {
            #[test]
            fn every_slug_is_unique() {
                let mut slugs: ::std::vec::Vec<&str> =
                    <super::$name as $crate::Catalog>::ALL
                        .iter()
                        .map(|k| $crate::Catalog::slug(*k))
                        .collect();
                let n = slugs.len();
                slugs.sort_unstable();
                slugs.dedup();
                assert_eq!(slugs.len(), n, "every catalog slug is unique");
            }

            #[test]
            fn from_slug_round_trips_every_variant() {
                for &k in <super::$name as $crate::Catalog>::ALL {
                    assert_eq!(
                        <super::$name as $crate::Catalog>::from_slug(
                            $crate::Catalog::slug(k)
                        ),
                        ::core::option::Option::Some(k)
                    );
                }
            }

            #[test]
            fn labels_and_emoji_are_nonempty() {
                for &k in <super::$name as $crate::Catalog>::ALL {
                    assert!(!$crate::Catalog::label(k).is_empty());
                    assert!(!$crate::Catalog::emoji(k).is_empty());
                }
            }

            #[test]
            fn intervals_are_positive() {
                for &k in <super::$name as $crate::Catalog>::ALL {
                    assert!($crate::Catalog::default_interval_secs(k) > 0);
                }
            }

            #[test]
            fn all_len_matches_variant_count() {
                assert_eq!(
                    <super::$name as $crate::Catalog>::ALL.len(),
                    [ $( $slug, )+ ].len(),
                    "ALL covers every declared variant exactly once"
                );
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::Catalog;
    use crate::testkit::TestKind;

    #[test]
    fn wire_form_is_the_slug_and_round_trips() {
        // Serialize = serialize_str(slug) — byte-identical to mado's
        // kebab-case unit-enum derive.
        let json = serde_json::to_string(&TestKind::GitBranchPr).unwrap();
        assert_eq!(json, "\"git-branch-pr\"");
        let back: TestKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, TestKind::GitBranchPr);
    }

    #[test]
    fn unknown_slug_errors_listing_the_valid_slugs() {
        let err = serde_json::from_str::<TestKind>("\"no-such-source\"")
            .expect_err("unknown slug must fail");
        let msg = err.to_string();
        assert!(msg.contains("no-such-source"), "names the offender: {msg}");
        assert!(msg.contains("git-branch-pr"), "lists valid slugs: {msg}");
    }

    #[test]
    fn display_renders_the_slug() {
        assert_eq!(TestKind::JiraSprint.to_string(), "jira-sprint");
    }

    #[test]
    fn ord_is_declaration_order() {
        assert!(TestKind::GitBranchPr < TestKind::TendRepos);
        assert!(TestKind::TendRepos < TestKind::GrafanaIncidents);
    }

    #[test]
    fn table_values_match_the_mado_catalog() {
        use crate::item::Urgency;
        assert_eq!(TestKind::GrafanaAlerts.default_urgency(), Urgency::Critical);
        assert_eq!(TestKind::GithubReviewRequested.default_urgency(), Urgency::High);
        assert_eq!(TestKind::JiraSprint.default_urgency(), Urgency::Normal);
        assert_eq!(TestKind::TendRepos.default_urgency(), Urgency::Low);
        assert_eq!(TestKind::RecentDirs.default_urgency(), Urgency::Idle);
        assert!(!TestKind::TendRepos.needs_auth());
        assert!(TestKind::JiraSprint.needs_auth());
        assert_eq!(TestKind::TendRepos.default_interval_secs(), 30);
        assert_eq!(TestKind::JiraSprint.default_interval_secs(), 300);
        assert_eq!(TestKind::GrafanaAlerts.default_interval_secs(), 90);
    }
}
